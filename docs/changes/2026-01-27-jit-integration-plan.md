# JIT 策略整合到 keep-rs filler 的方案（参考 C:\Users\Administrator\Desktop\fsdownload\测试1）

日期：2026-01-27

> 目标：把“外部价格驱动 + 订单簿 Maker 选择 + JIT 指令发送”的完整链路搬进 keep 的 filler 机器人。
> 本文结合测试项目的实现细节与 keep/drift-rs 当前结构，给出对齐点、缺口与落地步骤。


强调：本方案 **必须并入现有 filler 机器人**，共享同一进程/订阅/缓存，不另起独立机器人。
实现方式是在 filler 内新增 JIT 任务与数据流，复用现有 WS、DLOB、缓存与 TxWorker。

---

## 1. 参考项目（测试1）里的 JIT 关键组件与职责

### 1.1 调度入口（`main1.py`）
- 负责启动并连接：
  - Drift WS 订阅（用户/预言机/slot）。
  - L2 聚合器（`L2BOOKNEW2.py`）。
  - Binance 行情订阅（`binance_sol_tick8.py`）。
  - JIT 策略编排（`binance_drift_jit_strategy6.py`）。
- 核心逻辑：
  - WS 用户更新 → 更新 L2Book → 产生可交易意图（ArbIntent）。
  - Binance 价格事件 → 触发 JIT 策略回调 `on_opportunity`。
  - 回调中调用 `ArbPerpEngine.jit_with_makers` 发单。

### 1.2 策略编排（`binance_drift_jit_strategy6.py`）
- 事件驱动、0ms 轮询。仅用 **cooldown** 限制触发频率。
- 使用 `state` 快照避免并发下读到被覆盖的共享状态。
- 价格输入：
  - `reference_price` 来自 Binance fair_px。
  - `edge_ppm` 优先用 state.edge_ppm，兜底阈值或 config。
- Maker 选择：
  - `L2Book.makers_at_price` 获取 best bid/ask 价位上若干 maker（上限 max_makers_per_side）。

### 1.3 L2 聚合器（`L2BOOKNEW2.py`）
- 维护：聚合订单簿、用户原始订单、oracle/slot 跟踪。
- 具备 Reduce-Only 过滤（只计入“能减少持仓”的 RO 订单）。
- `top2_cache` 用于快速获取 best bid/ask 价。
- `makers_at_price` 用于给 JIT 选 Maker 列表。

### 1.4 交易发送层（`send2.py`）
- 同时支持 **arb_perp** 和 **jit** 指令。
- JIT 走 **子账户 1**，arb 走子账户 0。
- 构造 remaining_accounts：
  - 先用 driftpy `get_remaining_accounts` 构公共账户。
  - 再追加 maker user / user_stats（并可选追加 referrer）。
- JIT 指令参数：
  - `market_index`、`reference_price`、`edge_ppm`（见 `jit_proxy_idl3.json`）。

### 1.5 JIT Proxy IDL（`jit_proxy_idl3.json`）
- 指令 `jit(params)`
- 账户：`state, user, user_stats, taker, taker_stats, authority, drift_program`
- `JitParams`：
  - `market_index: u16`
  - `reference_price: i64`
  - `edge_ppm: i64`

---

## 2. 与 keep-rs 当前 filler 的对照

### 2.1 keep 已有能力
- DLOB / L3 snapshot（可用于 maker 选择）。
- WS user + stats 缓存（`WsAccountCache`）。
- Pyth 价格订阅（可作为“链上 oracle”，但不是外部 fair price）。
- 交易发送框架（`TxWorker` + `TransactionBuilder`）。

### 2.2 keep 缺口
1) **外部价格源**（Binance fair price）
   - 目前仅有 Pyth 与链上 oracle；JIT 策略需要“外部参考价”。
2) **JIT 指令构造**
   - keep 现阶段只会构建 Drift 原生指令（例如 `proxy_spread_capture`/`arb_perp` 相关路径），
     JIT 需要调用 **jit-proxy** 的 `jit` instruction。
3) **Maker 选择**
   - keep 的 DLOB 能提供 L3，但缺少“best 价位上的 maker 列表”结构化接口。
4) **配置层**
   - 需要可配置市场、edge_ppm、cooldown、max_makers、subaccount 等参数。
