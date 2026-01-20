# 交叉机会过滤修改方案（详细版）

## 总体目标
- 只修改“交叉判定”逻辑，不改交易构建、发送、确认、重连等流程
- 所有交叉机会必须满足：
  - 可成交价值 > 100 美金
  - 交叉 BP 扣除 maker 费用后仍 > 0

## 1) 常量与依赖的改动

### 新增常量
位置：`src/filler.rs` 顶部常量区
- `MIN_NOTIONAL_USD = 100`
  - 最低可成交价值（美元）
- `MAKER_FEE_BPS = 1`
  - 每一侧 maker 需要扣除的 bp
- `BPS_DENOMINATOR = 10_000`
  - bp 换算分母

### 新增精度依赖
位置：`use drift_rs::{ ... }` 里
- 引入 `BASE_PRECISION_U64` 和 `QUOTE_PRECISION_U64`
  - 用于把 base * price 转换成 quote 精度进行金额判断

这些是“过滤逻辑”的基础数据，不影响其它流程。

## 2) 新增辅助函数（仅用于过滤）
位置：`src/filler.rs` 中部新增 3 个函数

### 2.1 `calc_fillable_base(taker_size, maker_crosses)`
- 作用：计算实际可成交 base 量
- 逻辑：
  - maker 可成交量 = maker 列表里所有 `fill_size` 合计
  - 可成交量 = min(taker_size, maker_total)
  - 如果 maker_total 为 0（如只有 vAMM），退化为 taker_size

### 2.2 `calc_cross_bps(buy_price, sell_price)`
- 作用：计算交叉 BP
- 公式：
  - `cross_bps = (sell - buy) / buy * 10_000`
- 若 buy 为 0 或 sell <= buy，直接返回 0

### 2.3 `cross_meets_thresholds(buy_price, sell_price, base, maker_count)`
- 作用：统一封装过滤条件
- 包含两道门槛：
  1) 价值门槛
     - `notional_quote = base * buy_price / BASE_PRECISION`
     - `min_notional_quote = 100 * QUOTE_PRECISION`
     - 若小于门槛直接拒绝
  2) 利润门槛
     - `cross_bps = calc_cross_bps(...)`
     - `fee_bps = maker_count * 1`
     - `cross_bps - fee_bps > 0` 才通过

## 3) Swift 交叉过滤改动
位置：`run()` 中 swift 分支，`find_crosses_for_taker_order` 之后

### 原逻辑
- `crosses` 非空就直接 `try_swift_fill`

### 新逻辑
- 先计算价格、可成交量、maker 数量，再判断是否达标：
  - **maker_price**：
    - 优先取 maker 列表第一个订单价格
    - 如果没有 maker 且存在 vAMM，用 vAMM 价格
  - **maker_post_only**：
    - 取 maker 列表第一个订单的 `is_post_only()`
  - **maker_count**：
    - `maker_post_only || has_vamm_cross` 时为 1，否则 0
  - **fillable_base**：
    - `calc_fillable_base(taker_order.size, crosses)`
  - **buy/sell 价格**：
    - 在 taker 价与 maker 价里取低/高
- `cross_meets_thresholds(...) == true` 才会进入 `try_swift_fill`

说明：swift 交叉属于 taker vs maker/vAMM，不存在“双 maker”场景，因此 `maker_count` 只取 0/1。

## 4) Auction/Trigger 交叉过滤改动
位置：`run()` 中 `find_crosses_for_auctions` 之后

### 原逻辑
- 只对 `limiter` 做过滤，剩下直接 `try_auction_fill`

### 新逻辑
- 增加一层 `retain` 过滤：
  - **vamm_price**：根据 taker 方向取 ask 或 bid
  - **maker_price**：
    - 优先取 maker 列表第一个订单价格
    - 若没有 maker 且存在 vAMM，用 vAMM 价格
  - **maker_post_only**：取 maker 列表第一个订单的 `is_post_only()`
  - **maker_count**：`maker_post_only || has_vamm_cross` 时为 1，否则 0
  - **fillable_base**：`calc_fillable_base(taker_order.size, maker_crosses)`
  - **buy/sell 价格**：取低/高
- 只有通过 `cross_meets_thresholds(...)` 的交叉才会进入 `try_auction_fill`

说明：auction/trigger 也是 taker vs maker/vAMM，仍然只有单侧 maker 计费。

## 5) 盘口交叉（uncross）过滤改动
位置：`try_uncross` 内部，best_bid / best_ask 选出后

### 原逻辑
- 只要 bid/ask 存在就直接进入撮合

### 新逻辑
- 在撮合前增加过滤：
  - **buy/sell 价格**：best_bid 与 best_ask 的低/高
  - **fillable_base**：`min(best_bid.size, best_ask.size)`
  - **maker_count**：
    - `best_bid.is_post_only()` + `best_ask.is_post_only()`
    - 取值 0/1/2
  - 通过 `cross_meets_thresholds(...)` 才继续执行

说明：盘口交叉可能是双 maker，因此支持扣 2bp 的情形。

## 6) 为什么只改交叉判定
- 本次需求限定“最小化改动”
- 所有发送、确认、重试、CU 调整逻辑保持原样
- 便于后续继续扩展到自成交或 taker-taker 模式

## 7) 当前实现的限制说明
- maker 价格与 maker_post_only 仅取 maker 列表第一个订单
- 若需更精细的逐笔评估，可在下一步改为遍历 maker 列表
