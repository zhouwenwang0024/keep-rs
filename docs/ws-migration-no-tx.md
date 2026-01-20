# WS 订阅替换 gRPC 的改造说明（不依赖交易回执）

本说明基于 keep-rs 现有策略逻辑与 drift-rs SDK 实现，给出**改为 WS 订阅**且**放弃交易回执**时的详细改造方案。

---

## 0. 先明确结论

- gRPC 不是“全量推送”，WS 也不是“增量推送”。两者都是“增量更新”，区别在于**入口是否集中**。
- keep-rs 现在依赖 gRPC **一条流**同时提供：User / UserStats / Market / Oracle / Slot / Tx 更新。
- WS 方案在 SDK 中是**多个模块**，你需要把这些模块拼成同样的事件链路。
- 你打算放弃交易回执，因此 WS 方案不再需要“transaction update”，但必须处理 TxWorker 的 pending/指标逻辑。

---

## 1. SDK 中可用的 WS/RPC 组件（可替换 gRPC）

对应 drift-rs 的实现位置（供核对）：

1) **用户全量更新（User）**  
   - `crates/src/usermap.rs`：`GlobalUserMap`  
   - 基于 `WebsocketProgramAccountSubscriber`（程序账户 WS 订阅）

2) **UserStats 更新**  
   - SDK 没有单独封装，需要用 `WebsocketProgramAccountSubscriber` 订阅 `UserStats` 账户。

3) **市场更新（Perp/Spot）**  
   - `DriftClient::subscribe_markets_with_callback(...)`  
   - 内部使用 WS `MarketMap`（`crates/src/marketmap.rs`）

4) **预言机更新（Oracle）**  
   - `DriftClient::subscribe_oracles_with_callback(...)`  
   - 内部使用 `OracleMap`

5) **Slot 更新**  
   - `crates/src/slot_subscriber.rs`：`SlotSubscriber`（WS slot）

6) **交易回执**  
   - WS 方案没有“等价 SDK 封装”，你已决定放弃，可不实现。

---

## 2. 关键设计点：User/UserStats 的缓存来源

### 方案 A：保留 `account_map`（改 SDK）
优点：改动小，keep-rs 原逻辑大多不动。  
难点：WS program subscribe 事件里**没有 lamports/owner**，无法直接调用 `account_map.on_account_fn()`。

若想走这条路，需要**改 SDK**：
1) 修改 `WebsocketProgramAccountSubscriber` 事件结构，保留 lamports/owner/slot。  
2) 在 keep-rs 的 WS 回调里，把数据组装为 `AccountUpdate`，再调用 `account_map.on_account_fn()`。

### 方案 B：使用自建缓存（推荐）
优点：不改 SDK，逻辑可控。  
改动：把 `try_get_account::<User/UserStats>` 的读取来源替换为本地 `DashMap`。

推荐：  
`DashMap<Pubkey, User>` + `DashMap<Pubkey, UserStats>`  
每次 WS 更新时：
- 取旧值 -> `dlob_notifier.user_update(...)`  
- 更新缓存  
依赖这两个缓存替代 `account_map`。

---

## 3. 放弃交易回执后的必改点

目前 keep-rs 的 tx 确认来源是 gRPC `on_transaction`，会触发：  
`TxWorker.confirm_tx` -> 清理 pending + 统计成交。

放弃回执后要避免 pending 永久堆积：

**必须做其一：**
1) **不再记录 pending**：  
   - 在 `TxWorker::send_tx` 里不写 `pending_txs`，  
   - 将 `fill_expected` 等确认类指标降级为“发送级别”指标。  
2) **增加 TTL 清理**：  
   - 定时把 pending 超时条目删掉，  
   - 指标标记为 `unknown` 或 `timeout`。  

如果什么都不改，`pending_txs` 会无限增长。

---

## 4. 逐文件改造清单（WS + 无回执）

