# Filler / SDK / Proxy 机器人流程说明

本文基于当前代码实现，分别梳理 **keep-rs filler**、**drift-rs SDK**、**proxy 程序** 的执行流程与关键数据流。

---

## 1) Filler 启动与订阅阶段

Filler 启动时：
- 初始化 `DLOB`、`TxWorker`、优先费订阅
- 过滤 market 列表（排除 `bet` 和未初始化市场）
- 订阅 swift 订单流、blockhash、用户/统计账户 WS、pyth 价格流
- 构建 JIT 子账户与 Binance symbol 列表，准备 JIT 流程

对应代码：

```64:165:keep-rs/src/filler.rs
impl FillerBot {
    pub async fn new(config: Config, drift: DriftClient, metrics: Arc<Metrics>) -> Self {
        let dlob: &'static DLOB = Box::leak(Box::new(DLOB::default()));
        let tx_worker = TxWorker::new(drift.clone(), metrics, config.dry);
        // ...
        let priority_fee_subscriber =
            PriorityFeeSubscriber::new(drift.rpc().url(), &market_pubkeys);
        let priority_fee_subscriber = priority_fee_subscriber.subscribe();
        // ...
        let swift_order_stream = drift
            .subscribe_swift_orders(&market_ids, Some(true), None, None)
            .await
            .expect("subscribed swift orders");
        // ...
        drift.subscribe_blockhashes().await.expect("subscribed");
        let (slot_rx, book_rx, user_cache, ws_subscriptions) =
            setup_ws(drift.clone(), dlob, market_ids.clone()).await;
        // ...
        let pyth_price_feed = crate::util::subscribe_price_feeds(pyth_feed_cli, &market_ids, &[]);
        // ...
    }
}
```

---

## 2) 事件循环总览（Swift / ARB / JIT）

Filler 主循环 `tokio::select!` 中主要处理：
- **Swift 订单流** → 计算价格与 cross → 进入 `try_swift_fill`
- **WS 盘口/slot 更新** → 触发 onchain cross 检查
- **Binance / Pyth 更新** → 进入 JIT 机会判断 → `try_jit`

Swift 路径入口示例：

```220:339:keep-rs/src/filler.rs
swift_order = async { /* ... */ } => {
    match swift_order {
        Some(signed_order) => {
            let mut order_params = signed_order.order_params();
            // 计算 auction / limit 价格
            // 收集 maker crosses
            let crosses = collect_swift_crosses(/* ... */);
            // ...
            if !crosses.is_empty() || has_vamm_cross {
                let maker_crosses = MakerCrosses { /* ... */ };
                let pf = scale_priority_fee(priority_fee_subscriber.priority_fee_nth(0.3));
                // 进入 try_swift_fill
            }
        }
    }
}
```

JIT 路径入口示例（Binance 更新触发）：

```580:669:keep-rs/src/filler.rs
binance_update = async { /* ... */ } => {
    match binance_update {
        Some(update) => {
            jit_price_cache.insert(update.market_index, (update.binance_mid, update.ts_ms));
            if let Some(strategy) = jit_strategy.as_mut() {
                if let Some(intent) = strategy.maybe_intent(/* ... */) {
                    try_jit(/* ... */).await;
                }
            }
        }
    }
}
```

---

## 3) Swift / ARB 交易构造（keep-rs）

### 3.1 Swift fill
`try_swift_fill` 关键步骤：
1. 获取 taker / filler / maker 用户与 stats
2. 触发条件单（trigger orders）
3. 调用 `proxy_spread_capture` 构造 CPI 交易
4. 追加 Jito tip（若启用）
5. 发送交易

```19:188:keep-rs/src/filler_trades.rs
pub(crate) async fn try_swift_fill(/* ... */) {
    // 获取 filler/taker/maker user + stats
    // ...
    tx_builder = tx_builder
        .place_swift_order(&swift_order, &taker_account_data)
        .proxy_spread_capture(
            taker_order.market_index,
            &filler_stats,
            maker_accounts.as_slice(),
            maker_stats_vec.as_slice(),
            revenue_share_authority,
        );
    // ...
    tx_builder = maybe_add_jito_tip(tx_builder, *drift.wallet().authority());
    let tx = tx_builder.build();
    tx_worker_ref.send_tx(tx, /* ... */);
}
```

### 3.2 Onchain cross / ARB
`try_onchain_cross` 类似，但针对 DLOB crossing 区域构造：

