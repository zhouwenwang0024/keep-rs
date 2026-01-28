# JIT 逻辑修正与日志增强方案

## 背景
代理程序在 PlaceAndTake/PlaceAndMake 场景下的推荐人处理逻辑正确；当前需要修正的是**链下 JIT 机会判断**与**可观测性（日志）**。

## 问题 1：JIT 机会判断逻辑错误（edge 不应参与链下判断）

### 现状
链下 `JitStrategy::maybe_intent()` 用 `edge_ppm` 计算阈值：
- buy_ok：best_ask <= reference * (1 - edge)
- sell_ok：best_bid >= reference * (1 + edge)

### 正确行为
链下只判断是否“价格交叉”即可：
- buy_ok：best_ask < reference_price
- sell_ok：best_bid > reference_price

`edge_ppm` 仅作为参数传给链上代理程序用于下单逻辑，不参与链下机会判断。

### 修改点
文件：`D:\RUST\keep-rs\src\jit\jit_strategy.rs`
- 移除基于 `edge_ppm` 的阈值判断
- 改为“简单交叉判断”

建议逻辑：

```text
let sell_ok = best_bid_price > reference_price;
let buy_ok = best_ask_price < reference_price;
```

## 问题 2：JIT 流程日志不足

### 现状
目前主要在触发时打印 `jit trigger`，缺少“为什么没触发”“流程是否走通”的关键日志。

### 目标
补充低开销日志，帮助快速判断：
- 是否有 Binance 价格输入
- 是否被 staleness/cooldown 等条件过滤
- 是否成功获取 best bid/ask + makers
- 是否发生交叉判断失败
- 是否进入 `proxy_jit` 并走到发送交易路径

### 建议新增日志点（info/debug/trace 组合）

#### 1) Binance 价格更新
文件：`D:\RUST\keep-rs\src\filler.rs`
- 写入 `jit_price_cache` 时打印：market / binance_mid / ts_ms（建议 `debug`，必要时加节流）

#### 2) JIT 判断入口与过滤原因
文件：`D:\RUST\keep-rs\src\jit\jit_strategy.rs`
- 进入 `maybe_intent` 时打印：market / reference_price / best_bid / best_ask
- 被过滤时打印原因（stale / cooldown / invalid price / no book 等）（建议 `debug/trace`）

#### 3) 交叉判断结果
文件：`D:\RUST\keep-rs\src\jit\jit_strategy.rs`
- 打印 buy_ok/sell_ok 与 reference_price（建议 `debug`）

#### 4) 触发后发送链上
文件：`D:\RUST\keep-rs\src\jit\jit_trades.rs`
- 发送前打印 intent 信息（market/ref/edge/makers）（建议 `info`）

### 日志级别建议
- **关键路径**：`info`
- **过滤原因**：`debug`
- **高频细节**：`trace`

## 问题 3：构造交易时缺少对手用户的推荐人信息

### 现状
在本地构造交易时（无论是 JIT 路径还是 ARB 路径），当前代码**只添加了自己的推荐人信息**，没有获取并添加**所有对手用户（makers/counterparties）的推荐人信息**。

这会导致链上执行时出现 `ReferrerNotFound` 错误，因为：
- 代理程序在 `PlaceAndMake` 时需要对手（maker）的推荐人账户
- 代理程序在 `PlaceAndTake` 时需要自己（taker）的推荐人账户
- 但链下代码没有提供所有必要的推荐人账户到 `remaining_accounts`

### 正确行为
无论是 JIT 路径还是 ARB 路径，都应该：
1. **获取所有相关用户的 `UserStats`**（包括自己和所有对手）
2. **检查每个用户是否有推荐人**（`UserStats::is_referred()`）
3. **将所有推荐人的账户添加到 `remaining_accounts`**（包括推荐人的 `User` 账户和 `UserStats` 账户）
4. **去重处理**（多个用户可能有同一个推荐人）

### 修改点

#### JIT 路径
文件：`D:\RUST\keep-rs\src\jit\jit_trades.rs`
- 在 `try_jit` 中，获取所有 maker 的 `UserStats`
- 将 maker 的 `UserStats` 传递给 `proxy_jit`

文件：`D:\RUST\drift-rs\crates\src\lib.rs`
- `proxy_jit` 方法需要接受 `maker_stats: &[UserStats]` 参数
- `build_remaining_accounts_for_proxy` 需要处理所有 maker 的推荐人账户

