# DLOB 价格计算问题分析报告

本文档详细分析了 DLOB（Decentralized Limit Order Book）中发现的三个关键问题，每个问题包含：问题描述、原因分析、影响评估和解决方案。

---

## 术语说明

- **L3Book**：`DLOB` 的 L3 快照容器，包含多个订单列表（bids/asks/floating/vamm/trigger）。
- **L3Order**：`L3Book` 中单个订单的结构体，包含价格、数量、方向等字段。

> 说明：文档中“L3Order 固化价格”指的是 `L3Book` 内的单条订单数据被固化，不是 `L3Book` 本身被固化。

---

## 问题 1：拍卖订单 Slot 计价固化问题

### 问题描述

拍卖订单（JIT 订单）的价格应该根据当前 `slot` 动态计算，但在 `bids()` 和 `bids_with_price()` 方法中，拍卖订单的价格在快照构建时被固化，无法根据查询时的 slot 更新。

**对比**：触发订单（Trigger Orders）能够正确使用 slot 参数动态计算价格，而拍卖订单不能。

### 原因分析

#### 1.1 快照构建时价格被固化

**文件**：`drift-rs/crates/src/dlob/mod.rs`  
**方法**：`L3Book::load_orderbook()`  
**行号**：1846-1871（市场拍卖订单）、1943-1968（Oracle 拍卖订单）

```rust
// 第 1849-1851 行：使用快照时的 slot 计算价格
let price = order.get_price(auction_slot, oracle_price, market_tick_size)
    .unwrap_or_default();

// 第 1853 行：价格被固化存储在 L3Order.price 中
let order = L3Order {
    price,  // 固化价格，无法更新
    // ...
};
```

**关键问题**：
- `auction_slot = self.slot.saturating_add(1)`（第 1749 行），使用的是快照构建时的 slot
- 计算出的价格被固化存储在 `L3Order.price` 中
- 原始拍卖订单信息（`start_price`, `end_price`, `duration`, `slot`）被丢弃

#### 1.2 L3Order 结构缺少原始信息

**文件**：`drift-rs/crates/src/dlob/types.rs`  
**行号**：714-728

```rust
pub struct L3Order {
    pub price: u64,  // 只有固化价格，没有原始拍卖参数
    pub size: u64,
    pub max_ts: u64,
    pub order_id: u32,
    pub kind: OrderKind,
    pub user: Pubkey,
    pub(crate) flags: u8,
    // ❌ 缺少：start_price, end_price, duration, slot（原始订单的 slot）
}
```

#### 1.3 查询时无法重新计算

**文件**：`drift-rs/crates/src/dlob/mod.rs`  
**方法**：`L3Book::bids()`  
**行号**：1285

```rust
if let Some(x) = a {
    best_price = x.price;  // 直接使用固化价格，无法根据当前 slot 更新
    best_src = Some(Src::Fixed);
}
```

**对比触发订单**（第 1304 行）：
```rust
if let Some(post_trigger_price) =
    x.post_trigger_price(auction_slot, oracle_price_for_vamm as u64, market)
{
    // 能够根据当前 slot 动态计算
}
```

#### 1.4 拍卖价格需要动态计算

**文件**：`drift-rs/crates/src/dlob/types.rs`  
**方法**：`MarketOrder::get_price()`  
**行号**：441-464

```rust
let slots_elapsed = slot.saturating_sub(self.slot) as i64;  // 需要当前 slot
let price = match self.direction {
    Direction::Long => {
        self.start_price
            + ((self.end_price - self.start_price) * delta_numerator / delta_denominator)
    }
    // 价格在 start_price 和 end_price 之间线性插值，取决于 slots_elapsed
}
```

**关键点**：拍卖价格是动态的，随 slot 变化而变化，但被固化后无法更新。

### 影响评估

#### 1.1 价格不准确

