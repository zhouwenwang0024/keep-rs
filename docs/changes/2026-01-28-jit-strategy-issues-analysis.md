# JIT 策略问题分析与修复方案

## 1. 问题概述

当前 JIT 策略存在以下4个关键问题：

1. **基差 EMA 预热不足**：EMA 在启动后立即开始计算，导致初期不稳定
2. **edge_ppm 硬编码**：虽然可以从环境变量读取，但代码中仍有硬编码值
3. **RPC 断连风险**：DLOB 无法更新时，EMA 仍基于过时的 Drift 价格计算，导致极端错误
4. **EMA 更新频率不明确**：需要明确当前更新机制

---

## 2. 问题 1：基差 EMA 预热不足

### 2.1 当前实现

**代码位置：** `src/jit/jit_strategy.rs:132-140`

```rust
let now_sec = now_ms / 1000;
let last_sec = *self.last_basis_sec.get(&market_index).unwrap_or(&0);
if now_sec != last_sec {
    let spread = (binance_mid as f64) - (drift_mid as f64);
    let prev = *self.basis_ema.get(&market_index).unwrap_or(&spread);
    let ema = prev + BASIS_EMA_ALPHA * (spread - prev);
    self.basis_ema.insert(market_index, ema);
    self.last_basis_sec.insert(market_index, now_sec);
}
```

**问题：**
- EMA 在第一次调用时立即初始化（`unwrap_or(&spread)`）
- 没有预热期，初期 EMA 值不稳定
- 可能导致 `reference_price` 计算错误，触发错误的机会判断

### 2.2 影响

- **启动后前几分钟**：EMA 值波动大，`reference_price` 不准确
- **可能触发错误订单**：基于不稳定的 EMA 计算出的 `ref_px` 可能导致误判

### 2.3 修复方案

**方案：添加预热期检查**

在 `JitStrategy` 中添加：
```rust
struct JitStrategy {
    // ... 现有字段 ...
    start_time_ms: u64,  // 启动时间
    warmup_ms: u64,      // 预热时间（默认 5 分钟）
}

// 在 maybe_intent 中：
if now_ms.saturating_sub(self.start_time_ms) < self.warmup_ms {
    log::debug!(
        target: "filler",
        "jit skip: warmup period market={}, elapsed_ms={}, warmup_ms={}",
        market_index,
        now_ms.saturating_sub(self.start_time_ms),
        self.warmup_ms
    );
    return None;
}
```

**配置项：**
- 从 `Config` 添加 `jit_warmup_ms`（默认 300000ms = 5分钟）
- 环境变量：`JIT_WARMUP_MS`

---

## 3. 问题 2：edge_ppm 硬编码

### 3.1 当前实现

**代码位置：** `src/main.rs:71-72`

```rust
#[clap(long, env = "JIT_EDGE_PPM", default_value = "5000")]
pub jit_edge_ppm: i64,
```

**代码位置：** `src/filler.rs:204`

```rust
JitStrategy::new(
    config.jit_edge_ppm,
    // ...
)
```

**现状：**
- ✅ 已支持从环境变量 `JIT_EDGE_PPM` 读取
- ✅ 已支持从命令行参数 `--jit-edge-ppm` 读取
- ✅ 默认值为 5000（0.5%）

**问题：**
- 用户可能不知道可以从环境变量配置
- 需要确认 `.env.example` 中是否包含此配置

### 3.2 修复方案

**检查 `.env.example`：**
- 确认包含 `JIT_EDGE_PPM=5000`
- 添加注释说明用途

**代码层面：**
- ✅ 当前实现已正确，`edge_ppm` 可以从环境变量 `JIT_EDGE_PPM` 读取
- ✅ 默认值为 5000（0.5%）
- ⚠️ 需要检查 `.env.example` 是否包含此配置（当前未找到 `.env.example` 文件）

**建议：**
- 创建或更新 `.env.example`，添加 `JIT_EDGE_PPM=5000` 配置项
- 添加注释说明用途和取值范围

