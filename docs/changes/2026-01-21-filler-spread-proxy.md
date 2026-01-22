# Filler 吃价差 + 代理成功判断：修改思路总结（草案）

本文件仅总结“修改思路”。所有内容均为设计草案，**最终实现需按真实链上行为与账户约束再调整**，包括链上代理程序。

## 目标与边界

- 目标 1：将 `filler` 从“撮合 taker/maker”转为“自有账户吃价差”。
- 目标 2：交易是否成功由链上代理程序判断，失败即回滚，避免浪费费用。
- 边界：以下为参考雏形，最终以实测和链上约束为准。

## 现有雏形参考

- 代理程序参考：`D:\RUST\proxy\jit-proxy`
  - 错误码：`src/error.rs`
  - 成功/失败判断逻辑：`src/instructions/jit.rs`
  - 约束检查示例：`src/instructions/check_order_constraints.rs`
- 吃单逻辑参考：`D:\RUST\test\测试1`
  - 盘口聚合 + 交叉判断：`L2BOOKNEW2.py` 的 `build_intent_if_cross`
  - 下单触发路径：`main1.py` → `ENGINE.arb_with_makers(...)`

## 总体思路

1. **盘口感知 + 交叉判断**
   - 参考 `L2BOOKNEW2.py` 的 top-of-book 聚合。
   - 条件：`best_bid >= best_ask` 视为可吃价差机会。
2. **选择对手账户（makers）**
   - 仅取 top-of-book 对应价位的 makers（按数量限制取前 N 个）。
3. **执行路径**
   - 不再撮合 maker ↔ taker。
   - 自有账户作为 taker，调用链上代理指令吃单。
4. **成功判断**
   - 由代理程序返回 `Ok / ErrorCode` 决定是否成功。
   - 不进行链下“主观成功判断”，避免分叉逻辑。

## Filler 侧预期改动（思路）

### 1) 从“撮合”切到“吃价差”

- 将当前“撮合（fill_perp_order / uncross）”路径改为：
  - 仅在检测到 `bid >= ask` 时触发；
  - 构造“吃单”意图；
  - 调用代理指令完成 taker 成交。

### 2) 触发条件与风控

- 参考 `L2BOOKNEW2.py` 逻辑：
  - 只对 top-of-book 判定；
  - maker 列表来自最优价位；
  - 不满足交叉则不发。
- 额外建议：
  - 过滤过小名义价值；
  - 考虑费用（maker/taker/priority fee）后仍有价差才执行。

### 3) 账户与数据

- 需要高频获取：
  - 用户账户（maker + taker）；
  - 市场/预言机；
  - slot 与价格数据。
- 避免热路径 RPC：优先 WS 缓存。

## 代理程序（链上）改动思路

### 1) 明确成功/失败语义

- 参考 `error.rs` 的错误码：
  - `NoArbOpportunity`
  - `UnprofitableArb`
  - `NoFill`
  - `OrderSizeBreached`
  - `PositionLimitBreached`
- **成功即 `Ok(())`**，其余全部视为失败并回滚。

### 2) 在代理层完成机会判断

- 保持“链上最终判断”，链下只负责提供候选账户。
- 链上判断依据：
  - 盘口价差是否满足；
  - 是否满足仓位约束；
  - 是否有实际成交（fill）。

### 3) 预检查（可选）

- `check_order_constraints` 可作为前置指令：
  - 当仓位/开仓限制不满足时，提前失败；
  - 节省后续 CU 与风险。

## 建议的执行流程（链下）

1. 收到盘口变化 / 订单变化（WS）。
2. 聚合 top-of-book，判断交叉。
3. 选择 top makers（bid/ask 各取前 N）。
4. 构建交易：代理指令（只需要传 makers 和必要账户）。
5. 发送交易 → 链上代理判断成功与否。

## 建议的执行流程（链上代理）

1. 解析 remaining_accounts，加载订单簿与 makers。
2. 扫描可成交机会，计算价差与费用。
3. 校验约束：
   - 仓位上下限；
   - 最小名义；
   - 允许成交的方向/侧。
4. 构造并执行成交（place_and_take / place_and_make 等路径）。
5. 若无成交 → 返回 `NoFill`。

## 风险与注意事项

- **链上成功判断必须唯一且一致**，避免链下分叉。
- **盘口变化极快**，链上判断比链下更可信。
- **费用吞噬价差**，必须纳入阈值模型。
- **maker 列表过大**可能带来 CU 压力，应限制数量。

## 需要确认的关键参数（待定）

- 最小名义（quote）阈值
- 允许的最小价差（bps）
- maker 列表上限
- 代理程序的成功/失败错误码集合

## 后续落地建议

- 先在 `test/测试1` 的路径里验证“吃价差 + 代理判断”闭环；
- 再将逻辑迁移到 `keep-rs` 的 `filler`；
- 最后对代理程序进行参数化与 CU 优化。