- 拍卖订单价格在快照构建后不再更新
- 如果 slot 变化较大（如几百个 slot），价格偏差会显著
- 特别是在拍卖期间（价格快速变化），偏差会更大
- **严重程度**：高

#### 1.2 排序错误

- `bids()` 和 `bids_with_price()` 按价格排序
- 如果拍卖订单价格过时，排序结果会错误
- 可能导致错误的交易决策（如选择错误的对手方）
- **严重程度**：高

#### 1.3 与触发订单不一致

- 触发订单能正确使用 slot 动态计算
- 拍卖订单不能，导致行为不一致
- **严重程度**：中

#### 1.4 对 drift_mid 的影响

- 如果拍卖订单是最高价/最低价，会导致 `best_bid`/`best_ask` 不准确
- 进而影响 `drift_mid = (best_bid + best_ask) / 2` 的计算
- **严重程度**：高

### 解决方案

#### 方案 1：保留原始订单信息（推荐）

**实现**：
1. 在 `L3Order` 中增加可选字段存储拍卖订单的原始信息
2. 在 `bids()` 方法中，对拍卖订单重新计算价格

**代码修改**：

```rust
// types.rs: 扩展 L3Order 结构
pub struct L3Order {
    pub price: u64,
    // ... 现有字段
    
    // 新增字段（仅对拍卖订单有效）
    pub auction_start_price: Option<i64>,
    pub auction_end_price: Option<i64>,
    pub auction_duration: Option<u8>,
    pub auction_slot: Option<u64>,  // 原始订单的 slot
}

// mod.rs: 在 bids() 中重新计算
if let Some(x) = a {
    let price = if x.auction_slot.is_some() {
        // 重新计算拍卖价格
        calculate_auction_price(x, auction_slot, oracle_price, market_tick_size)
    } else {
        x.price  // 固定价格订单
    };
    best_price = price;
}
```

**优点**：
- 能够根据当前 slot 动态计算价格
- 保持现有数据结构的大部分兼容性

**缺点**：
- 需要修改 `L3Order` 结构
- 增加内存占用（可选字段）

#### 方案 2：单独存储拍卖订单

**实现**：
- 不将拍卖订单转换为 `L3Order`，而是单独存储
- 在查询时动态计算价格

**代码修改**：

```rust
pub struct L3Book {
    // ... 现有字段
    pub auction_bids: Vec<MarketOrder>,  // 单独存储
    pub auction_asks: Vec<MarketOrder>,
}
```

**优点**：
- 完全保留原始信息
- 查询时动态计算，价格准确

**缺点**：
- 需要大幅修改数据结构
- 查询逻辑更复杂

#### 方案 3：更频繁地重建快照

**实现**：
- 接受当前限制，但更频繁地重建快照
- 在文档中明确说明限制

**优点**：
- 不需要修改代码
- 实现简单

**缺点**：
- 性能开销大
- 仍然存在时间窗口内的价格不准确

---

## 问题 2：缺失 Metadata 的订单

### 问题描述

运行时日志显示某些订单在构建 L3Book 快照时找不到对应的 metadata，导致这些订单被跳过，不包含在 L3Book 中。

**日志示例**：
```
[2026-01-30T17:12:11Z WARN  dlob] L3Book: Found 1 orders without metadata out of 1929 total orders (market: 0)
[2026-01-30T17:12:11Z INFO  dlob] missing order id: 10869132459266884230
```

### 原因分析

#### 2.1 Metadata 插入时机

**文件**：`drift-rs/crates/src/dlob/mod.rs`  
**方法**：`DLOB::user_update()`  
**行号**：900

```rust
self.metadata.insert(order_id, OrderMetadata::new(*user, kind, order.order_id, order.max_ts.unsigned_abs()));
```

**关键点**：
- Metadata 在 `user_update()` 时插入
- 只有当订单事件（OrderEvent）到达时，才会创建 metadata
- `missing order id` 表示：Orderbook 里有订单，但 `metadata` 映射中没有该 `order_id`

