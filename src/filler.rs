//! 补单机器人
use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use drift_rs::{
    dlob::{CrossesAndTopMakers, CrossingRegion, MakerCrosses, OrderKind, TakerOrder, DLOB},
    event_subscriber::DriftEvent,
    ffi::calculate_auction_price,
    math::constants::{BASE_PRECISION_U64, QUOTE_PRECISION_U64},
    priority_fee_subscriber::PriorityFeeSubscriber,
    slot_subscriber::SlotSubscriber,
    swift_order_subscriber::{SignedOrderInfo, SwiftOrderStream},
    types::{
        accounts::{User, UserStats},
        CommitmentConfig, MarketId, MarketPrecision, MarketStatus, MarketType, Order,
        OrderTriggerCondition, OrderType, PositionDirection, PostOnlyParam,
        RpcSendTransactionConfig, UnsubHandle, VersionedMessage, AMM,
    },
    utils::get_ws_url,
    websocket_program_account_subscriber::{
        WebsocketProgramAccountOptions, WebsocketProgramAccountSubscriber,
    },
    DriftClient, Pubkey, TransactionBuilder, Wallet,
};
use futures_util::StreamExt;
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_rpc_client_api::config::RpcTransactionConfig;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction, signature::Signature, transaction::TransactionError,
};
use solana_transaction_status_client_types::UiTransactionEncoding;
use tokio::{runtime::Handle, sync::RwLock};

use crate::{
    http::Metrics,
    util::{OrderSlotLimiter, PendingTxMeta, PendingTxs, PythPriceUpdate, TxIntent},
    ws_cache::{sync_stats_accounts_ws, sync_user_accounts_ws, WsAccountCache},
    Config, UseMarkets,
};

const TARGET: &str = "filler";
const MIN_NOTIONAL_USD: u64 = 100;
const MAKER_FEE_BPS: u64 = 1;
const BPS_DENOMINATOR: u64 = 10_000;

struct WsSubscriptions {
    user_unsub: UnsubHandle,
    stats_unsub: UnsubHandle,
    slot_subscriber: SlotSubscriber,
}

pub struct FillerBot {
    drift: DriftClient,
    dlob: &'static DLOB,
    filler_subaccount: Pubkey,
    slot_rx: tokio::sync::mpsc::Receiver<u64>,
    swift_order_stream: SwiftOrderStream,
    limiter: OrderSlotLimiter<40>,
    market_ids: Vec<MarketId>,
    config: Config,
    tx_worker_ref: TxSender,
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    pyth_price_feed: tokio::sync::mpsc::Receiver<PythPriceUpdate>,
    user_cache: WsAccountCache,
    _ws_subscriptions: WsSubscriptions,
}

impl FillerBot {
    pub async fn new(config: Config, drift: DriftClient, metrics: Arc<Metrics>) -> Self {
        let dlob: &'static DLOB = Box::leak(Box::new(DLOB::default()));
        let tx_worker = TxWorker::new(drift.clone(), metrics, config.dry);
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

        log::info!(target: TARGET, "subscribing swift orders");
        let swift_order_stream = drift
            .subscribe_swift_orders(&market_ids, Some(true), None, None)
            .await
            .expect("subscribed swift orders");
        log::info!(target: TARGET, "subscribed swift orders");

        drift.subscribe_blockhashes().await.expect("subscribed");
        let (slot_rx, user_cache, ws_subscriptions) =
            setup_ws(drift.clone(), dlob, market_ids.clone()).await;
        log::info!(target: TARGET, "subscribed ws");
        if let Err(err) = user_cache
            .get_user_or_fetch(&drift, &filler_subaccount)
            .await
        {
            log::warn!(target: TARGET, "failed to warm filler account: {err:?}");
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
            slot_rx,
            swift_order_stream,
            limiter: OrderSlotLimiter::new(),
            market_ids,
            config,
            tx_worker_ref,
            priority_fee_subscriber,
            pyth_price_feed,
            user_cache,
            _ws_subscriptions: ws_subscriptions,
        }
    }

