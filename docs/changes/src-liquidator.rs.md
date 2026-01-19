# 变更说明：src/liquidator.rs

## 目的
- 将 liquidator 从 gRPC 订阅改为 WS 订阅，保留原事件驱动逻辑。
- 使用 WS 账户缓存替代 `account_map`，并避免回执依赖。

## 关键改动
- 新增 `WsSubscriptions` 与 `user_cache` 字段，保持 WS 订阅句柄存活。
- `setup_grpc` 替换为 `setup_ws`：
  - RPC 初始同步用户并初始化 DLOB（`sync_user_accounts_ws`）。
  - WS 订阅用户账户（`WebsocketProgramAccountSubscriber`），更新缓存并投递 `GrpcEvent::UserUpdate`。
  - WS 订阅市场与预言机（`subscribe_markets_with_callback` / `subscribe_oracles_with_callback`），转为 `GrpcEvent`。
  - WS slot 订阅用于 DLOB slot/oracle 更新。
  - 预言机解析按市场/解码器重新构造 `Account`，避免共享账户数据导致解码串扰。
- 初始化用户列表改为 `user_cache.snapshot_users()`，不再依赖 `account_map`。
- 交易构建路径改为使用缓存：
  - `try_liquidate_with_match` / `find_top_makers` 改用 `user_cache`。
  - `liquidate_spot` 使用 `get_user_or_fetch` 做 RPC 回补。
  - `LiquidateWithMatchStrategy` 持有 `user_cache`，统一入口。

## 行为变化
- 用户/行情/oracle 数据由 WS 推送驱动，`GrpcEvent` 名称保留但来源切换为 WS。
- 交易回执回调移除，避免 gRPC 依赖。

