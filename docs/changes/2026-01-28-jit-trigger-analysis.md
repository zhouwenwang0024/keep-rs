# JIT 触发机制与问题分析报告

## 1. 触发来源与限频机制

### 1.1 触发来源

JIT 策略的触发来源**只有一个**：**Binance 价格更新事件**

**代码位置：** `src/filler.rs:620-718`

**触发流程：**
1. Binance WebSocket feed 收到价格更新（`BinancePriceUpdate`）
2. 更新 `jit_price_cache`（存储 `(binance_mid, ts_ms)`）
3. 调用 `strategy.maybe_intent()` 检查是否有机会
4. 如果有机会，生成 `JitIntent` 并调用 `try_jit()` 下单

**关键代码：**
```rust
binance_update = async {
    match binance_feed.as_mut() {
        Some(feed) => feed.recv().await,
        None => None,
    }
}, if binance_feed.is_some() => {
    // ... 更新 jit_price_cache ...
    if let Some(intent) = strategy.maybe_intent(...) {
        // 触发下单
        try_jit(...).await;
    }
}
```

**注意：** JIT 策略**不会**在 `book_update` 事件中触发（虽然代码中有相关逻辑，但需要 `jit_price_cache` 中有数据，而该缓存只在 Binance 更新时写入）。

### 1.2 限频机制

JIT 策略有**三层限频机制**：

#### 1.2.1 Binance 事件节流（`EVENT_WINDOW_MS = 5ms`）

**位置：** `src/filler.rs:632`

```rust
if now_ms.saturating_sub(last_binance_event_ms) < EVENT_WINDOW_MS {
    continue;  // 5ms 内只处理一次
}
```

**作用：** 防止 Binance feed 更新过于频繁时，每次都触发 JIT 检查。

**影响：** 如果 Binance 更新频率 > 200Hz，会被节流到最多 200Hz。

#### 1.2.2 JIT 策略 Cooldown（`jit_cooldown_ms`）

**位置：** `src/jit/jit_strategy.rs:90-100`

```rust
if let Some(last) = self.last_fire_ms.get(&market_index) {
    if now_ms.saturating_sub(*last) < self.cooldown_ms {
        return None;  // cooldown 内不触发
    }
}
```

**作用：** 同一市场在 cooldown 时间内只触发一次下单。

**配置：** `jit_cooldown_ms`（从 `Config` 读取，默认值需查看配置）

**影响：** 这是**下单频率的主要限制**。如果 `jit_cooldown_ms = 50ms`，则同一市场最多每 50ms 触发一次下单。

#### 1.2.3 下单限频

**位置：** `src/jit/jit_trades.rs`（`try_jit` 函数）

**当前状态：** **无额外限频**

下单逻辑直接调用 `tx_worker_ref.send_tx_with_jito()`，没有额外的限频机制。限频完全依赖策略层的 cooldown。

**潜在问题：** 如果 cooldown 设置过短，可能导致下单频率过高，触发 RPC/Jito 限流。

---

## 2. `best_bid > best_ask` 问题分析

### 2.1 问题现象

日志显示：
```
jit trigger (binance): market=0, ref_px=125496463, edge_ppm=5000, 
best_bid=125680000, best_ask=125097910, ...
```

**异常：** `best_bid=125680000` > `best_ask=125097910`（价差约 4.6%）

大多数情况下，`best_bid` 应该 ≤ `best_ask`。但在部分情况下**确实可能出现 `best_bid > best_ask`**（订单本身交叉），因此不能简单视为异常或必然错误信号。

### 2.2 价格计算逻辑（已改为 SDK 计算价）

**现状：** 已在 `drift-rs` 新增 `bids_with_price()` / `asks_with_price()`，返回**排序时使用的计算价**。  
`keep-rs` 侧不再重算 `effective_price`，直接使用 SDK 返回的 `computed_price` 做 best 价和同价位聚合。

### 2.3 可能的原因（保留）

1. **订单本身交叉**：撮合层可能存在交叉的订单组合，本身可导致 `best_bid > best_ask`。  
2. **DLOB 快照差异**：即使使用 SDK 计算价，bid/ask 仍可能来自不同时间点的快照（需要进一步确认是否存在并发更新影响）。  
3. **订单类型差异**：Trigger/Oracle/VAMM 订单对价格路径影响不同，可能造成阶段性“交叉”表现。  

### 2.4 验证方法