5) **调度模型**
   - JIT 更偏“事件驱动 + 快速限流”，而 filler 是 slot 驱动 + swift stream。
---

## 3. 建议的整合架构（keep 侧）

### 3.0 运行形态（单机器人，多策略）
- 在 `FillerBot` 内部启动 JIT 子任务，与 Swift/onchain 交叉并行运行。
- 共享：WS 订阅、DLOB、WsAccountCache、TxWorker、Metrics。
- 子账户策略映射：
  - **子账户 0**：套利/arb 路径（`arb_perp`）。
  - **子账户 1**：JIT 路径（`jit`）。
- 不启动第二个机器人进程，避免重复订阅与状态分叉。

### 3.1 新增模块结构（keep-rs）
```
keep-rs/
  src/
    jit/
      mod.rs
      feed_binance.rs      # 外部价格源（可替换）
      maker_select.rs      # 基于 DLOB 的 maker 选择
      jit_strategy.rs      # JIT 逻辑（cooldown + 触发 + params）
      jit_trades.rs        # 构建并发送 jit 指令
```

### 3.2 外部价格源
- 定义 trait：
  - `trait ExternalPriceFeed { fn latest_fair_px(market) -> Option<i64>; }`
- 实现：
  - 版本 A：Binance WS（与测试项目一致）。
  - 版本 B：HTTP 报价 / 聚合服务（作为 fallback）。

### 3.3 Maker 选择（基于 DLOB）
- 从 DLOB L3 snapshot 中提取 best bid/ask 价位。
- 在该价位选最多 `max_makers_per_side` 个 maker（同价位）；可直接用 L3Order 的 user 列表。
- Reduce-Only 过滤：
  - 复用 keep 里已有的 RO 判断逻辑（与 `filter_crosses_reduce_only` 一致）。
- 可选：过滤拍卖订单或 Trigger 未触发订单（与链上逻辑对齐）。

### 3.4 JIT 触发逻辑（策略）
- 输入：
  - `reference_price`（外部价格）
  - `edge_ppm`（配置 or 来自行情模块状态）
  - best bid/ask（DLOB）
- 触发条件示例：
  - `abs(best_mid - reference_price) / reference_price >= edge_ppm`
  - 或 `best_bid > reference_price*(1+edge)` / `best_ask < reference_price*(1-edge)`
- 触发后：
  - 选择一侧：BUY_DRIFT / SELL_DRIFT
  - 构建 makers 列表 → 调用 jit 交易发送

### 3.5 JIT 指令发送（jit_trades.rs）
- 在 keep 侧引入 **jit-proxy 程序 id**（可配置）。
- 构造 accounts：
  - `state, user, user_stats, taker, taker_stats, authority, drift_program`
  - `taker` 与 `taker_stats` 作为占位（可与 user 同地址保持兼容）。
- Remaining accounts：
  - 使用 drift-rs/keep 内部 “get_remaining_accounts” + 附加 maker user/stats。
  - 若需要 referrer，按 proxy 逻辑追加。
- 子账户：建议独立子账户 1 用于 JIT（与 arb/filler 解耦）。


### 3.6 与 filler 主循环的融合点（单机器人）
- 在 `FillerBot::run` 内部新增 JIT 子任务：
  - 独立的价格流（Binance/HTTP）更新共享的 `jit_price_cache`。
  - 独立的策略调度（cooldown + side 选择）从 DLOB 取 best bid/ask + makers。
  - 通过 `TxWorker` 发送 JIT 交易（与 Swift/onchain 共享发送管道）。
- 复用已有的 `WsAccountCache` 与 `DLOB`，不新增重复订阅。
- 子账户切换只发生在构造 `TransactionBuilder` 时：
  - arb 路径：`sub_account_id = 0`
  - jit 路径：`sub_account_id = 1`
- 建议在 metrics 中增加 `jit_*` 指标，避免与 arb/swift 混淆。

### 3.7 数据流与共享缓存细化（参考测试项目）
- 复用 keep 的 WS 订阅与 DLOB：
  - 用户/订单更新 → DLOB/L3 更新（保持与现有 filler 一致）。
  - slot/oracle 更新 → DLOB 触发价刷新（可复用现有 notifier）。
