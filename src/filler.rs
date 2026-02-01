//! 补单机器人
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io,
    path::Path,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use drift_rs::{
    dlob::{CrossingRegionAll, L3Order, MakerCrosses, DLOB},
    ffi::calculate_auction_price,
    priority_fee_subscriber::PriorityFeeSubscriber,
    slot_subscriber::SlotSubscriber,
    swift_order_subscriber::SwiftOrderStream,
    types::{
        accounts::{User, UserStats},
        CommitmentConfig, MarketId, MarketStatus, MarketType, Order, OrderType, PositionDirection,
        PostOnlyParam, UnsubHandle,
    },
    utils::get_ws_url,
    websocket_program_account_subscriber::{
        WebsocketProgramAccountOptions, WebsocketProgramAccountSubscriber,
    },
    DriftClient, Pubkey,
};
use drift_rs::types::MarketPrecision;
use dashmap::DashMap;
use flate2::Compression;
use futures_util::StreamExt;
use solana_account_decoder_client_types::UiAccountEncoding;

use crate::{
    filler_trades::{try_onchain_cross, try_swift_fill},
    http::Metrics,
    jit::{
        feed_binance::spawn_binance_price_feed,
        feed_dlob_ws::spawn_dlob_l2_feed,
        jit_trades::try_jit,
        DriftL2Update,
        JitMarketState,
        JitStrategy,
    },
    tx_worker::{TxSender, TxWorker},
    util::{OrderSlotLimiter, PythPriceUpdate},
    ws_cache::{sync_stats_accounts_ws, sync_user_accounts_ws, WsAccountCache},
    Config, UseMarkets,
};

pub(crate) const TARGET: &str = "filler";
const CROSS_DEPTH: usize = 3;
const EVENT_WINDOW_MS: u64 = 5;

fn summarize_order_kinds(orders: &[L3Order]) -> String {
    let mut counts = BTreeMap::<String, usize>::new();
    for order in orders {
        let key = format!("{:?}", order.kind);
        *counts.entry(key).or_insert(0) += 1;
    }
    format!("{counts:?}")
}

fn pubkey_to_u32(key: &Pubkey) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.to_bytes().hash(&mut hasher);
    (hasher.finish() & 0xFFFF_FFFF) as u32
}

fn collect_jit_users(makers_bid: &[L3Order], makers_ask: &[L3Order]) -> Vec<Pubkey> {
    let mut users = HashSet::new();
    for order in makers_bid.iter().chain(makers_ask.iter()) {
        users.insert(order.user);
    }
    users.into_iter().collect()
}

#[derive(Clone, Debug, Default)]
struct MidSnapshot {
    binance_mid: i64,
    binance_ts_ms: u64,
    official_mid: i64,
    official_ts_ms: u64,
    dlob_mid: i64,
    dlob_ts_ms: u64,
    basis_ema: f64,
}

const MID_LOG_ROTATE_BYTES: u64 = 50 * 1024 * 1024;

