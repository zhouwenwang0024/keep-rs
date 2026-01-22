//! 交易构造与发送（从 filler.rs 拆分）
use drift_rs::{
    dlob::{CrossesAndTopMakers, MakerCrosses},
    types::{accounts::User, MarketType, OrderKind, OrderTriggerCondition, PositionDirection},
    DriftClient, Pubkey, TransactionBuilder, Wallet,
};
use solana_sdk::compute_budget::ComputeBudgetInstruction;

use crate::{
    filler::{cross_meets_thresholds, TxSender, TARGET},
    util::{PythPriceUpdate, TxIntent},
    ws_cache::WsAccountCache,
};

/// 尝试成交一笔 swift 订单
pub(crate) async fn try_swift_fill(
    drift: &'static DriftClient,
    priority_fee: u64,
    cu_limit: u32,
    filler_subaccount: Pubkey,
    swift_order: drift_rs::swift_order_subscriber::SignedOrderInfo,
    crosses: MakerCrosses,
    user_cache: &WsAccountCache,
    tx_worker_ref: TxSender,
) {
    log::info!(target: TARGET, "try fill swift order: {}", swift_order.order_uuid_str());
    let taker_order = swift_order.order_params();
    let taker_subaccount = swift_order.taker_subaccount();
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
    let filler_stats_pubkey = Wallet::derive_stats_account(&filler_account_data.authority);
    let filler_stats = match user_cache
        .get_stats_or_fetch(drift, &filler_stats_pubkey)
        .await
    {
        Ok(stats) => stats,
        Err(err) => {
            log::warn!(target: TARGET, "missing filler stats: {err:?}");
            return;
        }
    };
    let taker_account_data = match user_cache.get_user_or_fetch(drift, &taker_subaccount).await {
        Ok(user) => user,
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

    let mut maker_accounts: Vec<User> = crosses
        .orders
        .iter()
        .filter(|m| m.0.user != taker_subaccount) // 避免自成交
        .filter_map(|(m, _fill_size)| user_cache.get_user(&m.user))
        .collect();

    if maker_accounts.is_empty() && !crosses.has_vamm_cross {
        log::warn!("invalid cross: {crosses:?}");
        return;
    }
    maker_accounts.push(taker_account_data);

    // taker_order_id = taker_account_data.next_order_id;
    let mut tx_builder = tx_builder
        .with_priority_fee(priority_fee, Some(cu_limit))
        .place_swift_order(&swift_order, &taker_account_data)
        .proxy_spread_capture(
            taker_order.market_index,
            &filler_stats,
            maker_accounts.as_slice(),
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
pub(crate) async fn try_auction_fill(
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

    let filler_stats_pubkey = Wallet::derive_stats_account(&filler_account_data.authority);
    let filler_stats = match user_cache
        .get_stats_or_fetch(drift, &filler_stats_pubkey)
        .await
    {
        Ok(stats) => stats,
        Err(err) => {
            log::warn!(target: TARGET, "missing filler stats: {err:?}");
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
            .filter(|m| m.0.user != taker_subaccount) // 避免自成交
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

        maker_accounts.push(taker_account_data);
        tx_builder = tx_builder.proxy_spread_capture(
            market_index,
            &filler_stats,
            maker_accounts.as_slice(),
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
pub(crate) fn try_uncross(
    drift: &DriftClient,
    slot: u64,
    priority_fee: u64,
    cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    crosses: drift_rs::dlob::CrossingRegion,
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
    let filler_stats_pubkey = Wallet::derive_stats_account(&filler_account_data.authority);
    let filler_stats = match user_cache.get_stats(&filler_stats_pubkey) {
        Some(stats) => stats,
        None => {
            log::warn!(target: TARGET, "missing filler stats for uncross");
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

    // 用所有交叉的挂单尝试组合合法的主动/被动撮合
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

        let mut tx_builder = TransactionBuilder::new(
            drift.program_data(),
            filler_subaccount,
            std::borrow::Cow::Borrowed(&filler_account_data),
            false,
        );
        let mut makers = makers;
        makers.push(taker_account_data);
        tx_builder = tx_builder
            .with_priority_fee(priority_fee, Some(cu_limit))
            .proxy_spread_capture(market_index, &filler_stats, makers.as_slice());

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
