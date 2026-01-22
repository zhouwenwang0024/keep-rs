# 2026-01-22 代理程序 `arb_perp` 深度改造方案（联动 keep-rs + drift-rs）

> 目标：修复 `arb_perp` 现有逻辑在 **条件单/签名单、VAMM 出价、下单结构混乱、堆内存 OOM** 等问题，并与 `keep-rs` 的 `filler` 与 `drift-rs` SDK 账户构造保持一致。

---

## 0) 结论先行（不是“没有 BUG”）

当前链路并非没有问题，主要风险点集中在：
- `arb_perp` **扫描逻辑存在缺陷**：
  - **条件单**：如果 `order.must_be_triggered() && !order.triggered()`，直接 `continue` 跳过，**不会尝试触发**，导致未触发的条件单被完全忽略。
  - **签名单**：只扫描 `User.orders` 数组，如果签名单不在 `User.orders` 中（例如在 `SignedMsgUserOrders` 账户中），**不会被扫描到**。
- VAMM 价格未作为“可成交对手”参与规划，导致 **漏掉 VAMM 主导的价差机会**。
- `arb_perp` 的扫描 → 规划 → 执行分离不清晰，临时 Vec/Map 频繁构建，**仍可能触发 OOM**。
- remaining_accounts 的结构假设较强（User/Stats 紧邻、末尾追加 escrow），**对解析非常敏感**。

因此需要 **代理程序本身的结构性重构**，并在 SDK/keep 侧做更精确的账户与机会输入。

---

## 1) 当前 `arb_perp` 的关键流程（事实）

### 1.1 扫描阶段
- 使用 `load_maps` + `load_user_maps` 从 `remaining_accounts` 构建 UserMap。
- 仅从 UserMap 内的 **普通订单** 扫描 bid/ask，条件单只在 **已触发** 时才会被纳入。

```54:126:D:\RUST\proxy\jit-proxy\src\instructions\arb_perp.rs
    let mut rem_iter = ctx.remaining_accounts.iter().peekable();
    let AccountMaps { perp_market_map, mut oracle_map, spot_market_map } = load_maps(...)?;
    ...
    let (makers_map, _) = load_user_maps(&mut rem_iter, true)?;
    let (bids_tmp, asks_tmp) = find_bids_and_asks_from_users_with_auctions(...)?;
```

### 1.2 规划阶段
- 选取 bid/ask 侧，决定 `Take` 或 `Make` 路由。
- 计算 `chosen_base` 并约束最大可下单量。

```147:216:D:\RUST\proxy\jit-proxy\src\instructions\arb_perp.rs
    let sel_bid = if !has_non_auc_bid { SelectedSide::Make(best_auc) } else { SelectedSide::Take { makers: bids_scan } };
    let sel_ask = if !has_non_auc_ask { SelectedSide::Make(best_auc) } else { SelectedSide::Take { makers: asks_scan } };
    ...
    let max_base = calculate_max_base_asset_amount(...)?;
    chosen_base = chosen_base.min(max_base.cast()?).max(min_order_size);
```

### 1.3 执行阶段
- 每条腿 `exec_leg_min_heap`：构造最小 remaining_accounts → CPI（PlaceAndTake / PlaceAndMake）。
- `PlaceAndMake` 路径里 **仅对 AMM 价格做“贴价修正”**，并没有把 VAMM 当作真实对手侧。

```400:563:D:\RUST\proxy\jit-proxy\src\instructions\arb_perp.rs
    if let SelectedSide::Make(_) = side { params.post_only = MustPostOnly; params.bit_flags = IOC; }
    let (ra_vec, make_pair_opt, has_all_maker_accounts) = build_min_remaining_accounts_for_level(...)?;
    ...
    match side {
        SelectedSide::Take { .. } => place_and_take_perp_order(...)?,
        SelectedSide::Make(m) => { ... place_and_make_perp_order(..., m.taker_order_id)? }
    }
```

---

## 2) 已知问题与根因分析

### 2.1 条件单（Trigger）未处理
现状：扫描逻辑中，如果订单是条件单且未触发，直接 `continue` 跳过，**代理程序本身不会尝试触发条件单**。

```75:77:D:\RUST\proxy\jit-proxy\src\orders_ext.rs
    if order.must_be_triggered() && !order.triggered() {
        continue;
    }
```

**影响**：所有传入用户的 User 中，未触发的条件单都会被忽略，即使这些条件单在当前 slot/价格下应该被触发。这会导致漏掉大量潜在交易机会。

---

### 2.2 签名单（Swift）未被纳入扫描
**关键发现**：通过分析 `place_and_make_signed_msg` 函数和 `keep-rs` 的处理逻辑，确认了签名单的处理机制：