fn rotate_and_compress_log(path: &str) -> io::Result<()> {
    let p = Path::new(path);
    if !p.exists() {
        return Ok(());
    }
    let meta = fs::metadata(p)?;
    if meta.len() < MID_LOG_ROTATE_BYTES {
        return Ok(());
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let rotated = format!("{path}.{ts}");
    fs::rename(p, &rotated)?;
    let gz_path = format!("{rotated}.gz");
    let mut input = fs::File::open(&rotated)?;
    let output = fs::File::create(&gz_path)?;
    let mut encoder = flate2::write::GzEncoder::new(output, Compression::default());
    io::copy(&mut input, &mut encoder)?;
    let _ = encoder.finish()?;
    let _ = fs::remove_file(&rotated);
    Ok(())
}

struct WsSubscriptions {
    user_unsub: UnsubHandle,
    stats_unsub: UnsubHandle,
    slot_subscriber: SlotSubscriber,
}

enum DlobUpdate {
    User {
        pubkey: Pubkey,
        prev_user: Option<User>,
        user: User,
        slot: u64,
    },
    SlotOracle {
        market: MarketId,
        slot: u64,
        oracle_price: u64,
    },
}

struct BusyGuard {
    flag: Arc<AtomicBool>,
}

impl BusyGuard {
    fn new(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

pub struct FillerBot {
    drift: DriftClient,
    dlob: &'static DLOB,
    filler_subaccount: Pubkey,
    jit_subaccount: Pubkey,
    jit_symbols: Vec<(u16, String)>,
    jit_dlob_markets: Vec<(u16, String)>,
    slot_rx: tokio::sync::mpsc::Receiver<u64>,
    book_rx: tokio::sync::mpsc::Receiver<()>,
    swift_order_stream: SwiftOrderStream,
    limiter: OrderSlotLimiter<40>,
    market_ids: Vec<MarketId>,
    config: Config,
    tx_worker_ref: TxSender,
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    pyth_price_feed: tokio::sync::mpsc::Receiver<PythPriceUpdate>,
    user_cache: WsAccountCache,
    dlob_backlog: Arc<AtomicUsize>,
    last_dlob_update_ms: Arc<AtomicU64>,
    _ws_subscriptions: WsSubscriptions,
}

impl FillerBot {
    pub async fn new(config: Config, drift: DriftClient, metrics: Arc<Metrics>) -> Self {
        let dlob: &'static DLOB = Box::leak(Box::new(DLOB::default()));
        let tx_worker = TxWorker::new(
            drift.clone(),
            metrics,
            config.dry,
            config.rpc_skip_preflight,
        );
        let rt = tokio::runtime::Handle::current();
        let tx_worker_ref = tx_worker.run(rt);

        let mut market_ids = match config.use_markets() {
            UseMarkets::All => drift.get_all_perp_market_ids(),
            UseMarkets::Subset(m) => m,
        };
        // 移除 bet 永续市场
        market_ids.retain(|x| {
            let market = drift
                .program_data()
                .perp_market_config_by_index(x.index())
                .unwrap();
            let name = core::str::from_utf8(&market.name)
                .unwrap()
                .to_ascii_lowercase();

            !name.contains("bet") && market.status != MarketStatus::Initialized
        });

        let market_pubkeys: Vec<Pubkey> = market_ids
            .iter()
            .map(|x| {
                drift
                    .program_data()
                    .perp_market_config_by_index(x.index())
                    .unwrap()
                    .pubkey
            })
            .collect();

        let priority_fee_subscriber =
            PriorityFeeSubscriber::new(drift.rpc().url(), &market_pubkeys);
        let priority_fee_subscriber = priority_fee_subscriber.subscribe();

        let filler_subaccount = drift.wallet.sub_account(config.sub_account_id);
        let jit_subaccount = drift.wallet.sub_account(config.jit_sub_account_id);
        let jit_symbols = market_ids
            .iter()
            .filter_map(|m| {
                let market = drift
                    .program_data()
                    .perp_market_config_by_index(m.index())?;
                let name = core::str::from_utf8(&market.name).ok()?;
                let symbol = binance_symbol_from_name(name)?;
                Some((m.index(), symbol))
            })
            .collect();
        let jit_dlob_markets = market_ids
            .iter()
            .filter_map(|m| {
                let market = drift
                    .program_data()
                    .perp_market_config_by_index(m.index())?;
                let name = core::str::from_utf8(&market.name).ok()?;
                let dlob_market = dlob_market_from_name(name)?;
                Some((m.index(), dlob_market))
            })
            .collect();

        log::info!(target: TARGET, "subscribing swift orders");
        let swift_order_stream = drift
            .subscribe_swift_orders(&market_ids, Some(true), None, None)
            .await
            .expect("subscribed swift orders");
        log::info!(target: TARGET, "subscribed swift orders");

        drift.subscribe_blockhashes().await.expect("subscribed");
        let (slot_rx, book_rx, user_cache, ws_subscriptions, dlob_backlog, last_dlob_update_ms) =
            setup_ws(drift.clone(), dlob, market_ids.clone()).await;
        log::info!(target: TARGET, "subscribed ws");
        if let Err(err) = user_cache
            .get_user_or_fetch(&drift, &filler_subaccount)
            .await
        {
            log::warn!(target: TARGET, "failed to warm filler account: {err:?}");
        }
        if let Err(err) = user_cache.get_user_or_fetch(&drift, &jit_subaccount).await {
            log::warn!(target: TARGET, "failed to warm jit account: {err:?}");
        }

        // pyth 订阅
        let pyth_access_token = std::env::var("PYTH_LAZER_TOKEN").expect("pyth access token");
        let pyth_feed_cli = pyth_lazer_client::LazerClient::new(
            "wss://pyth-lazer.dourolabs.app/v1/stream",
            pyth_access_token.as_str(),
        )
        .expect("pyth price feed connects");
        let pyth_price_feed = crate::util::subscribe_price_feeds(pyth_feed_cli, &market_ids, &[]);
        log::info!(target: TARGET, "subscribed pyth price feeds");

        FillerBot {
            drift,
            dlob,
            filler_subaccount,
            jit_subaccount,
            jit_symbols,
            jit_dlob_markets,
            slot_rx,
            book_rx,
            swift_order_stream,
            limiter: OrderSlotLimiter::new(),
            market_ids,
            config,
            tx_worker_ref,
            priority_fee_subscriber,
            pyth_price_feed,
            user_cache,
            dlob_backlog,
            last_dlob_update_ms,
            _ws_subscriptions: ws_subscriptions,
        }
    }

    pub async fn run(self) {
        let mut swift_order_stream = Some(self.swift_order_stream);
        let mut swift_reconnect_task: Option<tokio::task::JoinHandle<SwiftOrderStream>> = None;
        let mut slot_rx = self.slot_rx;
        let mut book_rx = self.book_rx;
        let onchain_limiter = Arc::new(tokio::sync::Mutex::new(self.limiter));
        let mut jit_limiters =
            BTreeMap::<u16, Arc<tokio::sync::Mutex<OrderSlotLimiter<40>>>>::new();
        let drift: &'static DriftClient = Box::leak(Box::new(self.drift));
        let dlob = self.dlob;
        let market_ids = self.market_ids;
        let filler_subaccount = self.filler_subaccount;
        let jit_subaccount = self.jit_subaccount;
        let jit_symbols = self.jit_symbols;
        let jit_dlob_markets = self.jit_dlob_markets;
        let config = self.config.clone();
        let tx_worker_ref = self.tx_worker_ref.clone();
        let priority_fee_subscriber = Arc::clone(&self.priority_fee_subscriber);
        let user_cache = self.user_cache.clone();
        let mut slot = 0;
        let mut use_median_trigger_price = drift
            .state_account()
            .map(|s| s.has_median_trigger_price_feature())
            .unwrap_or(false);
        let mut pyth_price_feed = Some(self.pyth_price_feed);
        let mut pyth_reconnect_task: Option<
            tokio::task::JoinHandle<tokio::sync::mpsc::Receiver<PythPriceUpdate>>,
        > = None;
        let mut pyth_oracle_prices = BTreeMap::<u16, PythPriceUpdate>::new();
        let start_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let jit_strategy = Arc::new(JitStrategy::new(
            config.jit_edge_ppm,
            config.jit_cooldown_ms,
            config.jit_price_stale_ms,
            config.jit_max_makers_per_side,
            start_time_ms,
            config.jit_warmup_ms,
        ));
        let mut jit_price_cache = BTreeMap::<u16, (i64, u64)>::new();
        let mut jit_states = BTreeMap::<u16, Arc<tokio::sync::Mutex<JitMarketState>>>::new();
        let mut arb_busy = BTreeMap::<u16, Arc<AtomicBool>>::new();
        let mut jit_busy = BTreeMap::<u16, Arc<AtomicBool>>::new();
        let mut arb_skip_counts = BTreeMap::<u16, u64>::new();
        let mut jit_skip_counts = BTreeMap::<u16, u64>::new();
        for market in &market_ids {
            let market_index = market.index();
            jit_states.insert(
                market_index,
                Arc::new(tokio::sync::Mutex::new(JitMarketState::default())),
            );
            jit_limiters.insert(
                market_index,
                Arc::new(tokio::sync::Mutex::new(OrderSlotLimiter::new())),
            );
            arb_busy.insert(market_index, Arc::new(AtomicBool::new(false)));
            jit_busy.insert(market_index, Arc::new(AtomicBool::new(false)));
            arb_skip_counts.insert(market_index, 0);
            jit_skip_counts.insert(market_index, 0);
        }
        let jit_proxy_program_id = if config.jit_proxy_program_id.trim().is_empty() {
            None
        } else {
            match Pubkey::from_str(config.jit_proxy_program_id.trim()) {
                Ok(pk) => Some(pk),
                Err(err) => {
                    log::warn!(target: TARGET, "invalid jit proxy program id: {err:?}");
                    None
                }
            }
        };
        let jit_symbols_len = jit_symbols.len();
        let jit_symbols_log = jit_symbols
            .iter()
            .map(|(mi, s)| (*mi, s.clone()))
            .collect::<Vec<_>>();
        let mut binance_feed = if !jit_symbols.is_empty() {
            Some(spawn_binance_price_feed(jit_symbols))
        } else {
            None
        };
        log::info!(
            target: TARGET,
            "jit feed init: symbols={}, binance_enabled={}",
            jit_symbols_len,
            binance_feed.is_some()
        );
        log::info!(
            target: TARGET,
            "jit symbols: {:?}",
            jit_symbols_log
        );
        log::info!(
            target: TARGET,
            "jit official l2 feed: enabled={}, url={}",
            !config.dlob_l2_ws_url.trim().is_empty(),
            config.dlob_l2_ws_url
        );
        let mut official_l2_feed = if !config.dlob_l2_ws_url.trim().is_empty()
            && !jit_dlob_markets.is_empty()
        {
            Some(spawn_dlob_l2_feed(
                jit_dlob_markets.clone(),
                config.dlob_l2_ws_url.clone(),
            ))
        } else {
            None
        };
        let mut official_l2_cache = BTreeMap::<u16, (i64, u64)>::new();
        let mid_snapshots = Arc::new(DashMap::<u16, MidSnapshot>::new());
        let mid_log_path = config.jit_mid_log_path.trim().to_string();
        if !mid_log_path.is_empty() {
            let market_ids = market_ids.clone();
            let mid_snapshots = Arc::clone(&mid_snapshots);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tick.tick().await;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap();
                    let ts_secs = now.as_secs();
                    let mut lines = String::new();
                    for market in market_ids.iter() {
                        let market_index = market.index();
                        let snap = mid_snapshots
                            .get(&market_index)
                            .map(|s| s.clone())
                            .unwrap_or_default();
                        lines.push_str(&format!(
                            "{} m={} b={} o={} d={} e={:.6}\n",
                            ts_secs,
                            market_index,
                            snap.binance_mid,
                            snap.official_mid,
                            snap.dlob_mid,
                            snap.basis_ema
                        ));
                    }
                    if lines.is_empty() {
                        continue;
                    }
                    let path = mid_log_path.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = rotate_and_compress_log(&path);
                        if let Ok(mut file) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                        {
                            let _ = std::io::Write::write_all(&mut file, lines.as_bytes());
                        }
                    })
                    .await;
                }
            });
        }
        let mut last_book_event_ms: u64 = 0;
        let mut last_binance_event_ms = BTreeMap::<u16, u64>::new();
        let mut last_jit_check_ms: u64 = 0;
        let mut jit_idle_tick = tokio::time::interval(std::time::Duration::from_secs(60));
        let mut diag_tick = tokio::time::interval(Duration::from_secs(5));
        let dlob_backlog = Arc::clone(&self.dlob_backlog);
        let last_dlob_update_ms = Arc::clone(&self.last_dlob_update_ms);
        for market in &market_ids {
            last_binance_event_ms.insert(market.index(), 0);
        }
        let mut official_l2_reconnect_task: Option<
            tokio::task::JoinHandle<tokio::sync::mpsc::Receiver<DriftL2Update>>,
        > = None;

        loop {
            tokio::select! {
                biased;
                swift_order = async {
                    match swift_order_stream.as_mut() {
                        Some(stream) => stream.next().await,
                        None => None,
                    }
                }, if swift_order_stream.is_some() => {
                    match swift_order {
                        Some(signed_order) => {
                            // 尝试用 swift 订单吃掉挂单流动性
                            let mut order_params = signed_order.order_params();
                            log::info!(target: TARGET, "new swift order. uuid={}, market={}", signed_order.order_uuid_str(), order_params.market_index);
                            log::debug!(target: TARGET, "details: {signed_order:?}");
                            let perp_market = drift.try_get_perp_market_account(order_params.market_index).unwrap();
                            let oracle_price_data = drift.try_get_mmoracle_for_perp_market(order_params.market_index, slot).expect("got oracle price");
                            let oracle_price = oracle_price_data.price;
                            log::trace!(target: TARGET, "oracle price: slot:{:?},market:{:?},price:{:?}", slot, order_params.market_index, oracle_price);
                            order_params.update_perp_auction_params(
                                &perp_market,
                                oracle_price,
                                true,
                            );
                            log::debug!("updated order params");
                            let (start_price, end_price, duration) = (order_params.auction_start_price.unwrap_or_default(), order_params.auction_end_price.unwrap_or_default(), order_params.auction_duration.unwrap_or_default());
                            let order = Order {
                                slot: slot + 1,
                                price: order_params.price,
                                base_asset_amount: order_params.base_asset_amount,
                                trigger_price: order_params.trigger_price.unwrap_or_default(),
                                auction_duration: duration,
                                auction_start_price: start_price,
                                auction_end_price: end_price,
                                max_ts: order_params.max_ts.unwrap_or_default(),
                                oracle_price_offset: order_params.oracle_price_offset.unwrap_or_default(),
                                market_index: order_params.market_index,
                                order_type: order_params.order_type,
                                market_type: order_params.market_type,
                                direction: order_params.direction,
                                reduce_only: order_params.reduce_only,
                                post_only: order_params.post_only != PostOnlyParam::None,
                                immediate_or_cancel: order_params.immediate_or_cancel(),
                                trigger_condition: order_params.trigger_condition,
                                bit_flags: order_params.bit_flags,
                                ..Default::default()
                            };

                            let vamm_price = if order_params.direction == PositionDirection::Long {
                                perp_market.ask_price(None)
                            } else {
                                perp_market.bid_price(None)
                            };

                            let price = match order_params.order_type {
                                OrderType::Market | OrderType::Oracle => {
                                    match calculate_auction_price(&order, slot + 1, perp_market.price_tick(), Some(oracle_price), false) {
                                        Ok(p) => p,
                                        Err(err) => {
                                            log::warn!(target: TARGET, "could not get auction price {err:?}, params: {order_params:?}, skipping...");
                                            continue;
                                        }
                                    }
                                }
                                OrderType::Limit => {
                                    match order.get_limit_price(Some(oracle_price), Some(vamm_price), slot + 1, perp_market.price_tick(), false, None) {
                                        Ok(Some(p)) => p,
                                        _ => {
                                            log::warn!(target: TARGET, "could not get limit price: {order_params:?}, skipping...");
                                            continue;
                                        },
                                    }
                                }
                                _ => {
                                    log::warn!(target: TARGET, "invalid swift order type");
                                    unreachable!();
                                }
                            };
                            let vamm_min_order = perp_market.amm.min_order_size;
                            let has_vamm_cross = order_params.base_asset_amount > vamm_min_order
                                && match order_params.direction {
                                    PositionDirection::Long => price > vamm_price,
                                    PositionDirection::Short => price < vamm_price,
                                };
                            let unix_now = std::time::SystemTime::now()
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_secs() as i64;
                            let trigger_price = perp_market
                                .get_trigger_price(oracle_price as i64, unix_now, use_median_trigger_price)
                                .unwrap_or(oracle_price as u64);
                            let crosses = collect_swift_crosses(
                                dlob,
                                order_params.market_index,
                                MarketType::Perp,
                                oracle_price as u64,
                                &perp_market,
                                trigger_price,
                                order_params.direction,
                                price,
                                CROSS_DEPTH,
                            );
                            let crosses = filter_swift_crosses_reduce_only(
                                crosses,
                                order_params.market_index,
                                &user_cache,
                            );
                            if !crosses.is_empty() || has_vamm_cross {
                                let maker_crosses = MakerCrosses {
                                    has_vamm_cross,
                                    orders: crosses
                                        .into_iter()
                                        .map(|order| (order.clone(), order.size))
                                        .collect(),
                                    slot: slot + 1,
                                    is_partial: false,
                                    taker_direction: order_params.direction,
                                };
                                log::info!(target: TARGET, "found swift cross. crosses={maker_crosses:?}");
                                let pf = scale_priority_fee(priority_fee_subscriber.priority_fee_nth(0.3));
                                try_swift_fill(
                                    drift,
                                    pf,
                                    config.swift_cu_limit,
                                    filler_subaccount,
                                    signed_order,
                                    maker_crosses,
                                    trigger_price,
                                    &user_cache,
                                    tx_worker_ref.clone(),
                                ).await;
                            }
                        }
                        None => {
                            log::warn!(target: TARGET, "swift order stream finished; scheduling reconnect");
                            swift_order_stream = None;
                            if swift_reconnect_task.is_none() {
                                swift_reconnect_task = Some(spawn_swift_reconnect(drift, market_ids.clone()));
                            }
                        }
                    }
                }
                _ = jit_idle_tick.tick() => {
                    if binance_feed.is_some() {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        let last_update_ms = last_binance_event_ms
                            .values()
                            .copied()
                            .max()
                            .unwrap_or(0);
                        if last_update_ms == 0 || now_ms.saturating_sub(last_update_ms) >= 60_000 {
                            log::warn!(
                                target: TARGET,
                                "jit binance idle: last_update_ms={}, now_ms={}",
                                last_update_ms,
                                now_ms
                            );
                        }
                    }
                }
                _ = diag_tick.tick() => {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    let backlog = dlob_backlog.load(Ordering::Relaxed);
                    let dlob_age_ms = now_ms.saturating_sub(last_dlob_update_ms.load(Ordering::Relaxed));
                    let skipped_arb: u64 = arb_skip_counts.values().copied().sum();
                    let skipped_jit: u64 = jit_skip_counts.values().copied().sum();
                    log::info!(
                        target: TARGET,
                        "diag: dlob_backlog={}, dlob_age_ms={}, arb_skips={}, jit_skips={}",
                        backlog,
                        dlob_age_ms,
                        skipped_arb,
                        skipped_jit
                    );
                }
                swift_reconnect = async {
                    if let Some(task) = swift_reconnect_task.as_mut() {
                        Some(task.await)
                    } else {
                        None
                    }
                }, if swift_reconnect_task.is_some() => {
                    match swift_reconnect {
                        Some(Ok(stream)) => {
                            swift_order_stream = Some(stream);
                            swift_reconnect_task = None;
                            log::info!(target: TARGET, "swift order stream reconnected");
                        }
                        Some(Err(err)) => {
                            swift_reconnect_task = Some(spawn_swift_reconnect(drift, market_ids.clone()));
                            log::warn!(target: TARGET, "swift reconnect task failed: {err:?}");
                        }
                        None => {}
                    }
                }
                new_slot = slot_rx.recv() => {
                    slot = new_slot.expect("got slot update");
                    if slot % 300 == 0 {
                        use_median_trigger_price = drift
                            .state_account()
                            .map(|s| s.feature_bit_flags & 0b0000_0010 != 0) // FeatureBitFlags::MedianTriggerPrice 功能位
                            .unwrap_or(false);
                    }
                }
                book_update = book_rx.recv() => {
                    if book_update.is_some() {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        if now_ms.saturating_sub(last_book_event_ms) < EVENT_WINDOW_MS {
                            continue;
                        }
                        last_book_event_ms = now_ms;

                        let t0 = std::time::SystemTime::now();
                        let unix_now = t0
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_secs() as i64;
                        let priority_fee =
                            scale_priority_fee(priority_fee_subscriber.priority_fee_nth(0.5));
                        let pyth_snapshot = Arc::new(pyth_oracle_prices.clone());

                        for market in &market_ids {
                            let market_index = market.index();
                            let busy_flag = match arb_busy.get(&market_index) {
                                Some(flag) => Arc::clone(flag),
                                None => continue,
                            };
                            if busy_flag.swap(true, Ordering::AcqRel) {
                                if let Some(count) = arb_skip_counts.get_mut(&market_index) {
                                    *count += 1;
                                }
                                log::debug!(
                                    target: TARGET,
                                    "arb skip: busy market={}, slot={}",
                                    market_index,
                                    slot
                                );
                                continue;
                            }

                            let drift = drift;
                            let dlob = dlob;
                            let user_cache = user_cache.clone();
                            let tx_worker_ref = tx_worker_ref.clone();
                            let onchain_limiter = Arc::clone(&onchain_limiter);
                            let pyth_snapshot = Arc::clone(&pyth_snapshot);
                            let busy_guard = BusyGuard::new(busy_flag);
                            let config = config.clone();
                            let filler_subaccount = filler_subaccount;
                            let use_median_trigger_price = use_median_trigger_price;
                            let slot = slot;
                            let priority_fee = priority_fee;
                            let unix_now = unix_now;

                            tokio::spawn(async move {
                                let _guard = busy_guard;
                                let task_start = Instant::now();
                                let perp_market =
                                    match drift.try_get_perp_market_account(market_index) {
                                        Ok(m) => m,
                                        Err(_) => return,
                                    };
                                let chain_oracle_data = match drift
                                    .try_get_mmoracle_for_perp_market(market_index, slot)
                                {
                                    Ok(v) => v,
                                    Err(_) => return,
                                };
                                let mut oracle_price = chain_oracle_data.price as u64;
                                let trigger_price = perp_market
                                    .get_trigger_price(
                                        oracle_price as i64,
                                        unix_now,
                                        use_median_trigger_price,
                                    )
                                    .unwrap_or(oracle_price);
                                let mut pyth_update = None;
                                if let Some(p) = pyth_snapshot.get(&market_index) {
                                    if oracle_price != p.price {
                                        oracle_price = p.price;
                                        pyth_update = Some(p.clone());
                                    }
                                }

                                let crosses = dlob.find_crossing_region_all_types(
                                    oracle_price,
                                    market_index,
                                    MarketType::Perp,
                                    Some(&perp_market),
                                    trigger_price,
                                    CROSS_DEPTH,
                                );
                                if let Some(crosses) = crosses {
                                    let crosses = match filter_crosses_reduce_only(
                                        crosses,
                                        market_index,
                                        &user_cache,
                                        &perp_market,
                                        oracle_price,
                                    ) {
                                        Some(crosses) => crosses,
                                        None => return,
                                    };
                                    let (allow_bid, allow_ask) = {
                                        let mut limiter = onchain_limiter.lock().await;
                                        (
                                            limiter.allow_event(slot, crosses.best_bid.order_id),
                                            limiter.allow_event(slot, crosses.best_ask.order_id),
                                        )
                                    };
                                    if allow_bid || allow_ask {
                                        log::info!(
                                            target: TARGET,
                                            "event onchain cross. market: {},{crosses:?}",
                                            market_index
                                        );
                                        try_onchain_cross(
                                            drift,
                                            priority_fee,
                                            config.fill_cu_limit,
                                            market_index,
                                            filler_subaccount,
                                            crosses,
                                            &user_cache,
                                            tx_worker_ref.clone(),
                                            pyth_update,
                                            trigger_price,
                                        )
                                        .await;
                                    }
                                }

                            // jit 只由 binance 更新触发，这里不再触发

                                let elapsed_ms = task_start.elapsed().as_millis() as u64;
                                if elapsed_ms > 50 {
                                    log::warn!(
                                        target: TARGET,
                                        "market task slow: market={}, elapsed_ms={}",
                                        market_index,
                                        elapsed_ms
                                    );
                                }
                            });
                        }
                        let duration = std::time::SystemTime::now()
                            .duration_since(t0)
                            .unwrap()
                            .as_millis();
                        log::trace!(target: TARGET, "⏱️ event check at {slot}: {:?}ms", duration);
                    }
                }
                new_price = async {
                    match pyth_price_feed.as_mut() {
                        Some(feed) => feed.recv().await,
                        None => None,
                    }
                }, if pyth_price_feed.is_some() => {
                    match new_price {
                        Some(update) => {
                            let market_id = update.market_id;
                            pyth_oracle_prices.insert(market_id, update);
                            // jit 仅由 binance 更新触发，pyth 更新不再触发 jit
                        }
                        None => {
                            log::warn!(target: TARGET, "pyth price feed disconnected; scheduling reconnect");
                            pyth_price_feed = None;
                            if pyth_reconnect_task.is_none() {
                                pyth_reconnect_task = Some(spawn_pyth_reconnect(market_ids.clone()));
                            }
                        }
                    }
                }
                pyth_reconnect = async {
                    if let Some(task) = pyth_reconnect_task.as_mut() {
                        Some(task.await)
                    } else {
                        None
                    }
                }, if pyth_reconnect_task.is_some() => {
                    match pyth_reconnect {
                        Some(Ok(feed)) => {
                            pyth_price_feed = Some(feed);
                            pyth_reconnect_task = None;
                            log::info!(target: TARGET, "pyth price feed reconnected");
                        }
                        Some(Err(err)) => {
                            pyth_reconnect_task = Some(spawn_pyth_reconnect(market_ids.clone()));
                            log::warn!(target: TARGET, "pyth reconnect task failed: {err:?}");
                        }
                        None => {}
                    }
                }
                official_l2_update = async {
                    match official_l2_feed.as_mut() {
                        Some(feed) => feed.recv().await,
                        None => None,
                    }
                }, if official_l2_feed.is_some() => {
                    match official_l2_update {
                        Some(update) => {
                            official_l2_cache.insert(update.market_index, (update.mid, update.ts_ms));
                            let mut snap = mid_snapshots.entry(update.market_index).or_default();
                            snap.official_mid = update.mid;
                            snap.official_ts_ms = update.ts_ms;
                        }
                        None => {
                            log::warn!(target: TARGET, "jit official l2 feed ended");
                            official_l2_feed = None;
                            if official_l2_reconnect_task.is_none()
                                && !config.dlob_l2_ws_url.trim().is_empty()
                                && !jit_dlob_markets.is_empty()
                            {
                                let url = config.dlob_l2_ws_url.clone();
                                let markets = jit_dlob_markets.clone();
                                official_l2_reconnect_task = Some(tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(1)).await;
                                    spawn_dlob_l2_feed(markets, url)
                                }));
                            }
                        }
                    }
                }
                official_l2_reconnect = async {
                    if let Some(task) = official_l2_reconnect_task.as_mut() {
                        Some(task.await)
                    } else {
                        None
                    }
                }, if official_l2_reconnect_task.is_some() => {
                    match official_l2_reconnect {
                        Some(Ok(feed)) => {
                            official_l2_feed = Some(feed);
                            official_l2_reconnect_task = None;
                            log::info!(target: TARGET, "jit official l2 feed reconnected");
                        }
                        Some(Err(err)) => {
                            official_l2_reconnect_task = None;
                            log::warn!(target: TARGET, "jit official l2 reconnect failed: {err:?}");
                        }
                        None => {}
                    }
                }
                binance_update = async {
                    match binance_feed.as_mut() {
                        Some(feed) => feed.recv().await,
                        None => None,
                    }
                }, if binance_feed.is_some() => {
                    match binance_update {
                        Some(update) => {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                            let last_ms = *last_binance_event_ms.get(&update.market_index).unwrap_or(&0);
                            if now_ms.saturating_sub(last_ms) < EVENT_WINDOW_MS {
                                continue;
                            }
                            last_binance_event_ms.insert(update.market_index, now_ms);

                            jit_price_cache.insert(
                                update.market_index,
                                (update.binance_mid, update.ts_ms),
                            );
                            {
                                let mut snap = mid_snapshots.entry(update.market_index).or_default();
                                snap.binance_mid = update.binance_mid;
                                snap.binance_ts_ms = update.ts_ms;
                            }
                            log::debug!(
                                target: TARGET,
                                "jit price update (binance): market={}, price={}, ts_ms={}",
                                update.market_index,
                                update.binance_mid,
                                update.ts_ms
                            );
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                            let market_index = update.market_index;
                            if now_ms.saturating_sub(last_jit_check_ms) >= 60_000 {
                                last_jit_check_ms = now_ms;
                                log::info!(
                                    target: TARGET,
                                    "jit check: market={}, binance_mid={}, ts_ms={}",
                                    market_index,
                                    update.binance_mid,
                                    update.ts_ms
                                );
                            }

                            if let Some(jit_state) = jit_states.get(&market_index).map(Arc::clone) {
                                if let Some((mid, ts_ms)) = official_l2_cache.get(&market_index).copied() {
                                    if now_ms.saturating_sub(ts_ms) <= config.dlob_l2_stale_ms
                                        && update.ts_ms.abs_diff(ts_ms) <= 1000
                                    {
                                        let mut state = jit_state.lock().await;
                                        jit_strategy.update_basis_ema(
                                            &mut *state,
                                            update.binance_mid,
                                            mid,
                                            now_ms,
                                        );
                                        let mut snap = mid_snapshots.entry(market_index).or_default();
                                        snap.basis_ema = state.basis_ema;
                                    }
                                }
                            }

                            let busy_flag = match jit_busy.get(&market_index) {
                                Some(flag) => Arc::clone(flag),
                                None => continue,
                            };
                            if busy_flag.swap(true, Ordering::AcqRel) {
                                if let Some(count) = jit_skip_counts.get_mut(&market_index) {
                                    *count += 1;
                                }
                                log::debug!(
                                    target: TARGET,
                                    "jit skip: busy market={}, slot={}",
                                    market_index,
                                    slot
                                );
                                continue;
                            }

                            let drift = drift;
                            let dlob = dlob;
                            let user_cache = user_cache.clone();
                            let tx_worker_ref = tx_worker_ref.clone();
                            let jit_limiter = jit_limiters.get(&market_index).map(Arc::clone);
                            let jit_state = jit_states.get(&market_index).map(Arc::clone);
                            let jit_strategy = Arc::clone(&jit_strategy);
                            let priority_fee_subscriber = Arc::clone(&priority_fee_subscriber);
                            let busy_guard = BusyGuard::new(busy_flag);
                            let config = config.clone();
                            let jit_proxy_program_id = jit_proxy_program_id;
                            let jit_subaccount = jit_subaccount;
                            let use_median_trigger_price = use_median_trigger_price;
                            let slot = slot;
                            let dlob_update_ms = last_dlob_update_ms.load(Ordering::Relaxed);
                            let binance_mid = update.binance_mid;
                            let binance_ts = update.ts_ms;
                            let official_mid = official_l2_cache.get(&market_index).copied();
                            let mid_snapshots = Arc::clone(&mid_snapshots);

                            tokio::spawn(async move {
                                let _guard = busy_guard;
                                if let (Ok(perp_market), Ok(oracle_price_data)) = (
                                    drift.try_get_perp_market_account(market_index),
                                    drift.try_get_mmoracle_for_perp_market(market_index, slot),
                                ) {
                                    let oracle_price = oracle_price_data.price as u64;
                                    let unix_now = std::time::SystemTime::now()
                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs() as i64;
                                    let trigger_price = perp_market
                                        .get_trigger_price(
                                            oracle_price as i64,
                                            unix_now,
                                            use_median_trigger_price,
                                        )
                                        .unwrap_or(oracle_price);
                                    if let (Some(jit_limiter), Some(jit_state)) =
                                        (jit_limiter, jit_state)
                                    {
                                        let mut state = jit_state.lock().await;
                                        let prev_dlob_ts = state.last_dlob_ts_ms;
                                        let intent_opt = jit_strategy.maybe_intent(
                                            &mut *state,
                                            market_index,
                                            binance_mid,
                                            binance_ts,
                                            now_ms,
                                            dlob,
                                            &perp_market,
                                            oracle_price,
                                            trigger_price,
                                            &user_cache,
                                            dlob_update_ms,
                                            official_mid,
                                            config.dlob_l2_stale_ms,
                                        );
                                        if state.last_dlob_ts_ms != prev_dlob_ts {
                                            let mut snap = mid_snapshots.entry(market_index).or_default();
                                            snap.dlob_mid = state.last_dlob_mid;
                                            snap.dlob_ts_ms = state.last_dlob_ts_ms;
                                        }
                                        {
                                            let mut snap = mid_snapshots.entry(market_index).or_default();
                                            snap.basis_ema = state.basis_ema;
                                        }
                                        drop(state);

                                        if let Some(intent) = intent_opt {
                                            let (official_mid, official_age_ms) = match official_mid {
                                                Some((mid, ts_ms)) => {
                                                    (mid, now_ms.saturating_sub(ts_ms))
                                                }
                                                None => (0, u64::MAX),
                                            };
                                            let users = collect_jit_users(
                                                &intent.makers_bid,
                                                &intent.makers_ask,
                                            );
                                            let mut limiter = jit_limiter.lock().await;
                                            let blocked = users
                                                .iter()
                                                .filter(|user| {
                                                    !limiter.would_allow(slot, pubkey_to_u32(user))
                                                })
                                                .count();
                                            if blocked > 0 {
                                                log::debug!(
                                                    target: TARGET,
                                                    "jit skip: rate limited market={}, slot={}, blocked_users={}",
                                                    market_index,
                                                    slot,
                                                    blocked
                                                );
                                                return;
                                            }
                                            for user in &users {
                                                limiter.allow_event(slot, pubkey_to_u32(user));
                                            }
                                            let makers_bid_kinds =
                                                summarize_order_kinds(&intent.makers_bid);
                                            let makers_ask_kinds =
                                                summarize_order_kinds(&intent.makers_ask);
                                            log::info!(
                                                target: TARGET,
                                                "jit trigger (binance): market={}, ref_px={}, edge_ppm={}, best_bid={}, best_ask={}, drift_mid={}, official_mid={}, official_age_ms={}, binance_mid={}, basis_ema={:.4}, spread={:.4}, oracle_px={}, trigger_px={}, dlob_slot={}, makers_bid={}, makers_ask={}, makers_bid_kinds={}, makers_ask_kinds={}",
                                                market_index,
                                                intent.reference_price,
                                                intent.edge_ppm,
                                                intent.best_bid_price,
                                                intent.best_ask_price,
                                                intent.drift_mid,
                                                official_mid,
                                                official_age_ms,
                                                intent.binance_mid,
                                                intent.basis_ema,
                                                intent.spread,
                                                intent.oracle_price,
                                                intent.trigger_price,
                                                intent.dlob_slot,
                                                intent.makers_bid.len(),
                                                intent.makers_ask.len(),
                                                makers_bid_kinds,
                                                makers_ask_kinds,
                                            );
                                            let pf = scale_priority_fee(
                                                priority_fee_subscriber.priority_fee_nth(0.5),
                                            );
                                            try_jit(
                                                drift,
                                                pf,
                                                config.jit_cu_limit,
                                                jit_subaccount,
                                                &intent,
                                                &user_cache,
                                                tx_worker_ref.clone(),
                                                jit_proxy_program_id,
                                            )
                                            .await;
                                        }
                                    }
                                }
                            });
                        }
                        None => {
                            log::warn!(target: TARGET, "binance price feed disconnected");
                            binance_feed = None;
                        }
                    }
                }
            }
        }
        if let Err(err) = drift.unsubscribe().await {
            log::warn!(target: TARGET, "ws unsubscribe failed: {err:?}");
        }
        drift.grpc_unsubscribe();
        log::info!(target: TARGET, "filler shutting down...");
    }
}

