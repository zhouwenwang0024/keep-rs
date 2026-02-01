## 对比报告：Filler 机器人 vs 测试程序（D:\RUST\test\测试1）

本文对比 `keep-rs` 的 Filler 机器人与测试程序（Python）在定价准确性、触发时效、吞吐量与系统架构上的差异，并解释“测试程序更准、更快、成交更多”的可能原因。结论不是“Rust 比 Python 慢”，而是**两套系统的触发门控、数据源、并发模型与发送链路完全不同**。

---

### 一、总体流程差异（架构与触发路径）

**Filler（Rust）**
- 单一事件循环内处理：Book 更新 → 遍历所有市场 → 计算 cross & JIT。
- JIT 价格来自 Binance feed（spot+fut）并要求**强一致时间窗口**。
- 触发门控更严格：warmup / staleness / basis 对齐 / 冷却 / DLOB stale / per-user slot 限频。
- BlockhashSubscriber 后台 2s 刷新缓存，`get_latest_blockhash()` 优先从缓存读取（同步，无 RPC），缓存为空时才 fallback 到 RPC。

**测试程序（Python）**
- 事件驱动 + per-market worker：每市场一个队列与 worker。
- JIT 由 Binance feed “on_opportunity” 回调触发，**无需轮询**。
- 门控更轻：默认 50ms cooldown；允许无限 in-flight（JIT）。
- BlockhashManager 1s 刷新，避免每笔交易做 RPC。

**影响**：Python 更像“低延迟交易引擎”，Rust 更像“保守、安全优先”的执行器。

---

### 二、价格与机会判断的关键差异

**1) Binance 时序约束**
- Rust：spot + futures 必须都在 **100ms 内且互相 skew ≤ 100ms** 才更新。否则不会触发参考价。
- Python：仅要求 spot/fut “fresh（默认 1s）”，没有强制 skew；机会触发更频繁。

**结果**：Rust 会丢弃更多 tick，导致机会减少；Python 更“激进”。

**2) 基差 EMA 的输入源**
- Rust：EMA 使用 Drift DLOB L3 最佳 bid/ask 计算 drift_mid。
- Python：EMA 使用 **官方 DLOB WS 的 L2 mid**（`official_l2_mid`），而本地 L2 仅用于机会判断。

**结果**：Python 的 EMA 更稳定（来源一致、更新更“干净”），也更容易被认为“更准”。

**3) 参考价与阈值策略**
- Rust：`reference_price = binance_mid - 0.8 * basis_ema`，`edge_ppm` 固定配置。
- Python：阈值来自波动率 EMA + external threshold；`edge_ppm` 同 tick 写入，动态变化。

**结果**：Python 的“边界”会随波动自适应，触发更灵活。

---

### 三、性能与吞吐：为什么 Python 看起来更快

**1) blockhash 获取方式**
- Rust：`BlockhashSubscriber` 后台 2s 刷新缓存，`get_latest_blockhash()` 优先从缓存读取（同步，无 RPC），缓存为空时才 fallback 到 RPC。
- Python：BlockhashManager 后台 1s 刷新，发送时直接使用缓存。

**影响**：两者都从缓存读取（无 RPC），但 Python 刷新更频繁（1s vs 2s），blockhash 可能更新鲜。

**2) 并发模型**
- Rust：
  - **市场串行**：`for market in &market_ids` 循环，市场1处理完才处理市场2
  - **下单函数串行**：`try_jit(...).await` 和 `try_onchain_cross(...).await` 会等待函数完成
  - **缓存缺失会阻塞**：如果 `user_cache.get_user_or_fetch()` 缓存缺失，会 `await` RPC 请求，阻塞当前市场处理
  - **交易发送不阻塞**：`tx_worker_ref.send_tx_with_jito()` 只是发送到 channel，`TxWorker` 后台异步发送
- Python：每市场独立 worker，无需跨市场串行；JIT 直接 `create_task`，不等待返回。

**影响**：Rust 在缓存缺失时会串行等待 RPC，Python 能并行处理多个市场且不等待交易往返。

**3) 数据装载与缓存**
- Rust：JIT/ARB 下单会从 `WsAccountCache` 获取 user/stats，缺失时会 `await` 拉取。
- Python：maker/user/referrer 缓存优先，热路径几乎不 await。

**影响**：Rust 热路径更容易被缺失缓存拖慢。

**4) 触发门控**
- Rust：warmup（默认 5min）、staleness（binance 100ms）、cooldown、DLOB stale、per-user slot 限频。
- Python：cooldown 50ms；JIT 不限 in-flight；stale_sec 默认 1s。

**影响**：Rust 会“主动抑制”很多机会，Python 更容易“成交更多”。

---

### 四、可能的“架构缺陷/逻辑 bug”点（高优先级排查）

以下不是确定 bug，但会显著降低速度与触发频率：

1) **Binance feed 过于严格**
   - 100ms skew 与 stale 条件非常苛刻，可能把多数行情丢掉。
   - Python 版本只要求 freshness，不要求 spot/fut 强同步。

2) **blockhash 刷新频率差异**
   - Rust 的 BlockhashSubscriber 2s 刷新，Python 的 BlockhashManager 1s 刷新；
   - 两者都从缓存读取（无 RPC），但 Python 的 blockhash 可能更新鲜。

3) **单循环串行扫描所有市场**
   - 每次 DLOB 更新就遍历 `market_ids`，高市场数量下可能成为瓶颈。
   - Python 通过 per-market worker 将计算分散并行。

4) **JIT 严格门控导致“看起来不成交”**
   - warmup + basis_align + DLOB stale + cooldown + rate limit 叠加非常保守。
   - Python 更激进，短期成交更高。

---

### 五、优缺点总结（策略与工程取舍）

**Filler（Rust）优点**
- 强一致/稳定性优先（stale、warmup、DLOB 保护）。
- 风险控制更强（DLOB stale & per-user rate limit）。
- 交易构造更统一，适合长期运行与稳定控制。

**Filler（Rust）缺点**
- 触发门控过多，机会变少。
- blockhash 刷新频率较低（2s vs 1s），可能略慢。
- 单循环串行扫市场，在高市场数下不够快。

**测试程序（Python）优点**
- 强事件驱动 + per-market worker，低延迟。
- blockhash 缓存，发单更快。
- EMA & 阈值更动态，机会更易触发。

**测试程序（Python）缺点**
- 更激进，错误/噪声机会也容易触发。
- 依赖更多“软约束”，缺乏统一的风控/限频机制。
- 并发无限制，可能放大失败与链上费损。

---

### 六、为什么“Rust 不如 Python 快”的解释

不是语言问题，而是**架构与策略选择问题**：
- Rust 当前设计是“安全稳健型”；
- Python 设计是“抢机会型”；
二者对速度的边界不同，表现就完全不同。

当你看到“Python 速度更快、成交更多”，更可能是：
**Python 放宽门控 + 并发更激进 + blockhash 缓存导致更快的发送节奏**。

---

### 七、建议的下一步（若你希望 Rust 接近 Python）

可以逐项对齐测试程序的关键能力（不改策略逻辑也能提速）：
- 将 **BlockhashSubscriber 刷新频率从 2s 改为 1s**（与 Python 对齐）。
- 允许 **per-market worker** 或至少拆分 market loop。
- 放宽 Binance skew/stale 条件（或改为 500ms / 1s）。
- 区分 JIT 与 ARB 的门控策略（JIT 更激进、ARB 更稳健）。

---

如需我进一步对比某个具体模块（如 L2 聚合、maker 选择、LUT/remaining_accounts 构造等），告诉我具体文件/函数名，我可以继续拆解。