- JIT 只新增“外部价格流”，写入 `jit_price_cache`（market -> reference_price + ts）。
- JIT 策略从 DLOB 取 best bid/ask + makers，并与 `jit_price_cache` 做比对。
- 不额外维护 L2Book，避免与 DLOB 口径不一致。

### 3.8 Maker 选择实现细节（DLOB 侧）
- 目标：等价于测试项目的 `makers_at_price` 行为（同价位 top-N）。
- 选价位：
  - best_bid = L3 bids 头部价
  - best_ask = L3 asks 头部价
- 选 maker：
  - 在 best_bid/best_ask 价位上收集前 N 个 maker（按 size/先到先得）。
  - 过滤 reduce-only 无效订单（复用 keep 侧 `is_invalid_reduce_only` 逻辑）。
  - 过滤未触发的触发单 / 过期单（与订单簿扫描规则一致）。
- 结果：`makers_bid: Vec<L3OrderUser>` / `makers_ask: Vec<L3OrderUser>`
  - 后续在 `jit_trades` 中按 maker user/stats 组装 remaining_accounts。
---

## 4. drift-rs SDK 需要补齐的点

### 4.1 新增 JIT Proxy 指令构造
### 4.1.1 参考 SDK 模板：`proxy_spread_capture`
> 现有模板：`TransactionBuilder::proxy_spread_capture` + `build_accounts_proxy` + `build_remaining_accounts_for_proxy`。
> 建议 JIT 按同样风格新增，避免 remaining_accounts 顺序偏差。

**模板要点（来自 drift-rs/crates/src/lib.rs）**
- **主账户**：`state, user, user_stats, authority, drift_program`
- **remaining_accounts**：由 `build_remaining_accounts_for_proxy(...)` 统一拼装（市场、oracle、maker user/stats、referrer 等）。
- **指令数据**：`PROXY_ARB_PERP_DISCRIMINATOR + market_index`（固定 8 bytes + args）。

**JIT 对应的设计细化**
1) **新增账户构造器**
   - 新增 `JitAccounts`：包含 `state, user, user_stats, taker, taker_stats, authority, drift_program`。
   - 新增 `build_accounts_proxy_jit(...)`：顺序必须与 jit-proxy IDL 匹配。

2) **新增 remaining_accounts 构造**
   - 继续复用 `build_remaining_accounts_for_proxy(...)`：
     - `taker_account` = 子账户 1 的 User（JIT 专用）。
     - `taker_stats` = authority 级 UserStats（同 arb）。
     - `makers` = 由 DLOB 选出的 maker 列表（同价位 top-N）。
   - 注意：**maker user/stats 顺序与去重规则**必须保持一致（避免 InvalidUserStats）。

3) **新增指令 discriminator + 数据**
   - 定义 `PROXY_JIT_DISCRIMINATOR`（8 字节）。
   - 指令数据：`discriminator + borsh(JitParams)`
     - `JitParams { market_index: u16, reference_price: i64, edge_ppm: i64 }`

4) **新增 TransactionBuilder 方法**
   - `TransactionBuilder::proxy_jit(...)`（类似 `proxy_spread_capture`）。
   - 参数建议：
     - `market_index, reference_price, edge_ppm`
     - `taker_stats`（JIT 子账户对应的 stats）
     - `makers: &[User]`
     - `proxy_program_id: Option<Pubkey>`（可配置）

5) **子账户绑定**
   - arb 路径继续用 `sub_account_id = 0` 调 `proxy_spread_capture`。
   - JIT 路径用 `sub_account_id = 1` 调 `proxy_jit`。
   - 共享 `ProgramData/DriftClient`，仅切换 builder 的 sub_account。

### 4.2 Remaining Accounts Builder
- 复用现有：`get_remaining_accounts`（perp/spot/oracle）。
- 附加：maker user + maker stats（同 send2.py）。
- 若需要 referrer：额外 append referrer + referrer_stats。


- 内部用 `BTreeSet<RemainingAccount>` 去重，并保证“markets/oracles”顺序稳定。
- maker user/stats 在 markets/oracles 之后追加（user → stats 成对追加）。
- referrer/referrer_stats 在 maker 之后追加（若 taker_stats 被推荐）。
- revenue_share_escrow 仅在 Swift 填单场景使用；JIT 通常不需要追加。

