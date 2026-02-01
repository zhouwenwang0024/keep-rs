# 深度分析报告：Filler vs 测试程序 + Liquidator 模块化方案

## 一、整体流程深度对比

### 1.1 事件驱动模型

**Rust Filler（当前实现）**
```
主事件循环（单线程 tokio::select!）:
├── swift_order_stream.recv() → try_swift_fill().await
├── book_rx.recv() → 
│   ├── for market in markets (串行)
│   │   ├── find_crossing_region_all_types() (同步)
│   │   ├── try_onchain_cross().await (阻塞：等待 user_cache.get_user_or_fetch)
│   │   ├── jit_strategy.maybe_intent() (同步)
│   │   └── try_jit().await (阻塞：等待 user_cache.get_user_or_fetch)
│   └── 所有市场处理完才继续
├── slot_rx.recv() → 更新 slot
└── pyth_price_feed.recv() → 更新价格缓存
```

**关键瓶颈**：
- **市场串行**：`for market in &market_ids` 循环，市场1处理完才处理市场2
- **await 阻塞**：`try_jit().await` 和 `try_onchain_cross().await` 会等待函数完成
- **缓存缺失阻塞**：`user_cache.get_user_or_fetch().await` 如果缓存缺失，会等待 RPC 请求
- **单循环处理所有市场**：高市场数量下，处理时间线性增长

**Python 测试程序（当前实现）**
```
事件驱动 + 多任务并行:
├── AuctionSubscriber.on_account_update → on_user_update() (同步回调)
│   └── L2.update_from_user() → _emit_intents_from_changes()
│       └── _market_queues[mi].put_nowait(intent) (非阻塞)
├── oracle_watch_loop() (独立任务，5ms 轮询)
│   └── L2.on_oracle_update() → _emit_intents_from_changes()
├── slot_watch_loop() (独立任务，5ms 轮询)
│   └── L2.on_slot_advance() → _emit_intents_from_changes()
├── _market_worker(mi) (每市场独立任务，从队列取 intent)
│   ├── await q.get() (等待 intent)
│   ├── debounce (可选)
│   ├── in-flight 限流检查
│   └── ENGINE.arb_with_makers().await (不阻塞其他市场)
└── binance_sol_tick8.on_opportunity (事件回调)
    └── JIT_STRATEGY.on_binance_opportunity() (create_task，不等待)
        └── _fire_jit() (后台任务，无限并发)
```

**关键优势**：
- **市场并行**：每市场独立 worker，互不阻塞
- **事件驱动**：user/oracle/slot 更新立即触发，无需轮询
- **非阻塞发送**：JIT 使用 `create_task`，不等待返回
- **队列缓冲**：intent 先入队，worker 异步处理

### 1.2 数据流与缓存策略

**Rust Filler**
```
数据源:
├── WebSocket (user, stats, slot) → WsAccountCache (DashMap)
├── WebSocket (markets, oracles) → DriftClient.backend
├── Swift orders → SwiftOrderStream
├── Pyth → tokio::sync::mpsc::Receiver
└── Blockhash → BlockhashSubscriber (2s 刷新)

缓存策略:
├── WsAccountCache: DashMap<Pubkey, UserEntry> (并发安全)
├── 缺失时: await get_account_value() (阻塞)
└── 无预热机制: 首次访问可能触发 RPC
```

**Python 测试程序**
```
数据源:
├── AuctionSubscriber (user accounts) → USER_ACC_CACHE (dict)
├── WebsocketDriftClientAccountSubscriber (oracles) → ORACLE_SUB
├── SlotSubscriber → SLOT_SUB
├── Binance WS → SharedTickState
└── DLOB WS → official_l2_mid (可选)

缓存策略:
├── USER_ACC_CACHE: 全局 dict，热路径只读
├── ReferrerDirectory: 后台异步预热，热路径只读缓存
├── L2Book: 本地聚合，事件驱动更新
└── 缺失时: 跳过（不阻塞），等待下次更新
```

### 1.3 交易构造与发送

