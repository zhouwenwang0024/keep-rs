# proxy_spread_capture 设计说明（仅分析）

本文件基于 `TransactionBuilder` 的现有风格，提出新增方法 `proxy_spread_capture` 的**详细设计目标**。  
注意：**不改代码**，仅描述预期实现与接口。

---

## 1. 参考的 TransactionBuilder 设计风格

从 `drift-rs/crates/src/lib.rs` 的方法可以看出通用模式：

- `add_ix / set_ix / ixs / build`：负责维护 `ixs` 列表并生成交易消息  
- `place_swift_order / place_and_take / fill_perp_order`：  
  - 组装账户（`build_accounts` + 追加 remaining）  
  - 构造 `Instruction`  
  - `self.ixs.push(ix)`  
  - 返回 `Self` 保持链式调用

`proxy_spread_capture` 预期**完全遵循此模式**。

---

## 2. proxy_spread_capture 的目标职责

**目标**：替代 `fill_perp_order`，将交易发送给“代理程序”，  
由代理内部执行两次 CPI 并判断是否盈利。

核心要求：

- keep 侧不做最终成功判断  
- 若无盈利，代理指令失败回滚，避免浪费手续费

---

## 3. 建议的方法签名（概念）

> 本方案以 **我方子账户作为 ArbPerp 主 accounts**，  
> 原本 `fill_perp_order` 的 taker/taker_stats **统一并入 makers**。  
> 仅保留 `taker_stats` 用于 referrer 相关账户的构造。

```
pub fn proxy_spread_capture(
    mut self,
    market_index: u16,
    taker_stats: &UserStats,     // 我方 stats（用于 referrer）
    makers: &[User],
) -> Self
```

**说明**：
- 保持 `TransactionBuilder` 链式调用形式  
- `makers` 包含原 taker 的 User（原 taker/taker_stats 视为对手侧）  

---

## 4. ProxySpreadParams（建议字段）

> 目标：足够表达“吃价差”并允许代理判断盈利

当前采用 `arb_perp` 入口时，**参数只有一个**：

- `market_index: u16`

其余参数（`reference_price` / `edge_ppm` / `side_mode` 等）属于 `jit` 路径，  
在 `arb_perp` 入口下应删除，避免误导。

是否必需取决于代理简化版的最终逻辑，但**至少需要 `market_index`**。

---

## 5. 账户组装方式（参考现有风格）

参考 `arb_perp` 的账户声明与 Python 模版：

1. 固定账户（state / user / user_stats / authority / drift_program）  
2. remaining_accounts **在方法内部构造**，不要求 caller 传入  

### 目标账户最小集合（概念）

- `state`
- `user`（自有账户）
- `user_stats`
- `authority`
- `drift_program`

> 具体依赖应以代理程序的账户声明为准。

### 关键修正（采用方案 A：新增 proxy 专用 build_accounts）

为避免误用 drift 的 `build_accounts`，采用 **方案 A**：

1. 新增 `build_accounts_proxy(...)`  
2. 只面向代理程序（`jit_proxy`），不污染 drift SDK  
3. 按代理 IDL 组装主 accounts  
4. remaining_accounts **不调用 build_accounts**，但严格复刻其“市场/预言机/quote”收集逻辑