#### 2.2 missing order id 的直接含义

**文件**：`drift-rs/crates/src/dlob/mod.rs`  
**方法**：`L3Book::load_orderbook()`  
**行号**：1758, 1779, 1805, 1827, 1848, 1875, 1903, 1924, 1945, 1972

```rust
if let Some(meta) = metadata.get(&order.id) {
    // 创建 L3Order
    self.bids.push(L3Order { ... });
} else {
    missing_metadata_count += 1;  // 订单被跳过
    DLOB::log_missing_order_events_helper(order.id, order_events);
    log::info!(target: TARGET, "missing order id: {:?}", order.id);
}
```

**关键点**：
- `order.id` 是 `order_hash(user, order_id)` 的哈希值
- 如果 `metadata.get(&order.id)` 返回 `None`，订单会被跳过
- 订单不会被添加到 `L3Book` 中
- `log_missing_order_events_helper` 用于打印该订单的事件轨迹（需要 `dlob_dbg` 功能开关）

#### 2.3 可能的原因（针对日志现象）

1. **事件缺失或顺序错位**  
   - Orderbook 已包含订单，但 OrderEvent 未到达或未被处理  
   - 结果：订单进入 Orderbook，但 metadata 仍为空  

2. **metadata 先删除后仍残留订单**  
   - 订单被移除/状态转换时，metadata 被删除  
   - 但 orderbook 中该订单未被正确移除（可能是 kind 识别失败或时序问题）  

3. **跨线程更新不一致**  
   - 订单更新与 metadata 更新在不同任务中  
   - 短时间窗口可能出现 orderbook 已更新但 metadata 还未写入  

#### 2.4 定位方法（强相关）

**文件**：`drift-rs/crates/src/dlob/mod.rs`  
**方法**：`DLOB::log_missing_order_events_helper()`  
**行号**：364-385  

启用 `dlob_dbg` 后，该函数会打印 `order_id` 的 Insert/Remove 轨迹，用于判断：  
- 是否有 Insert 但 metadata 未写入  
- 是否 Remove 后残留订单  
- 事件顺序是否异常  

### 影响评估

#### 2.1 对出价计算的直接影响

**文件**：`keep-rs/src/jit/maker_select.rs`  
**方法**：`best_levels_with_makers()`  
**行号**：26-35

```rust
let book = dlob.get_l3_snapshot(market_index, market_type);

for bid in book.bids_with_price(...) {
    // 如果订单缺失 metadata，不会出现在这里
}
```

**影响**：
- ✅ **直接影响**：缺失 metadata 的订单不会出现在 L3Book 中
- ✅ **间接影响**：如果缺失的订单是最高价买单或最低价卖单，会导致 `best_bid` 或 `best_ask` 不准确
- ✅ **最终影响**：`drift_mid = (best_bid + best_ask) / 2` 会不准确

#### 2.2 严重程度评估

- **如果缺失的订单数量少**（如日志中的 1/1929 ≈ 0.05%），影响可能较小
- **但如果缺失的订单恰好是最高价/最低价**，影响会很大
- **需要监控**：缺失订单的价格是否在 top of book 附近

**严重程度**：中-高（取决于缺失订单的位置）

### 解决方案

#### 方案 1：延迟处理（推荐）

**实现**：
- 如果订单缺失 metadata，不要立即跳过
- 等待一段时间（如 100ms），看 metadata 是否会到达
- 或者使用默认 metadata（基于订单 ID 推断）

**优点**：
- 能够处理时序问题
- 实现相对简单

**缺点**：
- 增加延迟
- 可能仍然遗漏某些订单

#### 方案 2：RPC 回填

**实现**：
- 如果订单缺失 metadata，通过 RPC 查询订单信息
- 创建 metadata 并插入

**代码修改**：

