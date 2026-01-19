# 变更说明：src/filler.rs

## 目的
- 将 filler 从 gRPC 订阅改为 WS 订阅，并放弃交易回执。
- 引入 WS 账户缓存，替代 `account_map` 的同步与读取。

## 关键改动
- 新增 `WsSubscriptions` 与 `user_cache` 字段，保持 WS 订阅句柄存活。
- `setup_grpc` 替换为 `setup_ws`：
  - RPC 初始同步用户与统计（`sync_user_accounts_ws` / `sync_stats_accounts_ws`）。
  - WS 订阅用户与统计账户（`WebsocketProgramAccountSubscriber`）。
  - WS 订阅 slot（`SlotSubscriber`），用于 DLOB slot/oracle 更新与主循环 slot 推送。
  - 订阅市场与预言机（`subscribe_markets` / `subscribe_oracles`）保证行情与 oracle 缓存更新。
- 填单逻辑读取改为 `WsAccountCache`：
  - `try_swift_fill` 使用缓存 + RPC 回补（`get_user_or_fetch` / `get_stats_or_fetch`）。
  - `try_auction_fill` / `try_uncross` 优先缓存，缺失时跳过并告警。
- 关闭阶段调用 `drift.unsubscribe().await`，主动释放 WS 资源（保留 `grpc_unsubscribe` 兼容）。

## 行为变化
- 不再使用 gRPC 交易回执确认路径；`TxWorker::confirm_tx` 不会被回调触发。
- 用户与统计数据来自 WS + RPC 初始同步，减少对 gRPC 的依赖。

