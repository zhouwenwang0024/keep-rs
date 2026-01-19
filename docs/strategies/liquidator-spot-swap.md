# Spot Swap 清算策略（Liquidate Spot With Swap）

## 目标
对可清算的 spot 借款仓位，通过 swap 路由把抵押资产换成负债资产进行清算。

## 触发时机
- `LiquidateWithMatchStrategy::liquidate_user` 中启用 `use_spot_liquidation` 时执行。

## 关键输入
- 用户 spot 仓位（borrow/deposit）。
- `MarketState` spot 市场配置与 oracle 价格。
- Jupiter/Titan 报价结果。
- keeper 子账户与 authority 的 ATA。

## 详细流程
1) 遍历用户的 borrow 仓位（`balance_type = Borrow` 且非 available）。
2) 通过 `spot_market` 计算借款 token 数量，跳过 dust（< 2 * `min_order_size`）。
3) 选取用户最大 deposit 仓位作为抵押资产。
4) 计算 keeper authority 的 in/out ATA。
5) 并发请求 Jupiter 与 Titan 报价：
   - 若两者均失败，直接跳过。
   - 若两者均成功，选 `out_amount` 更高的 route。
6) 构建 swap 清算交易：
   - `jupiter_swap_liquidate(...)` 或 `titan_swap_liquidate(...)`
   - 添加 `priority_fee`，CU 预算固定为 400_000（代码中写死）。
7) 通过 `TxWorker` 发送 `TxIntent::LiquidateSpot`。

## 跳过/保护规则
- 无可用抵押资产时跳过。
- 报价全部失败时跳过。
- dust 级别仓位跳过。

## 性能与费用
- 报价延迟记录在 `swap_quote_latency_ms`。
- Jupiter/Titan 失败次数分别计数。

## 可观测性
- `metrics.jupiter_quote_failures` / `metrics.titan_quote_failures`。
- `TxWorker` 跟踪交易确认与结果。

## 相关代码
- `src/liquidator.rs`：`LiquidateWithMatchStrategy::liquidate_spot`