### 4.3 keep 侧调用模板的伪流程（JIT）
1) `let jit_user = wallet.sub_account(jit_sub_account_id)`
2) `let builder = TransactionBuilder::new(program_data, jit_user, user_data, false)`
3) 计算 `reference_price` / `edge_ppm` / makers 列表
4) `builder.proxy_jit(market_index, reference_price, edge_ppm, taker_stats, makers, Some(jit_proxy_program_id))`
5) `TxWorker::send_tx` 统一发送（与 Swift/onchain 共用队列）
6) 若需要 `update_amm`，在 `builder` 前置加入相同指令（与其他策略保持一致）

### 4.4 SDK 落点清单（文件级）
- `drift-rs/crates/src/lib.rs`：
  - 新增 `JitAccounts` 结构体与 `build_accounts_proxy_jit(...)`。
  - 新增 `PROXY_JIT_DISCRIMINATOR` 常量。
  - 新增 `TransactionBuilder::proxy_jit(...)` 方法。
- `drift-rs/crates/src/drift_idl.rs`：
  - 若需要，加入 jit-proxy 的 discriminator 或手动序列化 JitParams。
- `drift-rs/crates/src/utils.rs`：
  - 可复用现有 borsh/IDL 工具，避免手写序列化错误。
---

## 5. keep 配置建议

在 `Config` 中新增：
- `jit_enabled: bool`
- `arb_sub_account_id: u8`（默认 0，用于 `arb_perp`）
- `jit_sub_account_id: u8`（默认 1，用于 `jit`）
- `jit_edge_ppm: i64`
- `jit_max_makers_per_side: usize`
- `jit_cooldown_ms: u64`
- `jit_markets: Vec<u16>`
- `jit_proxy_program_id: Pubkey`
- `jit_external_feed: enum { Binance, Http, None }`

---

## 6. 实现步骤（建议分期）

### 6.1 任务拆分（更细粒度）
1) SDK：新增 JIT 账户构造 + discriminator + proxy_jit 方法。
2) keep：新增 `jit_trades.rs`（仅发送，不含策略）。
3) keep：新增 `jit_price_cache` 与外部行情订阅（Binance/HTTP）。
4) keep：新增 `maker_select.rs`（DLOB 选价位 + maker 列表）。
5) keep：新增 `jit_strategy.rs`（cooldown + side 决策）。
6) 配置与指标：增加 jit_* 配置与 metrics 埋点。

### 阶段 A：基础接入
1) 在 drift-rs 加入 jit-proxy 指令构造方法（仅构造、无策略）。
2) keep 新增 `jit_trades.rs`，支持发送 `jit` 指令。
3) keep 新增配置项 + 子账户初始化。

### 阶段 B：maker 选择与策略
1) 在 keep 中实现 `maker_select.rs`：
   - 使用 DLOB 选 best price + makers。
   - Reduce-only 过滤。
2) 实现 `jit_strategy.rs`：
   - cooldown + side 选择 + edge 判定。

### 阶段 C：外部价格源
1) Binance WS 订阅实现（或先用 HTTP 轮询版）。
2) 将外部价格接入 JIT 策略。

### 阶段 D：稳定性与监控
1) 日志：输出参考价、best_bid/ask、makers 数量、edge 判定结果。
2) 失败分类：区分价格不足/RA 不完整/指令失败。
3) 指标：成功率、延迟、maker 命中率。

---

## 7. 风险与注意点

1) **remaining_accounts 顺序**：
   - 与 jit-proxy 程序要求一致，否则会触发 InvalidUserStats 等错误。
2) **maker 列表缺失**：
   - 必须允许“仅 vAMM”的 JIT（makers 为空的场景）。
3) **外部价格过期**：
   - 必须设置 staleness 限制，否则会在失真价格上 JIT。
4) **RO 订单过滤**：
   - 如果过滤过严会损失机会；过松则会误报套利。
5) **子账户隔离**：
   - 建议 JIT 使用专用子账户，避免与 arb/filler 竞争保证金。

---

## 8. 验证与回归测试