    pub async fn run(self) {
        let mut swift_order_stream = Some(self.swift_order_stream);
        let mut swift_reconnect_task: Option<tokio::task::JoinHandle<SwiftOrderStream>> = None;
        let mut slot_rx = self.slot_rx;
        let mut limiter = self.limiter;
        let drift: &'static DriftClient = Box::leak(Box::new(self.drift));
        let dlob = self.dlob;
        let market_ids = self.market_ids;
        let filler_subaccount = self.filler_subaccount;
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
                            let taker_order = TakerOrder::from_order_params(order_params, price);
                            let crosses = dlob.find_crosses_for_taker_order(slot + 1, oracle_price as u64, taker_order, Some(&perp_market), None);
                            if !crosses.is_empty() {
                                let maker_price = crosses
                                    .orders
                                    .first()
                                    .map(|(order, _)| order.price)
                                    .or_else(|| {
                                        if crosses.has_vamm_cross {
                                            Some(vamm_price)
                                        } else {
                                            None
                                        }
                                    });
                                let maker_post_only = crosses
                                    .orders
                                    .first()
                                    .map(|(order, _)| order.is_post_only())
                                    .unwrap_or(false);
                                let maker_count = u64::from(maker_post_only || crosses.has_vamm_cross);
                                let fillable_base = calc_fillable_base(taker_order.size, &crosses);
                                if let Some(maker_price) = maker_price {
                                    let buy_price = maker_price.min(taker_order.price);
                                    let sell_price = maker_price.max(taker_order.price);
                                    if cross_meets_thresholds(
                                        buy_price,
                                        sell_price,
                                        fillable_base,
                                        maker_count,
                                    ) {
                                        log::info!(target: TARGET, "found resting cross. crosses={crosses:?}");
                                        let pf = priority_fee_subscriber.priority_fee_nth(0.3);
                                        try_swift_fill(
                                            drift,
                                            pf,
                                            config.swift_cu_limit,
                                            filler_subaccount,
                                            signed_order,
                                            crosses,
                                            &user_cache,
                                            tx_worker_ref.clone(),
                                        ).await;
                                    }
                                }
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

                    let priority_fee = priority_fee_subscriber.priority_fee_nth(0.5) + slot % 2; // 增加随机性，避免连续重提导致交易哈希重复
                    let t0 = std::time::SystemTime::now();
                    let unix_now = t0.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;

                    // 检查所有市场的拍卖/限价可成交
                    for market in &market_ids {
                        let market_index = market.index();

                        let perp_market = drift.try_get_perp_market_account(market_index).expect("got perp market");
                        let chain_oracle_data = drift.try_get_mmoracle_for_perp_market(market_index, slot).expect("got oracle price");
                        log::debug!(target: "oracle", "oracle price: delay:{:?},market:{:?},oracle:{:?},amm:{:?}", chain_oracle_data.delay, market, chain_oracle_data.price, perp_market.amm.mm_oracle_price);
                        let mut oracle_price = chain_oracle_data.price as u64;
                        let trigger_price = perp_market.get_trigger_price(oracle_price as i64, unix_now, use_median_trigger_price).unwrap_or(oracle_price);
                        let mut pyth_update = None;
                        if let Some(p) = pyth_oracle_prices.get(&market_index) {
                            if oracle_price != p.price {
                                oracle_price = p.price;
                                pyth_update = Some(p.clone());
                            }
                        }

                        let mut crosses_and_top_makers = dlob.find_crosses_for_auctions(
                            market_index,
                            MarketType::Perp,
                            slot,
                            oracle_price,
                            Some(&perp_market),
                            trigger_price,
                            None,
                        );
                        crosses_and_top_makers.crosses.retain(|(o, _)| limiter.allow_event(slot, o.order_id));
                        crosses_and_top_makers.crosses.retain(|(taker_order, maker_crosses)| {
                            let vamm_price = if taker_order.is_long() {
                                perp_market.ask_price(None)
                            } else {
                                perp_market.bid_price(None)
                            };
                            let maker_price = maker_crosses
                                .orders
                                .first()
                                .map(|(order, _)| order.price)
                                .or_else(|| {
                                    if maker_crosses.has_vamm_cross {
                                        Some(vamm_price)
                                    } else {
                                        None
                                    }
                                });
                            let maker_post_only = maker_crosses
                                .orders
                                .first()
                                .map(|(order, _)| order.is_post_only())
                                .unwrap_or(false);
                            let maker_count =
                                u64::from(maker_post_only || maker_crosses.has_vamm_cross);
                            let fillable_base =
                                calc_fillable_base(taker_order.size, maker_crosses);
                            let maker_price = match maker_price {
                                Some(price) => price,
                                None => return false,
                            };
                            let buy_price = maker_price.min(taker_order.price);
                            let sell_price = maker_price.max(taker_order.price);
                            cross_meets_thresholds(
                                buy_price,
                                sell_price,
                                fillable_base,
                                maker_count,
                            )
                        });

                        if !crosses_and_top_makers.crosses.is_empty() {
                            log::info!(target: TARGET, "found auction crosses. market: {},{crosses_and_top_makers:?}", market.index());
                            try_auction_fill(
                                drift,
                                priority_fee,
                                config.fill_cu_limit,
                                market_index,
                                filler_subaccount,
                                crosses_and_top_makers,
                                &user_cache,
                                tx_worker_ref.clone(),
                                pyth_update,
                                trigger_price,
                                move |maker_cross| {
                                    perp_market.has_too_much_drawdown() && amm_wants_to_jit_make(&perp_market.amm, maker_cross.taker_direction)
                                },
                            ).await;
                        }

                        // 简易限速
                        if slot % 2 == 0 {
                            if let Some(crosses) = dlob.find_crossing_region(oracle_price, market_index, MarketType::Perp, Some(&perp_market)) {
                                log::info!(target: TARGET, "found limit crosses (market: {market_index})");
                                try_uncross(drift, slot + 1, priority_fee, config.fill_cu_limit, market_index, filler_subaccount, crosses, &user_cache, &tx_worker_ref);
                            }
                        }

                        // 大约每分钟检查一次状态配置
                        if slot % 300 == 0 {
                            use_median_trigger_price = drift
                            .state_account()
                            .map(|s| s.feature_bit_flags & 0b0000_0010 != 0) // FeatureBitFlags::MedianTriggerPrice 功能位
                            .unwrap_or(false);
                        }
                    }
                    let duration = std::time::SystemTime::now().duration_since(t0).unwrap().as_millis();
                    log::trace!(target: TARGET, "⏱️ checked fills at {slot}: {:?}ms", duration);
                }
                new_price = async {
                    match pyth_price_feed.as_mut() {
                        Some(feed) => feed.recv().await,
                        None => None,
                    }
                }, if pyth_price_feed.is_some() => {
                    match new_price {
                        Some(update) => {
                            pyth_oracle_prices.insert(update.market_id, update);
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
            }
        }
        if let Err(err) = drift.unsubscribe().await {
            log::warn!(target: TARGET, "ws unsubscribe failed: {err:?}");
        }
        drift.grpc_unsubscribe();
        log::info!(target: TARGET, "filler shutting down...");
    }
}

fn calc_fillable_base(taker_size: u64, maker_crosses: &MakerCrosses) -> u64 {
    let maker_total: u64 = maker_crosses.orders.iter().map(|(_, size)| *size).sum();
    if maker_total == 0 {
        taker_size
    } else {
        taker_size.min(maker_total)
    }
}

fn calc_cross_bps(buy_price: u64, sell_price: u64) -> u64 {
    if buy_price == 0 || sell_price <= buy_price {
        return 0;
    }
    let spread = sell_price - buy_price;
    ((spread as u128) * (BPS_DENOMINATOR as u128) / (buy_price as u128)) as u64
}

fn cross_meets_thresholds(buy_price: u64, sell_price: u64, base: u64, maker_count: u64) -> bool {
    if buy_price == 0 || base == 0 {
        return false;
    }
    let notional_quote =
        (base as u128) * (buy_price as u128) / (BASE_PRECISION_U64 as u128);
    let min_notional_quote = (MIN_NOTIONAL_USD as u128) * (QUOTE_PRECISION_U64 as u128);
    if notional_quote < min_notional_quote {
        return false;
    }
    let cross_bps = calc_cross_bps(buy_price, sell_price);
    let fee_bps = maker_count.saturating_mul(MAKER_FEE_BPS);
    cross_bps.saturating_sub(fee_bps) > 0
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
    WsAccountCache,
    WsSubscriptions,
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
        let dlob_notifier = dlob_notifier.clone();
        move |update| {
            let pubkey = match Pubkey::from_str(update.pubkey.as_str()) {
                Ok(pubkey) => pubkey,
                Err(err) => {
                    log::warn!(target: TARGET, "invalid user pubkey: {err:?}");
                    return;
                }
            };
            user_cache.apply_user_update(
                pubkey,
                update.data_and_slot.data,
                update.data_and_slot.slot,
                Some(&dlob_notifier),
            );
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
        let dlob_notifier = dlob_notifier.clone();
        let slot_tx = slot_tx.clone();
        let market_ids = market_ids.clone();
        move |update| {
            let new_slot = update.latest_slot;
            for market in market_ids.iter() {
                match drift.try_get_mmoracle_for_perp_market(market.index(), new_slot) {
                    Ok(oracle_price_data) => {
                        dlob_notifier.slot_and_oracle_update(
                            *market,
                            new_slot,
                            oracle_price_data.price as u64,
                        );
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
        }
    }) {
        log::warn!(target: TARGET, "slot ws subscribe failed: {err:?}");
    }

    (
        slot_rx,
        user_cache,
        WsSubscriptions {
            user_unsub,
            stats_unsub,
            slot_subscriber,
        },
    )
}

/// 尝试成交一笔 swift 订单
async fn try_swift_fill(
    drift: &'static DriftClient,
    priority_fee: u64,
    cu_limit: u32,
    filler_subaccount: Pubkey,
    swift_order: SignedOrderInfo,
    crosses: MakerCrosses,
    user_cache: &WsAccountCache,
    tx_worker_ref: TxSender,
) {
    log::info!(target: TARGET, "try fill swift order: {}", swift_order.order_uuid_str());
    let taker_order = swift_order.order_params();
    let taker_subaccount = swift_order.taker_subaccount();
    let taker_authority = swift_order.taker_authority;

    let taker_stats = Wallet::derive_stats_account(&taker_authority);
    let filler_account_data = match user_cache
        .get_user_or_fetch(drift, &filler_subaccount)
        .await
    {
        Ok(user) => user,
        Err(err) => {
            log::warn!(target: TARGET, "missing filler account: {err:?}");
            return;
        }
    };
    let (taker_account_data, taker_stats) = match tokio::try_join!(
        user_cache.get_user_or_fetch(drift, &taker_subaccount),
        user_cache.get_stats_or_fetch(drift, &taker_stats)
    ) {
        Ok(res) => res,
        Err(err) => {
            log::warn!(target: TARGET, "missing taker data: {err:?}");
            return;
        }
    };
    let tx_builder = TransactionBuilder::new(
        drift.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    );

    let maker_accounts: Vec<User> = crosses
        .orders
        .iter()
        .filter(|m| m.0.user != taker_subaccount) // 不能自己成交自己
        .filter_map(|(m, _fill_size)| user_cache.get_user(&m.user))
        .collect();

    if maker_accounts.is_empty() && !crosses.has_vamm_cross {
        log::warn!("invalid cross: {crosses:?}");
        return;
    }

    // taker_order_id = taker_account_data.next_order_id;
    let mut tx_builder = tx_builder
        .with_priority_fee(priority_fee, Some(cu_limit))
        .place_swift_order(&swift_order, &taker_account_data)
        .fill_perp_order(
            taker_order.market_index,
            taker_subaccount,
            &taker_account_data,
            &taker_stats,
            None, // Some(taker_order_id), // 假设足够快，认为就是 taker_order_id，对零售用户可接受
            maker_accounts.as_slice(),
            Some(swift_order.has_builder()),
        );

    // 账户列表较大，提高 CU 上限补偿
    if let Some(ix) = tx_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            tx_builder = tx_builder.set_ix(
                1,
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit * 2),
            );
        }
    }
    let tx = tx_builder.build();