1. **签名单的识别方式**：
   - 签名单**不是**从 `User.orders` 中识别的，而是通过外部数据源（WebSocket 订阅）获取的
   - 在 `keep-rs` 中，签名单通过 `swift_order: SignedOrderInfo` 传入，这是一个独立的数据结构
   - 签名单存储在 `SignedMsgUserOrders` 账户中（由 `Wallet::derive_swift_order_account(authority)` 派生）
   - **关键问题**：`keep-rs` 的 `try_swift_fill` 中，先调用 `place_swift_order` 放置签名单，然后调用 `proxy_spread_capture`，但是**没有传递签名单信息**（`uuid` 和 `taker_signed_msg_user_orders` 账户地址）给代理程序
   - 代理程序无法知道哪些用户有签名单，也无法知道签名单的 `uuid`

2. **签名单的处理方式**：
   - 签名单需要使用 `drift::cpi::place_and_make_signed_msg_perp_order` 而不是 `place_and_make_perp_order`
   - 签名单需要 `taker_signed_msg_user_orders` 账户在账户结构中（不是 remaining_accounts）
   - 签名单使用 `signed_msg_order_uuid: [u8; 8]` 而不是 `order_id: Option<u32>` 来标识

3. **代理程序的问题**：
   - **无法识别签名单**：代理程序只扫描 `User.orders` 数组，无法识别哪些用户有签名单
   - **缺少 `SignedMsgUserOrders` 账户**：代理程序没有添加 `SignedMsgUserOrders` 账户到 remaining_accounts（`build_remaining_accounts_for_proxy` 中没有相关逻辑）
   - **使用错误的 CPI**：代理程序使用的是 `drift::cpi::place_and_make_perp_order`，这个函数**不支持签名单**
   - **账户结构不完整**：代理程序的 `ArbPerp` 账户结构中没有 `taker_signed_msg_user_orders` 字段

3. **代理程序的过滤条件分析**：
   代理程序的扫描逻辑在 `orders_ext.rs` 中，包含以下过滤条件：
   
   ```rust
   // 基础过滤（尽早 continue）
   if order.status != OrderStatus::Open {
       continue;  // 跳过非 Open 状态的订单
   }
   if !(order.market_type == MarketType::Perp && order.market_index == market_index) {
       continue;  // 跳过非目标市场的订单
   }
   if order.must_be_triggered() && !order.triggered() {
       continue;  // 跳过未触发的条件单
   }
   if order.max_ts != 0 && now > order.max_ts {
       continue;  // 跳过过期的订单
   }
   ```
   
   **关键问题**：
   - 代理程序只扫描 `user_ref.orders.iter()`，即只扫描 `User.orders` 数组中的订单
   - **签名单存储在 `SignedMsgUserOrders` 账户中，不在 `User.orders` 数组中**
   - 根据 Drift 源码（参考 `PlaceAndMakeSignedMsgPerpOrder`），程序从 `SignedMsgUserOrders` 账户中，通过 `uuid` 来查找签名单
   - 当 `FillPerpOrder` 的 `order_id` 是 `None` 时，程序需要从 remaining_accounts 中读取 `SignedMsgUserOrders` 账户，然后通过某种方式匹配签名单
   - 代理程序没有添加 `SignedMsgUserOrders` 账户到 remaining_accounts，因此无法找到签名单

4. **正确的处理方式（参考 `place_and_make_signed_msg`）**：
   ```rust
   // 需要 taker_signed_msg_user_orders 账户
   let taker_signed_msg_user_orders = ctx.accounts.taker_signed_msg_user_orders.to_account_info();
   
   // 使用 place_and_make_signed_msg_perp_order 而不是 place_and_make_perp_order
   drift::cpi::place_and_make_signed_msg_perp_order(
       cpi_context_place_and_make,
       order_params,
       signed_msg_order_uuid,  // 使用 uuid 而不是 order_id
   )?;
   ```

```65:80:D:\RUST\proxy\jit-proxy\src\orders_ext.rs
        for order in user_ref.orders.iter() {
            scanned_orders += 1;

            // —— 基础过滤（尽早 continue）——
            if order.status != OrderStatus::Open {
                continue;
            }
            if !(order.market_type == MarketType::Perp && order.market_index == market_index) {
                continue;
            }
            if order.must_be_triggered() && !order.triggered() {
                continue;
            }
            if order.max_ts != 0 && now > order.max_ts {
                continue;
            }
```

**影响**：所有传入用户的签名单都会被忽略，导致漏掉签名单相关的交易机会。

**解决方案**：

