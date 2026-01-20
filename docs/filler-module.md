# Filler 模块详细说明

本文解释 `keep-rs/src/filler.rs` 的职责、主要流程，以及为什么日志里经常出现 `slot=1`。

## 1. 模块职责概览

`FillerBot` 是做市/撮合机器人，核心任务是：
1) 订阅用户、市场、预言机、slot、Swift 订单等数据源。  
2) 维护 DLOB（订单簿逻辑），持续寻找可成交订单。  
3) 构建并发送 fill/uncross 交易。  

入口文件：`keep-rs/src/filler.rs`。

## 2. 主要结构体与字段

### 2.1 `FillerBot`

- `drift`: DriftClient，RPC/WS/交易都通过它完成。  
- `dlob`: DLOB 订单簿，用于撮合与找交叉。  
- `slot_rx`: slot 更新 channel（来自 WS slot 订阅）。  
- `swift_order_stream`: Swift 订单流。  
- `limiter`: 简单的订单去重/节流器。  
- `market_ids`: 当前参与市场集合。  
- `priority_fee_subscriber`: 优先费订阅器。  
- `pyth_price_feed`: Pyth Lazer 价格流。  
- `user_cache`: WS 用户/统计缓存。  
- `_ws_subscriptions`: 保存订阅句柄，防止订阅被 drop。  

### 2.2 `WsSubscriptions`

保存订阅句柄：
- `user_unsub`：User 账户订阅句柄  
- `stats_unsub`：UserStats 订阅句柄  
- `slot_subscriber`：SlotSubscriber 实例  

### 2.3 `TxWorker` / `TxSender`

独立线程发送交易，避免主循环阻塞：
- `TxSender::send_tx` 把交易任务发送到工作线程  
- `TxWorker::send_tx` 真正调用 RPC 发交易  

## 3. 启动流程（FillerBot::new）

入口：`keep-rs/src/filler.rs`。

1) 初始化 DLOB 与 TxWorker  
2) 根据配置选择市场  
   - `--all-markets`：所有 perp 市场  
   - 否则用 `--market-ids`  
   - 过滤 `bet` 和 `Initialized` 市场  
3) 订阅优先费  
4) 订阅 Swift 订单流  
5) 启动 WS 订阅（`setup_ws`）  
6) 预热自身账户  
7) 启动 Pyth Lazer 价格流（需要 `PYTH_LAZER_TOKEN`）  

## 4. 主循环（FillerBot::run）

主循环是 `tokio::select!`，有三个分支：

### 4.1 Swift 订单分支
流程：
1) 收到新 Swift 订单  
2) 读取市场/预言机价格  
3) 调用 `update_perp_auction_params` 填充 auction 参数  
4) 构造 `Order`（注意 `slot: slot + 1`）  
5) 计算价格 `calculate_auction_price(...)`  
6) DLOB 寻找 cross  
7) 找到后进入 `try_swift_fill(...)`  

### 4.2 Slot 分支
流程：
1) 从 `slot_rx.recv()` 获取最新 slot  
2) 更新 `slot` 变量  
3) 用最新 slot 更新 DLOB oracle  
4) 遍历市场查找 auction/uncross  

### 4.3 Pyth 分支
更新 `pyth_oracle_prices`，用于更精细的定价。

## 5. WS 订阅（setup_ws）

入口：`keep-rs/src/filler.rs`。

1) 创建 DLOB notifier 与 user_cache  
2) RPC 同步初始 User / UserStats  
3) 订阅 markets / oracles  
4) 订阅 User / UserStats 账户  
5) 订阅 slot  
   - slot 回调里把 slot 推到 `slot_tx`  

## 6. 交易路径

### 6.1 `try_swift_fill`
1) 拉取/缓存 taker 与 maker 账户  
2) 构建交易：`place_swift_order + fill_perp_order`  
3) 账户过多时提升 CU  
4) 通过 TxWorker 发送  

### 6.2 `try_auction_fill`
1) 获取 taker/maker/stats  
2) 处理 trigger 订单  
3) 构建 `fill_perp_order`  
4) 发送交易  

### 6.3 `try_uncross`
1) 取 crossing bids/asks  
2) 构建 `fill_perp_order`  
3) 发送交易  

## 7. 为什么日志里总是 `slot=1`

**关键点：**

1) `slot` 变量初始值是 0  
   - 位置：`keep-rs/src/filler.rs`（`let mut slot = 0;`）  

2) Swift 订单分支大量使用 `slot + 1`  
   - 例如：  
     - `Order { slot: slot + 1, ... }`  
     - `find_crosses_for_taker_order(slot + 1, ...)`  

**因此：**
- 如果 `slot_rx.recv()` 没收到更新，`slot` 就一直是 0。  
- 于是日志里 `MakerCrosses { slot: 1, ... }` 就一直显示 1。  

### 7.1 这和价格计算有关吗？
有关系。`slot` 变量会用于：
- `try_get_mmoracle_for_perp_market(..., slot)`  
- `calculate_auction_price(..., slot + 1, ...)`  

如果 slot 一直没更新，理论上会影响本地估价与触发逻辑。

### 7.2 日志里的 `observed_slot=0` 是什么？
`observed_slot` 来自 `TxIntent::slot()`：  
对 `SwiftFill` 这类 intent，`slot()` 返回 `None`，日志用 `unwrap_or(0)` 打印，所以一直是 0。  

### 7.3 为什么可能一直收不到 slot？
常见原因有两类：

1) **WS 连接/订阅问题**  
   - `SlotSubscriber` 没收到 slot 更新  
   - RPC 节点不支持或被限流  

2) **事件循环被快速流量压制**  
   - `tokio::select!` 用了 `biased;`  
   - 如果 swift 订单流量很大，slot 分支可能长期抢不到执行机会  

### 7.4 如何验证 slot 是否更新
可以用两种方法确认：

1) 临时把日志级别提高  
   - `RUST_LOG=filler=trace`  
   - 会打印 `⏱️ checked fills at {slot}`，看 slot 是否变化  

2) 在 slot 回调里加一条 info 日志  
   - 当 `slot_tx.try_send(new_slot)` 成功时打印 `new_slot`  
   - 这样能直接确认订阅是否在工作  

## 8. 总结（你需要记住的 3 件事）

1) **日志里的 `observed_slot=0` 不是计算 slot**，只是日志默认值。  
2) **`slot=1` 来自 `slot + 1`，说明 slot 没更新**。  
3) 如果 slot 长时间不更新，应该重点排查 WS 订阅和 `biased` 选择器。  