**Rust Filler**
```
try_jit() / try_onchain_cross():
├── user_cache.get_user_or_fetch().await (可能阻塞)
├── user_cache.get_stats_or_fetch().await (可能阻塞)
├── TransactionBuilder 构造 (同步)
├── tx_worker_ref.send_tx_with_jito() (同步，发送到 channel)
└── 返回 (await 完成)

TxWorker:
├── send_tx() → rt.spawn(async move { ... }) (后台任务)
│   ├── get_latest_blockhash().await (从缓存读取，无 RPC)
│   ├── sign_and_send_with_config() (异步发送)
│   └── jito.send_bundle_base64() (异步发送)
└── 不阻塞调用者
```

**Python 测试程序**
```
ArbPerpEngine.arb_with_makers() / jit_with_makers():
├── _build_remaining_for_makers() (同步，从缓存读取)
├── _build_raw() (同步，LUT 复用)
├── _send_business_ix() (异步)
│   ├── _send_jito() (并行)
│   └── _send_rpc() (并行)
└── 立即返回 (不等待发送完成)

BlockhashManager:
├── 后台 1s 刷新
└── _bhm.get() (同步读取缓存)
```

## 二、订阅方式与种类对比

### 2.1 Rust Filler 订阅

**订阅类型**：
1. **WebSocket Program Account Subscriber** (用户账户)
   - Filter: `get_user_filter()`, `get_non_idle_user_filter()`
   - Encoding: `Base64Zstd`
   - 回调: 更新 `WsAccountCache` + 触发 `book_tx`

2. **WebSocket Program Account Subscriber** (用户统计)
   - Filter: `get_user_stats_filter()`
   - Encoding: `Base64Zstd`
   - 回调: 更新 `WsAccountCache.stats`

3. **SlotSubscriber** (Slot 更新)
   - 回调: 更新 DLOB + 发送 `slot_tx` + 触发 `book_tx`

4. **DriftClient.subscribe_markets()** (市场账户)
   - 订阅所有目标市场的 PerpMarket 账户

5. **DriftClient.subscribe_oracles()** (预言机)
   - 订阅所有目标市场的 Oracle 账户

6. **SwiftOrderStream** (Swift 订单)
   - gRPC 流订阅

7. **Pyth Lazer Client** (Pyth 价格)
   - WebSocket 订阅

8. **BlockhashSubscriber** (Blockhash)
   - 后台 2s 轮询 RPC

**订阅特点**：
- 使用 WebSocket 订阅账户变化
- 初始同步：`sync_user_accounts_ws()` 和 `sync_stats_accounts_ws()` 一次性拉取所有账户
- 事件驱动：账户更新立即触发回调

### 2.2 Rust Liquidator 订阅

**订阅类型**（与 Filler 高度重叠）：
1. **WebSocket Program Account Subscriber** (用户账户) - 相同
2. **WebSocket Program Account Subscriber** (用户统计) - 相同
3. **SlotSubscriber** (Slot 更新) - 相同
4. **DriftClient.subscribe_markets()** - 相同
5. **DriftClient.subscribe_oracles()** - 相同
6. **Pyth Lazer Client** - 相同
7. **BlockhashSubscriber** - 相同

**差异**：
- Liquidator 不订阅 Swift orders
- Liquidator 需要订阅 spot markets（如果启用 spot liquidation）

### 2.3 Python 测试程序订阅

**订阅类型**：
1. **AuctionSubscriber** (用户账户)
   - 基于 gRPC 流（但代码注释说"忽略 gRPC 流"）
   - 实际使用 WebSocket: `AccountSubscriptionConfig("websocket")`
   - 回调: `on_user_update()` → `L2.update_from_user()`

2. **WebsocketDriftClientAccountSubscriber** (预言机)
   - 订阅 perp markets 的 oracle
   - 回调: `oracle_watch_loop()` 轮询读取

3. **SlotSubscriber** (Slot 更新)
   - 回调: `slot_watch_loop()` 轮询读取

4. **Binance WebSocket** (外部价格)
   - Spot + Futures bookTicker

5. **DLOB WebSocket** (可选，用于 official_l2_mid)
   - 官方 DLOB 的 L2 数据

**订阅特点**：
- **不使用 gRPC**：代码注释明确说明"忽略 gRPC 流"
- **轮询读取**：oracle 和 slot 使用独立任务轮询（5ms 间隔），而非事件回调
- **事件驱动更新**：user 更新通过回调立即触发 L2 重建

### 2.4 订阅方式差异总结