```rust
if let Some(meta) = metadata.get(&order.id) {
    // 正常处理
} else {
    // 尝试通过 RPC 获取
    if let Ok(order_account) = drift.get_order_account(order.id).await {
        let meta = OrderMetadata::from_order_account(&order_account);
        metadata.insert(order.id, meta);
        // 重新处理订单
    } else {
        missing_metadata_count += 1;
    }
}
```

**优点**：
- 能够完全解决缺失问题
- 数据准确性高

**缺点**：
- 增加 RPC 调用，性能开销大
- 可能阻塞快照构建

#### 方案 3：监控和告警

**实现**：
- 监控缺失订单的数量和价格
- 如果缺失订单在 top of book 附近，发出告警
- 记录缺失订单的详细信息，便于分析

**代码修改**：

```rust
if missing_metadata_count > 0 {
    // 检查缺失订单的价格
    let missing_orders = collect_missing_order_prices(...);
    if is_near_top_of_book(missing_orders, &book) {
        log::error!(
            target: TARGET,
            "CRITICAL: Missing orders near top of book! market={}, orders={:?}",
            market_index,
            missing_orders
        );
    }
}
```

**优点**：
- 能够及时发现严重问题
- 不影响现有逻辑

**缺点**：
- 不能解决问题本身
- 只能用于监控

---

## 问题 3：Drift Mid 大幅偏离真实值

### 问题描述

即使只遗漏了拍卖订单的出价，计算出的 `drift_mid` 仍然大幅偏离真实的 `drift_mid`。怀疑是时间对齐问题或阻塞问题，导致 Binance 中间价和 DLOB 中间价获取的时间切片不一致。

### 原因分析

#### 3.1 时间对齐问题

**Binance 中间价时间戳**：

**文件**：`keep-rs/src/jit/feed_binance.rs`  
**行号**：140-145

```rust
.send(BinancePriceUpdate {
    binance_mid: bin_mid,
    ts_ms: fut_ts,  // Binance 事件时间戳（来自事件的 E 字段）
})
```

**DLOB 快照时间戳**：

**文件**：`keep-rs/src/filler.rs`  
**行号**：812

```rust
let dlob_update_ms = last_dlob_update_ms.load(Ordering::Relaxed);  // DLOB 快照更新时间
```

**问题**：
- `binance_ts`（Binance 事件时间）和 `dlob_update_ms`（DLOB 快照时间）可能不一致
- 如果时间差较大（如 > 100ms），比较的是不同时间点的价格

**示例场景**：
1. `binance_ts = 1000ms`（Binance 事件时间）
2. `dlob_update_ms = 800ms`（DLOB 快照时间）
3. **时间差 = 200ms**
4. 在这 200ms 内，DLOB 可能已经发生了变化
5. 但使用的是旧的快照，导致 `drift_mid` 不准确

#### 3.2 拍卖订单价格固化问题

结合**问题 1**：
- 即使 DLOB 快照时间对齐，拍卖订单的价格也可能不准确
- 因为拍卖订单价格在快照构建时被固化，无法根据当前 slot 更新
- 如果 slot 变化较大，价格偏差会显著

#### 3.3 阻塞导致的时间差

**文件**：`keep-rs/src/filler.rs`  
**行号**：816-850

```rust
tokio::spawn(async move {
    // 如果任务被阻塞，实际执行时间可能晚于 binance_ts
    let intent_opt = jit_strategy.maybe_intent(
        binance_mid,
        binance_ts,  // Binance 事件时间
        now_ms,      // 当前时间（可能晚于 binance_ts）
        dlob_update_ms,  // DLOB 快照时间（可能早于 binance_ts）
    );
});
```

**问题**：
- Binance 事件到达时间：`binance_ts`
- 实际处理时间：`now_ms`（可能晚于 `binance_ts`）
- DLOB 快照时间：`dlob_update_ms`（可能早于 `binance_ts`）
- **三个时间点不一致**

#### 3.4 时间对齐检查不足

**文件**：`keep-rs/src/jit/jit_strategy.rs`  
**行号**：100-118

