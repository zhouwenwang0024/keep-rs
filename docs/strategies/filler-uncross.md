# 盘口去交叉策略（Limit Uncross）

## 目标
当盘口出现买一价 >= 卖一价的交叉时，通过主动撮合消除交叉，恢复盘口有效性。

## 触发时机
- 在 `FillerBot::run` 的 slot 分支里，当 `slot % 2 == 0` 时执行。
- 通过 `DLOB.find_crossing_region(...)` 检测盘口交叉区域。

## 关键输入
- `CrossingRegion`（包含交叉的 bids/asks 列表）。
- `priority_fee` 与 `fill_cu_limit`。
- keeper/filler 子账户。

## 详细流程
1) 从 `CrossingRegion` 取出 best bid 与 best ask。
2) 若任一为空则停止。
3) 构造 maker 列表：
   - 对 best ask：取 top 5 crossing bids 作为 maker。
   - 对 best bid：取 top 5 crossing asks 作为 maker。
   - 排除与 taker 同一 user 的订单。
4) 遍历两个方向（best ask/best bid）作为 taker：
   - 若 taker 是 post-only，跳过。
   - 拉取 taker 的 `User` 与 `UserStats`。
   - 构建 `fill_perp_order(...)` 交易并附带 `priority_fee`。
   - 若指令账户 >= 40，则 CU 上限提升到 `cu_limit * 2.5`。
5) 通过 `TxWorker` 发送 `TxIntent::LimitUncross`。

## 跳过/保护规则
- best bid 或 best ask 缺失时跳过。
- post-only taker 直接跳过。
- maker 列表为空会导致撮合失败，因此不发送。

## 性能与费用
- 每 2 个 slot 执行一次，属于低频保护机制。
- 账户列表较长时自动增加 CU。

## 可观测性
- 日志打印交叉盘口和处理结果。
- `TxWorker` 统计发送与确认结果。

## 相关代码
- `src/filler.rs`：`FillerBot::run` (slot 分支)、`try_uncross`
