# Filler 策略改造规划报告：全类型交叉机会发现（链上订单为主）

> 范围：本报告面向 `keep-rs` 的 `FillerBot`，在不破坏现有 fill/uncross/swift 流程的前提下，新增一条“**全类型交叉机会发现**”路径。  
> 关键约束：当机会涉及 **未触发 trigger 订单**时，仍应 **先触发再处理**（至少在同一笔 tx 中 trigger + 后续动作）。

---

## 1. 现状复盘：Filler 的 3 条路径（链上 + swift）

### 1.1 拍卖/触发撮合（auction crosses）
- 入口：`dlob.find_crosses_for_auctions(...)` → `try_auction_fill(...)`
- 本质：以 “taker ↔ maker” 撮合为中心，输出 `CrossesAndTopMakers { crosses, top_maker_* ... }`
- trigger 处理：`try_auction_fill` 内部会检查 taker 是否 trigger 单，若满足触发条件会在同 tx 中 `trigger_order` 后再 `proxy_spread_capture`

### 1.2 限价穿价解撮合（uncross）
- 入口：`dlob.find_crossing_region(...)` → `try_uncross(...)`
- 本质：仅基于 L3 的 `bids()/asks()`（`trigger_price=None`），因此 **不包含 trigger 单进入排序**

### 1.3 swift 订单流（暂不纳入本次“链上全类型交叉”）
- 入口：swift stream → `try_swift_fill(...)`
- 本报告主要先覆盖链上订单；swift 在后续扩展章节说明如何并入统一框架

---

## 2. 新目标：链上“全类型订单交叉机会”发现

### 2.1 “全类型”的精确定义（基于 SDK/DLOB）
在 `drift-rs` 里，L3 由 `load_orderbook` 将订单放入不同集合，并由 `L3Book::bids/asks` 合流：
- fixed：`bids/asks`（resting limit + market_orders）
- floating：`floating_bids/asks`（floating_limit_orders + oracle_orders）
- trigger：`trigger_bids/asks`（仅当 `trigger_price=Some` 时，以 post-trigger price 参与排序）
- vamm：`vamm_bids/asks`（需要 `perp_market.fallback_price` 计算）

因此，“全类型交叉”应覆盖：
- 限价‑限价（fixed/floating）
- 拍卖‑限价、拍卖‑拍卖（market_orders ↔ 其他）
- 触发‑触发、触发‑限价（trigger 触发后价参与排序）
- VAMM‑订单（vamm fallback 参与排序）

### 2.2 为什么现有 `find_crossing_region` 不足
`find_crossing_region` 调用 `book.bids(..., trigger_price=None)` / `book.asks(..., trigger_price=None)`，**触发单不会进入排序**，因此不可能“全类型”。

---

## 3. 方案总览：新增一条“全类型交叉检测”路径（不替换原路径）

### 3.1 新增核心方法（基于 SDK 合流能力）
在策略层（keep-rs）新增一个检测函数（或模块），直接复用 `L3Book::bids/asks` 的合流排序：

**输入（每个 market）**
- `oracle_price`：当前 oracle/pyth 价格（已经有）
- `perp_market`：用于 vamm/trigger/post-trigger 定价（已经有）
- `trigger_price`：由 `perp_market.get_trigger_price(...)` 计算（已经有）
- `depth`：最多取多少档（建议 32~128，可配置）

**输出**
- `CrossingRegionAll { best_bid, best_ask, crossing_bids, crossing_asks, slot }`
- 其中 `crossing_*` 为 “全类型合流后” 的穿价区域订单集合（`Vec<L3Order>`）

### 3.2 “发现机会”与“处理机会”分层
新增逻辑只负责 **发现**：
- 判断是否交叉：`best_bid.price >= best_ask.price`
- 产出 `CrossingRegionAll`

后续 “如何处理” 仍由既有流程与新增 handler 完成：
- 若包含未触发 trigger 单：先触发（同 tx）再处理
- 否则可选择：
  - 继续走现有 `try_uncross`（保守）
  - 或走新代理指令（arb_perp / jit 等，按你目标）

---

## 4. 未触发 trigger 订单的处理策略（必须满足你的约束）

### 4.1 识别“未触发 trigger”的方式
在 DLOB/L3 里，trigger 订单存放在 `trigger_bids/trigger_asks`，并且：
- 只有当你在 `book.bids/asks` 里传入 `trigger_price=Some(x)`，且满足触发条件时，才会以 **post-trigger price** 参与排序。

