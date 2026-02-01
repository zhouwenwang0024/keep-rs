# Filler 网络请求说明与修改规划（执行前文档）

## 目标与约束

- 启动阶段：允许拉取账户数据以构建完整缓存
- 交易构造/发送阶段：尽量减少额外 RPC，提升速度
- 账户缓存缺失：必须输出日志，并跳过该笔交易
- blockhash 缓存缺失：必须输出告警

## 现状概览（关键网络请求）

1) 账户/Stats 兜底拉取
- `get_user_or_fetch` / `get_stats_or_fetch` 在缓存缺失时走 RPC
- 当前发生在交易路径（swift/onchain）与启动预热

2) 发送交易路径
- 当前 `TxWorker` 在发送前显式调用 `drift.rpc().get_latest_blockhash()`（RPC）
- 然后调用 `sign_and_send_with_config` 发送交易（RPC）

3) `sign_and_send_with_config` 行为
- 传 `None` 时，SDK 内部会尝试读取 blockhash 缓存；缓存缺失则回退 RPC
- 发送交易必然走 RPC

## 变更策略（按你的要求调整）

### A. 启动阶段保持完整预热（允许 RPC）

- 保留 `filler.rs` 启动阶段的 `get_user_or_fetch`
- 目的：确保运行前缓存充足，提升后续交易速度

### B. 交易路径禁止缺失时兜底拉取（加日志并跳过）

- `try_swift_fill` / `try_onchain_cross` 等交易路径改为仅缓存读取：
  - `get_user` / `get_stats`
  - 缺失立即 `log::warn!` 并 `return/continue`
- 目的：避免每笔交易上额外 RPC，缩短构造时间

### C. blockhash 只用缓存为主（加告警）

- 去掉 `TxWorker` 内显式 `drift.rpc().get_latest_blockhash()`
- 调用 `sign_and_send_with_config(tx, None, ...)`
- 在 `get_latest_blockhash()` 内统一告警：
  - 缓存缺失时输出 `log::warn!`
  - 然后回退到 RPC 获取 blockhash

### D. SDK 调整点

- 直接在 `get_latest_blockhash()` 内增加缓存缺失告警

## 影响评估

- 启动阶段网络负载不变（仍会预热）
- 交易路径 RPC 显著减少（账户兜底拉取被禁止）
- blockhash 在缓存缺失时仍可回退 RPC，但会有告警提示

## 验证建议

- 启动后立即触发交易：应不再出现账户兜底拉取 RPC
- 手动关闭 blockhash 订阅：应出现 blockhash 缓存缺失告警
- 账户缓存缺失：应输出 warn 并跳过，不发送交易