    tx_worker_ref.send_tx(
        tx,
        TxIntent::SwiftFill {
            maker_crosses: crosses,
        },
        cu_limit as u64,
    );
}

/// 尝试成交一笔拍卖订单
///
/// - `auction_crosses` 为一个或多个可成交撮合
async fn try_auction_fill(
    drift: &'static DriftClient,
    priority_fee: u64,
    cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    auction_crosses: CrossesAndTopMakers,
    user_cache: &WsAccountCache,
    tx_worker_ref: TxSender,
    oracle_update: Option<PythPriceUpdate>,
    trigger_price: u64,
    is_vamm_inactive: impl Fn(&MakerCrosses) -> bool,
) {
    let filler_account_data = match user_cache
        .get_user_or_fetch(drift, &filler_subaccount)
        .await
    {
        Ok(user) => user,
        Err(err) => {
            log::warn!(target: TARGET, "missing filler account: {err:?}");
            return;
        }
    };

    let top_maker_asks: Vec<User> = auction_crosses
        .top_maker_asks
        .iter()
        .filter_map(|m| user_cache.get_user(m))
        .collect();

    let top_maker_bids: Vec<User> = auction_crosses
        .top_maker_bids
        .iter()
        .filter_map(|m| user_cache.get_user(m))
        .collect();
    let mut sent_oracle_update = false;
    for (taker_order, crosses) in auction_crosses.crosses {
        log::info!(target: TARGET, "try fill auction order: {taker_order:?}");
        let taker_subaccount = taker_order.user;

        let taker_account_data = match user_cache.get_user_or_fetch(drift, &taker_subaccount).await
        {
            Ok(user) => user,
            Err(err) => {
                log::warn!(target: TARGET, "missing taker account: {err:?}");
                continue;
            }
        };

        let taker_stats_pubkey = Wallet::derive_stats_account(&taker_account_data.authority);
        let taker_stats = match user_cache
            .get_stats_or_fetch(drift, &taker_stats_pubkey)
            .await
        {
            Ok(stats) => stats,
            Err(err) => {
                log::warn!(target: TARGET, "failed to fetch taker stats: {err:?}");
                continue;
            }
        };

        let mut tx_builder = TransactionBuilder::new(
            drift.program_data(),
            filler_subaccount,
            std::borrow::Cow::Borrowed(&filler_account_data),
            false,
        );

        tx_builder = tx_builder.with_priority_fee(priority_fee, Some(cu_limit));

        if let Some(ref update_msg) = oracle_update {
            if !sent_oracle_update {
                tx_builder = tx_builder
                    .post_pyth_lazer_oracle_update(&[update_msg.feed_id], &update_msg.message);
                sent_oracle_update = true;
            }
        }

        let taker_is_trigger = matches!(
            taker_order.kind,
            OrderKind::TriggerMarket | OrderKind::TriggerLimit
        );
        if taker_is_trigger {
            let actual_order = taker_account_data
                .orders
                .iter()
                .find(|o| o.order_id == taker_order.order_id)
                .expect("trigger order exists");

            let trigger_above = matches!(
                actual_order.trigger_condition,
                OrderTriggerCondition::Above | OrderTriggerCondition::TriggeredAbove
            );

            let can_trigger = if trigger_above && trigger_price > actual_order.trigger_price {
                true
            } else if !trigger_above && trigger_price < actual_order.trigger_price {
                true
            } else {
                false
            };
            if !can_trigger {
                return;
            }
            log::info!(
                target: TARGET,
                "attempting trigger and fill: trigger_price={trigger_price}, order_price={}, {:?}/{:?}",
                actual_order.trigger_price,
                taker_order.order_id,
                taker_order.user
            );
            tx_builder = tx_builder.trigger_order(
                taker_subaccount,
                &taker_account_data,
                taker_order.order_id,
                (market_index, MarketType::Perp),
            );
        }

        let mut maker_accounts: Vec<User> = crosses
            .orders
            .iter()
            .filter(|m| m.0.user != taker_subaccount) // 不能自己成交自己
            .filter_map(|(m, _fill_size)| user_cache.get_user(&m.user))
            .collect();

        if crosses.has_vamm_cross && is_vamm_inactive(&crosses) {
            log::debug!(target: TARGET, "skip inactive vamm cross: {crosses:?}");
            return;
        }
        if !crosses.has_vamm_cross && maker_accounts.is_empty() {
            log::debug!(target: TARGET, "skip empty maker cross: {crosses:?}");
            return;
        }

        if maker_accounts.len() < 3 {
            if crosses.taker_direction == PositionDirection::Long {
                maker_accounts = top_maker_asks.clone();
            } else {
                maker_accounts = top_maker_bids.clone();
            }
        }

        tx_builder = tx_builder.fill_perp_order(
            market_index,
            taker_subaccount,
            &taker_account_data,
            &taker_stats,
            Some(taker_order.order_id),
            maker_accounts.as_slice(),
            None,
        );

        // 账户列表较大，提高 CU 上限补偿
        if let Some(ix) = tx_builder.ixs().last() {
            if ix.accounts.len() >= 20 {
                tx_builder = tx_builder.set_ix(
                    1,
                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit * 2),
                );
            }
        }

        let tx = tx_builder.build();

        tx_worker_ref.send_tx(
            tx,
            TxIntent::AuctionFill {
                taker_order_id: taker_order.order_id,
                maker_crosses: crosses,
                has_trigger: taker_is_trigger,
            },
            cu_limit as u64,
        );
    }
}

