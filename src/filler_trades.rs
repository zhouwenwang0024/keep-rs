
// Transaction construction and send helpers (split from filler.rs).
use std::collections::HashSet;

use drift_rs::{
    dlob::{CrossingRegionAll, MakerCrosses, OrderKind},
    types::{accounts::{User, UserStats}, MarketType, OrderTriggerCondition},
    DriftClient, Pubkey, TransactionBuilder, Wallet,
};
use solana_sdk::compute_budget::ComputeBudgetInstruction;

use crate::{
    filler::TARGET,
    tx_worker::TxSender,
    util::{maybe_add_jito_tip, PythPriceUpdate, TxIntent},
    ws_cache::WsAccountCache,
};

pub(crate) async fn try_swift_fill(
    drift: &'static DriftClient,
    priority_fee: u64,
    cu_limit: u32,
    filler_subaccount: Pubkey,
    swift_order: drift_rs::swift_order_subscriber::SignedOrderInfo,
    crosses: MakerCrosses,
    trigger_price: u64,
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

    let mut maker_accounts: Vec<User> = Vec::new();
    let mut seen = HashSet::<Pubkey>::new();
    for (order, _fill_size) in crosses.orders.iter() {
        if order.user == taker_subaccount {
            continue;
        }
        if !seen.insert(order.user) {
            continue;
        }
        match user_cache.get_user_or_fetch(drift, &order.user).await {
            Ok(user) => maker_accounts.push(user),
            Err(err) => {
                log::warn!(target: TARGET, "missing maker account: {err:?}");
            }
        }
    }
    maker_accounts.push(taker_account_data);

    let mut maker_stats_vec: Vec<UserStats> = Vec::new();
    for maker in &maker_accounts {
        let maker_stats_pubkey = Wallet::derive_stats_account(&maker.authority);
        match user_cache.get_stats_or_fetch(drift, &maker_stats_pubkey).await {
            Ok(stats) => maker_stats_vec.push(stats),
            Err(err) => {
                log::warn!(target: TARGET, "missing maker stats: {err:?}");
            }
        }
    }

    let revenue_share_authority = if swift_order.has_builder() {
        Some(taker_account_data.authority)
    } else {
        None
    };

    let base_cu = cu_limit.saturating_mul(3);
    let mut tx_builder = TransactionBuilder::new(
        drift.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    )
    .with_priority_fee(priority_fee, Some(base_cu));
    tx_builder = tx_builder.update_amms(vec![taker_order.market_index]);

    let mut seen_triggers = HashSet::<(Pubkey, u32)>::new();
    for (order, _fill_size) in crosses.orders.iter() {
        if !matches!(order.kind, OrderKind::TriggerMarket | OrderKind::TriggerLimit) {
            continue;
        }
        let key = (order.user, order.order_id);
        if !seen_triggers.insert(key) {
            continue;
        }

        let trigger_user = order.user;
        let trigger_order_id = order.order_id;
        let taker_account_data = match user_cache.get_user_or_fetch(drift, &trigger_user).await {
            Ok(user) => user,
            Err(err) => {
                log::warn!(target: TARGET, "missing trigger user: {err:?}");
                return;
            }
        };
        let actual_order = match taker_account_data
            .orders
            .iter()
            .find(|o| o.order_id == trigger_order_id)
        {
            Some(order) => order,
            None => {
                log::warn!(target: TARGET, "trigger order missing: user={trigger_user}, order_id={trigger_order_id}");
                return;
            }
        };
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
            log::debug!(target: TARGET, "trigger conditions not met: user={trigger_user}, order_id={trigger_order_id}");
            return;
        }
        tx_builder = tx_builder.trigger_order(
            trigger_user,
            &taker_account_data,
            trigger_order_id,
            (taker_order.market_index, MarketType::Perp),
        );
    }

    tx_builder = tx_builder
        .place_swift_order(&swift_order, &taker_account_data)
        .proxy_spread_capture(
            taker_order.market_index,
            &filler_stats,
            maker_accounts.as_slice(),
            maker_stats_vec.as_slice(),
            revenue_share_authority,
        );

    if let Some(ix) = tx_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            tx_builder = tx_builder.set_ix(
                1,
                ComputeBudgetInstruction::set_compute_unit_limit(base_cu * 3),
            );
        }
    }

    tx_builder = maybe_add_jito_tip(tx_builder, *drift.wallet().authority());
    let tx = tx_builder.build();
    tx_worker_ref.send_tx(
        tx,
        TxIntent::SwiftFill {
            maker_crosses: crosses,
        },
        base_cu as u64,
    );
}