```191:328:keep-rs/src/filler_trades.rs
pub(crate) async fn try_onchain_cross(/* ... */) {
    // 获取 filler stats
    // ...
    // 收集 maker user + maker stats
    // ...
    tx_builder = tx_builder.proxy_spread_capture(
        market_index,
        &filler_stats,
        maker_accounts.as_slice(),
        maker_stats_vec.as_slice(),
        None,
    );
    // ...
}
```

---

## 4) JIT 策略与交易构造（keep-rs）

### 4.1 JIT 机会判断
JIT 使用 Binance mid + Drift DLOB 计算参考价，并仅依赖“价格交叉”触发：

```50:176:keep-rs/src/jit/jit_strategy.rs
pub fn maybe_intent(/* ... */) -> Option<JitIntent> {
    if now_ms.saturating_sub(reference_ts_ms) > self.staleness_ms { return None; }
    // ...
    let (best_bid, best_ask) = match best_levels_with_makers(/* ... */) { /* ... */ };
    let reference_price = (binance_mid as f64 - 0.8 * basis).round() as i64;
    // 仅用价格交叉判断
    let sell_ok = best_bid.price as i128 > reference_price as i128;
    let buy_ok = best_ask.price as i128 < reference_price as i128;
    if !sell_ok && !buy_ok { return None; }
    Some(JitIntent { /* ... */ })
}
```

### 4.2 JIT 交易构造
`try_jit`：
1. 收集 maker 用户与 stats
2. 调用 `proxy_jit`
3. 追加 Jito tip
4. 发送交易

```13:116:keep-rs/src/jit/jit_trades.rs
pub(crate) async fn try_jit(/* ... */) {
    // maker accounts + maker stats
    // ...
    tx_builder = tx_builder.proxy_jit(
        intent.market_index,
        intent.reference_price,
        intent.edge_ppm,
        &jit_stats,
        maker_accounts.as_slice(),
        maker_stats_vec.as_slice(),
        proxy_program_id,
    );
    // ...
    tx_builder = maybe_add_jito_tip(tx_builder, *drift.wallet().authority());
    let tx = tx_builder.build();
    tx_worker_ref.send_tx(tx, /* ... */);
}
```

---

## 5) Jito Tip 与发送机制（keep-rs）

### 5.1 Tip 构造（同一笔交易内）
`maybe_add_jito_tip` 会在启用 Jito 时，向当前 `TransactionBuilder` 追加一条 transfer 小费指令：

```33:69:keep-rs/src/util.rs
fn jito_enabled() -> bool {
    // 检查 JITO_UUIDS / JITO_UUID1/2/UUID
}
pub fn maybe_add_jito_tip(mut tx_builder: TransactionBuilder<'_>, authority: Pubkey) -> TransactionBuilder<'_> {
    if !jito_enabled() { return tx_builder; }
    let tip_account = std::env::var("JITO_TIP_ACCOUNT")/* ... */;
    let tip_lamports = std::env::var("JITO_TIP_LAMPORTS")/* ... */;
    let ix = system_instruction::transfer(&authority, &tip_account, tip_lamports);
    tx_builder = tx_builder.add_ix(ix);
    tx_builder
}
```

### 5.2 发送：RPC 与 Jito 并行
`TxWorker` 仍并行发送：
- RPC 发送（`sign_and_send_with_config`）
- Jito 发送（bundle 只包含单笔业务交易）

```120:169:keep-rs/src/tx_worker.rs
let jito_fut = async {
    if let Some(jito) = jito {
        let business_tx = drift.wallet().sign_tx(tx, blockhash)?;
        let raw_business = bincode::serialize(&business_tx)?;
        jito.sender.send_bundle_base64(&[raw_business]).await?;
    }
    Ok::<(), String>(())
};
let (rpc_res, jito_res) = tokio::join!(rpc_fut, jito_fut);
```

---

## 6) SDK：proxy_spread_capture / proxy_jit / remaining_accounts

SDK 侧关键逻辑：
1. `proxy_spread_capture` / `proxy_jit` 接收 `makers` 和 `maker_stats`
2. 通过 `build_remaining_accounts_for_proxy` 构造 remaining_accounts
3. remaining_accounts 包含 **市场、maker user/stats、taker referrer、maker referrer**