1) **单元测试**
   - maker_select：在给定 L3 snapshot 下返回正确的 best price & makers。
   - RO 过滤逻辑与 keep 现有逻辑一致。

2) **集成测试**
   - 仅在 devnet/本地环境：模拟 external price → 触发 jit → 成功发指令。
   - 验证 remaining_accounts 是否包含 maker user / stats 顺序正确。

3) **日志核查**
   - 每次触发 JIT 输出：ref_px、edge_ppm、best_bid/ask、maker_count。

---

## 9. 建议的落地路径

1) 先把 SDK 构造逻辑补齐（最小化侵入）。
2) 再在 keep 添加 jit_trades（只发指令，不含策略）。
3) 最后加策略与外部行情源。

---

### 附：参考项目可复用的关键设计思想
- **maker_resolver**：通过缓存直接取 maker 账号，避免 RPC。
- **cooldown**：极短锁只保护 last_fire_ts，避免阻塞业务。
- **snapshot**：在并发任务中冻结参数，避免竞态。
- **subaccount 分离**：arb 与 jit 使用不同子账户，降低互相影响。

---

如需我继续落地：可以指定优先实现哪个阶段，我再按阶段输出更细的实现改动清单。

## 10. 后续目标计划（待实现）

### 10.1 RPC 预检可配置
- 目标：把 `skip_preflight` 从“写死 true”改为“可配置开关”。
- 默认策略：生产环境先保守启用 preflight，减少无效上链交易与手续费浪费。
- 落点：`TxWorker::send_tx` 的 `RpcSendTransactionConfig`（可新增 `Config.skip_preflight` 或 `Config.enable_preflight`）。

### 10.2 Jito + RPC 双通道发送（带 UUID 轮询 + 小费）
- 目标：像测试程序 `send2.py` 一样，同时发送 Jito bundle 与普通 RPC。
- 关键点：
  - 维护 Jito UUID 列表，轮询使用（round-robin）。
  - 发送 Jito 的交易附加 tip 指令（小费），RPC 通道不加 tip。
  - 同一笔交易同时投递到 Jito 与 RPC，允许任一路成功。
  - 可选：提供 `simulateBundle` 预模拟开关。
- 参考实现：`C:\Users\Administrator\Desktop\fsdownload\测试1\send2.py` 中的 `_pick_jito_uuid`、`_send_bundle_base64`、`_send_business_ix` 逻辑。

---

## 17. 关键改动目标的最优方案（深度分析）

### 17.1 外部价格源接入（目标：贴近测试程序的 fair_px 驱动）
**目标结果（测试程序）**
- fair_px 来自 Binance WS；触发是事件驱动、低延迟、低抖动。
- 支持 staleness 限制（超过阈值不触发）。
- 与订单簿独立，避免彼此阻塞。

**已有基础（filler + sdk）**
- filler 已有 Pyth 订阅（链上 oracle），但不是外部 fair price。
- drift-rs 未提供 Binance feed，需在 keep 侧新增。

**最优方案**
- 在 keep 内新增 **独立 price feed 任务**（只负责外部价格，不接触订单）。
- 使用无锁/轻锁缓存（`jit_price_cache: HashMap<u16, (i64, ts_ms)>`），由 feed 任务写、策略任务读。
- staleness 规则：`now_ms - ts_ms <= max_stale_ms`，不满足则不触发。
- 不要求替代 Pyth：Pyth 保留供链上价格/触发判断使用。
- 这样“外部价格 + DLOB”解耦，行为与测试程序一致，且不影响现有 filler。

**为什么是最优**
- 低耦合、低延迟；不会扩大 DLOB/WS 的时间预算。
- staleness 可配置，避免因外部延迟而误触发。
- 与现有 Pyth 共存，风险可控。

---

### 17.2 Maker 选择逻辑（目标：等价 L2BOOK 的 makers_at_price）
**目标结果（测试程序）**
- best bid/ask 来自 L2Book；
- makers_at_price 返回同价位上 top-N makers（用于 remaining_accounts）。

**已有基础**
- keep 有 DLOB/L3 snapshot，能拿到 L3Order（包含 user、price、size）。
- 已有 reduce-only 过滤逻辑可复用。

**最优方案**
- 用 DLOB L3 代替 L2Book：
  - best_bid/best_ask = L3 book 头部价
  - makers_at_price = 在该价位收集 top-N L3Order
