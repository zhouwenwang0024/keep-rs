# Perp 撮合清算策略（Liquidate With Match）

## 目标
当用户处于可清算状态时，通过撮合 top-of-book maker 进行 perp 清算，提高成功率与成交深度。

## 触发时机
- `LiquidatorBot` 在 oracle 更新后重新计算高风险用户。
- 若 `UserMarginStatus` 判定为 liquidatable，则发送至清算工作线程。
- `LiquidateWithMatchStrategy::liquidate_user` 执行 perp 清算逻辑。

## 关键输入
- 用户账户与 `UserMarginStatus`（cross/isolated）。
- DLOB L3 订单簿（仅挑 maker 订单）。
- `MarketState` oracle 价格与可选 Pyth 更新。
- `priority_fee`、`cu_limit`、keeper 子账户。

## 详细流程
1) **隔离仓优先**：
   - 遍历 `status.isolated`，仅当该市场为 `Liquidatable` 才处理。
   - 找到对应 perp 仓位（`isolated_position_scaled_balance != 0`）。
2) **跨保证金补充**：
   - 若 cross margin 为 liquidatable，则选择 `quote_asset_amount` 最大的 perp 仓位。
3) **maker 选择**：
   - 从 DLOB L3 快照中挑选 top 3 maker（只取 maker 订单）。
   - 按仓位方向选择 bids/asks。
4) **Pyth 更新**：
   - 当 Pyth 价格与 oracle 价格不一致时才附带 `post_pyth_lazer_oracle_update`。
5) **交易构建**：
   - `with_priority_fee(priority_fee, Some(cu_limit))`
   - `liquidate_perp_with_fill(market_index, liquidatee, makers)`
   - 若账户 >= 20，CU 上限翻倍。
6) 通过 `TxWorker` 发送 `TxIntent::LiquidateWithFill`。

## 清算任务调度约束
- 清算任务超过 1 秒会被丢弃（防止积压）。
- 同一用户 5 个 slot 内只会尝试一次清算。
- 单次清算执行超时 1 秒会被取消。

## 跳过/保护规则
- 找不到 maker 或 maker 账户拉取失败时跳过。
- 用户/keeper 账户不存在时跳过。

## 可观测性
- `metrics.liquidation_attempts` 记录 perp 清算次数。
- `TxWorker` 统计交易确认与失败原因。

## 相关代码
- `src/liquidator.rs`：`LiquidateWithMatchStrategy::liquidate_perp`、`try_liquidate_with_match`、`spawn_liquidation_worker`