结论（新版）：
**主 accounts = build_accounts_proxy(ArbPerp<'info>)；remaining_accounts = 手工复刻 build_accounts 的收集逻辑。**

---

## 6. 指令构造流程（概念）

遵循 `TransactionBuilder` 现有模式：

1. `build_remaining_accounts_for_proxy(...)`（内部生成 remaining_accounts）
2. `Instruction { program_id: 固定代理ID, accounts, data }`
3. `self.ixs.push(ix)`
4. `self`

其中 `data` 为代理程序的指令数据编码（IDL）。

---

## 7. filler_trades.rs 中的替换目标

现有调用：

- `fill_perp_order(...)`

目标替换为：

- `proxy_spread_capture(...)`

调用结构保持链式风格，并保留 referrer 相关入参：

- `with_priority_fee(...)`
- `place_swift_order(...)`（仅 Swift 路径保留）
- `proxy_spread_capture(market_index, taker_stats, makers)`
- `build()`
- `TxWorker::send_tx(...)`

对齐 `filler_trades.rs` 的三个调用点：
- `try_swift_fill`：`place_swift_order` 之后调用 `proxy_spread_capture`  
- `try_auction_fill`：**保留触发单逻辑**（`trigger_order`），随后用 `proxy_spread_capture` 替换 `fill_perp_order`  
- `try_uncross`：直接调用 `proxy_spread_capture`

> Swift 分支原先 `fill_perp_order(..., Some(swift_order.has_builder()))`  
> 仅用于 revenue share escrow 的可选账户判断。  
> 代理路径不需要该账户，因此 `swift_order.has_builder()` 不再参与构造。

---

## 8. 代理程序的预期配合点

代理指令内部应完成：

1. **CPI #1**（吃 bid）  
2. **CPI #2**（吃 ask）  
3. 计算净收益（考虑手续费）  
4. 无盈利则返回错误  

从 keep 视角看：  
代理指令 **成功=盈利成立**，失败则自动回滚。

---

## 9. 与现有方法的对齐关系

| 现有方法 | 作用 | proxy_spread_capture 对齐点 |
| --- | --- | --- |
| `fill_perp_order` | 撮合成交 | 替换为代理指令 |
| `place_swift_order` | 下 swift taker 单 | 仅作为机会来源，不再用于成交 |
| `place_and_take` | 直接 place+take | 参考其账户组装逻辑 |
| `add_ix` / `set_ix` / `build` | 交易指令管理 | 复用，不改变 |

---

## 10. 结论

`proxy_spread_capture` 只是 `TransactionBuilder` 的**一个新指令封装**。  
它的重点在于**账户组装与参数封装**，  
真实盈利判定与失败回滚完全交给代理程序。  
这样 keep 侧仅负责发交易，避免白白付手续费。

---

## 11. 代码实现（完整版本，按方案 A）

> 下面代码为**完整实现形态**（写在 MD 中作为目标规格）。  
> 仍然是“设计稿”，不直接修改工程代码，但要求结构完整、可落地。

### 11.1 代理入口与参数结构（当前真实定义）

代理程序入口在 `D:\RUST\proxy\jit-proxy/src/lib.rs`：
- `arb_perp(ctx, market_index)`
- `jit(ctx, params)`

本次以 **`arb_perp`** 作为入口（按你的要求）。

对应的 accounts 在 `D:\RUST\proxy\jit-proxy/src/instructions/arb_perp.rs`：

```
pub struct ArbPerp<'info> {
    pub state: Box<Account<'info, State>>,
    pub user: AccountLoader<'info, User>,
    pub user_stats: AccountLoader<'info, UserStats>,
    pub authority: Signer<'info>,
    pub drift_program: Program<'info, Drift>,
}
```

### 11.2 build_accounts_proxy（完整实现）

```rust
// drift-rs/crates/src/lib.rs
// 仅为设计稿：新增 proxy 专用 accounts 结构与构造函数

#[derive(Clone, Copy, Debug)]
pub struct ArbPerpAccounts {
    pub state: Pubkey,
    pub user: Pubkey,
    pub user_stats: Pubkey,
    pub authority: Pubkey,
    pub drift_program: Pubkey,
}

pub fn build_accounts_proxy(accounts: ArbPerpAccounts) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(accounts.state, false),
        AccountMeta::new(accounts.user, true),       // user (mut)
        AccountMeta::new(accounts.user_stats, true), // user_stats (mut)
        AccountMeta::new(accounts.authority, true),
        AccountMeta::new_readonly(accounts.drift_program, false),
    ]
}
```

### 11.3 TransactionBuilder 新方法（完整实现）

```rust
// drift-rs/crates/src/lib.rs
impl<'a> TransactionBuilder<'a> {
    /// 调用代理程序 arb_perp 指令（吃价差入口）
    ///
    /// 以我方账户为主 accounts，makers 内包含原 taker。
    /// 仍保留 taker_stats 以构造 referrer 相关账户。
    pub fn proxy_spread_capture(
        mut self,
        market_index: u16,
        taker_stats: &UserStats,
        makers: &[User],
    ) -> Self {
        // 1) 主 accounts（严格对齐 ArbPerp<'info>）
        let mut accounts = build_accounts_proxy(ArbPerpAccounts {
            state: *state_account(),
            user: self.sub_account, // 我方子账户
            user_stats: Wallet::derive_stats_account(&self.owner()),
            authority: self.authority,
            drift_program: drift::ID,
        });

        // 2) remaining_accounts（按 build_accounts 风格构造）
        let remaining_accounts = build_remaining_accounts_for_proxy(
            self.program_data,
            market_index,
            self.account_data.as_ref(), // 我方 user 作为 taker_user
            taker_stats,
            makers,
            std::iter::empty(),                 // markets_readable
            std::iter::once(&MarketId::perp(market_index)), // markets_writable
        );
        accounts.extend(remaining_accounts);

        // 3) 代理指令（固定 program_id）
        let proxy_program_id = Pubkey::from_str("Ecx5sm34EyesW26hiYT8KYnZJT5E79Arm6RHXX2e5c4x")
            .expect("valid proxy program id");
        let ix = Instruction {
            program_id: proxy_program_id,
            accounts,
            data: InstructionData::data(&jit_proxy_idl::instructions::ArbPerp { market_index }),
        };

        self.ixs.push(ix);
        self
    }
}
```

### 11.4 remaining_accounts 构造函数（完整实现，参考 build_accounts 逻辑但不调用它）

```rust
// drift-rs/crates/src/lib.rs
// 仅为设计稿：proxy remaining_accounts 构造（手工复刻 build_accounts 的收集逻辑，不调用 build_accounts）
pub fn build_remaining_accounts_for_proxy(
    program_data: &ProgramData,
    market_index: u16,
    taker_account: &User,
    taker_stats: &UserStats,
    makers: &[User],
    markets_readable: impl Iterator<Item = &MarketId>,
    markets_writable: impl Iterator<Item = &MarketId>,
) -> Vec<AccountMeta> {
    // 1) 手工收集市场/预言机账户（不使用 build_accounts）
    // 顺序必须与 drift optional_accounts 解析一致：perp/spot -> oracle
    // 使用 BTreeSet 保证顺序稳定并自动去重
    let mut accounts = std::collections::BTreeSet::<RemainingAccount>::new();

    let mut include_market =
        |market_index: u16, market_type: MarketType, writable: bool| match market_type {
            MarketType::Spot => {
                let SpotMarket { pubkey, oracle, .. } = program_data
                    .spot_market_config_by_index(market_index)
                    .expect("exists");
                accounts.extend(
                    [
                        RemainingAccount::Spot {
                            pubkey: *pubkey,
                            writable,
                        },
                        RemainingAccount::Oracle { pubkey: *oracle },
                    ]
                    .iter(),
                )
            }
            MarketType::Perp => {
                let PerpMarket { pubkey, amm, .. } = program_data
                    .perp_market_config_by_index(market_index)
                    .expect("exists");
                accounts.extend(
                    [
                        RemainingAccount::Perp {
                            pubkey: *pubkey,
                            writable,
                        },
                        RemainingAccount::Oracle { pubkey: amm.oracle },
                    ]
                    .iter(),
                )
            }
        };

    // 1.1) 显式指定的可写市场优先
    for market in markets_writable {
        include_market(market.index(), market.kind(), true);
    }
    // 1.2) 显式指定的只读市场
    for market in markets_readable {
        include_market(market.index(), market.kind(), false);
    }
    // 1.3) taker/makers 持仓市场（只读）
    for user in makers.iter().chain(std::iter::once(taker_account)) {
        for p in user.spot_positions.iter().filter(|p| !p.is_available()) {
            include_market(p.market_index, MarketType::Spot, false);
        }
        for p in user.perp_positions.iter().filter(|p| !p.is_available()) {
            include_market(p.market_index, MarketType::Perp, false);
        }
        include_market(MarketId::QUOTE_SPOT.index(), MarketType::Spot, false);
    }

    let mut rem: Vec<AccountMeta> = accounts.into_iter().map(Into::into).collect();

    // 2) 追加 maker user / maker stats（保持 user->stats 相邻）
    let mut seen_pairs = std::collections::HashSet::<(Pubkey, Pubkey)>::new();
    for maker in makers {
        let maker_user = Wallet::derive_user_account(&maker.authority, maker.sub_account_id);
        let maker_stats = Wallet::derive_stats_account(&maker.authority);
        if seen_pairs.insert((maker_user, maker_stats)) {
            rem.push(AccountMeta::new(maker_user, false));
            rem.push(AccountMeta::new(maker_stats, false));
        }
    }

    // 3) 追加 referrer（若启用）
    if taker_stats.is_referred() {
        rem.push(AccountMeta::new(
            Wallet::derive_user_account(&taker_stats.referrer, 0),
            false,
        ));
        rem.push(AccountMeta::new(
            Wallet::derive_stats_account(&taker_stats.referrer),
            false,
        ));
    }

    // 4) 不再追加“额外账户”占位
    //    推荐人账户已在上方 referrer 分支加入，避免重复/顺序混乱
    rem
}
```

### 11.5 filler_trades.rs 调用方式（对齐 build_accounts 风格）

构造方式为：

- `proxy_spread_capture(...)` 内部完成 remaining_accounts 生成  

等价的 Rust 调用示例（完整形态）：

```rust
let tx = TransactionBuilder::new(...)
    .with_priority_fee(priority_fee, Some(cu_limit))
    // Swift 路径保留（仅用于下 taker 单）
    // .place_swift_order(&swift_order, &taker_account_data)
    .proxy_spread_capture(
        market_index,
        &taker_stats, // 我方 stats
        makers.as_slice(), // makers 必须包含原 taker User
    )
    .build();
```

---

## 12. 实现说明（对照示例）

- `proxy_spread_capture` 的结构与 `fill_perp_order` 类似：  
  **主 accounts 对齐 ArbPerp<'info> → remaining_accounts 内部构造 → push 到 ixs**。
- **禁止**用 `build_accounts / to_account_metas` 替代 proxy 主 accounts。  
- `remaining_accounts` **不调用 build_accounts**，但复刻其账户收集逻辑：  
  - 市场/预言机/quote spot 等必要账户（含可读/可写市场迭代器）  
  - makers 的 User / UserStats  
  - referrer 相关账户（如启用）  
  并保证顺序与代理端 `load_maps / load_user_maps` 解析一致。
- 代理程序内部完成两次 CPI，并在无盈利时返回错误，  
  交易自动回滚，避免手续费浪费。