```3352:3394:drift-rs/crates/src/lib.rs
pub fn proxy_spread_capture(
    mut self,
    market_index: u16,
    taker_stats: &UserStats,
    makers: &[User],
    maker_stats: &[UserStats],
    revenue_share_authority: Option<Pubkey>,
) -> Self {
    let remaining_accounts = build_remaining_accounts_for_proxy(
        self.program_data,
        self.account_data.as_ref(),
        taker_stats,
        makers,
        maker_stats,
        std::iter::empty(),
        std::iter::once(&MarketId::perp(market_index)),
        revenue_share_authority,
    );
    // ...
}
```

```4068:4101:drift-rs/crates/src/lib.rs
pub fn proxy_jit(
    mut self,
    market_index: u16,
    reference_price: i64,
    edge_ppm: i64,
    taker_stats: &UserStats,
    makers: &[User],
    maker_stats: &[UserStats],
    proxy_program_id: Option<Pubkey>,
) -> Self {
    let remaining_accounts = build_remaining_accounts_for_proxy(
        self.program_data,
        self.account_data.as_ref(),
        taker_stats,
        makers,
        maker_stats,
        std::iter::empty(),
        std::iter::once(&MarketId::perp(market_index)),
        None,
    );
    // ...
}
```

```4445:4549:drift-rs/crates/src/lib.rs
pub fn build_remaining_accounts_for_proxy<'a>(/* ... */) -> Vec<AccountMeta> {
    // 追加 maker user/stats
    for maker in makers { /* ... */ }
    // taker referrer
    if taker_stats.is_referred() { /* ... */ }
    // maker referrers
    for stats in maker_stats { /* ... */ }
}
```

---

## 7) Proxy 程序：JIT / ARB 与推荐人处理

### 7.1 JIT 中的 remaining_accounts 组合
JIT 指令内部会调用 `build_min_remaining_accounts_for_level` 来拼装最低 RA，
该函数会在 **Take/Make** 模式下分别追加 **taker 或 maker 的推荐人**：

```708:752:proxy/jit-proxy/src/instructions/jit.rs
let (agg_ra, _pair, _has_all) = build_min_remaining_accounts_for_level(
    user,
    user_stats,
    remaining_accounts,
    perp_market_map,
    spot_market_map,
    oracle_map,
    market_index,
    quote_spot_index,
    &[],
    JitCpiSide::Take,
)?;
// ... place_and_take_perp_order
```

```255:293:proxy/jit-proxy/src/instructions/new_jit_ra_min.rs
match side {
    JitCpiSide::Take => {
        let my_ref_auth = { taker_user_stats.load()?.referrer };
        if my_ref_auth != Pubkey::default() {
            if let Some((ref_user, ref_stats)) =
                find_referrer_pair_by_authority_in_ra(all_remaining, &my_ref_auth)
            { /* add referrer */ }
        }
    }
    JitCpiSide::Make => {
        // 使用 maker 的 stats 作为 taker stats
        if let Some((_, taker_stats_ai)) = make_taker_pair {
            // ... add referrer for taker
        }
    }
}
```

### 7.2 ARB 指令复用同一 RA 构造
ARB 直接复用 `build_min_remaining_accounts_for_level`，并要求 RA 完整：

```530:565:proxy/jit-proxy/src/instructions/arb_perp.rs
let (ra, make_pair, has_all_maker_accounts) = build_min_remaining_accounts_for_level(
    &ctx.accounts.user,
    &ctx.accounts.user_stats,
    &ctx.remaining_accounts,
    perp_market_map,
    spot_market_map,
    oracle_map,
    plan.target_perp_index,
    plan.quote_spot_index,
    levels,
    cpi_side,
)?;
if !has_all_maker_accounts { return err!(ErrorCode::MissingAccount); }
// place_and_take / place_and_make
```

---

## 8) 总结（核心数据流）

1. **Filler 收集链上/链下数据**：DLOB + swift + pyth + binance  
2. **Swift/ARB/JIT 触发**：分别进入 `try_swift_fill` / `try_onchain_cross` / `try_jit`  
3. **SDK 构造交易**：`proxy_jit` / `proxy_spread_capture` → `build_remaining_accounts_for_proxy`  
4. **推荐人完整性**：taker + all makers referrer 必须被补齐  
5. **Jito 发送**：同一笔交易内追加 tip 指令，Jito bundle 只发送该交易  

---

如需继续补充：配置项、日志枚举、失败场景诊断等，可继续告诉我补充点。  