- 过滤顺序：
  1) 非 open / 触发未满足 / 过期 → 丢弃
  2) reduce-only 无效 → 丢弃
  3) 同 user 去重（避免重复 stats）
- makers 列表直接用于 SDK remaining_accounts 构造。
- 这样可以保持与测试程序“同价位 top-N maker”的效果，同时复用 keep 内部 DLOB。

**为什么是最优**
- DLOB 与 filler 本身的撮合逻辑一致，不引入 L2Book 口径偏差。
- 减少外部依赖，减少内存与复杂度。

---

### 17.3 JIT 指令构造（目标：与 send2.py 行为一致）
**目标结果（测试程序）**
- `jit(params)` 调用 jit-proxy
- remaining_accounts 包含 markets/oracles + makers + 可选 referrer
- taker = 子账户 1

**已有基础**
- SDK 已有 `proxy_spread_capture` 模板（arb_perp）
- `build_remaining_accounts_for_proxy` 已经对 markets/oracles 与 maker stats 做了稳定排序

**最优方案**
- 完全复用 `proxy_spread_capture` 的“账户构造 & remaining_accounts 逻辑”。
- 新增 `proxy_jit`：
  - `JitAccounts` 顺序严格对齐 IDL
  - `PROXY_JIT_DISCRIMINATOR + borsh(JitParams)`
  - remaining_accounts 调用 `build_remaining_accounts_for_proxy`
- 这样确保 RA 顺序一致，避免 InvalidUserStats 类错误。

**为什么是最优**
- 最低风险：沿用 SDK 已验证的账户构造方式。
- 最少重复代码：保持扩展简洁、易维护。

---

### 17.4 子账户分离（目标：arb 与 jit 互不干扰）
**目标结果（测试程序）**
- arb 使用子账户 0
- jit 使用子账户 1

**已有基础**
- keep already 支持 `wallet.sub_account(id)`
- TransactionBuilder 在创建时绑定 sub_account

**最优方案**
- 在 Config 中新增 `arb_sub_account_id` 与 `jit_sub_account_id`
- arb 路径保持 0，jit 固定 1
- 在构建 JIT tx 时仅切换 builder.sub_account
- 不改变 WS 订阅 / DLOB，避免重复数据流

**为什么是最优**
- 逻辑清晰且与测试程序一致
- 避免资金/保证金冲突

---

### 17.5 Jito + RPC 双通道（目标：提高成交率）
**目标结果（测试程序）**
- Jito + RPC 并发发送
- Jito UUID 轮询
- Jito 路径追加 tip

**已有基础**
- keep 有 TxWorker 统一发送通道
- 尚未有 Jito bundle 管理

**最优方案**
- 在 TxWorker 增加“发送策略”分支：
  - RPC 通道保留
  - Jito 通道作为并行发送
- 同一 tx build 两个版本：
  - RPC：原始交易
  - Jito：追加 tip ix
- Jito UUID 轮询保存在进程级缓存
- 任一路成功即可接受（日志区分）

**为什么是最优**
- 最大化成交率且与测试程序一致
- 并行发送降低单一路径失败风险

---

### 17.6 Preflight 可配置（目标：减少浪费手续费）
**目标结果**
- RPC 先预检，失败不广播
- 仍是一次 RPC 请求

**已有基础**
- `TxWorker::send_tx` 中写死 `skip_preflight = true`

**最优方案**
- 配置化：`Config.enable_preflight`
- 当启用：`skip_preflight = false`
- 与 Jito 并行时可单独配置 RPC 预检（Jito 路径不模拟）

**为什么是最优**
- 降低失败交易成本
- 可按环境切换（高频/低频）

---

## 18. 分阶段“验收标准”建议

### 18.1 SDK 层
- 能构造 `proxy_jit` 且 remaining_accounts 顺序与 arb_perp 模板一致
- mock 环境中成功构造指令（不一定上链）

### 18.2 keep 交易层
- JIT 交易可发送（dry-run/实际）
- maker 列表为空时仍可构造 tx（允许 vAMM-only）

### 18.3 策略层
- 外部价格触发成功
- cooldown 生效
- 日志中可看到正确 side / edge / maker_count

