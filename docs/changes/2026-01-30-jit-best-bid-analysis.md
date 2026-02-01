# JIT best_bid/best_ask 计算方式专项分析（独立报告）

## 1. 结论摘要
- **JIT 当前不是用 `top_bids/top_asks`**，而是使用 **`bids_with_price/asks_with_price`**。
- `bids_with_price/asks_with_price` 会计算“**有效价格**”，包含浮动单、触发单与 VAMM fallback。
- 因此 `best_bid/best_ask` 可能偏离你直观看到的“盘口最优价”，这是 **逻辑设计结果**，不是排序 BUG。

---

## 2. 三组方法的定义与差异

### 2.1 `bids(...) / asks(...)`
**位置**：`drift-rs/crates/src/dlob/mod.rs`  
**作用**：返回 L3Book 视角下的“已排序订单迭代器”。  
**排序逻辑**：  
- 会合并以下来源，并在每次迭代中选择“当前最优价来源”：  
  - 固定价挂单（resting limit）  
  - 浮动挂单（相对 oracle 偏移）  
  - 触发单（post-trigger price）  
  - VAMM fallback  

**结论**：`bids/asks` 输出的是“**有效排序结果**”，而非单一来源的原始挂单顺序。

---

### 2.2 `top_bids(...) / top_asks(...)`
**位置**：`drift-rs/crates/src/dlob/mod.rs`  
**定义**：  
```rust
pub fn top_bids(...) -> impl Iterator<Item = &L3Order> {
    self.bids(oracle_price, perp_market, trigger_price).take(count)
}
pub fn top_asks(...) -> impl Iterator<Item = &L3Order> {
    self.asks(oracle_price, perp_market, trigger_price).take(count)
}
```

**含义**：  
`top_bids/top_asks` 只是 `bids/asks` 的前 N 个结果。  

**重要区别**：  
即使是 `top_bids/top_asks`，也不是“原始挂单列表”，而是**经过“有效价格排序”后的前 N 个**。

---

### 2.3 `bids_with_price(...) / asks_with_price(...)`
**位置**：`drift-rs/crates/src/dlob/mod.rs`  
**作用**：返回 `(order, computed_price)`，其中 `computed_price` 是“有效价格”。  
**参与计算的来源**：
- 固定价挂单  
- 浮动挂单（oracle diff）  
- 触发单（post-trigger price）  
- VAMM fallback  

**关键**：`bids_with_price/asks_with_price` 直接给出“用于排序的价格”，因此更容易出现
与直观看到的订单价格不一致的情况。

---

## 3. JIT 实际使用的是哪一个？

### 3.1 JIT 调用链
```
JitStrategy::maybe_intent
  -> best_levels_with_makers(...)
     -> L3Book::bids_with_price / asks_with_price
```

### 3.2 `best_levels_with_makers` 的取价方式
```rust
for bid in book.bids_with_price(...) {
    if best_bid_price.is_none() { best_bid_price = Some(price); }
    if Some(price) != best_bid_price { break; }
    ...
}
```
同理对 asks。  

**结论**：JIT 的 `best_bid/best_ask` 是 **“有效价格排序后的第一层价格”**，而不是
“直观看到的 top_bids/top_asks”。

---

## 4. 为什么会出现 drift_mid 偏离？

### 4.1 drift_mid 的计算公式
```
drift_mid = (best_bid + best_ask) / 2
```

### 4.2 drift_mid 偏离的常见原因
1) **触发单参与计算**  
   触发单的 post-trigger price 可能偏离当前盘口价格。  
2) **浮动单参与计算**  
   浮动单基于 oracle diff，会偏离普通限价单。  
3) **VAMM fallback 参与计算**  
   某些订单价格可能来自 VAMM fallback。  

**结果**：`best_bid/best_ask` 不是“传统盘口最优价”，导致 drift_mid 偏离。

---

## 5. 如何确认是否为“计算方式”导致？

建议观察一次 `jit trigger` 的完整日志字段：
- `best_bid / best_ask`
- `drift_mid / binance_mid`
- `basis_ema / spread`
- `dlob_slot`

如果：
- `drift_mid` 明显偏离 `binance_mid`
- `basis_ema` 偏大
- `dlob_age_ms` 不高  
则更可能是 **有效价格参与导致的偏移**，而不是阻塞导致的过期。

---

## 6. 如果想要“只用普通盘口最优价”

你可以考虑：
- 改用 `book.bids()` / `book.asks()` 的 **固定价订单**  
或在 `best_levels_with_makers` 内过滤掉触发/浮动/VAMM 来源  

代价是：  
- 可能遗漏可成交订单  
- 与当前 DLOB “可执行最优价”语义不一致

---

## 7. 总结
- **JIT 当前不使用 top_bids/top_asks**  
- `bids_with_price/asks_with_price` 会混入“有效价格”  
- drift_mid 偏差更可能来自“有效价格参与”，而非排序 BUG  
