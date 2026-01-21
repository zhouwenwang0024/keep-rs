# 交叉机会过滤修改方案（详细版）

## 0) 背景与目标
- 只修改“交叉判定/过滤”逻辑，不改交易构建、发送、确认、重连等流程。
- 交叉机会必须同时满足：
  - 可成交价值 > 100 美金。
  - 交叉价差 BP 扣除 maker 费用 BP 后仍 > 0。
- maker 费用仅在对手为 maker 时生效（post-only 或 vAMM）。

## 1) 原始行为（未修改前）
**Swift**：`find_crosses_for_taker_order` 只要返回非空 `crosses`，直接进入 `try_swift_fill`；没有价格差、手续费、最小金额过滤。  
**Auction/Trigger**：`find_crosses_for_auctions` 后只做 `limiter` 过滤，剩余全部进入 `try_auction_fill`；没有价格差、手续费、最小金额过滤。  
**Uncross**：选出 `best_bid` / `best_ask` 后直接尝试撮合；仅跳过 `taker_order.is_post_only()` 的情况；没有价格差、手续费、最小金额过滤。

## 2) 新增统一过滤规则（所有交叉共用）
**可成交价值门槛**：  
`notional_quote = base * buy_price / BASE_PRECISION_U64`  
`min_notional_quote = 100 * QUOTE_PRECISION_U64`  
若 `notional_quote <= min_notional_quote`，直接过滤。

**交叉价差 BP**：  
`cross_bps = (sell_price - buy_price) / buy_price * 10_000`  
若 `sell_price <= buy_price` 或 `buy_price == 0`，视为 `cross_bps = 0`。

**maker 费用 BP**：  
`fee_bps = maker_count * 1`  
`maker_count` 仅统计对手为 maker 的次数（post-only 或 vAMM）。

**最终门槛**：  
`cross_bps - fee_bps > 0` 才认为有利可图。

## 3) 新增常量与精度依赖
**新增常量**（位于 `src/filler.rs` 顶部）：
- `MIN_NOTIONAL_USD = 100`
- `MAKER_FEE_BPS = 1`
- `BPS_DENOMINATOR = 10_000`

**新增精度依赖**（`use drift_rs::math::constants`）：
- `BASE_PRECISION_U64`
- `QUOTE_PRECISION_U64`

这些仅服务过滤计算，不影响撮合/发送流程。

## 4) 新增辅助函数（仅用于过滤）
**`calc_fillable_base(taker_size, maker_crosses)`**  
作用：估算“可成交 base 数量”。  
逻辑：  
`maker_total = sum(maker fill_size)`  
`fillable = min(taker_size, maker_total)`  
若 `maker_total == 0`（例如只有 vAMM），退化为 `taker_size`。

**`calc_cross_bps(buy_price, sell_price)`**  
作用：计算交叉价差 BP。  
逻辑：`(sell - buy) / buy * 10_000`，并处理 `buy==0` 或 `sell<=buy` 的兜底。

**`cross_meets_thresholds(buy_price, sell_price, base, maker_count)`**  
作用：统一封装“金额门槛 + 利润门槛”的判断。

## 5) Swift 交叉过滤（原逻辑 vs 新逻辑）
位置：`run()` 的 swift 分支，`find_crosses_for_taker_order` 之后。

### 原逻辑
只要 `crosses` 非空直接调用 `try_swift_fill`。  
没有计算 maker 价格、没有手续费扣减、没有最小成交金额判断。

### 新逻辑
先计算价格、可成交数量、maker 数量，然后应用统一过滤：  
`cross_meets_thresholds(...) == true` 才进入 `try_swift_fill`。

### 变量说明（逐项对比）
**maker_price**：  
原来：不需要显式计算，`try_swift_fill` 直接用 DLOB 的 maker 列表撮合。  
现在：为了计算价差，优先取 maker 列表第一个订单价格；若 maker 为空且 `has_vamm_cross == true`，用 vAMM 侧价格作为参考价。

**maker_post_only**：  
原来：只在撮合指令内部隐式处理。  
现在：显式读取 maker 列表第一个订单的 `is_post_only()`，用于判断是否需要扣 maker 费用。

**maker_count**：  
原来：没有 maker 计数概念。  
现在：`maker_post_only || has_vamm_cross` 为 1，否则为 0。  
说明：Swift 属于 taker vs maker/vAMM，不存在“双方都是 maker”的情况。

**fillable_base**：  
原来：不做可成交数量估算，直接尝试撮合。  
现在：用 `calc_fillable_base` 计算可成交 base，用于最小金额过滤。

**buy/sell 价格**：  
原来：不计算交叉 BP。  
现在：取 taker 与 maker 价格的较低值为买价、较高值为卖价（不使用中间价），据此计算 `cross_bps`。

## 6) Auction/Trigger 交叉过滤（原逻辑 vs 新逻辑）
位置：`run()` 的 `find_crosses_for_auctions` 之后。

### 原逻辑
仅对 `crosses` 做 `limiter` 过滤，剩余全部进入 `try_auction_fill`。

### 新逻辑
在 `limiter` 之后追加一层 `retain` 过滤：  
只有 `cross_meets_thresholds(...) == true` 的交叉才保留。

### 变量说明（逐项对比）
**vamm_price**：  
原来：不需要显式计算。  
现在：根据 taker 方向取 vAMM 的 ask/bid 作为参考价（taker 做多取 ask，做空取 bid）。

**maker_price**：  
原来：不需要显式计算。  
现在：优先取 maker 列表第一个订单价格；若 maker 为空且 `has_vamm_cross == true`，使用 vAMM 价格。

**maker_post_only / maker_count / fillable_base / buy/sell**：  
与 Swift 部分相同逻辑，只是数据来源为 `CrossesAndTopMakers` 里的 `taker_order` 与 `maker_crosses`。

说明：Auction/Trigger 也是 taker vs maker/vAMM 结构，因此 `maker_count` 只会是 0 或 1。

## 7) Uncross 交叉过滤（原逻辑 vs 新逻辑）
位置：`try_uncross` 内部，`best_bid` / `best_ask` 选出之后。

### 原逻辑
只要 bid/ask 存在就进入撮合；仅跳过 `taker_order.is_post_only()` 的情况。  
没有价差、手续费、最小金额过滤。

### 新逻辑
在撮合前先执行统一过滤：  
`cross_meets_thresholds(...) == true` 才继续撮合，否则直接返回。

### 变量说明（逐项对比）
**buy/sell 价格**：  
原来：不计算价差。  
现在：`buy_price = min(best_bid.price, best_ask.price)`，`sell_price = max(best_bid.price, best_ask.price)`。

**fillable_base**：  
原来：不估算可成交数量。  
现在：`min(best_bid.size, best_ask.size)` 作为可成交 base。

**maker_count**：  
原来：不考虑双方是否为 maker。  
现在：`best_bid.is_post_only()` 与 `best_ask.is_post_only()` 各记 1，合计 0/1/2。  
说明：盘口交叉可能是“双 maker”，因此支持扣 2bp。

## 8) 当前实现的取舍与限制
- maker 价格与 post-only 状态只取 maker 列表的第一个订单（DLOB 已按最佳价格排序）。  
- 价格差计算使用买价（较低价）作为基准，避免使用中间价。  
- 仅修改交叉过滤逻辑，撮合路径、发送逻辑、确认逻辑完全保持原样。  

## 9) 后续可扩展方向（本次不做）
- 更精细地遍历 maker 列表计算“逐笔成交金额与真实价差”。  
- 增加 taker-taker 交叉的自成交策略与风控限制。