---

## 4. 问题 3：RPC 断连导致 DLOB 过时风险

### 4.1 风险场景

**场景描述：**
1. RPC 连接断开（网络故障、RPC 限流等）
2. DLOB 无法更新，停留在断连前的状态
3. Binance feed 仍在更新，EMA 继续计算
4. **问题**：EMA 基于**过时的 Drift 价格**和**最新的 Binance 价格**计算
5. 结果：基差计算错误，`reference_price` 异常，可能触发错误订单

**示例：**
```
t=0:    DLOB 正常更新，drift_mid=100, binance_mid=101, spread=1
t=100:  RPC 断连，DLOB 停止更新（drift_mid 仍为 100）
t=200:  Binance 价格涨到 110，EMA 基于 (110-100=10) 计算
t=300:  EMA 收敛到 10，但实际基差可能是 0（如果 Drift 也涨到 110）
        结果：reference_price 计算错误，可能触发错误订单
```

### 4.2 当前实现分析

**DLOB 更新机制：**
- DLOB 通过 WebSocket 订阅更新（`book_update` 事件）
- 如果 WebSocket 断开，DLOB 停止更新
- 但 `maybe_intent` 仍会被调用（Binance feed 触发）

**代码位置：** `src/filler.rs:247-563`

```rust
loop {
    tokio::select! {
        book_update = async { ... } => {
            // 更新 DLOB
        },
        binance_update = async { ... } => {
            // 触发 JIT 检查
            if let Some(intent) = strategy.maybe_intent(...) {
                // 可能使用过时的 DLOB
            }
        }
    }
}
```

**问题：**
- 没有检查 DLOB 的"新鲜度"（last update time）
- 没有检查 WebSocket 连接状态
- EMA 计算不验证 DLOB 数据是否过期

### 4.3 修复方案

**方案 1：检查 DLOB 更新时间戳（推荐）**

**问题：** Slot 不能直接转换成时间，因为 Solana 的 slot 时间不固定（平均 ~400ms，但会波动）。

**更好的方案：** 使用 `last_book_event_ms` 时间戳。

在 `FillerBot` 中维护 DLOB 最后更新时间：
```rust
struct FillerBot {
    // ... 现有字段 ...
    last_dlob_update_ms: Arc<AtomicU64>,  // DLOB 最后更新时间
}

// 在 book_update 事件中更新：
last_dlob_update_ms.store(now_ms, Ordering::Relaxed);

// 在 maybe_intent 中检查：
pub fn maybe_intent(
    &mut self,
    // ... 现有参数 ...
    last_dlob_update_ms: u64,  // 新增参数
    now_ms: u64,
    // ...
) -> Option<JitIntent> {
    const MAX_DLOB_STALE_MS: u64 = 5000;  // 5 秒
    if now_ms.saturating_sub(last_dlob_update_ms) > MAX_DLOB_STALE_MS {
        log::warn!(
            target: "filler",
            "jit skip: stale DLOB market={}, age_ms={}, max_ms={}",
            market_index,
            now_ms.saturating_sub(last_dlob_update_ms),
            MAX_DLOB_STALE_MS
        );
        return None;
    }
    // ...
}
```

**或者方案 1b：使用 slot 差估算（不推荐，但可行）**

如果必须使用 slot，可以估算：
```rust
// Solana 平均 slot 时间约 400ms，但会波动
const AVG_SLOT_MS: u64 = 400;
const MAX_DLOB_STALE_MS: u64 = 5000;  // 5 秒
const MAX_DLOB_STALE_SLOTS: u64 = MAX_DLOB_STALE_MS / AVG_SLOT_MS;  // ~12 slots

if current_slot.saturating_sub(dlob_slot) > MAX_DLOB_STALE_SLOTS {
    // DLOB 可能过期
}
```

**推荐：方案 1（使用时间戳）**，因为：
- 更准确（不依赖 slot 时间估算）
- 代码中已有 `last_book_event_ms`，可以直接使用

