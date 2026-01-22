# 2026-01-22 代理吃价差实现改动总结

本文记录本次**实际代码修改**（非设计草案）的范围、原因与关键细节，覆盖 `keep-rs` 与 `drift-rs` 两个项目。

---

## 1) 改动目标

- 将 `keep-rs` 中所有 `fill_perp_order` 调用替换为 `proxy_spread_capture`  
- 代理程序负责失败筛选与回滚  
- 我方账户作为主 accounts；原 taker 进入 makers  
- 保留拍卖触发单逻辑（`trigger_order`）  
- Swift 路径仅保留 `place_swift_order`，不再参与 revenue 相关构造  

---

## 2) keep-rs 修改

### 2.1 `src/filler_trades.rs`

**替换点**

- `try_swift_fill`：  
  - 仍 `place_swift_order`  
  - `fill_perp_order` → `proxy_spread_capture`  
  - 将 taker 的 `User` **加入 makers**（保证代理端可见）  
  - 传入 **filler 的 `UserStats`** 以追加我方推荐人账户  

- `try_auction_fill`：  
  - **保留 `trigger_order`**（不修改）  
  - `fill_perp_order` → `proxy_spread_capture`  
  - 将 taker 的 `User` **加入 makers**  
  - 传入 **filler 的 `UserStats`**  

- `try_uncross`：  
  - `fill_perp_order` → `proxy_spread_capture`  
  - 将 taker 的 `User` **加入 makers**  
  - 传入 **filler 的 `UserStats`**

**说明**

- 原逻辑不会把 taker 加入 makers；本次明确加入以满足“我方=主账户，原 taker 作为对手侧”的新结构  
- 未再使用 `swift_order.has_builder()`，因为代理路径不需要 revenue share escrow  

---

## 3) drift-rs 修改

### 3.1 `crates/src/lib.rs`

**新增结构与函数**

- `ArbPerpAccounts` + `build_accounts_proxy(...)`  
  - 仅生成代理程序的**主 accounts**（严格对齐 `ArbPerp<'info>`）  
  - 明确 signer/writable：state readonly、user/user_stats writable、authority signer readonly  

- `build_remaining_accounts_for_proxy(...)`  
  - **不调用 `build_accounts`**  
  - 手工复刻其账户收集策略：  
    - 使用 `BTreeSet<RemainingAccount>` 去重与稳定排序  
    - 可写市场优先  
    - 自动补齐用户持仓 market/oracle/quote  
    - makers 的 user/stats  
    - 我方推荐人（基于 `taker_stats.is_referred()`）  

- `ProxyArbPerpIx`  
  - 本地构造代理指令数据  
  - 使用 `global:arb_perp` 的 Anchor discriminator  

**新增方法**

- `TransactionBuilder::proxy_spread_capture(...)`  
  - 构造代理主 accounts  
  - 构造 remaining_accounts（手工复刻逻辑）  
  - 指令 program_id **硬编码**为 `Ecx5sm34EyesW26hiYT8KYnZJT5E79Arm6RHXX2e5c4x`  

---

## 4) 关键行为变化

- **撮合逻辑被代理替换**：所有 `fill_perp_order` 调用替换为 `proxy_spread_capture`  
- **触发单逻辑保留**：`try_auction_fill` 的 `trigger_order` 未改动  
- **taker 进入 makers**：原 taker 作为对手侧账户传入代理  
- **推荐人**：基于 **filler stats** 追加我方推荐人账户  
- **不再使用 has_builder**：Swift 的 builder 标志不影响代理路径构造  
- **remaining_accounts 不再调用 build_accounts**：避免错误引入 Drift 主 accounts  
- **Swift 的 has_builder**：旧逻辑仅影响 revenue share escrow，代理路径不需要，已移除  

---

## 5) 修改文件清单

- `keep-rs/src/filler_trades.rs`  
- `drift-rs/crates/src/lib.rs`  

---

## 6) 后续验证建议

- 编译 `keep-rs` 与 `drift-rs`  
- 发送一笔测试交易，确认代理端 `arb_perp` 的 remaining_accounts 解析顺序与权限  
- 若代理程序新增/减少所需账户，需同步更新 `build_remaining_accounts_for_proxy`
