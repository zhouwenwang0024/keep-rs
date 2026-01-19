# 变更说明：src/ws_cache.rs

## 目的
- 新增 WS 账户缓存层，用于替代 gRPC `account_map`。
- 提供初始同步与回补路径，保证策略在 WS 模式下仍能拿到 `User` / `UserStats`。

## 主要内容
- 定义 `WsAccountCache`：
  - `users` / `stats` 使用 `DashMap` 并记录 `slot`，便于判断乱序更新。
  - `apply_user_update` 支持可选 `DLOBNotifier`，用于生成订单变化增量。
  - `upsert_stats` 维护 `UserStats` 缓存。
  - `snapshot_users` 提供初始化快照给 liquidator。
  - `get_user_or_fetch` / `get_stats_or_fetch` 提供 RPC 回补并写回缓存。
- 新增同步函数：
  - `sync_user_accounts_ws`：RPC 拉取非 idle 用户并写入缓存，同时触发 DLOB 初始化。
  - `sync_stats_accounts_ws`：RPC 拉取用户统计并写入缓存。

## 设计要点
- 乱序更新直接丢弃并记录日志，避免 DLOB 回退。
- 回补走 `DriftClient::get_account_value`，在缓存缺失时兜底。

