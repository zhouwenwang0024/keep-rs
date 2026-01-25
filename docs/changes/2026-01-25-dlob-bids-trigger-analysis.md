# DLOB `bids()` 方法对条件订单的处理分析

> 核心问题：`bids()` 返回的 trigger 订单，其 `L3Order.price` 字段是**触发价**还是**触发后的下单价**？

---

## 1. 关键发现（源码直读）

### 1.1 在 `load_orderbook` 阶段（存储 trigger 订单）

**位置**：`mod.rs:1540-1554`

```rust
for order in orderbook.trigger_orders.bids.values() {
    self.trigger_bids.push(L3Order {
        price: order.price, // This is the trigger price, not the post-trigger price
        // ...
    });
}
```

**结论**：`L3Order.price` 存储的是 **trigger price（触发价）**，不是触发后的下单价。

---

### 1.2 在 `bids()` 方法中（排序与返回）

**位置**：`mod.rs:1205-1310`

#### 步骤 1：跳过不满足触发条件的订单（1227-1236行）
```rust
if let Some(trig_price) = trigger_price {
    while let Some(x) = trigger_iter.peek() {
        if trig_price > x.price && x.is_trigger_above()
            || trig_price < x.price && !x.is_trigger_above()
        {
            break; // 满足触发条件，停止跳过
        }
        trigger_iter.next(); // 不满足，跳过
    }
}
```

**作用**：只保留**满足触发条件**的 trigger 订单进入后续排序。

#### 步骤 2：使用 post-trigger price 进行排序比较（1268-1282行）
```rust
if let (Some(x), Some(trig_price)) = (t, trigger_price) {
    let would_trigger = (x.is_trigger_above() && trig_price > x.price)
        || (!x.is_trigger_above() && trig_price < x.price);
    if would_trigger {
        if let Some(post_trigger_price) =
            x.post_trigger_price(slot, oracle_price_for_vamm as u64, market)
        {
            if post_trigger_price > best_price {
                best_price = post_trigger_price;  // 用 post-trigger price 比较
                best_src = Some(Src::Trigger);
            }
        }
    }
}
```

**作用**：用 `post_trigger_price()` 计算触发后的下单价，并用它来**决定排序顺序**。

#### 步骤 3：返回原始 L3Order（1295-1300行）
```rust
match best_src {
    Some(Src::Fixed) => bids_iter.next(),
    Some(Src::Floating) => floating_iter.next(),
    Some(Src::Vamm) => vamm_iter.next(),
    Some(Src::Trigger) => trigger_iter.next(),  // 返回原始的 L3Order
    None => None,
}
```

**关键**：返回的是**原始的 `L3Order`**，其 `price` 字段仍然是 **trigger price**。

---

## 2. `post_trigger_price()` 方法（计算触发后的下单价）

**位置**：`types.rs:759-792`

```rust
pub fn post_trigger_price(
    &self,
    slot: u64,
    oracle_price: u64,
    perp_market: &PerpMarket,
) -> Option<u64> {
    if matches!(self.kind, OrderKind::TriggerMarket | OrderKind::TriggerLimit) {
        // 构造 TriggerOrder，调用 get_price 计算触发后的价格
        let order = TriggerOrder { ... };
        order.get_price(slot, oracle_price, Some(perp_market)).ok()
    } else {
        None
    }
}
```

**作用**：计算如果 trigger 订单在当前 slot 和 oracle_price 下被触发，它的**实际下单价**。

---

## 3. 结论（明确回答你的问题）

### ✅ `bids()` 返回的 trigger 订单的 `price` 字段是：**触发价（trigger price）**

### ⚠️ 但排序时使用的是：**触发后的下单价（post-trigger price）**

**具体流程**：
1. **存储阶段**：`L3Order.price = trigger_price`（触发价）
2. **排序阶段**：用 `post_trigger_price()` 计算的值来**比较和排序**
3. **返回阶段**：返回的 `L3Order.price` 仍然是 **trigger price**

---

## 4. 实际影响（对你的代码）

### 4.1 在 `find_crossing_region_all_types` 中
- `best_bid.price` / `best_ask.price` 如果是 trigger 订单，是 **trigger price**
- 但排序时已经用 `post_trigger_price` 比较过了，所以排序是正确的
- **交叉判断**：`best_bid.price >= best_ask.price` 比较的是 trigger price，这可能**不准确**

### 4.2 ⚠️ 潜在问题
**问题**：如果 `best_bid` 或 `best_ask` 是 trigger 订单，直接用 `price` 字段比较交叉可能不准确。

**原因**：
- `best_bid.price` 是 trigger price（例如 100）
- 但实际触发后的下单价可能是 105（通过 `post_trigger_price` 计算）
- 如果 `best_ask.price = 102`（普通限价单），那么：
  - `best_bid.price (100) < best_ask.price (102)` → 看起来不交叉
  - 但实际 `post_trigger_price (105) > best_ask.price (102)` → 应该交叉

**影响**：可能导致**漏掉有效的交叉机会**。

---

## 5. 建议修复

### 方案 A：在 `find_crossing_region_all_types` 中重新计算 post-trigger price
```rust
let best_bid_effective_price = if matches!(best_bid.kind, OrderKind::TriggerMarket | OrderKind::TriggerLimit) {
    best_bid.post_trigger_price(slot, oracle_price, perp_market).unwrap_or(best_bid.price)
} else {
    best_bid.price
};
// 同样处理 best_ask
if best_bid_effective_price < best_ask_effective_price {
    return None;
}
```

### 方案 B：SDK 提供 `effective_price()` 方法
- `L3Order` 增加 `effective_price(slot, oracle, market)` 方法
- 自动判断：如果是 trigger 且满足条件，返回 `post_trigger_price`，否则返回 `price`

---

## 6. 总结

| 阶段 | `L3Order.price` 的值 | 排序使用的值 |
|---|---|---|
| 存储 | trigger price | - |
| 排序比较 | trigger price（字段值） | **post-trigger price**（计算值） |
| 返回 | **trigger price** | - |

**关键点**：
- ✅ 排序是正确的（使用 post-trigger price）
- ⚠️ 但返回的 `price` 字段是 trigger price，不是实际下单价
- ⚠️ 如果直接用 `best_bid.price >= best_ask.price` 判断交叉，可能不准确