| 特性 | Rust Filler | Rust Liquidator | Python 测试程序 |
|------|-------------|-----------------|-----------------|
| User 订阅 | WebSocket 事件回调 | WebSocket 事件回调 | WebSocket 事件回调 |
| Oracle 订阅 | WebSocket 事件回调 | WebSocket 事件回调 | WebSocket + 轮询读取 |
| Slot 订阅 | WebSocket 事件回调 | WebSocket 事件回调 | WebSocket + 轮询读取 |
| Swift 订单 | gRPC 流 | ❌ | ❌ |
| Pyth 价格 | WebSocket | WebSocket | ❌ |
| Blockhash | 2s 轮询 RPC | 2s 轮询 RPC | 1s 轮询 RPC |
| 初始同步 | 一次性 RPC 拉取所有 | 一次性 RPC 拉取所有 | 通过 DriftClient.subscribe() |

**关键差异**：
- Python 使用**轮询读取**而非事件回调来处理 oracle/slot，可能增加延迟但更简单
- Rust 使用**事件回调**，延迟更低但需要处理回调并发

## 三、Liquidator 模块化方案

### 3.1 当前架构问题

**重复订阅**：
- Filler 和 Liquidator 都订阅相同的：
  - User accounts (WebSocket)
  - User stats (WebSocket)
  - Slot (WebSocket)
  - Markets (WebSocket)
  - Oracles (WebSocket)
  - Pyth prices
  - Blockhash

**资源浪费**：
- 两个独立的 `DriftClient` 实例
- 两个独立的 `DLOB` 实例
- 两个独立的 `WsAccountCache` 实例
- 重复的 WebSocket 连接

### 3.2 模块化设计

**目标**：将 Liquidator 改为可选的 Filler 模块，共享所有订阅和数据。

**关键澄清**：Filler 本身已经完成订阅与缓存初始化，模块化并不是“再创建一套订阅”。更合理的做法是**把 Filler 现有的订阅/缓存构建逻辑抽成共享构建器**（例如 `SharedSubscriptions`），由 Filler 与 Liquidator 共同持有。

**共享资源**：
```rust
pub struct SharedResources {
    drift: DriftClient,
    dlob: &'static DLOB,
    user_cache: WsAccountCache,
    slot_rx: tokio::sync::mpsc::Receiver<u64>,
    book_rx: tokio::sync::mpsc::Receiver<()>,
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    pyth_price_feed: tokio::sync::mpsc::Receiver<PythPriceUpdate>,
    market_state: &'static MarketState,  // Liquidator 专用
}
```

**Liquidator 模块接口**：
```rust
pub struct LiquidatorModule {
    // 输入：共享资源
    shared: SharedResources,
    
    // 内部状态
    config: LiquidatorConfig,
    market_state: &'static MarketState,
    dashboard_state: DashboardStateRef,
    liq_tx: tokio::sync::mpsc::Sender<LiquidationIntent>,
    
    // 输出：liquidation worker
    _worker_handle: tokio::task::JoinHandle<()>,
}
```

### 3.3 具体实现方案

**步骤 1：提取共享订阅逻辑**

创建 `src/shared_subscriptions.rs`：
```rust
pub struct SharedSubscriptions {
    drift: DriftClient,
    dlob: &'static DLOB,
    user_cache: WsAccountCache,
    slot_rx: tokio::sync::mpsc::Receiver<u64>,
    book_rx: tokio::sync::mpsc::Receiver<()>,
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    pyth_price_feed: tokio::sync::mpsc::Receiver<PythPriceUpdate>,
    _ws_subscriptions: WsSubscriptions,
}

impl SharedSubscriptions {
    pub async fn new(
        drift: DriftClient,
        market_ids: Vec<MarketId>,
        spot_market_ids: Option<Vec<MarketId>>,
    ) -> Self {
        // 统一的订阅逻辑
        // 返回共享资源
    }
}
```

**步骤 2：重构 Filler**

```rust
impl FillerBot {
    pub async fn new(
        config: Config,
        drift: DriftClient,
        metrics: Arc<Metrics>,
        shared: Option<SharedSubscriptions>,  // 可选：外部提供共享资源
    ) -> Self {
        let shared = shared.unwrap_or_else(|| {
            // 复用现有订阅构建逻辑，避免重复创建订阅
            // SharedSubscriptions::new(...) 只是“抽取已有步骤”
        });
        
        // 使用共享资源
        FillerBot {
            drift: shared.drift,
            dlob: shared.dlob,
            user_cache: shared.user_cache,
            // ...
        }
    }
}
```