1. **添加日志：** 在 `effective_price` 中记录每个订单的原始 `order.price`、`post_trigger_price` 结果、订单类型
2. **检查 DLOB slot：** 记录 `book.slot`，确认 bid 和 ask 是否来自同一 slot
3. **检查订单类型：** 记录 bid 和 ask 的订单类型分布
4. **检查 oracle_price：** 记录计算 bid 和 ask 时使用的 `oracle_price` 和 `trigger_price`

### 2.5 建议的修复方案（更新）

1. **优先使用 SDK 计算价**：已完成（`bids_with_price`/`asks_with_price`）。  
2. **快照一致性确认**：需要进一步核实 bid/ask 是否来自同一快照。  
3. **明确交叉语义**：确认 `best_bid > best_ask` 在业务上是否应当触发 JIT（当前结论：允许出现，不做 guard）。  

---

## 3. 日志信息缺失分析

### 3.1 当前日志内容

**位置：** `src/filler.rs:691-701`

```rust
log::info!(
    target: TARGET,
    "jit trigger (binance): market={}, ref_px={}, edge_ppm={}, best_bid={}, best_ask={}, makers_bid={}, makers_ask={}",
    market_index,
    intent.reference_price,
    intent.edge_ppm,
    intent.best_bid_price,
    intent.best_ask_price,
    intent.makers_bid.len(),
    intent.makers_ask.len(),
);
```

### 3.2 缺失的关键信息（已补齐到日志）

1. **`drift_mid`**：Drift 中间价 `(best_bid + best_ask) / 2`
   - **用途：** 用于计算基差和验证价格合理性
   - **当前状态：** 在 `jit_strategy.rs:124` 计算，但未传递到 `JitIntent`

2. **`binance_mid`**：Binance 中间价
   - **用途：** 用于计算基差和验证参考价合理性
   - **当前状态：** 在 `maybe_intent` 参数中，但未传递到 `JitIntent`

3. **`basis_ema`**：基差 EMA 值
   - **用途：** 用于计算 `reference_price = binance_mid - 0.8 * basis_ema`
   - **当前状态：** 在 `jit_strategy.rs:134` 计算，但未传递到 `JitIntent`

4. **`spread`**：当前基差 `binance_mid - drift_mid`
   - **用途：** 用于验证基差是否合理
   - **当前状态：** 在 `jit_strategy.rs:128` 计算，但未保存

5. **`oracle_price`**：链上 Oracle 价格
   - **用途：** 用于验证价格合理性
   - **当前状态：** 在 `maybe_intent` 参数中，但未传递

6. **`trigger_price`**：触发价格
   - **用途：** 用于验证触发订单的价格计算
   - **当前状态：** 在 `maybe_intent` 参数中，但未传递

7. **订单类型分布：** bid 和 ask 的订单类型（Limit、TriggerMarket 等）
   - **用途：** 用于诊断 `best_bid > best_ask` 问题
   - **当前状态：** 未记录

8. **DLOB slot：** 获取 bid/ask 时的 DLOB slot
   - **用途：** 用于验证 bid 和 ask 是否来自同一快照
   - **当前状态：** 在 `maker_select.rs:26` 获取，但未传递

### 3.3 建议的日志增强

**方案 1：扩展 `JitIntent` 结构体**

在 `JitIntent` 中添加字段：
```rust
pub struct JitIntent {
    // ... 现有字段 ...
    pub drift_mid: i64,
    pub binance_mid: i64,
    pub basis_ema: f64,
    pub spread: f64,
    pub oracle_price: u64,
    pub trigger_price: u64,
    pub dlob_slot: u64,
}
```

**方案 2：在日志中直接计算**

在输出日志时，重新计算这些值（但可能不准确，因为 DLOB 可能已更新）。

**推荐：** 方案 1，因为：
- 数据准确（来自触发时的快照）
- 便于后续分析和调试
- 不影响现有逻辑

---

## 4. 总结

### 4.1 触发机制总结

- **触发来源：** 仅 Binance 价格更新
- **限频机制：** 
  - Binance 事件节流：5ms
  - JIT 策略 cooldown：`jit_cooldown_ms`（配置项）
  - 下单限频：无（依赖 cooldown）

### 4.2 `best_bid > best_ask` 问题

**最可能的原因：**
1. `post_trigger_price` 计算异常（bid 和 ask 使用不同的 oracle/trigger 快照）
2. DLOB 数据不同步（bid 和 ask 来自不同的 slot）

**建议：**
1. 添加验证逻辑，如果 `best_bid > best_ask`，记录警告并跳过
2. 确保 bid 和 ask 来自同一 DLOB 快照
3. 增强日志，记录订单类型、DLOB slot、oracle_price 等信息

### 4.3 日志增强建议

