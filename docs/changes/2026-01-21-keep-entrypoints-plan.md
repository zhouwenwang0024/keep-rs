# Keep 入口函数改造思路：try_auction_fill / try_uncross / try_swift_fill

本文件仅讨论 **入口函数层面的改造思路**。  
要求：每个入口函数说明 **1) 当前逻辑** 与 **2) 目标逻辑**。  
仅为思路文档，不改代码；最终实现需与代理简化版接口对齐。

---

## 1) `try_auction_fill(...)`

### 1.1 当前逻辑（撮合）
- 输入：`CrossesAndTopMakers`（拍卖/限价撮合机会）。
- 处理流程：
  - 拉取 taker / makers / stats 账户；
  - 如是 trigger 订单先触发；
  - 构造 `fill_perp_order(...)`；
  - 发送交易。
- 语义：**撮合 taker ↔ makers**，不是自有账户吃价差。

### 1.2 目标逻辑（吃价差）
- 输入仍来自 `CrossesAndTopMakers`，但 **只用作机会来源**。
- 处理流程：
  - 从 crosses 中提取 best bid / best ask（或 topN makers）；
  - 构造“自有账户作为 taker”的吃价差交易；
  - 交易指令改为 **代理指令 + remaining_accounts**；
  - 由代理程序判断成功/失败。
- 语义：**自有账户吃掉交叉价差**，不再撮合 taker/maker。

---

## 2) `try_uncross(...)`

### 2.1 当前逻辑（解交叉）
- 输入：`CrossingRegion`（盘口交叉区域）。
- 处理流程：
  - 取 best bid / best ask；
  - 选 taker 与 makers（交叉簿上的订单）；
  - 构造 `fill_perp_order(...)` 解交叉；
  - 发送交易。
- 语义：**撮合交叉盘口，修复盘口有效性**。

### 2.2 目标逻辑（吃价差）
- 输入仍是 `CrossingRegion`，但 **仅用于判断交叉机会**。
- 处理流程：
  - 提取 `best_bid_price / best_ask_price`；
  - 选取两侧对应价位的 makers（topN）；
  - 构造“自有账户吃价差”的交易；
  - 指令为 **代理指令 + remaining_accounts**；
  - 由代理程序返回成功/失败。
- 语义：**自有账户吃掉交叉价差**，而不是“撮合修复”。

---

## 3) `try_swift_fill(...)`

### 3.1 当前逻辑（Swift 撮合）
- 输入：Swift 订单流（taker signed order）。
- 处理流程：
  - 构造 taker 订单参数；
  - 找到 maker crosses；
  - `place_swift_order + fill_perp_order`；
  - 发送交易。
- 语义：**补单撮合**，不是吃价差。

### 3.2 目标逻辑（吃价差）
- Swift 订单路径本质是“撮合/补单”，与吃价差不一致。  
  目标逻辑有两种策略（择一）：

1. **停用 Swift 撮合路径**  
   - Swift 订单不再触发交易构造。
2. **Swift 仅做机会发现**  
   - Swift 订单流只作为“机会信号”；  
   - 交易构造统一进入“吃价差路径”；  
   - 不再构造 `place_swift_order`。

---

## 入口层改造后统一形态（概念）

三条入口最终收敛为：

- 识别机会  
- 抽取 makers（best bid/ask 对应价位）  
- 构造 “代理指令 + remaining_accounts”  
- 发送交易（由 `TxWorker` 负责）  
- 成功/失败由代理程序返回

---

## 保持不变的部分

- 机会发现逻辑（DLOB / crossing region / auction crosses）
- slot / oracle / user cache 的维护

这些是上游数据基础，可原样保留。
