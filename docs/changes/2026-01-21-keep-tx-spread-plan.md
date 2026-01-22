# Keep 交易构造/发送阶段改造思路（吃价差）

本文件只讨论 **keep 的交易构造与发送阶段**，假设“发现机会”的阶段已经可用。  
目标：把现有“撮合 taker/maker”改为“自有账户吃掉中间价差”。  
注意：代理程序也需要后续改造（简化版），但本文件仅聚焦 keep 的交易构造与发送。

## 结论先行

- **是的**：如果 keep 已能稳定发现交叉机会，那么主要修改点集中在“交易构造与发送”阶段。
- 交易路径要从 `fill_perp_order / place_swift_order` 这类“撮合”逻辑，改为“自有账户 taker 吃单”逻辑。

## 现状简述（与改动相关）

在 `keep-rs/src/filler.rs` 里，交易构造路径主要集中在：

- `try_auction_fill(...)`  
  使用 `fill_perp_order` 撮合 taker / makers。
- `try_uncross(...)`  
  使用 `fill_perp_order` 解交叉。
- `try_swift_fill(...)`  
  使用 `place_swift_order + fill_perp_order`。

**以上都属于“撮合模式”**，不是“自有账户吃价差”。

## 目标交易形态（吃价差）

核心行为：  
你自己的子账户作为 taker，同时吃到 bid/ask 价差的一侧或两侧，赚取 spread。

具体可以分成两种执行模式（后续与代理简化版对齐）：

1. **单侧吃单**  
   - 只吃 best bid 或 best ask 的一侧。  
   - 优点：简单、账户少、CU 低。  
   - 缺点：暴露方向性风险（未对冲）。
2. **双侧吃单（推荐）**  
   - 在同一交易内吃 bid + ask，锁定价差。  
   - 优点：方向风险小。  
   - 缺点：账户更多，交易更重，对代理程序要求更高。

## Keep 侧需要改的代码位置（思路级）

### 1) 改造交易构造入口

把下列“撮合函数”改为“吃价差函数”：

- `try_auction_fill(...)`  
- `try_uncross(...)`  
- `try_swift_fill(...)`

思路：  
- **保留机会发现逻辑**，但进入交易构造阶段时改走“吃价差交易构造函数”。  
- 例如新增 `try_spread_capture(...)`，替代当前的 `try_uncross(...)` 或 `try_auction_fill(...)`。

### 2) 改造交易指令构造

当前构造逻辑主要依赖：
- `TransactionBuilder::fill_perp_order(...)`
- `TransactionBuilder::place_swift_order(...)`

这两者是“撮合/补单”语义，不适合“自有账户吃单”。  

替换方向（思路）：

- **改为调用代理程序指令**（简化版代理仍需改造）  
  keep 侧只负责：
  - 选择 makers（best bid/ask 对应价位）；  
  - 构造 remaining_accounts；  
  - 发起单一指令交易（或极简多指令）。

### 3) 交易发送与确认逻辑

当前发送路径由 `TxWorker` 统一处理，保留即可。  
但需要确保 intent / 日志 / 统计与新交易类型匹配：

- 新增/替换 `TxIntent` 类型（用于统计和日志标识）。  
- 保持 `tx_sent / tx_confirmed / tx_failed` 统计可用。  

## 建议的最小改造路径（Keep 侧）

### Step A：新增“吃价差”交易构造函数

新增函数（示例命名）：

- `try_spread_capture(...)`

职责：
- 输入：`CrossingRegion` 或 `CrossesAndTopMakers`  
- 输出：构造交易并交给 `TxWorker::send_tx`  
- 不再构建 `fill_perp_order`  
- 改为“代理指令 + remaining_accounts”

### Step B：替换触发入口

把以下入口切到 `try_spread_capture(...)`：

- `try_uncross(...)` → 改为“吃价差”  
- 视情况处理 `try_auction_fill(...)`（如果拍卖单也要吃价差）

### Step C：保留机会发现逻辑

机会发现阶段保持不动：
- `find_crossing_region(...)`
- `find_crosses_for_auctions(...)`

只在“构造交易”阶段替换。

## 交易构造应携带的最低信息

Keep 侧需要提供给代理的最小集合（思路级）：

- market_index  
- makers（bid/ask 两侧的 user / stats）  
- taker（自有子账户）  
- 相关的 market / oracle / state / stats 账户  
- 任何代理指令所需的额外账户

> 目标是 **keep 侧只做“选择 + 传递”**，不做最终成功判断。

## 与代理简化版的接口约定（占位）

由于代理需要简化，建议最终对齐如下契约：

- 代理输入：  
  - market_index  
  - makers（remaining_accounts）  
  - 可选：参考价、阈值  
- 代理输出：  
  - `Ok(())` 表示成功吃价差  
  - 其他错误码即失败回滚  

Keep 侧只依赖 **代理的成功/失败返回值**。

## 风险与注意事项

- **双侧吃单** 会显著增加账户数量与 CU，需权衡。  
- **单侧吃单** 会暴露方向风险，但更轻量。  
- **盘口变化快**：建议链上最终判断而非链下判断。  
- **maker 账户缺失** 将导致交易失败，需限量与缓存。  

## 结语

只要“机会发现”已经稳定，**Keep 侧的主要改动集中在交易构造与发送**。  
后续可在代理简化版确定接口后，再完成 keep 侧最小改造。