**方案 2：检查 WebSocket 连接状态**

在 `FillerBot` 中跟踪 WebSocket 连接状态：
```rust
struct FillerBot {
    // ... 现有字段 ...
    ws_connected: Arc<AtomicBool>,
}

// 在 maybe_intent 中：
if !self.ws_connected.load(Ordering::Relaxed) {
    log::warn!(
        target: "filler",
        "jit skip: WS disconnected market={}",
        market_index
    );
    return None;
}
```

**方案 3：暂停 EMA 更新（当 DLOB 过期时）**

在 EMA 更新逻辑中添加检查：
```rust
let now_sec = now_ms / 1000;
let last_sec = *self.last_basis_sec.get(&market_index).unwrap_or(&0);

// 如果 DLOB 过期，不更新 EMA
if now_ms.saturating_sub(last_dlob_update_ms) <= MAX_DLOB_STALE_MS {
    if now_sec != last_sec {
        let spread = (binance_mid as f64) - (drift_mid as f64);
        let prev = *self.basis_ema.get(&market_index).unwrap_or(&spread);
        let ema = prev + BASIS_EMA_ALPHA * (spread - prev);
        self.basis_ema.insert(market_index, ema);
        self.last_basis_sec.insert(market_index, now_sec);
    }
} else {
    log::warn!(
        target: "filler",
        "jit skip: DLOB stale, EMA update paused market={}, age_ms={}, max_ms={}",
        market_index,
        now_ms.saturating_sub(last_dlob_update_ms),
        MAX_DLOB_STALE_MS
    );
}
```

**推荐：方案 1 + 方案 3**
- 方案 1：防止使用过时 DLOB 进行机会判断
- 方案 3：防止基于过时数据更新 EMA

**实现细节：**
- `FillerBot` 中已有 `last_book_event_ms` 记录最后一次 book_update 的时间戳
- 在 `maybe_intent` 调用时，传入 `last_book_event_ms` 和 `now_ms`
- 检查 `now_ms - last_book_event_ms > MAX_DLOB_STALE_MS`（例如 5 秒）

**为什么不用 slot？**
- Solana 的 slot 时间不固定（平均 ~400ms，但会波动）
- 使用时间戳更准确、更可靠
- 代码中已有 `last_book_event_ms`，无需额外维护

---

## 5. 问题 4：EMA 更新频率分析

### 5.1 当前实现

**代码位置：** `src/jit/jit_strategy.rs:132-140`

```rust
let now_sec = now_ms / 1000;
let last_sec = *self.last_basis_sec.get(&market_index).unwrap_or(&0);
if now_sec != last_sec {
    let spread = (binance_mid as f64) - (drift_mid as f64);
    let prev = *self.basis_ema.get(&market_index).unwrap_or(&spread);
    let ema = prev + BASIS_EMA_ALPHA * (spread - prev);
    self.basis_ema.insert(market_index, ema);
    self.last_basis_sec.insert(market_index, now_sec);
}
```

**更新机制：**
- **触发条件**：`now_sec != last_sec`（每秒最多更新一次）
- **触发来源**：Binance 价格更新事件（`binance_update`）
- **实际频率**：取决于 Binance feed 更新频率，但**最多每秒一次**

### 5.2 更新频率计算

**EMA 参数：**
- `BASIS_EMA_POINTS = 300`
- `BASIS_EMA_ALPHA = 2.0 / (300 + 1) ≈ 0.0066`

**理论更新频率：**
- **最大频率**：1 Hz（每秒一次）
- **实际频率**：取决于 Binance feed 更新频率
  - 如果 Binance 更新频率 > 1 Hz，会被节流到 1 Hz
  - 如果 Binance 更新频率 < 1 Hz，按实际频率更新

**EMA 收敛时间：**
- 300 个数据点 ≈ 300 秒（5 分钟）达到稳定
- 这与用户要求的"5 分钟预热"一致