**必须添加：**
- `drift_mid`
- `binance_mid`
- `basis_ema`
- `spread`
- `oracle_price`
- `trigger_price`
- `dlob_slot`

**可选添加：**
- 订单类型分布
- `post_trigger_price` 使用情况
- `fallback_price` 使用情况

---

## 5. 关键问题深度分析

### 5.1 问题 1：`best_bid` 和 `best_ask` 明显不对

**现象：**
```
jit trigger: market=2, ref_px=2990798198, edge_ppm=5000, 
best_bid=2994240975, best_ask=2980670429, ...
```

**分析：**

#### 5.1.1 价格计算的双重逻辑

**关键发现：** `book.bids()` 和 `book.asks()` 迭代器**内部已经计算了 `post_trigger_price` 并排序**，但 `maker_select.rs` 中的 `effective_price` **又计算了一次**。

**代码位置：**
- `drift-rs/crates/src/dlob/mod.rs:1235-1335` - `bids()` 方法内部计算 `post_trigger_price` 并排序
- `drift-rs/crates/src/dlob/mod.rs:1371-1470` - `asks()` 方法内部计算 `post_trigger_price` 并排序
- `keep-rs/src/jit/maker_select.rs:32-51` - `effective_price` 再次计算 `post_trigger_price`

**问题：**
1. **Slot 不一致：**
   - `book.bids()` 内部使用 `auction_slot = self.slot.saturating_add(1)`
   - `effective_price` 使用 `book.slot`（没有 +1）
   - 这可能导致 `post_trigger_price` 计算结果不同

2. **Oracle price 可能不一致：**
   - `book.bids()` 内部使用 `oracle_price_for_vamm = oracle_price.unwrap_or(self.oracle_price)`
   - `effective_price` 使用传入的 `oracle_price`
   - 如果传入的 `oracle_price` 与 `book.oracle_price` 不同，计算结果会不同

3. **Trigger price 使用：**
   - `book.bids()` 内部已经根据 `trigger_price` 过滤和排序触发订单
   - `effective_price` 再次计算 `post_trigger_price`，但可能使用不同的参数

#### 5.1.2 `best_bid > best_ask` 的根本原因

**最可能的原因：** `effective_price` 重新计算的价格与 `book.bids()/asks()` 内部排序使用的价格不一致。

**具体场景：**
1. `book.bids()` 返回的第一个订单（best bid）的 `order.price` 可能是原始价格
2. `effective_price` 计算出的 `post_trigger_price` 可能远高于原始价格
3. 如果这个订单是触发订单，`post_trigger_price` 可能异常高
4. 同样，`book.asks()` 返回的第一个订单的 `post_trigger_price` 可能异常低
5. 结果：`best_bid > best_ask`

**验证方法：**
- 记录 `order.price`、`post_trigger_price` 结果、订单类型、订单方向
- 记录 `book.slot`、`auction_slot`、`oracle_price`、`trigger_price`
- 对比 `book.bids()/asks()` 内部使用的参数与 `effective_price` 使用的参数

#### 5.1.3 可能的 bid/ask 混淆错误

**检查结果：** **未发现明显的 bid/ask 混淆**

- `book.bids()` 返回的是 `Direction::Long` 的订单（买入订单）
- `book.asks()` 返回的是 `Direction::Short` 的订单（卖出订单）
- `effective_price(bid, true)` 和 `effective_price(ask, false)` 的使用是正确的

**但是，** 存在一个潜在问题：
- `book.bids()` 迭代器内部已经按价格排序（最高价 first）
- `book.asks()` 迭代器内部已经按价格排序（最低价 first）
- 但 `effective_price` 重新计算的价格可能与排序使用的价格不同

### 5.2 问题 2：为什么总是 bid 和 ask 都越过 ref_px？

**现象：** 日志显示 `sell_ok=true` 和 `buy_ok=true` 同时为 true。

**分析：**

#### 5.2.1 触发条件逻辑

**代码位置：** `src/jit/jit_strategy.rs:149-150`

```rust
let sell_ok = best_bid_i128 > ref_i128;
let buy_ok = best_ask_i128 < ref_i128;
```

**逻辑：**
- `sell_ok = true`：当 `best_bid > ref_px` 时，可以卖出（做空）
- `buy_ok = true`：当 `best_ask < ref_px` 时，可以买入（做多）