fn collect_swift_crosses(
    dlob: &DLOB,
    market_index: u16,
    market_type: MarketType,
    oracle_price: u64,
    perp_market: &drift_rs::types::accounts::PerpMarket,
    trigger_price: u64,
    direction: PositionDirection,
    taker_price: u64,
    depth: usize,
) -> Vec<L3Order> {
    let book = dlob.get_l3_snapshot(market_index, market_type);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let effective_price = |order: &L3Order| -> u64 {
        if let Some(price) = order.post_trigger_price(book.slot, oracle_price, perp_market) {
            return price;
        }
        if order.price == 0 {
            let dir = if order.is_long() {
                PositionDirection::Long
            } else {
                PositionDirection::Short
            };
            if let Ok(vamm_price) = perp_market.fallback_price(
                dir,
                oracle_price as i64,
                order.max_ts.saturating_sub(now) as i64,
            ) {
                return vamm_price;
            }
        }
        order.price
    };
    let mut crosses = Vec::new();

    match direction {
        PositionDirection::Long => {
            for ask in book.asks(Some(oracle_price), Some(perp_market), Some(trigger_price)) {
                if effective_price(ask) > taker_price {
                    break;
                }
                crosses.push(ask.clone());
                if crosses.len() >= depth {
                    break;
                }
            }
        }
        PositionDirection::Short => {
            for bid in book.bids(Some(oracle_price), Some(perp_market), Some(trigger_price)) {
                if effective_price(bid) < taker_price {
                    break;
                }
                crosses.push(bid.clone());
                if crosses.len() >= depth {
                    break;
                }
            }
        }
    }

    crosses
}

