# 总体变更说明（WS 订阅 + 无回执）

## 背景
- 原实现依赖 gRPC 订阅，`account_map` 由 gRPC 写入，策略通过 `try_get_account` 读取。
- 需求是切换为 WS 订阅，并放弃交易回执。

## 关键调整
- **新增 WS 账户缓存**：`WsAccountCache` 缓存 `User/UserStats` + slot，用于替代 gRPC 的 `account_map` 在策略侧的读取。
- **filler/liquidator 改为 WS 订阅**：
  - 用户与统计：`WebsocketProgramAccountSubscriber`（GPA + 过滤器）
  - slot：`SlotSubscriber`
  - 市场/预言机：`subscribe_markets(_with_callback)` / `subscribe_oracles(_with_callback)`
- **DLOB 增量更新保留**：通过 old/new 对比更新订单簿。
- **交易回执移除**：不再使用 gRPC transaction 回调。
- **预言机解析一致性**：同一 oracle pubkey 对不同 market/source 单独解析，避免解码串扰。

## 影响与权衡
- WS GPA 不产出 `AccountUpdate`，因此无法写入 `account_map`，改为本地缓存。
- `try_uncross` 等同步路径没有 RPC 回补，可能在极端情况下跳过部分机会（可后续优化）。

## 变更文件
- `Cargo.toml`：新增 `dashmap`
- `src/ws_cache.rs`：新增 WS 账户缓存
- `src/filler.rs`：WS 订阅替换 gRPC，使用缓存
- `src/liquidator.rs`：WS 订阅替换 gRPC，使用缓存
- `src/main.rs`：新增模块并在退出时释放 WS 订阅

## 建议验证
- `cargo check`
- devnet 启动 filler / liquidator，观察 WS 订阅与 DLOB 更新日志