**问题：** 如果 `best_bid > best_ask`（异常情况），那么：
- 如果 `ref_px` 在 `best_ask` 和 `best_bid` 之间，`sell_ok` 和 `buy_ok` 都会是 `true`
- 例如：`best_ask=2980670429 < ref_px=2990798198 < best_bid=2994240975`
- 结果：`buy_ok=true`（因为 `best_ask < ref_px`）且 `sell_ok=true`（因为 `best_bid > ref_px`）

**结论：** 这不是单侧触发的问题，而是 `best_bid > best_ask` 异常导致的。

#### 5.2.2 单侧触发是否可能？

**理论上：** 单侧触发是可能的：
- 如果 `best_bid > ref_px` 但 `best_ask >= ref_px`，只有 `sell_ok=true`
- 如果 `best_ask < ref_px` 但 `best_bid <= ref_px`，只有 `buy_ok=true`

**但实际上：** 由于 `best_bid > best_ask` 的异常，导致总是同时触发。

### 5.3 问题 3：bid 和 ask 是否混淆？

**检查结果：** **未发现明显的混淆**

**验证：**
1. `book.bids()` 返回 `Direction::Long` 订单（买入订单）✅
2. `book.asks()` 返回 `Direction::Short` 订单（卖出订单）✅
3. `effective_price(bid, true)` 使用 `PositionDirection::Long` ✅
4. `effective_price(ask, false)` 使用 `PositionDirection::Short` ✅

**但是，** 存在一个**逻辑错误**：

在 `maker_select.rs:64-69`：
```rust
let price = effective_price(bid, true);
if best_bid_price.is_none() {
    best_bid_price = Some(price);
}
if Some(price) != best_bid_price {
    break;  // 如果价格不同，停止
}
```

**问题：** 这个逻辑假设 `book.bids()` 返回的第一个订单的 `effective_price` 就是 best bid。但是：
1. `book.bids()` 已经按价格排序（使用内部计算的 `post_trigger_price`）
2. `effective_price` 重新计算的价格可能与排序使用的价格不同
3. 如果第一个订单的 `effective_price` 计算结果异常，`best_bid_price` 就会异常

**更严重的问题：** `book.bids()` 返回的订单的 `order.price` 字段是**原始价格**，不是 `post_trigger_price`。如果第一个订单是触发订单，`order.price` 可能是触发价格，而不是触发后的价格。

### 5.4 根本原因总结

**核心问题：** `effective_price` 函数与 `book.bids()/asks()` 迭代器的价格计算逻辑不一致。

**具体表现：**
1. `book.bids()/asks()` 内部已经计算 `post_trigger_price` 并排序
2. `effective_price` 重新计算 `post_trigger_price`，但可能使用不同的参数（slot、oracle_price）
3. 导致 `best_bid_price` 和 `best_ask_price` 异常
4. 如果 `best_bid > best_ask`，触发条件会同时满足

**修复方向：**
1. **方案 1：** 移除 `effective_price` 的 `post_trigger_price` 计算，直接使用 `order.price`（但需要确认 `book.bids()/asks()` 返回的订单价格是否正确）
2. **方案 2：** 确保 `effective_price` 使用的参数与 `book.bids()/asks()` 内部使用的参数一致（slot、oracle_price、trigger_price）
3. **方案 3：** 不使用 `book.bids()/asks()` 迭代器，直接从 DLOB 获取原始订单并自己计算价格

---

## 6. 代码位置索引

- **触发逻辑：** `src/filler.rs:620-718`
- **策略检查：** `src/jit/jit_strategy.rs:49-177`
- **价格计算：** `src/jit/maker_select.rs:16-111`
- **限频配置：** `src/main.rs:78` (`jit_cooldown_ms`)
- **事件节流：** `src/filler.rs:37` (`EVENT_WINDOW_MS = 5`)
- **DLOB bids 实现：** `drift-rs/crates/src/dlob/mod.rs:1235-1335`
- **DLOB asks 实现：** `drift-rs/crates/src/dlob/mod.rs:1371-1470`
- **post_trigger_price 实现：** `drift-rs/crates/src/dlob/types.rs:759-792`

---

## 7. 未解决问题（当前待处理）

1. **JIT 关键日志仍不完整**  
   需要补齐：`drift_mid`、`binance_mid`、`basis_ema`、`spread`、`oracle_price`、`trigger_price`、`dlob_slot`，以及订单类型分布。

2. **bid/ask 快照一致性未验证**  
   仍需确认 `bids_with_price`/`asks_with_price` 使用时，bid/ask 是否来自同一 DLOB slot。

3. **`best_bid > best_ask` 的业务语义**  
   已确认可能出现，但需要明确：在这种交叉场景下是否应触发 JIT，触发策略是否需要额外过滤条件。