pub(crate) async fn try_onchain_cross(
    drift: &'static DriftClient,
    priority_fee: u64,
    cu_limit: u32,
    market_index: u16,
    filler_subaccount: Pubkey,
    crosses: CrossingRegionAll,
    user_cache: &WsAccountCache,
    tx_worker_ref: TxSender,
    oracle_update: Option<PythPriceUpdate>,
    trigger_price: u64,
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

    let base_cu = cu_limit.saturating_mul(3);
    let mut tx_builder = TransactionBuilder::new(
        drift.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    )
    .with_priority_fee(priority_fee, Some(base_cu));

    if let Some(ref update_msg) = oracle_update {
        tx_builder = tx_builder
            .post_pyth_lazer_oracle_update(&[update_msg.feed_id], &update_msg.message);
    }
    tx_builder = tx_builder.update_amms(vec![market_index]);

    let mut maker_accounts: Vec<User> = Vec::new();
    let mut seen = HashSet::<Pubkey>::new();
    let mut seen_triggers = HashSet::<(Pubkey, u32)>::new();

    for order in crosses
        .crossing_bids
        .iter()
        .chain(crosses.crossing_asks.iter())
    {
        if seen.insert(order.user) {
            match user_cache.get_user_or_fetch(drift, &order.user).await {
                Ok(user) => maker_accounts.push(user),
                Err(err) => {
                    log::warn!(target: TARGET, "missing maker account: {err:?}");
                }
            }
        }

        if matches!(order.kind, OrderKind::TriggerMarket | OrderKind::TriggerLimit) {
            let key = (order.user, order.order_id);
            if seen_triggers.insert(key) {
                let trigger_user = order.user;
                let trigger_order_id = order.order_id;
                let taker_account_data =
                    match user_cache.get_user_or_fetch(drift, &trigger_user).await {
                        Ok(user) => user,
                        Err(err) => {
                            log::warn!(target: TARGET, "missing trigger user: {err:?}");
                            return;
                        }
                    };
                let actual_order = match taker_account_data
                    .orders
                    .iter()
                    .find(|o| o.order_id == trigger_order_id)
                {
                    Some(order) => order,
                    None => {
                        log::warn!(
                            target: TARGET,
                            "trigger order missing: user={trigger_user}, order_id={trigger_order_id}"
                        );
                        return;
                    }
                };
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
                    log::debug!(target: TARGET, "trigger conditions not met: user={trigger_user}, order_id={trigger_order_id}");
                    return;
                }
                tx_builder = tx_builder.trigger_order(
                    trigger_user,
                    &taker_account_data,
                    trigger_order_id,
                    (market_index, MarketType::Perp),
                );
            }
        }
    }

    let mut maker_stats_vec: Vec<UserStats> = Vec::new();
    for maker in &maker_accounts {
        let maker_stats_pubkey = Wallet::derive_stats_account(&maker.authority);
        match user_cache.get_stats_or_fetch(drift, &maker_stats_pubkey).await {
            Ok(stats) => maker_stats_vec.push(stats),
            Err(err) => {
                log::warn!(target: TARGET, "missing maker stats: {err:?}");
            }
        }
    }

    tx_builder = tx_builder.proxy_spread_capture(
        market_index,
        &filler_stats,
        maker_accounts.as_slice(),
        maker_stats_vec.as_slice(),
        None,
    );

    if let Some(ix) = tx_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            tx_builder = tx_builder.set_ix(
                1,
                ComputeBudgetInstruction::set_compute_unit_limit(base_cu * 3),
            );
        }
    }

    tx_builder = maybe_add_jito_tip(tx_builder, *drift.wallet().authority());
    let tx = tx_builder.build();
    tx_worker_ref.send_tx(
        tx,
        TxIntent::OnchainCross {
            slot: crosses.slot,
            market_index,
        },
        base_cu as u64,
    );
}