1. **在 `keep-rs` 侧传递签名单信息**：
   - 修改 `proxy_spread_capture` 方法签名，添加签名单信息参数：
     ```rust
     pub fn proxy_spread_capture(
         mut self,
         market_index: u16,
         taker_stats: &UserStats,
         makers: &[User],
         revenue_share_authority: Option<Pubkey>,
         // 新增：签名单信息
         signed_order_info: Option<(&SignedOrderInfo, &User)>, // (签名单信息, taker账户)
     ) -> Self
     ```
   - 在 `try_swift_fill` 中传递签名单信息：
     ```rust
     .proxy_spread_capture(
         taker_order.market_index,
         &filler_stats,
         maker_accounts.as_slice(),
         revenue_share_authority,
         Some((&swift_order, &taker_account_data)), // 传递签名单信息
     )
     ```

2. **修改 `build_remaining_accounts_for_proxy`**：
   - 添加参数接收签名单信息：
     ```rust
     pub fn build_remaining_accounts_for_proxy<'a>(
         // ... 现有参数 ...
         signed_order_info: Option<(&SignedOrderInfo, &User)>, // 新增
     ) -> Vec<AccountMeta>
     ```
   - 当存在签名单时，添加 `SignedMsgUserOrders` 账户到 remaining_accounts：
     ```rust
     if let Some((signed_order_info, taker_account)) = signed_order_info {
         let taker_signed_msg_user_orders = Wallet::derive_swift_order_account(
             &taker_account.authority,
         );
         rem.push(AccountMeta::new(taker_signed_msg_user_orders, false));
     }
     ```

3. **修改代理程序的账户结构**：
   - 修改 `ArbPerp` 账户结构，添加 `taker_signed_msg_user_orders` 字段（可选）：
     ```rust
     pub struct ArbPerp<'info> {
         // ... 现有字段 ...
         #[account(mut)]
         pub taker_signed_msg_user_orders: Option<AccountLoader<'info, SignedMsgUserOrders>>,
     }
     ```
   - 或者，在 remaining_accounts 中传递 `SignedMsgUserOrders` 账户（推荐，因为更灵活）

4. **修改代理程序的 CPI 调用**：
   - 在 `exec_leg_min_heap` 中，检测是否有签名单：
     - 如果 `taker_signed_msg_user_orders` 账户存在，且能找到对应的 `uuid`，使用 `place_and_make_signed_msg_perp_order`
     - 否则，使用 `place_and_make_perp_order`
   - 需要从 `keep-rs` 传递 `uuid` 信息，可以通过以下方式：
     - 在 `arb_perp` 指令参数中添加 `signed_msg_order_uuid: Option<[u8; 8]>`
     - 或者在 remaining_accounts 的特定位置传递 `uuid`（不推荐，容易出错）

5. **修改 `proxy_spread_capture` 的指令数据**：
   - 在 `ProxyArbPerpIx` 结构体中添加 `signed_msg_order_uuid` 字段：
     ```rust
     struct ProxyArbPerpIx {
         market_index: u16,
         signed_msg_order_uuid: Option<[u8; 8]>, // 新增
     }
     ```

---

### 2.3 VAMM 价差未参与“对手侧规划”
当前逻辑只把 VAMM 作为 **Make 侧的价格修正**，并不把 AMM 本身作为潜在交易对手（可被吃）。  
因此当价差来自 **maker vs VAMM** 或 **VAMM vs VAMM** 时，`arb_perp` 不会进入该路径。

---

### 2.4 下单结构混乱导致可读性差、容易出错
`arb_perp` 当前把 “扫描 → 规划 → 执行” 混在一个巨函数里，且多处依赖临时 Vec/clone。  
执行阶段又依赖 `build_min_remaining_accounts_for_level` 动态收集账户（链上运行时成本高）。

---

### 2.5 堆内存 / OOM 风险
原因组合：
- `load_user_maps` 对 remaining_accounts 做全量解析，若 remaining_accounts 很大，会占用大量堆。
- `find_bids_and_asks_from_users_with_auctions` 扫描所有用户订单，且包含若干 Vec 暂存。
- `exec_leg_min_heap` 每腿重新构造多组 Vec（perp/spot/oracle/ra）。

现有 heap snapshot 虽做了 reset，但 **高峰期间仍可能 OOM**，尤其在 remaining_accounts 较大或 maker 数量多时。

---

## 3) 修改方案总览

目标：把 `arb_perp` 拆成 **“输入更精确 + 逻辑更清晰 + 内存更稳定”** 的执行链路。

### 3.1 方案 A（推荐）：keep 侧预选机会 + proxy 侧执行/验证
> keep-rs 负责发现“候选跨价差”，proxy 只做 **二次验证 + 两腿 CPI + 盈利检查**。

