# DLOB 内全部订单类型的“交叉”清单（增强版）

> 目标：在 **不改 DLOB 内部语义** 的前提下，基于 SDK（drift‑rs）的现有结构，设计一个“全类型交叉判断”方法，能够识别 **拍卖‑拍卖、拍卖‑限价、触发‑触发、浮动‑浮动、VAMM‑订单** 等所有类型交叉。

---

## 1) DLOB 内的订单类型（按来源/子簿划分）

### 1.1 Resting Limit Orders
- 来源：`orderbook.resting_limit_orders`
- 进入：`L3Book.bids / L3Book.asks`
- 可直接参与交叉判断（固定价格）

### 1.2 Floating Limit Orders
- 来源：`orderbook.floating_limit_orders`
- 进入：`L3Book.floating_bids / L3Book.floating_asks`
- 价格由 `oracle_price` 动态计算

### 1.3 Market Orders（拍卖结束/触发后进入）
- 来源：`orderbook.market_orders`
- 进入：`L3Book.bids / L3Book.asks` 或 `vamm_bids / vamm_asks`
- 价格通过 `order.get_price(slot, oracle_price, tick)` 计算

### 1.4 Oracle Orders
- 来源：`orderbook.oracle_orders`
- 进入：`floating_bids / floating_asks` 或 `vamm_bids / vamm_asks`
- 价格由 `oracle_price` + 订单偏移计算

### 1.5 Trigger Orders（触发单）
- 来源：`orderbook.trigger_orders`
- 进入：`trigger_bids / trigger_asks`
- **仅在传入 `trigger_price` 时**才纳入 `bids()/asks()` 排序

### 1.6 VAMM Orders（系统价）
- 来源：`market_orders / oracle_orders` 中价格为 0 的订单
- 进入：`vamm_bids / vamm_asks`
- 需要 `perp_market.fallback_price(...)` 计算有效价

---

## 2) “交叉判断”覆盖情况（当前 DLOB 方法）

### 2.1 `find_crossing_region`
- 使用 `bids(..., trigger_price=None)` / `asks(..., trigger_price=None)`
- ✅ 覆盖：resting limit / floating / market / oracle / vamm（在排序里）
- ❌ 不覆盖：**trigger orders**

### 2.2 `find_crosses_for_auctions`
- 会拆分 taker/resting，专门处理拍卖/触发撮合
- ✅ 覆盖：拍卖/触发订单与 maker 的撮合
- ⚠️ 不是“全量交叉”，而是“可撮合交叉”

---

## 3) SDK 视角：订单在 L3 的合流规则（关键）

SDK（`drift-rs`）里，L3 的 `bids()/asks()` 会把以下子簿进行**价格合流排序**：
1. **Fixed**：`bids / asks`（resting limit + market）
2. **Floating**：`floating_bids / floating_asks`
3. **Trigger**：`trigger_bids / trigger_asks`（仅当 `trigger_price=Some`）
4. **VAMM**：`vamm_bids / vamm_asks`（需要 `perp_market` 计算 fallback）

因此，要覆盖**所有类型交叉**，必须：
- 使用 `book.bids(oracle_price, perp_market, Some(trigger_price))`
- 使用 `book.asks(oracle_price, perp_market, Some(trigger_price))`
- 让 trigger 订单以 **post‑trigger price** 参与排序

---

## 4) 全类型交叉判断方法（基于 SDK 的新增方法）

### 4.1 方法签名（建议）

```
find_crossing_region_all_types(
    oracle_price: u64,
    market_index: u16,
    market_type: MarketType,
    perp_market: Option<&PerpMarket>,
    trigger_price: u64,
    depth: Option<usize>,
) -> Option<CrossingRegionAll>
```

### 4.2 输出结构（示意）

```
CrossingRegionAll {
    slot,
    best_bid,
    best_ask,
    crossing_bids, // 含所有类型（limit/market/oracle/floating/trigger/vamm）
    crossing_asks,
}
```

### 4.3 核心流程
1. 取 L3 快照  
2. 通过 `bids(..., Some(trigger_price))` / `asks(..., Some(trigger_price))` 合流排序  
3. 若 `best_bid < best_ask` → 无交叉  
4. 否则取出：  
   - `crossing_bids`：`price >= best_ask` 的全部 bids  
   - `crossing_asks`：`price <= best_bid` 的全部 asks  

### 4.4 能覆盖的交叉类型
| 交叉类型 | 是否覆盖 | 说明 |
|---|---|---|
| 限价‑限价 | ✅ | fixed/floating 合流 |
| 拍卖‑限价 | ✅ | market_orders → bids/asks |
| 拍卖‑拍卖 | ✅ | market_orders ↔ market_orders |
| 触发‑触发 | ✅ | trigger_price 触发后进入排序 |
| 触发‑限价 | ✅ | trigger + fixed 合流 |
| VAMM‑订单 | ✅ | vamm_* 参与排序 |

---

## 5) 结论

要“完整覆盖 DLOB 中所有类型订单的交叉判断”，  
必须新增 **基于 SDK 合流规则** 的统一方法，确保 trigger/VAMM/浮动/拍卖全部进入排序。