```rust
if now_ms.saturating_sub(last_dlob_update_ms) > MAX_DLOB_STALE_MS {
    // 只检查 DLOB 是否过期（> 5 秒）
    return None;
}
// ❌ 不检查 Binance 时间戳和 DLOB 时间戳的差异
```

**问题**：
- 只检查 DLOB 是否过期（相对于当前时间）
- **不检查 Binance 时间戳和 DLOB 时间戳的差异**
- 即使 DLOB 不过期，时间不对齐也会导致价格不准确

### 影响评估

#### 3.1 对 drift_mid 的影响

**文件**：`keep-rs/src/jit/jit_strategy.rs`  
**行号**：152-173

```rust
let (best_bid, best_ask, dlob_slot) = match best_levels_with_makers(...) {
    Some(levels) => levels,
    None => return None,
};

let drift_mid = (best_bid.price as i128 + best_ask.price as i128) / 2;
```

**问题**：
1. `best_bid` 和 `best_ask` 来自 DLOB 快照（时间：`dlob_update_ms`）
2. `binance_mid` 来自 Binance 事件（时间：`binance_ts`）
3. 如果时间不一致，比较的是不同时间点的价格
4. 导致 `spread = binance_mid - drift_mid` 不准确

**严重程度**：高

#### 3.2 对 basis_ema 的影响

**文件**：`keep-rs/src/jit/jit_strategy.rs`  
**行号**：177-188

```rust
let spread = (binance_mid as f64) - (drift_mid as f64);
// ... EMA 计算
let ema = prev + BASIS_EMA_ALPHA * (spread - prev);
state.basis_ema = ema;

let reference_price = (binance_mid as f64 - 0.8 * basis).round() as i64;
```

**问题**：
- 如果 `spread` 不准确（因为时间不对齐），`basis_ema` 也会不准确
- 导致 `reference_price` 计算错误
- 最终影响交易决策

**严重程度**：高

#### 3.3 对交易决策的影响

- `reference_price` 不准确 → 交易价格不准确
- 可能导致：
  - 错过交易机会
  - 以错误价格成交
  - 亏损

**严重程度**：高

### 解决方案

#### 方案 1：时间对齐检查（推荐）

**实现**：在 `maybe_intent()` 中增加时间对齐检查

**代码修改**：

**文件**：`keep-rs/src/jit/jit_strategy.rs`  
**行号**：100-120

```rust
const MAX_TIME_DIFF_MS: u64 = 100;  // 最大允许时间差

// 检查 Binance 时间戳和 DLOB 时间戳的差异
let time_diff_ms = if binance_ts > dlob_update_ms {
    binance_ts.saturating_sub(dlob_update_ms)
} else {
    dlob_update_ms.saturating_sub(binance_ts)
};

if time_diff_ms > MAX_TIME_DIFF_MS {
    log::warn!(
        target: "filler",
        "jit skip: time misalignment market={}, binance_ts={}, dlob_ts={}, diff_ms={}, max_ms={}",
        market_index,
        binance_ts,
        dlob_update_ms,
        time_diff_ms,
        MAX_TIME_DIFF_MS
    );
    return None;
}
```

**优点**：
- 能够及时发现时间不对齐问题
- 避免使用不准确的价格
- 实现简单

**缺点**：
- 可能增加跳过次数
- 需要调整 `MAX_TIME_DIFF_MS` 阈值

#### 方案 2：使用 DLOB 快照的 slot 时间

**实现**：
- 记录 DLOB 快照构建时的 slot
- 在计算 `drift_mid` 时，使用快照的 slot 时间，而不是 `dlob_update_ms`

**优点**：
- 更准确地反映快照的时间点
- 与链上状态更一致

**缺点**：
- 需要修改数据结构
- 仍然存在拍卖订单价格固化问题

#### 方案 3：修复拍卖订单价格固化问题

