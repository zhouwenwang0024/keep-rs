# gRPC 与 WS 订阅说明（keep-rs / drift-rs）

本说明回答三个问题：
1) gRPC 订阅是不是“回调”？  
2) WS 是否可以一个连接订阅多种数据源？  
3) 为什么 keep-rs 不能“直接换成 WS 就完事”？  

---

## 1) gRPC 订阅是否是回调？

是的。SDK 的 gRPC 订阅接口是回调式的：
- `GrpcSubscribeOpts::on_slot(...)`
- `GrpcSubscribeOpts::on_account(...)`
- `GrpcSubscribeOpts::on_oracle_update(...)`
- `GrpcSubscribeOpts::on_transaction(...)`
- `GrpcSubscribeOpts::on_block_meta(...)`

在 keep-rs 中：
- `src/filler.rs`：gRPC 回调更新 DLOB、推送 slot、触发 tx confirm。  
- `src/liquidator.rs`：gRPC 回调转成 `GrpcEvent`，驱动清算事件循环。  

结论：gRPC 在当前实现里就是“回调驱动的推送流”。

---

## 2) WS 是否可以一个连接订阅多种数据源？

**可以**。Solana WebSocket 支持同一连接订阅多个主题（account / program / slot / signature 等）。  
但在 SDK 层是否“复用同一个连接”，取决于实现：

- `drift-rs` 里 `MarketMap` / `OracleMap` / `AccountMap` / `SlotSubscriber` 共享同一个 `PubsubClient`（复用连接）。  
- `GlobalUserMap` 使用 `WebsocketProgramAccountSubscriber`，内部会**新建 `PubsubClient`**，等于额外开连接。  

结论：**协议允许复用，但 SDK 当前实现不是全统一复用**。  
如果你想“一条 WS 连接订阅全部”，需要改 SDK 或自己做连接复用层。

---

## 3) 为什么 keep-rs 不能“直接换成 WS 就完事”？

因为 gRPC 在 keep-rs 中是“一条完整事件流”，而 WS 在 SDK 中是“多个模块”。  
keep-rs 依赖 gRPC 同时提供：
1) 用户更新（User）  
2) 用户统计（UserStats）  
3) 市场更新（Perp/Spot Market）  
4) 预言机更新（Oracle）  
5) Slot 更新  
6) 交易回执（Tx Update）  

WS 方案需要你把这些模块**重新拼起来**并手动喂给逻辑层：  
否则 DLOB、清算、交易确认会断链。

---

## 4) 若切换 WS，需要替换的模块（映射表）

| 功能 | gRPC 当前实现 | WS/RPC 方案 |
| --- | --- | --- |
| 用户更新 | gRPC `on_account(User)` | `GlobalUserMap` 或 `WebsocketProgramAccountSubscriber` |
| UserStats | gRPC `statsmap_on()` | `WebsocketProgramAccountSubscriber` 或 RPC 轮询 |
| 市场更新 | gRPC `on_account(Perp/Spot)` | `subscribe_markets_with_callback` |
| 预言机更新 | gRPC `on_oracle_update` | `subscribe_oracles_with_callback` |
| Slot 更新 | gRPC `on_slot` | `SlotSubscriber` |
| 交易回执 | gRPC `on_transaction` | WS `signature_subscribe` 或 RPC 轮询 |

---

## 5) 为什么 WS 可能占用更多内存？

不是“WS 必然更大”，而是**如果你同时保留两套缓存，就会翻倍**。

- gRPC 模式下 `account_map` 已经缓存了全量 User/UserStats。  
- 如果你改 WS 还再启用 `GlobalUserMap`，等于**两套 User 缓存**。  

正确做法是选一套缓存作为“唯一真相”，不要双缓存。

---

## 6) 推荐改造路径（最稳）

1) **新增订阅模式开关**：`grpc` / `ws`。  
2) **WS 模式下初始化**：  
   - `GlobalUserMap`（User）  
   - `WebsocketProgramAccountSubscriber`（UserStats）  
   - `subscribe_markets_with_callback`（Perp/Spot）  
   - `subscribe_oracles_with_callback`（Oracle）  
   - `SlotSubscriber`（Slot）  
3) **改事件注入**：  
   - User 更新 -> `DLOBNotifier.user_update`  
   - Slot 更新 -> `slot_and_oracle_update` + 通知 filler 扫描  
   - Market/Oracle 更新 -> 维持 liquidator `MarketState`  
4) **交易确认**：  
   - 新增 WS signature 订阅 或 RPC 轮询确认  

---

## 7) 关键现实：gRPC 封装更完整

SDK 里 gRPC 是“一站式订阅回调”，而 WS 是“拼装模块”。  
因此切换 WS **不是无法做，而是需要更大改动**。