---

## 11. 进一步细化（按模块/接口拆分）

### 11.1 keep-rs 新模块接口草案
- `jit/feed_binance.rs`
  - `struct BinanceFeed { .. }`
  - `fn spawn(self, markets: &[u16]) -> Receiver<ExternalPriceUpdate>`
  - `ExternalPriceUpdate { market_index: u16, reference_price: i64, ts_ms: u64 }`
- `jit/maker_select.rs`
  - `fn best_levels(dlob: &DLOB, market: u16, oracle: u64, trigger: u64) -> (Option<u64>, Option<u64>)`
  - `fn makers_at_price(dlob: &DLOB, market: u16, side: Side, price: u64, max: usize) -> Vec<L3Order>`
  - `fn filter_reduce_only(orders: Vec<L3Order>, user_cache: &WsAccountCache, market: u16) -> Vec<L3Order>`
- `jit/jit_strategy.rs`
  - `struct JitStrategy { cooldown_ms, edge_ppm, max_makers, ... }`
  - `fn on_price_update(&mut self, update: ExternalPriceUpdate, dlob: &DLOB, ...) -> Option<JitIntent>`
  - `JitIntent { market_index, reference_price, edge_ppm, side, makers_bid, makers_ask }`
- `jit/jit_trades.rs`
  - `async fn try_jit(drift: &DriftClient, intent: JitIntent, ...)`

### 11.2 drift-rs SDK 侧新增函数签名（建议）
- `pub fn proxy_jit(mut self, params: JitParams, taker_stats: &UserStats, makers: &[User], proxy_program_id: Option<Pubkey>) -> Self`
- `pub fn build_accounts_proxy_jit(accounts: JitAccounts) -> Vec<AccountMeta>`
- `pub struct JitAccounts { state, user, user_stats, taker, taker_stats, authority, drift_program }`

---

## 12. 价格/触发逻辑细化（与测试项目对齐）

### 12.1 参考价与阈值
- `reference_price`：外部行情（Binance fair_px）按 Drift 价格精度转成 `i64`。
- `edge_ppm`：优先取外部 feed/策略状态，其次取配置默认值。
- `staleness`：外部价格超过阈值（如 500ms/1s）则不触发。

### 12.2 触发条件示例（建议保守）
- `best_bid > reference_price * (1 + edge)` → `SELL_DRIFT`
- `best_ask < reference_price * (1 - edge)` → `BUY_DRIFT`
- 避免同时触发，按 `prefer_side_when_both` 选择一侧（与测试项目一致）。

---

## 13. remaining_accounts 顺序细节（避免 InvalidUserStats）
- **主账户顺序**必须完全匹配 jit-proxy IDL。
- **remaining_accounts** 顺序：markets/oracles → maker user/stats → referrer → revenue_share_escrow（仅 Swift）。
- maker user/stats 必须成对追加（user 在前，stats 在后），并去重。
- maker 中禁止混入 taker 自己的 user/stats（避免自撮与校验失败）。

---

## 14. 并发与限流
- JIT 建议单市场独立 cooldown（与测试项目一致），但仍建议保留轻量 inflight 限制（如 1~2）。
- Swift/onchain 与 JIT 共用 `TxWorker` 时应增加 intent 级别限流，避免排队拖累。
- JIT 交易优先级可用更低 CU limit / fee，减少对套利路径的干扰。

---

## 15. 日志与错误分类（建议补齐）
- 新增 `JitError` 枚举：`StalePrice`, `NoMakers`, `NoCross`, `MissingAccounts`, `SendFailed`。
- 日志关键字段：`market`, `ref_px`, `edge_ppm`, `best_bid/ask`, `makers_cnt`, `side`, `slot`.
- 统计：`jit_triggered`, `jit_sent`, `jit_failed_{reason}`。

---

## 16. 从测试项目迁移的对照清单
- `send2.py` → `jit_trades.rs`（交易构造 + Jito/RPC 双通道）。
- `binance_drift_jit_strategy6.py` → `jit_strategy.rs`（cooldown/side 选择）。
- `L2BOOKNEW2.py` → `maker_select.rs`（best price + makers_at_price）。
- `main1.py` → `FillerBot::run` 内的 JIT 子任务启动逻辑。