**实现**：
- 解决**问题 1**，确保拍卖订单价格根据当前 slot 动态计算
- 这样即使时间有轻微差异，价格也会更准确

**优点**：
- 从根本上解决问题
- 提高价格准确性

**缺点**：
- 需要实现问题 1 的解决方案
- 实现复杂度较高

#### 方案 4：增加时间戳日志

**实现**：在 JIT trigger 日志中增加时间戳信息

**代码修改**：

**文件**：`keep-rs/src/jit/jit_strategy.rs`  
**行号**：220-240（在返回 JitIntent 之前）

```rust
log::info!(
    target: "filler",
    "jit trigger: market={}, binance_ts={}, dlob_ts={}, time_diff_ms={}, drift_mid={}, binance_mid={}, spread={}",
    market_index,
    binance_ts,
    dlob_update_ms,
    binance_ts.saturating_sub(dlob_update_ms),
    drift_mid,
    binance_mid,
    spread
);
```

**优点**：
- 能够监控时间对齐情况
- 便于问题诊断

**缺点**：
- 不能解决问题本身
- 增加日志量

#### 方案 5：使用订阅 Drift Mid 计算 EMA + 三路价格落盘（按秒）

**用户建议**：不再使用本地 DLOB 计算 EMA，而是使用订阅到的 Drift mid（WS 价格）计算 EMA；并且每秒记录三类 mid 到本地文件：  
1) Binance mid（订阅）  
2) Drift mid（订阅）  
3) 本地 DLOB 计算的 mid  

**预期效果**：
- EMA 与链上/订阅价格对齐，减少 DLOB 构造偏差影响
- 落盘便于后续对比：订阅 vs DLOB 的偏差、时间对齐问题

**注意点**：
- 需要新增 Drift mid 的订阅源（可参考测试程序 `D:\\RUST\\test\\测试1`）
- 需要保证落盘频率固定（建议每秒），避免过大 IO
- 需要记录时间戳以进行对齐分析

### 验证方法

1. **日志分析**：
   - 对比 `binance_ts` 和 `dlob_update_ms` 的差异
   - 如果差异 > 100ms，可能是时间对齐问题
   - 分析差异的分布和频率

2. **价格对比**：
   - 记录计算出的 `drift_mid` 和实际链上的 `drift_mid`
   - 如果差异持续存在，可能是拍卖订单价格固化问题
   - 对比不同时间点的价格差异

3. **阻塞分析**：
   - 记录 `binance_ts`、`dlob_update_ms`、`now_ms` 三个时间点
   - 分析是否存在阻塞导致的时间差
   - 检查任务队列和处理时间

---

## 总结

### 问题优先级

1. **问题 1（拍卖订单 Slot 计价固化）**：高优先级
   - 直接影响价格准确性
   - 需要修改数据结构

2. **问题 3（Drift Mid 时间对齐）**：高优先级
   - 直接影响交易决策
   - 可以通过时间检查快速缓解

3. **问题 2（缺失 Metadata）**：中优先级
   - 影响相对较小
   - 可以通过监控和告警管理

### 建议实施顺序

1. **短期**（立即实施）：
   - 问题 3 的方案 1：增加时间对齐检查
   - 问题 3 的方案 4：增加时间戳日志
   - 问题 2 的方案 3：增加监控和告警

2. **中期**（1-2 周内）：
   - 问题 1 的方案 1：保留原始订单信息
   - 问题 2 的方案 1：延迟处理缺失 metadata

3. **长期**（1 个月以上）：
   - 问题 1 的方案 2：单独存储拍卖订单（如果需要）
   - 问题 2 的方案 2：RPC 回填（如果需要）

### 监控指标

建议监控以下指标：
- 拍卖订单价格偏差（快照时 vs 查询时）
- 缺失 metadata 的订单数量和位置
- Binance 和 DLOB 时间戳差异
- `drift_mid` 计算准确性
- `basis_ema` 和 `reference_price` 的稳定性
