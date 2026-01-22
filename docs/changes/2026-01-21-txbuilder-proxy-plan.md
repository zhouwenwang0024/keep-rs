# TransactionBuilder + 代理吃价差改造思路（仅分析）

本文件基于 `TransactionBuilder` 的现有结构，提出**如何构造交易改为走代理程序**的目标方案。  
注意：**不改代码**，仅分析与目标设计。

---

## 背景与目标

当前 `filler_trades.rs` 通过 `fill_perp_order` 实现撮合成交。  
目标是改为：

- **吃价差**（不撮合 taker/maker）
- 交易发送到**代理程序**（`D:\RUST\proxy`）
- 代理内做两次 CPI，并判断是否盈利
- 若无盈利则失败回滚，避免浪费手续费

---

## TransactionBuilder 现状理解（关键点）

`TransactionBuilder` 的角色是：**收集指令 → build 交易消息**。  
它本身并不执行业务判断，只负责拼装 `Instruction`。

关键能力（与代理接入相关）：

- 能追加任意指令（`add_ix` / 直接 `ixs.push`）
- 能替换指令（`set_ix`）
- 最终 `build()` 返回交易消息

这意味着：  
**无需强行改动底层交易系统**，只需要把 `fill_perp_order` 替换为“代理指令”即可。

---

## 目标交易构造：从 fill_perp_order → proxy 指令

### 当前路径（简化）

`filler_trades.rs` 中：

- `TransactionBuilder::with_priority_fee(...)`
- `TransactionBuilder::fill_perp_order(...)`
- `TransactionBuilder::build()`
- `TxWorker::send_tx(...)`

### 目标路径（简化）

替换为：

- `TransactionBuilder::with_priority_fee(...)`
- **`TransactionBuilder::proxy_spread_capture(...)`**  ← 新增方法
- `TransactionBuilder::build()`
- `TxWorker::send_tx(...)`

注意：`proxy_spread_capture` 是“概念方法名”，仅用于表达目标设计。

---

## TransactionBuilder 需要新增的能力（目标）

新增一个方法，让调用方无需手工拼 `Instruction`：

```
TransactionBuilder::proxy_spread_capture(
    proxy_program_id,
    params,
    remaining_accounts
)
```

### 目标职责

- 构造一条“代理程序指令”
- 填充所需账户（taker / maker / market / oracle / stats 等）
- 允许携带 `remaining_accounts`（用于 makers 与 referrer）
- 输出仍是 `TransactionBuilder`，与现有链式调用保持一致

---

## 代理指令目标行为（抽象）

代理程序内部应：

1. 接收市场与价差参数
2. **CPI #1：吃 bid**
3. **CPI #2：吃 ask**
4. 计算净收益
5. 无盈利则返回错误 → 交易回滚

这样 keep 侧只关心“发指令”，不做成功判断。

---

## filler_trades.rs 的改造方向（目标）

以下为 **目标替换形态**（概念级）：

### try_uncross / try_auction_fill / try_swift_fill

- 不再构造 `fill_perp_order`  
- 改为 `proxy_spread_capture`  
- 仍保留：
  - `priority_fee`
  - `cu_limit`
  - `remaining_accounts` 构造逻辑

---

## 需要代理程序配合的最低接口

为了让 keep 侧改造最小化，代理程序应提供：

- 简单入参结构（market_index + makers + 可选参考价）
- 一条指令即可完成“吃价差并判定”
- 失败回滚（ErrorCode）

---

## 结论

要实现“吃价差 + 失败筛选”，**核心是把 `fill_perp_order` 替换为代理指令**。  
TransactionBuilder 本身只需要“新增一条封装代理指令的方法”，  
Keep 侧不需要改变交易发送系统。