pub(crate) fn is_invalid_reduce_only(
    order: &L3Order,
    market_index: u16,
    user_cache: &WsAccountCache,
) -> bool {
    if !order.is_reduce_only() {
        return false;
    }

    let user = match user_cache.get_user(&order.user) {
        Some(user) => user,
        None => return false,
    };
    let base_asset_amount = user
        .perp_positions
        .iter()
        .find(|pos| pos.market_index == market_index)
        .map(|pos| pos.base_asset_amount)
        .unwrap_or(0);

    if base_asset_amount == 0 {
        return true;
    }
    if base_asset_amount > 0 && order.is_long() {
        return true;
    }
    if base_asset_amount < 0 && !order.is_long() {
        return true;
    }

    false
}

fn filter_swift_crosses_reduce_only(
    crosses: Vec<L3Order>,
    market_index: u16,
    user_cache: &WsAccountCache,
) -> Vec<L3Order> {
    crosses
        .into_iter()
        .filter(|order| !is_invalid_reduce_only(order, market_index, user_cache))
        .collect()
}

fn filter_crosses_reduce_only(
    crosses: CrossingRegionAll,
    market_index: u16,
    user_cache: &WsAccountCache,
    perp_market: &drift_rs::types::accounts::PerpMarket,
    oracle_price: u64,
) -> Option<CrossingRegionAll> {
    let crossing_bids: Vec<L3Order> = crosses
        .crossing_bids
        .into_iter()
        .filter(|order| !is_invalid_reduce_only(order, market_index, user_cache))
        .collect();
    let crossing_asks: Vec<L3Order> = crosses
        .crossing_asks
        .into_iter()
        .filter(|order| !is_invalid_reduce_only(order, market_index, user_cache))
        .collect();

    if crossing_bids.is_empty() || crossing_asks.is_empty() {
        return None;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let auction_slot = crosses.slot.saturating_add(1);
    let effective_price = |order: &L3Order, is_bid: bool| -> u64 {
        if let Some(price) = order.post_trigger_price(auction_slot, oracle_price, perp_market) {
            return price;
        }
        if order.price == 0 {
            let dir = if is_bid {
                PositionDirection::Long
            } else {
                PositionDirection::Short
            };
            if let Ok(vamm_price) = perp_market.fallback_price(
                dir,
                oracle_price as i64,
                order.max_ts.saturating_sub(now) as i64,
            ) {
                return vamm_price;
            }
        }
        order.price
    };

    let best_bid = crossing_bids[0].clone();
    let best_ask = crossing_asks[0].clone();
    let best_bid_price = effective_price(&best_bid, true);
    let best_ask_price = effective_price(&best_ask, false);

    if best_bid_price < best_ask_price {
        return None;
    }

    let crossing_bids: Vec<L3Order> = crossing_bids
        .into_iter()
        .filter(|b| effective_price(b, true) >= best_ask_price)
        .collect();
    let crossing_asks: Vec<L3Order> = crossing_asks
        .into_iter()
        .filter(|a| effective_price(a, false) <= best_bid_price)
        .collect();

    if crossing_bids.is_empty() || crossing_asks.is_empty() {
        return None;
    }

    Some(CrossingRegionAll {
        slot: crosses.slot,
        best_bid,
        best_ask,
        crossing_bids,
        crossing_asks,
    })
}

fn scale_priority_fee(fee: u64) -> u64 {
    fee.saturating_mul(10)
}

fn binance_symbol_from_name(name: &str) -> Option<String> {
    let mut s = name.trim_matches('\0').trim().to_ascii_uppercase();
    if s.is_empty() {
        return None;
    }
    if let Some(stripped) = s.strip_suffix("-PERP") {
        s = stripped.trim().to_string();
    } else if let Some(stripped) = s.strip_suffix("PERP") {
        s = stripped.trim().to_string();
    }
    if s.is_empty() {
        return None;
    }
    Some(format!("{s}USDC"))
}

fn dlob_market_from_name(name: &str) -> Option<String> {
    let mut s = name.trim_matches('\0').trim().to_ascii_uppercase();
    if s.is_empty() {
        return None;
    }
    if !s.contains("-") {
        s = format!("{s}-PERP");
    }
    Some(s)
}

fn spawn_swift_reconnect(
    drift: &'static DriftClient,
    market_ids: Vec<MarketId>,
) -> tokio::task::JoinHandle<SwiftOrderStream> {
    tokio::spawn(async move {
        let mut backoff_secs = 1u64;
        loop {
            match drift
                .subscribe_swift_orders(&market_ids, Some(true), None, None)
                .await
            {
                Ok(stream) => {
                    return stream;
                }
                Err(err) => {
                    log::warn!(
                        target: TARGET,
                        "swift resubscribe failed: {err:?}, retry in {backoff_secs}s"
                    );
                }
            }

            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    })
}

fn spawn_pyth_reconnect(
    market_ids: Vec<MarketId>,
) -> tokio::task::JoinHandle<tokio::sync::mpsc::Receiver<PythPriceUpdate>> {
    tokio::spawn(async move {
        let mut backoff_secs = 1u64;
        loop {
            let pyth_access_token = match std::env::var("PYTH_LAZER_TOKEN") {
                Ok(token) => token,
                Err(err) => {
                    log::warn!(
                        target: TARGET,
                        "pyth token missing: {err:?}, retry in {backoff_secs}s"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(30);
                    continue;
                }
            };

            match pyth_lazer_client::LazerClient::new(
                "wss://pyth-lazer.dourolabs.app/v1/stream",
                pyth_access_token.as_str(),
            ) {
                Ok(cli) => {
                    let feed = crate::util::subscribe_price_feeds(cli, &market_ids, &[]);
                    return feed;
                }
                Err(err) => {
                    log::warn!(
                        target: TARGET,
                        "pyth reconnect failed: {err:?}, retry in {backoff_secs}s"
                    );
                }
            }

            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    })
}

/// 设置 WS 订阅
///
/// 同步 User 订单与 UserStat 账户
pub async fn setup_ws(
    drift: DriftClient,
    dlob: &'static DLOB,
    market_ids: Vec<MarketId>,
) -> (
    tokio::sync::mpsc::Receiver<u64>,
    tokio::sync::mpsc::Receiver<()>,
    WsAccountCache,
    WsSubscriptions,
    Arc<AtomicUsize>,
    Arc<AtomicU64>,
) {
    let dlob_notifier = dlob.spawn_notifier();
    let user_cache = WsAccountCache::new();

    let _ = tokio::try_join!(
        sync_stats_accounts_ws(&drift, &user_cache),
        sync_user_accounts_ws(&drift, &dlob_notifier, &user_cache),
    );

    if let Err(err) = drift.subscribe_markets(&market_ids).await {
        log::warn!(target: TARGET, "market ws subscribe failed: {err:?}");
    }
    if let Err(err) = drift.subscribe_oracles(&market_ids).await {
        log::warn!(target: TARGET, "oracle ws subscribe failed: {err:?}");
    }

    let (slot_tx, slot_rx) = tokio::sync::mpsc::channel(64);
    let (book_tx, book_rx) = tokio::sync::mpsc::channel(1024);
    let (dlob_tx, mut dlob_rx) = tokio::sync::mpsc::unbounded_channel::<DlobUpdate>();
    let dlob_backlog = Arc::new(AtomicUsize::new(0));
    let last_dlob_update_ms = Arc::new(AtomicU64::new(0));

    {
        let dlob_notifier = dlob_notifier.clone();
        let dlob_backlog = Arc::clone(&dlob_backlog);
        let last_dlob_update_ms = Arc::clone(&last_dlob_update_ms);
        tokio::spawn(async move {
            while let Some(update) = dlob_rx.recv().await {
                let t0 = Instant::now();
                let kind = match &update {
                    DlobUpdate::User { .. } => "user",
                    DlobUpdate::SlotOracle { .. } => "slot_oracle",
                };
                match update {
                    DlobUpdate::User {
                        pubkey,
                        prev_user,
                        user,
                        slot,
                    } => {
                        match prev_user.as_ref() {
                            Some(prev) => dlob_notifier.user_update(pubkey, Some(prev), &user, slot),
                            None => dlob_notifier.user_update(pubkey, None, &user, slot),
                        }
                    }
                    DlobUpdate::SlotOracle {
                        market,
                        slot,
                        oracle_price,
                    } => {
                        dlob_notifier.slot_and_oracle_update(market, slot, oracle_price);
                    }
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                last_dlob_update_ms.store(now_ms, Ordering::Relaxed);
                dlob_backlog.fetch_sub(1, Ordering::Relaxed);
                let elapsed_ms = t0.elapsed().as_millis() as u64;
                if elapsed_ms > 25 {
                    log::warn!(
                        target: TARGET,
                        "dlob update slow: kind={}, elapsed_ms={}",
                        kind,
                        elapsed_ms
                    );
                }
            }
            log::warn!(target: TARGET, "dlob update task ended");
        });
    }

    let ws_url = get_ws_url(drift.rpc().url().as_str()).expect("ws url");
    let commitment = CommitmentConfig::processed();
    let user_subscriber = WebsocketProgramAccountSubscriber::new(
        ws_url.clone(),
        WebsocketProgramAccountOptions {
            filters: vec![
                drift_rs::memcmp::get_user_filter(),
                drift_rs::memcmp::get_non_idle_user_filter(),
            ],
            commitment,
            encoding: UiAccountEncoding::Base64Zstd,
        },
    );
    let user_unsub = user_subscriber.subscribe::<User, _>("filler-user", {
        let user_cache = user_cache.clone();
        let dlob_tx = dlob_tx.clone();
        let dlob_backlog = Arc::clone(&dlob_backlog);
        let book_tx = book_tx.clone();
        move |update| {
            let pubkey = match Pubkey::from_str(update.pubkey.as_str()) {
                Ok(pubkey) => pubkey,
                Err(err) => {
                    log::warn!(target: TARGET, "invalid user pubkey: {err:?}");
                    return;
                }
            };
            if let Some(prev_user) = user_cache.apply_user_update_and_get_prev(
                pubkey,
                update.data_and_slot.data,
                update.data_and_slot.slot,
            ) {
                dlob_backlog.fetch_add(1, Ordering::Relaxed);
                if dlob_tx
                    .send(DlobUpdate::User {
                        pubkey,
                        prev_user,
                        user: update.data_and_slot.data,
                        slot: update.data_and_slot.slot,
                    })
                    .is_err()
                {
                    dlob_backlog.fetch_sub(1, Ordering::Relaxed);
                    log::error!(target: TARGET, "dlob update channel closed");
                }
            }
            let _ = book_tx.try_send(());
        }
    });

    let stats_subscriber = WebsocketProgramAccountSubscriber::new(
        ws_url,
        WebsocketProgramAccountOptions {
            filters: vec![drift_rs::memcmp::get_user_stats_filter()],
            commitment,
            encoding: UiAccountEncoding::Base64Zstd,
        },
    );
    let stats_unsub = stats_subscriber.subscribe::<UserStats, _>("filler-user-stats", {
        let user_cache = user_cache.clone();
        move |update| {
            let pubkey = match Pubkey::from_str(update.pubkey.as_str()) {
                Ok(pubkey) => pubkey,
                Err(err) => {
                    log::warn!(target: TARGET, "invalid stats pubkey: {err:?}");
                    return;
                }
            };
            user_cache.upsert_stats(pubkey, update.data_and_slot.data, update.data_and_slot.slot);
        }
    });

    let mut slot_subscriber = SlotSubscriber::new(drift.ws());
    if let Err(err) = slot_subscriber.subscribe({
        let drift = drift.clone();
        let dlob_tx = dlob_tx.clone();
        let dlob_backlog = Arc::clone(&dlob_backlog);
        let slot_tx = slot_tx.clone();
        let book_tx = book_tx.clone();
        let market_ids = market_ids.clone();
        move |update| {
            let new_slot = update.latest_slot;
            for market in market_ids.iter() {
                match drift.try_get_mmoracle_for_perp_market(market.index(), new_slot) {
                    Ok(oracle_price_data) => {
                        dlob_backlog.fetch_add(1, Ordering::Relaxed);
                        if dlob_tx
                            .send(DlobUpdate::SlotOracle {
                                market: *market,
                                slot: new_slot,
                                oracle_price: oracle_price_data.price as u64,
                            })
                            .is_err()
                        {
                            dlob_backlog.fetch_sub(1, Ordering::Relaxed);
                            log::error!(target: TARGET, "dlob update channel closed");
                        }
                    }
                    Err(err) => {
                        log::debug!(
                            target: TARGET,
                            "oracle price unavailable: market={}, err={err:?}",
                            market.index()
                        );
                    }
                }
            }
            if slot_tx.try_send(new_slot).is_err() {
                log::debug!(target: TARGET, "slot channel full; drop slot={new_slot}");
            }
            let _ = book_tx.try_send(());
        }
    }) {
        log::warn!(target: TARGET, "slot ws subscribe failed: {err:?}");
    }

    (
        slot_rx,
        book_rx,
        user_cache,
        WsSubscriptions {
            user_unsub,
            stats_unsub,
            slot_subscriber,
        },
        dlob_backlog,
        last_dlob_update_ms,
    )
}