/// 尝试解撮合盘口顶部
///
/// - `crosses` 为一个或多个可成交撮合
fn try_uncross(
    drift: &DriftClient,
    slot: u64,
    priority_fee: u64,
    cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    crosses: CrossingRegion,
    user_cache: &WsAccountCache,
    tx_worker_ref: &TxSender,
) {
    let filler_account_data = match user_cache.get_user(&filler_subaccount) {
        Some(user) => user,
        None => {
            log::warn!(target: TARGET, "missing filler account for uncross");
            return;
        }
    };

    let best_bid = &crosses.crossing_bids.first();
    let best_ask = &crosses.crossing_asks.first();

    if best_bid.is_none() || best_ask.is_none() {
        return;
    }

    let best_bid = best_bid.unwrap();
    let best_ask = best_ask.unwrap();

    let buy_price = best_bid.price.min(best_ask.price);
    let sell_price = best_bid.price.max(best_ask.price);
    let fillable_base = best_bid.size.min(best_ask.size);
    let maker_count = u64::from(best_bid.is_post_only()) + u64::from(best_ask.is_post_only());
    if !cross_meets_thresholds(buy_price, sell_price, fillable_base, maker_count) {
        return;
    }

    let maker_asks: Vec<User> = crosses
        .crossing_asks
        .iter()
        .take(5)
        .filter_map(|x| {
            let maker = x.user;
            if maker != best_bid.user {
                user_cache.get_user(&maker)
            } else {
                None
            }
        })
        .collect();

    let maker_bids: Vec<User> = crosses
        .crossing_bids
        .iter()
        .take(5)
        .filter_map(|x| {
            let maker = x.user;
            if maker != best_ask.user {
                user_cache.get_user(&maker)
            } else {
                None
            }
        })
        .collect();

    log::info!(target: TARGET, "try uncross book={market_index},slot={slot}");
    log::info!(
        target: TARGET,
        "X asks: {:?}, X bids: {:?}",
        crosses.crossing_asks,
        crosses.crossing_bids
    );

    // 用所有交叉的挂单尝试组合合法的 taker/maker
    for (taker_order, makers) in [(best_ask, maker_bids), (best_bid, maker_asks)] {
        if taker_order.is_post_only() {
            continue;
        }

        let taker_order_id = taker_order.order_id;
        let taker_subaccount = taker_order.user;
        let taker_account_data = match user_cache.get_user(&taker_subaccount) {
            Some(user) => user,
            None => {
                log::warn!(target: TARGET, "missing taker account for uncross");
                continue;
            }
        };

        let taker_stats_pubkey = Wallet::derive_stats_account(&taker_account_data.authority);
        let taker_stats = match user_cache.get_stats(&taker_stats_pubkey) {
            Some(stats) => stats,
            None => {
                log::warn!(
                    target: TARGET,
                    "failed to fetch taker stats: {:?}",
                    taker_account_data.authority
                );
            continue;
            }
        };

        let mut tx_builder = TransactionBuilder::new(
            drift.program_data(),
            filler_subaccount,
            std::borrow::Cow::Borrowed(&filler_account_data),
            false,
        );
        tx_builder = tx_builder
            .with_priority_fee(priority_fee, Some(cu_limit))
            .fill_perp_order(
                market_index,
                taker_subaccount,
                &taker_account_data,
                &taker_stats,
                Some(taker_order_id),
                makers.as_slice(),
                None,
            );

        // 账户列表较大，提高 CU 上限补偿
        if let Some(ix) = tx_builder.ixs().last() {
            if ix.accounts.len() >= 40 {
                tx_builder = tx_builder.set_ix(
                    1,
                    ComputeBudgetInstruction::set_compute_unit_limit((cu_limit * 25) / 10),
                );
            }
        }
        let tx = tx_builder.build();

        tx_worker_ref.send_tx(
            tx,
            TxIntent::LimitUncross {
                slot,
                market_index,
                taker_order_id,
                maker_order_id: 0,
            },
            cu_limit as u64,
        );
    }
}