因此，“全类型交叉”检测时 **可能出现两种情况**：
1) **交叉区域里已经包含 trigger 单**：说明它在当前 `trigger_price` 下是可触发的（would_trigger=true）  
2) **交叉区域里完全不包含 trigger 单**：可能是没有 trigger 单，或 trigger 还不满足触发条件

为了满足“若机会包含未触发 trigger 订单也应先触发”的要求，需要在处理阶段额外检查：
- 交叉候选订单中是否存在 `OrderKind::TriggerMarket/TriggerLimit`
- 并用链上 User 的 `actual_order.trigger_price/trigger_condition` 再次确认 `can_trigger`

> keep-rs 现有 `try_auction_fill` 已有一套 `can_trigger` 校验 + `trigger_order` 同 tx 的实现，可复用这套逻辑。

### 4.2 触发与后续动作的组合方式
推荐保持与现有实现一致：
- **同一笔 tx**：先 `trigger_order`，再执行后续动作（uncross 或 proxy）
- 若触发不满足（`can_trigger=false`），直接跳过该机会

---

## 5. 模块改造点（建议改哪些文件）

### 5.1 `src/filler.rs`
- 新增：`find_crossing_region_all_types` 的调用（每 market / 每 slot）
- 新增：基于全类型 crossing 的机会过滤（notional、bps、限速）
- 新增：将机会分流到对应 handler：
  - trigger 相关 → 新 handler（触发 + 处理）
  - 非 trigger → 走现有 `try_uncross` 或新 proxy handler

### 5.2 `src/filler_trades.rs`
- 新增：`try_uncross_all_types(...)`（或扩展现有 `try_uncross` 以接受全类型 crossing）
- 新增：当发现 trigger 单参与机会时，复用 `trigger_order` 逻辑（参考 `try_auction_fill` 现有实现）
- 若后续要走代理程序（arb_perp），在此处新增对应的 tx builder 逻辑更合适（与交易构造解耦）

### 5.3 `src/util.rs`
- 新增/扩展：`TxIntent`（例如 `AllCrossDetect`、`AllCrossHandle`、`TriggerThenProxy`）
- 指标：记录“检测到的交叉数量、含 trigger 的交叉数量、触发成功率”

### 5.4 （可选）新增 `src/crossing_all.rs`
- 放置纯算法与数据结构：
  - `CrossingRegionAll`
  - `find_crossing_region_all_types(...)`
  - 过滤与去重工具
- 目的：避免 `filler.rs` 继续膨胀

---

## 6. 新方法的伪代码（基于 SDK 合流）

```
fn find_crossing_region_all_types(
  dlob: &DLOB,
  market_index: u16,
  oracle_price: u64,
  perp_market: &PerpMarket,
  trigger_price: u64,
  depth: usize,
) -> Option<CrossingRegionAll> {
  let book = dlob.get_l3_snapshot(market_index, MarketType::Perp);
  let mut bids = book.bids(Some(oracle_price), Some(perp_market), Some(trigger_price)).peekable();
  let mut asks = book.asks(Some(oracle_price), Some(perp_market), Some(trigger_price)).peekable();
  let best_bid = bids.peek()?.price;
  let best_ask = asks.peek()?.price;
  if best_bid < best_ask { return None; }
  let crossing_bids = bids.take(depth).take_while(|b| b.price >= best_ask).cloned().collect();
  let crossing_asks = asks.take(depth).take_while(|a| a.price <= best_bid).cloned().collect();
  if crossing_bids.is_empty() || crossing_asks.is_empty() { return None; }
  Some(CrossingRegionAll { slot: book.slot, crossing_bids, crossing_asks, best_bid, best_ask })
}
```

---

## 7. 行为与现有路径的关系（避免重复与冲突）

### 7.1 不替换 `find_crosses_for_auctions`
- 现有拍卖/触发撮合路径继续用于 “可执行撮合”  
- 新增全类型交叉检测用于 “更广义机会发现/代理”

### 7.2 与 `try_uncross` 的关系
- 当前 `try_uncross` 只吃 `CrossingRegion`（不含 trigger）
- 需要新增 `CrossingRegionAll` 或在 `try_uncross` 内部重新用 `bids/asks(Some(trigger_price))` 计算

---

## 8. 交付顺序（推荐）

1) 新增 `CrossingRegionAll` + `find_crossing_region_all_types`（纯检测，不发交易）  
2) 在 `filler.rs` 打印检测日志（含 best_bid/best_ask、是否含 trigger）  
3) 新增 handler：若含 trigger 且可触发 → 构造 tx：trigger + 后续动作  
4) 再将“后续动作”从 `try_uncross` 升级为代理指令（arb_perp）或保持原 `uncross`  
5) 加入指标与限频，避免日志刷屏
