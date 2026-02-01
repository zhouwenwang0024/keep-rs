
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
    util::{jito_tip_ix, PythPriceUpdate, TxIntent},
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
    fn bump_cu_limit(base: u32) -> u32 {
        base.saturating_mul(12) / 10
    }
    log::info!(target: TARGET, "try fill swift order: {}", swift_order.order_uuid_str());
    let taker_order = swift_order.order_params();
    let taker_subaccount = swift_order.taker_subaccount();

    let filler_account_data = match user_cache.get_user(&filler_subaccount) {
        Some(user) => user,
        None => {
            log::warn!(target: TARGET, "missing filler account in cache: {filler_subaccount}");
            return;
        }
    };
    let filler_stats_pubkey = Wallet::derive_stats_account(&filler_account_data.authority);
    let filler_stats = match user_cache.get_stats(&filler_stats_pubkey) {
        Some(stats) => stats,
        None => {
            log::warn!(target: TARGET, "missing filler stats in cache: {filler_stats_pubkey}");
            return;
        }
    };
    let taker_account_data = match user_cache.get_user(&taker_subaccount) {
        Some(user) => user,
        None => {
            log::warn!(target: TARGET, "missing taker data in cache: {taker_subaccount}");
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
        if let Some(user) = user_cache.get_user(&order.user) {
            maker_accounts.push(user);
        } else {
            log::warn!(target: TARGET, "missing maker account in cache: {}", order.user);
        }
    }
    maker_accounts.push(taker_account_data);

    let mut maker_stats_vec: Vec<UserStats> = Vec::new();
    for maker in &maker_accounts {
        let maker_stats_pubkey = Wallet::derive_stats_account(&maker.authority);
        if let Some(stats) = user_cache.get_stats(&maker_stats_pubkey) {
            maker_stats_vec.push(stats);
        } else {
            log::warn!(target: TARGET, "missing maker stats in cache: {maker_stats_pubkey}");
        }
    }

    let revenue_share_authority = if swift_order.has_builder() {
        Some(taker_account_data.authority)
    } else {
        None
    };

    let base_cu: u32 = 600_000;
    let mut tx_builder = TransactionBuilder::new(
        drift.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    );
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
        let taker_account_data = match user_cache.get_user(&trigger_user) {
            Some(user) => user,
            None => {
                log::warn!(target: TARGET, "missing trigger user in cache: {trigger_user}");
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

    // Jito: no priority fee, only set CU limit + tip.
    let mut jito_builder = TransactionBuilder::new(
        drift.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    )
    .add_ix(ComputeBudgetInstruction::set_compute_unit_limit(base_cu))
    .update_amms(vec![taker_order.market_index])
    .place_swift_order(&swift_order, &taker_account_data)
    .proxy_spread_capture(
        taker_order.market_index,
        &filler_stats,
        maker_accounts.as_slice(),
        maker_stats_vec.as_slice(),
        revenue_share_authority,
    );
    if let Some(ix) = jito_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            jito_builder = jito_builder.set_ix(
                0,
                ComputeBudgetInstruction::set_compute_unit_limit(bump_cu_limit(base_cu)),
            );
        }
    }
    let jito_tx = jito_tip_ix(*drift.wallet().authority())
        .map(|ix| jito_builder.build_with_extra_ixs(&[ix]));
    let mut rpc_builder = tx_builder.with_priority_fee(priority_fee, Some(base_cu));
    if let Some(ix) = rpc_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            rpc_builder = rpc_builder.set_ix(
                1,
                ComputeBudgetInstruction::set_compute_unit_limit(bump_cu_limit(base_cu)),
            );
        }
    }
    let rpc_tx = rpc_builder.build();
    tx_worker_ref.send_tx_with_jito(
        rpc_tx,
        jito_tx,
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
    fn bump_cu_limit(base: u32) -> u32 {
        base.saturating_mul(12) / 10
    }
    let filler_account_data = match user_cache.get_user(&filler_subaccount) {
        Some(user) => user,
        None => {
            log::warn!(target: TARGET, "missing filler account in cache: {filler_subaccount}");
            return;
        }
    };

    let filler_stats_pubkey = Wallet::derive_stats_account(&filler_account_data.authority);
    let filler_stats = match user_cache.get_stats(&filler_stats_pubkey) {
        Some(stats) => stats,
        None => {
            log::warn!(target: TARGET, "missing filler stats in cache: {filler_stats_pubkey}");
            return;
        }
    };

    let base_cu: u32 = 600_000;
    let mut tx_builder = TransactionBuilder::new(
        drift.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    );

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
            if let Some(user) = user_cache.get_user(&order.user) {
                maker_accounts.push(user);
            } else {
                log::warn!(target: TARGET, "missing maker account in cache: {}", order.user);
            }
        }

        if matches!(order.kind, OrderKind::TriggerMarket | OrderKind::TriggerLimit) {
            let key = (order.user, order.order_id);
            if seen_triggers.insert(key) {
                let trigger_user = order.user;
                let trigger_order_id = order.order_id;
                let taker_account_data = match user_cache.get_user(&trigger_user) {
                    Some(user) => user,
                    None => {
                        log::warn!(target: TARGET, "missing trigger user in cache: {trigger_user}");
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
        if let Some(stats) = user_cache.get_stats(&maker_stats_pubkey) {
            maker_stats_vec.push(stats);
        } else {
            log::warn!(target: TARGET, "missing maker stats in cache: {maker_stats_pubkey}");
        }
    }

    tx_builder = tx_builder.proxy_spread_capture(
        market_index,
        &filler_stats,
        maker_accounts.as_slice(),
        maker_stats_vec.as_slice(),
        None,
    );

    // Jito: no priority fee, only set CU limit + tip.
    let mut jito_builder = TransactionBuilder::new(
        drift.program_data(),
        filler_subaccount,
        std::borrow::Cow::Borrowed(&filler_account_data),
        false,
    )
    .add_ix(ComputeBudgetInstruction::set_compute_unit_limit(base_cu));
    if let Some(ref update_msg) = oracle_update {
        jito_builder =
            jito_builder.post_pyth_lazer_oracle_update(&[update_msg.feed_id], &update_msg.message);
    }
    jito_builder = jito_builder
        .update_amms(vec![market_index])
        .proxy_spread_capture(
            market_index,
            &filler_stats,
            maker_accounts.as_slice(),
            maker_stats_vec.as_slice(),
            None,
        );
    if let Some(ix) = jito_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            jito_builder = jito_builder.set_ix(
                0,
                ComputeBudgetInstruction::set_compute_unit_limit(bump_cu_limit(base_cu)),
            );
        }
    }
    let jito_tx = jito_tip_ix(*drift.wallet().authority())
        .map(|ix| jito_builder.build_with_extra_ixs(&[ix]));
    let mut rpc_builder = tx_builder.with_priority_fee(priority_fee, Some(base_cu));
    if let Some(ix) = rpc_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            rpc_builder = rpc_builder.set_ix(
                1,
                ComputeBudgetInstruction::set_compute_unit_limit(bump_cu_limit(base_cu)),
            );
        }
    }
    let rpc_tx = rpc_builder.build();
    tx_worker_ref.send_tx_with_jito(
        rpc_tx,
        jito_tx,
        TxIntent::OnchainCross {
            slot: crosses.slot,
            market_index,
        },
        base_cu as u64,
    );
}
