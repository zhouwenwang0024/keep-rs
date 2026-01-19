# Swift 订单填充策略

## 目标
尽快将 Swift 签名订单与盘口中可成交的流动性匹配成交，降低滑点和被抢先成交的风险。

## 触发时机
- Swift 订单流收到新的 `SignedOrderInfo`。
- 只有当 DLOB 判断该订单在当前价格下可与 maker/vAMM 发生交叉时才会继续。

## 关键输入
- `SignedOrderInfo`：包含 taker 订单参数、签名、taker 子账户等信息。
- Perp 市场配置与当前 oracle 价格。
- DLOB 订单簿快照（maker 订单、vAMM 交叉标记）。
- `priority_fee` 与 `swift_cu_limit`（来自 `PriorityFeeSubscriber` 与配置）。
- keeper/filler 子账户（交易签名者）。

## 详细流程
1) 从 Swift 订单中取出 `order_params`，并基于当前 oracle 价格调用 `update_perp_auction_params(...)` 补齐拍卖参数。
2) 构建本地 `Order` 结构，按订单类型计算 taker 价格：
   - `Market/Oracle`：用 `calculate_auction_price(...)` 计算拍卖价格。
   - `Limit`：用 `order.get_limit_price(...)` 计算限价价格。
   - 若价格无法计算，直接跳过。
3) 生成 `TakerOrder`，调用 `DLOB.find_crosses_for_taker_order(...)` 查找可成交 cross。
4) 从 cross 中收集 maker 账户列表（排除 taker 自成交），并检查：
   - 若没有 maker 且无 vAMM cross，则跳过。
5) 构建交易：
   - `with_priority_fee(priority_fee, Some(swift_cu_limit))`
   - `place_swift_order(...)`
   - `fill_perp_order(...)`
6) 若最终指令账户数量过多（>= 30），将 CU 上限翻倍以避免超限。
7) 通过 `TxWorker` 发送交易，并记录 `TxIntent::SwiftFill` 用于后续确认与统计。

## 跳过/保护规则
- 无法计算拍卖/限价价格时跳过。
- 避免与自身账户自成交（过滤 maker 列表）。
- 无 maker 且无 vAMM cross 时跳过。

## 性能与费用
- 交易费用与 CU 预算由 `PriorityFeeSubscriber` 估计。
- 指令账户数量过大时提高 CU 上限以提升成功率。

## 可观测性
- 日志会打印订单与 cross 情况。
- `TxWorker` 统计发送/确认/实际成交数（如 `tx_sent`、`fill_expected`、`fill_actual`）。

## 相关代码
- `src/filler.rs`：`FillerBot::run` (swift 分支)、`try_swift_fill`