**步骤 3：创建 Liquidator 模块**

```rust
pub mod liquidator {
    pub struct LiquidatorModule {
        config: LiquidatorConfig,
        market_state: &'static MarketState,
        dashboard_state: DashboardStateRef,
        liq_tx: tokio::sync::mpsc::Sender<LiquidationIntent>,
        _worker_handle: tokio::task::JoinHandle<()>,
    }
    
    impl LiquidatorModule {
        pub fn new(
            shared: &SharedSubscriptions,
            config: LiquidatorConfig,
            tx_sender: TxSender,
            metrics: Arc<Metrics>,
        ) -> Self {
            // 初始化 market_state
            // 启动 liquidation worker
            // 返回模块实例
        }
        
        pub async fn process_event(
            &self,
            event: LiquidationEvent,  // user update, slot update, etc.
        ) {
            // 处理清算逻辑
            // 发送到 liq_tx
        }
    }
}
```

**步骤 4：在 Filler 中集成**

```rust
impl FillerBot {
    pub async fn new(...) -> Self {
        let shared = SharedSubscriptions::new(...).await;
        
        // 可选：创建 Liquidator 模块
        let liquidator = if config.liquidator_enabled {
            Some(LiquidatorModule::new(
                &shared,
                config.liquidator_config,
                tx_worker_ref.clone(),
                metrics.clone(),
            ))
        } else {
            None
        };
        
        FillerBot {
            // ...
            liquidator,
        }
    }
    
    pub async fn run(self) {
        // 在主循环中处理 liquidator 事件
        tokio::select! {
            // ... existing events ...
            event = liquidator_event_rx.recv() => {
                if let Some(liq) = &self.liquidator {
                    liq.process_event(event).await;
                }
            }
        }
    }
}
```

### 3.4 数据流设计

**事件流**：
```
User Update (WebSocket) 
    → WsAccountCache (共享)
    → DLOB (共享)
    → Filler: book_tx.send() (触发 ARB/JIT)
    → Liquidator: 检查 margin status (如果启用)

Slot Update (WebSocket)
    → DLOB (共享)
    → Filler: slot_rx.send()
    → Liquidator: 更新 oracle prices (如果启用)

Oracle Update (WebSocket)
    → DriftClient.backend (共享)
    → Filler: 用于 cross detection
    → Liquidator: 用于 margin calculation
```

**清算流程**：
```
LiquidatorModule.process_event()
    → 检查 margin status
    → 如果 liquidatable → liq_tx.send()
    → LiquidationWorker (独立任务)
        → 构造 liquidation tx
        → tx_sender.send_tx_with_jito()
```

### 3.5 配置与初始化

**Config 扩展**：
```rust
pub struct Config {
    // ... existing fields ...
    
    /// Enable liquidator module in filler
    #[clap(long, env = "LIQUIDATOR_ENABLED", default_value = "false")]
    pub liquidator_enabled: bool,
    
    /// Use spot liquidation
    #[clap(long, env = "USE_SPOT_LIQUIDATION", default_value = "true")]
    pub use_spot_liquidation: bool,
    
    /// Minimum collateral threshold
    #[clap(long, env = "MIN_COLLATERAL", default_value = "1000000")]
    pub min_collateral: u64,
}
```

**初始化顺序**：
1. 创建 `SharedSubscriptions`（一次性订阅所有资源）
2. 创建 `FillerBot`（使用共享资源）
3. 如果启用，创建 `LiquidatorModule`（使用共享资源）
4. 启动主循环（处理所有事件）

### 3.6 优势与注意事项

**优势**：
- ✅ **消除重复订阅**：所有数据源只订阅一次
- ✅ **共享缓存**：user_cache、DLOB 等共享，减少内存占用
- ✅ **统一事件流**：所有事件在一个循环中处理
- ✅ **灵活配置**：可以通过配置开关启用/禁用 liquidator

**注意事项**：
- ⚠️ **生命周期管理**：`&'static` 引用需要确保资源不被释放
- ⚠️ **并发安全**：确保 `WsAccountCache` 等共享资源的并发访问安全
- ⚠️ **错误隔离**：liquidator 的错误不应影响 filler
- ⚠️ **性能影响**：liquidator 的 margin 计算可能增加主循环延迟