### 5.3 潜在问题

**问题 1：更新频率可能过低**
- 如果 Binance feed 更新频率 < 1 Hz，EMA 更新会更慢
- 可能导致 EMA 对市场变化响应延迟

**问题 2：更新频率可能过高（被节流）**
- 如果 Binance feed 更新频率 > 1 Hz，会被节流到 1 Hz
- 可能丢失部分价格变化信息

**问题 3：不同市场的更新频率不一致**
- 不同市场的 Binance feed 更新频率可能不同
- 导致不同市场的 EMA 收敛速度不一致

### 5.4 建议

**当前实现合理，但可以优化：**

1. **保持每秒更新一次的限制**（避免过度更新）
2. **添加日志**：记录 EMA 更新频率，便于监控
3. **考虑动态调整**：根据市场波动性调整更新频率（可选）

---

## 6. 修复优先级

### P0（必须修复）
1. **问题 3：RPC 断连风险** - 可能导致极端错误价格和错误订单
2. **问题 1：EMA 预热不足** - 启动初期可能触发错误订单

### P1（建议修复）
3. **问题 2：edge_ppm 配置** - 确认文档和示例配置完整

### P2（可选优化）
4. **问题 4：EMA 更新频率** - 当前实现基本合理，可添加监控日志

---

## 7. 修复方案总结

### 7.1 添加预热期

**修改文件：** `src/jit/jit_strategy.rs`, `src/main.rs`, `src/filler.rs`

**改动：**
1. `JitStrategy` 添加 `start_time_ms` 和 `warmup_ms` 字段
2. `Config` 添加 `jit_warmup_ms`（默认 300000ms）
3. `maybe_intent` 中添加预热期检查

### 7.2 添加 DLOB 新鲜度检查

**修改文件：** `src/jit/jit_strategy.rs`, `src/filler.rs`

**改动：**
1. `maybe_intent` 添加 `last_dlob_update_ms` 参数
2. 检查 `now_ms - last_dlob_update_ms > MAX_DLOB_STALE_MS`（例如 5000ms = 5 秒）
3. 如果过期，跳过机会判断和 EMA 更新
4. 在 `book_update` 事件中更新 `last_book_event_ms`（已有，无需修改）

### 7.3 确认 edge_ppm 配置

**修改文件：** `.env.example`

**改动：**
1. 确认包含 `JIT_EDGE_PPM=5000`
2. 添加注释说明

### 7.4 添加 EMA 更新频率监控

**修改文件：** `src/jit/jit_strategy.rs`

**改动：**
1. 添加日志记录 EMA 更新频率（可选）
2. 记录 DLOB 过期情况

---

## 8. 测试建议

### 8.1 预热期测试
- 启动后前 5 分钟不应触发任何 JIT 订单
- 5 分钟后应正常触发

### 8.2 DLOB 过期测试
- 模拟 WebSocket 断开
- 验证 JIT 策略正确跳过机会判断
- 验证 EMA 更新被暂停

### 8.3 edge_ppm 配置测试
- 通过环境变量设置不同的 `JIT_EDGE_PPM`
- 验证配置生效

---

## 9. 代码位置索引

- **EMA 更新逻辑：** `src/jit/jit_strategy.rs:132-140`
- **EMA 参数：** `src/jit/jit_strategy.rs:10-12`
- **edge_ppm 配置：** `src/main.rs:71-72`
- **JIT 策略初始化：** `src/filler.rs:200-210`
- **Binance 触发逻辑：** `src/filler.rs:620-725`
- **DLOB 更新逻辑：** `src/filler.rs:247-563`（book_update 事件）

---

## 10. 未解决问题

1. **EMA 更新频率监控**：需要添加日志记录实际更新频率
2. **不同市场的 EMA 收敛速度**：可能需要分别跟踪每个市场的预热状态
3. **WebSocket 重连后的恢复机制**：需要确认 DLOB 恢复更新后，EMA 是否能正确恢复