#### ARB 路径
文件：`D:\RUST\keep-rs\src\arb\arb_trades.rs`（如果存在）
- 同样需要获取所有对手的 `UserStats` 并传递推荐人信息

文件：`D:\RUST\drift-rs\crates\src\lib.rs`
- `proxy_spread_capture` 或其他 ARB 相关方法也需要类似处理

### 实现要点
1. **获取 UserStats**：对于每个对手用户，调用 `user_cache.get_stats_or_fetch()` 获取其 `UserStats`
2. **添加推荐人账户**：在 `build_remaining_accounts_for_proxy` 中，遍历所有 maker 的 `UserStats`，如果 `is_referred()`，则添加：
   - `Wallet::derive_user_account(&referrer, 0)`（推荐人的用户账户）
   - `Wallet::derive_stats_account(&referrer)`（推荐人的统计账户）
3. **去重**：使用 `HashSet<Pubkey>` 确保同一个推荐人只添加一次

## 影响范围
- **keep-rs**：JIT 和 ARB 路径的推荐人信息获取与传递
- **drift-rs**：`proxy_jit`、`proxy_spread_capture` 和 `build_remaining_accounts_for_proxy` 的签名与实现
- **不涉及代理程序改动**（代理程序的推荐人处理逻辑已正确）

## Jito 发送机制说明（修正方案）

### 当前实现（存在问题）

#### 1. 是否启用 Jito 发送
- **条件**：如果环境变量中存在 Jito UUID（`JITO_UUIDS`、`JITO_UUID1`、`JITO_UUID2` 或 `JITO_UUID`），就会启用 Jito 发送
- **位置**：`TxWorker::new()` 中通过 `JitoSender::from_env()` 初始化

#### 2. 发送方式
- **并行发送**：在 `send_tx()` 中使用 `tokio::join!` **同时执行 RPC 发送和 Jito 发送**
- **不是替代关系**：即使 Jito 发送失败，RPC 发送仍会继续执行

#### 3. Bundle 构造方式（问题点）
- 当前实现将小费作为**单独的转账交易**，然后和业务交易打包成 bundle 发送
- 这会导致**Jito 发送使用两笔交易**，而不是在同一笔交易内附加小费

### 正确行为（需要改动）

#### 1. 小费应在同一笔交易内追加
- **不要再发送两笔交易**
- 应在原本发送给 RPC 的业务交易中，**追加一条 transfer 小费指令**
- 这样 **RPC 与 Jito 使用的是同一笔交易**，且包含 tip

#### 2. Jito 发送应使用“带 tip 的业务交易”
- 构造交易时追加 tip instruction
- Jito 发送只发**这单笔交易**（不再额外构造 tip_tx）

### UUID 轮询机制
- **自动轮询**：在 `jito_sender.rs` 的 `next_uuid()` 方法中实现
- **实现方式**：使用 `AtomicUsize` 和取模运算（`idx % uuids.len()`）
- **每次调用**：`send_bundle_base64()` 时都会调用 `next_uuid()` 获取下一个 UUID

### 配置参数
- `JITO_UUIDS`：逗号分隔的 UUID 列表（优先级最高）
- `JITO_UUID1`、`JITO_UUID2`、`JITO_UUID`：单个 UUID（向后兼容）
- `JITO_BUNDLES_URL`：Jito bundle API 端点（默认：`https://tokyo.mainnet.block-engine.jito.wtf/api/v1/bundles`）
- `JITO_TIP_ACCOUNT`：Jito 小费接收账户（默认：`HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe`）
- `JITO_TIP_LAMPORTS`：小费金额（默认：`10_000` lamports）

## 验证步骤
1. 启动 filler 并观察 JIT 流程日志
2. 发生“价格交叉”时应触发 JIT
3. 无交叉时能看到明确的过滤原因日志
4. 触发后能看到发送交易的日志
5. **验证推荐人处理**：检查交易不再出现 `ReferrerNotFound` 错误，确认所有相关用户的推荐人账户都已正确添加到 `remaining_accounts`
6. **验证 Jito 发送**：如果配置了 Jito UUID，应能看到 Jito bundle 发送日志（成功或失败）

## 注意事项
- 日志需要节流或使用 `debug/trace` 以避免高频噪音
- 交叉判断使用严格不等号，避免价格相等时误触发