fn amm_wants_to_jit_make(amm: &AMM, taker_direction: PositionDirection) -> bool {
    let amm_wants_to_jit_make = match taker_direction {
        PositionDirection::Long => {
            amm.base_asset_amount_with_amm.as_i128() < -(amm.order_step_size as i128)
        }
        PositionDirection::Short => {
            amm.base_asset_amount_with_amm.as_i128() > amm.order_step_size as i128
        }
    };
    amm_wants_to_jit_make && amm.amm_jit_intensity > 0
}

pub enum TxWork {
    Send {
        tx: VersionedMessage,
        ts: u64,
        intent: TxIntent,
        cu_limit: u64,
    },
    Confirm {
        tx: Signature,
        ts: u64,
    },
}

pub struct TxWorker {
    drift: &'static DriftClient,
    pending_txs: Arc<RwLock<PendingTxs<1024>>>,
    metrics: Arc<Metrics>,
    dry_run: bool,
}

impl TxWorker {
    pub fn new(drift: DriftClient, metrics: Arc<Metrics>, dry_run: bool) -> Self {
        Self {
            drift: Box::leak(Box::new(drift)),
            pending_txs: Arc::new(RwLock::new(PendingTxs::new())),
            metrics,
            dry_run,
        }
    }
    pub fn run(self, rt: tokio::runtime::Handle) -> TxSender {
        let (tx, rx) = crossbeam::channel::bounded(1024);
        std::thread::spawn(move || {
            let _ = env_logger::try_init();
            while let Ok(work) = rx.recv() {
                match work {
                    TxWork::Send {
                        tx,
                        ts: _,
                        intent,
                        cu_limit,
                    } => {
                        if self.dry_run {
                            log::debug!(target: TARGET, "skip tx dry run: {intent:?}");
                            continue;
                        }
                        self.send_tx(&rt, tx, intent, cu_limit);
                    }
                    TxWork::Confirm { tx, ts: _ } => {
                        self.confirm_tx(&rt, tx);
                    }
                }
            }
        });
        TxSender(tx)
    }
    fn send_tx(&self, rt: &Handle, tx: VersionedMessage, intent: TxIntent, cu_limit: u64) {
        log::debug!(target: TARGET, "txworker send tx: {intent:?}");
        let drift = self.drift;
        let pending_txs = Arc::clone(&self.pending_txs);
        let metrics = self.metrics.clone();
        let intent_label = intent.label();
        metrics.tx_sent.with_label_values(&[intent_label]).inc();
        metrics
            .fill_expected
            .with_label_values(&[intent_label])
            .inc();
        if intent.expected_trigger() {
            metrics.trigger_expected.inc();
        }

        rt.spawn(async move {
            match drift
                .sign_and_send_with_config(
                    tx,
                    None,
                    RpcSendTransactionConfig {
                        skip_preflight: true,
                        max_retries: Some(0),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(sig) => {
                    log::info!(
                        target: TARGET,
                        r#"{{"intent": "{}", "txn": "{}", "observed_slot": {}}}"#,
                        intent_label,
                        sig,
                        intent.slot().unwrap_or(0)
                    );
                    let mut pending = pending_txs.write().await;
                    pending.insert(PendingTxMeta::new(sig, intent, cu_limit));
                }
                Err(err) => {
                    log::info!(target: TARGET, "fill failed 🐢: {err}");
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "send_error"])
                        .inc();
                }
            }
        });
    }
    fn confirm_tx(&self, rt: &Handle, tx: Signature) {
        // TODO: CU 上限过低时用更高值重发
        log::debug!(target: TARGET, "txworker confirm tx: {tx:?}");
        let drift = self.drift;
        let pending_txs = Arc::clone(&self.pending_txs);
        let metrics = self.metrics.clone();
        rt.spawn(async move {
            let pending_tx_meta = {
                let mut pending = pending_txs.write().await;
                pending.confirm(&tx)
            };
            if pending_tx_meta.is_none() {
                return;
            }
            let PendingTxMeta {
                signature: _,
                intent,
                cu_limit: sent_cu_limit,
                ts: _,
            } = pending_tx_meta.unwrap();
            let intent_label = intent.label();
            let expected_fill_count = intent.expected_fill_count();
            let _ = tokio::time::sleep(Duration::from_secs(1)).await;
            match drift
                .rpc()
                .get_transaction_with_config(
                    &tx,
                    RpcTransactionConfig {
                        encoding: Some(UiTransactionEncoding::Base64),
                        commitment: Some(CommitmentConfig::confirmed()),
                        max_supported_transaction_version: Some(0),
                    },
                )
                .await
            {
                Ok(tx_log) => {
                    if let Some(meta) = tx_log.transaction.meta {
                        match meta.err {
                            None => {
                                // 交易确认成功
                                let sig = tx.to_string();
                                let logs = meta.log_messages.unwrap();
                                let tx_confirmed_slot = tx_log.slot;
                                let (_, sent_slot) = intent.crosses_and_slot();
                                let mut actual_fills = 0;
                                for (tx_idx, log) in logs.iter().enumerate() {
                                    if let Some(event) = drift_rs::event_subscriber::try_parse_log(
                                        log.as_str(),
                                        &sig,
                                        tx_idx,
                                    ) {
                                        if let DriftEvent::OrderFill { ..} = event
                                        {
                                            actual_fills += 1;
                                        } else if let DriftEvent::OrderTrigger { .. } = event {
                                            metrics.trigger_actual.inc();
                                        } else if log.as_str().contains("exceeded CUs meter") {
                                            metrics
                                            .tx_failed
                                            .with_label_values(&[
                                                intent_label,
                                                "insufficient_cus",
                                            ])
                                            .inc();
                                        }
                                    }
                                }
                                let confirmation_slots = tx_confirmed_slot - sent_slot;
                                log::debug!(target: TARGET, "txworker: {tx:?} confirmed after {confirmation_slots} slots");
                                metrics
                                    .fill_actual
                                    .with_label_values(&[intent_label])
                                    .inc();
                                metrics
                                    .confirmation_slots
                                    .with_label_values(&[intent_label])
                                    .observe(confirmation_slots as f64);
                                let cus_spent =
                                    sent_cu_limit - meta.compute_units_consumed.unwrap();
                                metrics
                                    .cu_spent
                                    .with_label_values(&[intent_label])
                                    .observe(cus_spent as f64);

                                if actual_fills == 0 {
                                    metrics
                                        .tx_confirmed
                                        .with_label_values(&[intent_label, "no_fills"])
                                        .inc();
                                } else if actual_fills < expected_fill_count as u64 {
                                    metrics
                                        .tx_confirmed
                                        .with_label_values(&[intent_label, "partial"])
                                        .inc();
                                } else {
                                        metrics
                                        .tx_confirmed
                                        .with_label_values(&[intent_label, "ok"])
                                        .inc();
                                }

                                match intent {
                                    TxIntent::LiquidateWithFill { .. } => {
                                        metrics.liquidation_success.with_label_values(&["perp"]).inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics.liquidation_success.with_label_values(&["spot"]).inc();
                                    }
                                    _ => {}
                                }
                            }
                            Some(
                                TransactionError::InsufficientFundsForFee
                                | TransactionError::InsufficientFundsForRent { .. },
                            ) => {
                                log::error!(target: TARGET, "bot needs more SOL!");
                                metrics
                                    .tx_failed
                                    .with_label_values(&[
                                        intent_label,
                                        "insufficient_funds",
                                    ])
                                    .inc();
                            }
                            Some(err) => {
                                log::warn!(target: TARGET, "tx failed: {err:?}");
                                // 交易失败
                                metrics
                                    .tx_failed
                                    .with_label_values(&[
                                        intent_label,
                                        &format!("{:?}", err),
                                    ])
                                    .inc();
                                match intent {
                                    TxIntent::LiquidateWithFill { .. } => {
                                        metrics.liquidation_failed.with_label_values(&["perp"]).inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics.liquidation_failed.with_label_values(&["spot"]).inc();
                                    }
                                    _ => {}
                                }
                            }
                        }
                    } else {
                        log::warn!(target: TARGET, "tx metadata missing");
                        metrics
                            .tx_failed
                            .with_label_values(&[intent_label, "metadata_missing"])
                            .inc();
                    }
                }
                Err(err) => {
                    log::info!(target: TARGET, "tx confirmation failed 🐢: {err}");
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "confirmation_failed"])
                        .inc();
                }
            }
        });
    }
}

#[derive(Clone)]
pub struct TxSender(crossbeam::channel::Sender<TxWork>);

impl TxSender {
    pub fn confirm_tx(&self, tx: Signature) {
        self.0
            .send(TxWork::Confirm {
                tx,
                ts: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            })
            .expect("sent");
    }
    pub fn send_tx(&self, tx: VersionedMessage, intent: TxIntent, cu_limit: u64) {
        self.0
            .send(TxWork::Send {
                tx,
                ts: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                intent,
                cu_limit,
            })
            .expect("sent");
    }
}