## 四、Rust 版本优化建议

### 4.1 并发模型优化

**建议 1：Per-Market Worker**
```rust
// 为每个市场创建独立的 worker task
for market in &market_ids {
    let market_index = market.index();
    let tx_worker_ref = tx_worker_ref.clone();
    // ... other clones ...
    
    tokio::spawn(async move {
        let mut market_rx = market_book_rx.recv();  // 每个市场独立 channel
        while let Some(book_update) = market_rx.recv().await {
            // 处理该市场的 cross/JIT
            // 不阻塞其他市场
        }
    });
}
```

**建议 2：非阻塞下单**
```rust
// 将 try_jit 改为返回 Future，不 await
pub fn try_jit_spawn(
    // ... params ...
) {
    tokio::spawn(async move {
        // 所有 await 在这里，不阻塞调用者
        try_jit(...).await;
    });
}
```

**建议 3：缓存预热**
```rust
// 启动时预热常用账户
async fn warmup_cache(
    user_cache: &WsAccountCache,
    drift: &DriftClient,
    accounts: Vec<Pubkey>,
) {
    let tasks: Vec<_> = accounts.into_iter()
        .map(|pk| {
            let cache = user_cache.clone();
            let drift = drift.clone();
            tokio::spawn(async move {
                let _ = cache.get_user_or_fetch(&drift, &pk).await;
            })
        })
        .collect();
    futures::future::join_all(tasks).await;
}
```

### 4.2 订阅优化

**建议 4：Blockhash 刷新频率**
```rust
// 将 BlockhashSubscriber 刷新频率改为 1s（与 Python 对齐）
BlockhashSubscriber::new(Duration::from_secs(1), rpc_client)
```

**建议 5：事件驱动 Oracle/Slot**
```rust
// 当前已使用事件回调，无需改动
// 但可以优化回调处理，避免阻塞
```

### 4.3 数据流优化

**建议 6：Binance Feed 放宽条件**
```rust
// 将 BINANCE_STALE_MS 从 100ms 改为 500ms 或 1000ms
const BINANCE_STALE_MS: u64 = 500;  // 或 1000
// 放宽 skew 检查
if skew_ms <= BINANCE_STALE_MS * 2 {  // 允许更大的 skew
    // ...
}
```

**建议 7：JIT 门控优化**
```rust
// 区分 JIT 和 ARB 的门控策略
// JIT: 更激进（减少 warmup、cooldown）
// ARB: 更稳健（保持现有门控）
```

### 4.4 架构优化

**建议 8：引入 Intent Queue**
```rust
// 类似 Python 的 market_queues
struct IntentQueue {
    arb_intents: VecDeque<ArbIntent>,
    jit_intents: VecDeque<JitIntent>,
}

// 事件触发时，将 intent 入队
// 独立 worker 从队列取 intent 并处理
```

**建议 9：共享资源模块化**
```rust
// 实现 SharedSubscriptions（如 3.2 节）
// 支持 Filler + Liquidator 共享订阅
```

## 五、总结

### 5.1 关键差异

1. **并发模型**：Python 使用 per-market worker + 队列，Rust 使用单循环串行
2. **事件处理**：Python 使用轮询读取 + 事件回调混合，Rust 使用纯事件回调
3. **阻塞点**：Rust 在缓存缺失时会 await RPC，Python 跳过缺失数据
4. **发送策略**：Python 使用 create_task 不等待，Rust 使用 await 等待函数完成

### 5.2 优化优先级

**高优先级**（立即实施）：
1. Per-market worker 架构
2. 非阻塞下单（spawn task）
3. 缓存预热机制
4. Blockhash 刷新频率调整

**中优先级**（短期实施）：
5. Binance feed 条件放宽
6. Intent queue 引入
7. Liquidator 模块化

**低优先级**（长期优化）：
8. 订阅方式统一
9. 性能监控与调优

### 5.3 Liquidator 模块化收益

- **资源节省**：消除重复订阅，减少 ~50% WebSocket 连接
- **内存优化**：共享缓存，减少 ~30% 内存占用
- **维护简化**：统一事件流，减少代码重复
- **灵活性**：通过配置开关启用/禁用，无需单独进程

---