### 4.1 `src/filler.rs`

**当前：**
- `setup_grpc(...)` / `subscribe_grpc(...)` 负责 slot + user + tx 回执。

**改为：**
1) 新增 `setup_ws(...)`：
   - 启动 `GlobalUserMap`（User）
   - 启动 `UserStats` WS 订阅（`WebsocketProgramAccountSubscriber`）
   - 启动 `SlotSubscriber`  
2) 在 User 更新回调里：  
   - `dlob_notifier.user_update(pubkey, old_user, new_user, slot)`  
   - 更新 `user_cache`  
3) Slot 回调：  
   - 对市场调用 `dlob_notifier.slot_and_oracle_update(...)`  
   - 把 slot 推到 `slot_rx`（filler 主循环用）  
4) 移除 `on_transaction_update_fn` 回调使用  
5) `try_get_account::<User/UserStats>` 改为读 `user_cache` / `stats_cache`

**会影响的位置：**
`try_swift_fill` / `try_auction_fill` / `try_uncross` 等。

---

### 4.2 `src/liquidator.rs`

**当前：**
- `setup_grpc(...)` 构造 `GrpcEvent` 并驱动事件循环。

**改为：**
1) 新增 `setup_ws(...)`：
   - User 更新：送到 `events_rx`（或直接更新 `users`）  
   - Market 更新：`subscribe_markets_with_callback` -> 送 `GrpcEvent::Perp/SpotMarketUpdate`  
   - Oracle 更新：`subscribe_oracles_with_callback` -> 送 `GrpcEvent::OracleUpdate`
2) `users` 来源由 WS User 订阅驱动  
3) `user_cache` / `stats_cache` 供 liquidate tx 构建使用（替代 `try_get_account`)

---

### 4.3 `src/main.rs`

新增配置开关（示例）：
- `--subscribe-mode ws|grpc`

在 `main()` 里根据模式选择：
- gRPC：走原逻辑  
- WS：调用 `setup_ws` 并启动 bot

---

### 4.4 `drift-rs`（可选）

如需沿用 `account_map`（方案 A），需要改动 SDK：
- `crates/src/websocket_program_account_subscriber.rs`  
  让 ProgramAccountUpdate 包含 lamports/owner，便于拼出 `AccountUpdate`。

若走方案 B（自建缓存），不需要改 SDK。

---

## 5. WS 订阅后仍要保留的初始化同步（RPC）

WS 只提供增量更新。为了避免冷启动缺数据，仍建议保留：
- `sync_user_accounts`  
- `sync_stats_accounts`  
它们用 RPC 拉全量，把 DLOB 与缓存先填满。

---

## 6. 关键风险点（务必处理）

1) **断线重连后数据缺口**  
   - `WebsocketProgramAccountSubscriber` 会重连，但不会补拉丢失数据  
   - 建议断线后触发 `sync_user_accounts` 再构建 DLOB

2) **缓存一致性**  
   - User 更新必须先用旧值更新 DLOB，再覆盖 `user_cache`  
   - 不要同时保留 `account_map` + `GlobalUserMap`（内存翻倍）

3) **放弃回执后的 pending 堆积**  
   - 必须禁用 pending 或增加 TTL 清理

---

## 7. 推荐的最小可行实现顺序

1) 加 `--subscribe-mode ws` 开关  
2) 在 filler 里做 WS User + Slot + DLOB 注入  
3) 在 liquidator 里做 WS Market/Oracle + User 更新  
4) 删除 gRPC transaction 回执回调  
5) 修改 TxWorker：禁用 pending 或 TTL 清理  
6) 加断线后全量 sync（最少 User）

---

## 8. 小结

WS 能做，但不是“替换一行”：
- gRPC 是一条完整事件流  
- WS 必须拼装多个订阅模块  
你已放弃交易回执，改造复杂度会下降，但**缓存/断线/tx pending**仍需要处理。