关键点：
- keep-rs 在 `filler_trades` 内 **已掌握最优盘口与 makers**。
- proxy 侧不再扫描全量 UserMap，而是 **只处理 keep 给出的 makers/taker 集合**。
- `build_min_remaining_accounts_for_level` 仍可用，但只处理很小的 levels，显著降低 OOM 风险。

#### keep-rs 改动
- 在 `proxy_spread_capture` 里增加可选的 “预选 makers/taker 标识” 输入（或直接在 remaining_accounts 中只传这些用户）。
- 保持 `trigger_order` 在 keep 侧执行（当前 auction 路径已这样做）。

#### proxy 改动
- 新增 `arb_perp_min`（或在 `arb_perp` 增加模式参数）：
  - 不再调用 `load_user_maps` 做全量扫描；
  - 直接读取 remaining_accounts 中的 makers/taker；
  - 用 `build_min_remaining_accounts_for_level` 组装最小 RA。

---

### 3.2 方案 B：proxy 自身增强（仍扫描，但修复缺陷）
如果仍要在链上做全量扫描，则必须：

1) **条件单处理**  
   - 在扫描阶段记录“可触发订单”；
   - 先执行一批 `trigger_order` CPI；
   - 再扫描并执行 arb。

2) **Swift 订单处理**  
   - 显式把 SignedMsgUserOrders 加入 remaining_accounts；
   - 在 `find_bids_and_asks` 中扩展读取 SignedMsg orders；
   - 或在 proxy 内主动调用 Drift 的 “place_signed_msg_order” CPI。

3) **VAMM 作为对手侧**
   - 将 AMM bid/ask 作为 `SelectedSide::Vamm { price }`；
   - 执行腿时走 `place_and_take_perp_order`，并且允许无 maker accounts。

4) **内存重构**
   - 预分配固定数组，避免 Vec 扩容；
   - 避免 `load_user_maps` 解析全部 RA；
   - 将扫描限制为少量用户或少量订单。

---

## 4) 具体改造细节（推荐方案 A）

### 4.1 keep-rs 侧（机会驱动）
**目标**：让 proxy 执行时只处理“必要账户”，不做全量扫描。

建议修改点：
- `filler_trades.rs` 仍由 keep 侧筛选跨价差机会；
- 传给 `proxy_spread_capture` 的 makers 列表维持“小集合”（已是 topN）。
- `build_remaining_accounts_for_proxy` 只包含：
  - 我方账户 + 对手集合 + 目标市场；
  - 不再自动扫描所有用户持仓市场（可选：仅限需要市场）。

预期效果：remaining_accounts 大幅缩短，`arb_perp` OOM 风险大幅降低。

---

### 4.2 drift-rs 侧（账户最小化）
在 `build_remaining_accounts_for_proxy` 增加“最小化模式”开关：

建议签名：
```
build_remaining_accounts_for_proxy(..., makers, markets_readable, markets_writable, revenue_share_authority, minimal: bool)
```

当 `minimal=true` 时：
- 不遍历所有用户持仓市场；
- 只加入目标 perp + quote spot（以及必要 oracle）；
- 对手仅限 `makers` 的 User/Stats；

---

### 4.3 proxy 侧（显式两腿执行）
新增指令：`arb_perp_min`（或 `arb_perp` 增加 `mode` 参数）。

行为：
- 不扫描 `load_user_maps`；
- 直接读取 remaining_accounts 中的 makers/taker；
- 调用 `build_min_remaining_accounts_for_level` 组装 RA；
- 两腿执行 + PnL 校验，逻辑与现有 `arb_perp` 保持一致。

---

## 5) 风险与验证清单

1) **Swift 订单可见性**  
   - 如果 Swift 订单未落入 User orders，必须通过 SignedMsg 账户解析或在 keep 侧先入账。

2) **remaining_accounts 顺序敏感**  
   - `build_min_remaining_accounts_for_level` 假设 `User → UserStats` 紧邻；
   - Swift escrow 必须放在最后（已在 SDK 侧保证）。

3) **VAMM 逻辑分支**  
   - 引入 `SelectedSide::Vamm` 后要确保 CPI 没有 makers 时仍能走 AMM 成交。

---

## 6) 输出物清单（建议）

- `proxy`：新增 `arb_perp_min` / 或改造 `arb_perp`  
- `drift-rs`：`build_remaining_accounts_for_proxy` 支持 minimal 模式  
- `keep-rs`：默认走 minimal 账户路径（更稳、更省内存）

---

## 7) 下一步建议

1. 先实现 **方案 A**（最小改动、最稳定）。  
2. 再根据实际链上行为决定是否需要方案 B 的全量扫描增强。  