**报告生成时间**：2026-01-29  
**分析范围**：Filler vs 测试程序流程对比、订阅方式对比、Liquidator 模块化方案

---

## 六、针对问题的补充深度分析（逐条回答）

### 6.1 Rust 是否比测试程序多订阅了 PerpMarket（合约市场）数据？
**结论**：是。  
Filler 明确调用 `drift.subscribe_markets(&market_ids)`，订阅 **PerpMarket 账户**（合约市场状态/配置）。测试程序侧并没有显式订阅 “PerpMarket 账户”，而是通过用户账户更新 + Oracle 订阅驱动 L2 聚合与机会判断。

**影响**：Filler 订阅覆盖更全，但连接与处理更重；测试程序更轻量、触发更快。

### 6.2 Filler 已有数据，为何还要创建新的 SharedSubscriptions？
不需要重新创建一套订阅。  
应当**复用 Filler 已有订阅与缓存**，把 `setup_ws()`、`subscribe_markets()`、`subscribe_oracles()` 等步骤抽成共享构建器，Liquidator 直接使用该共享资源即可。

### 6.3 JIT 合并到 ARB 后，为什么 ARB 一次成交都没有？
从当前代码路径看，这更像“触发/数据路径被阻断”，而不是“性能慢”。可能原因（按概率排序）：  
1. **book_rx 触发减少**：`EVENT_WINDOW_MS` 节流或 channel 满导致 `book_tx.try_send()` 被丢弃，ARB 触发减少。  
2. **缓存缺失阻塞**：合并后主循环新增 await（maker/user/stats 缓存 miss），阻塞导致后续 book 更新被覆盖。  
3. **限频/过滤逻辑影响**：`limiter.allow_event()` 依赖 order_id；若 order_id 变化或重复，会导致全部被限频。  
4. **DLOB 数据路径变化**：JIT 与 ARB 共用 DLOB 更新，若改动了 DLOB 价格逻辑，ARB 的 `find_crossing_region_all_types()` 返回更少。  
5. **门控迁移错误**：若把 JIT 的 stale/warmup/edge 逻辑误加到 ARB，会导致 ARB 机会被过滤。  

建议优先验证：  
- `book_rx` 是否持续触发  
- `find_crossing_region_all_types()` 是否稳定返回 Some  
- `limiter.allow_event()` 是否大量 false  
- `try_onchain_cross()` 是否频繁因缓存缺失提前 return

### 6.4 当前 Filler 订阅了多少条 WS？每条订阅多少种数据？
**按逻辑链路拆分**：  
- 账户订阅（用户账户）→ 1 条 WS  
- 账户订阅（用户统计）→ 1 条 WS  
- Drift ws（slot/markets/oracles）→ 通常共用 1 条 WS  
- Pyth Lazer → 1 条 WS  
- Swift → gRPC 流（非 WS）  
- Binance feed → **每市场 2 条 WS**（spot + futures）  

**估算**：基础 4~5 条 WS（不含 Binance），外加每市场 2 条 Binance WS。  
如果同时启用 Liquidator 且不共享订阅，WS 数量会重复翻倍。

### 6.5 如何学习测试程序：并行检查 + 缓存缺失直接跳过
**并行检查**：  
- 将 `for market in &market_ids` 拆为 per-market worker  
- 每个市场独立 queue，收到 book_update 时只处理对应市场  
- 这样不会因为某个市场阻塞而拖累其他市场  

**缓存缺失直接跳过**：  
- 用 `get_user()` / `get_stats()`（同步）替代 `get_user_or_fetch().await`  
- 缓存 miss 时 `warn` 并跳过本次机会  
- 可选：后台异步预热缓存，不在热路径阻塞  

### 6.6 还有哪些补充优化/规划？
1. **事件去抖与批处理**：把 book 更新合并为按市场的 “最新状态”，避免重复处理  
2. **maker/stat 缓存预热**：启动后主动拉取活跃 maker 的 user/stats  
3. **JIT/ARB 分离队列**：避免 JIT 负载拖慢 ARB  
4. **DLOB 更新监控**：记录 DLOB 更新延迟，异常时暂停策略  
5. **交易构造缓存**：对固定 accounts 元数据/remaining_accounts 做缓存复用  
6. **逐市场优先级**：为高流动性市场设置更高处理优先级  
