use std::collections::HashSet;

use drift_rs::{types::accounts::{User, UserStats}, DriftClient, Pubkey, TransactionBuilder, Wallet};
use solana_sdk::compute_budget::ComputeBudgetInstruction;

use crate::{
    filler::TARGET,
    jit::jit_strategy::JitIntent,
    tx_worker::TxSender,
    util::{jito_tip_ix, TxIntent as TxIntentKind},
    ws_cache::WsAccountCache,
};

pub(crate) async fn try_jit(
    drift: &'static DriftClient,
    priority_fee: u64,
    cu_limit: u32,
    jit_subaccount: Pubkey,
    intent: &JitIntent,
    user_cache: &WsAccountCache,
    tx_worker_ref: TxSender,
    proxy_program_id: Option<Pubkey>,
) {
    fn bump_cu_limit(base: u32) -> u32 {
        base.saturating_mul(12) / 10
    }
    let jit_account_data = match user_cache.get_user(&jit_subaccount) {
        Some(user) => user,
        None => {
            log::warn!(target: TARGET, "jit missing subaccount: {}", jit_subaccount);
            return;
        }
    };
    let jit_stats_pubkey = Wallet::derive_stats_account(&jit_account_data.authority);
    let jit_stats = match user_cache.get_stats(&jit_stats_pubkey) {
        Some(stats) => stats,
        None => {
            log::warn!(target: TARGET, "jit missing stats: {}", jit_stats_pubkey);
            return;
        }
    };

    let mut maker_accounts: Vec<User> = Vec::new();
    let mut seen = HashSet::<Pubkey>::new();
    for order in intent.makers_bid.iter().chain(intent.makers_ask.iter()) {
        if order.user == jit_subaccount {
            continue;
        }
        if !seen.insert(order.user) {
            continue;
        }
        if let Some(user) = user_cache.get_user(&order.user) {
            maker_accounts.push(user);
        } else {
            log::warn!(target: TARGET, "jit missing maker account: {}", order.user);
        }
    }
    let mut maker_stats_vec: Vec<UserStats> = Vec::new();
    for maker in &maker_accounts {
        let maker_stats_pubkey = Wallet::derive_stats_account(&maker.authority);
        if let Some(stats) = user_cache.get_stats(&maker_stats_pubkey) {
            maker_stats_vec.push(stats);
        } else {
            log::warn!(target: TARGET, "jit missing maker stats: {}", maker_stats_pubkey);
        }
    }

    let base_cu: u32 = 400_000;
    let mut tx_builder = TransactionBuilder::new(
        drift.program_data(),
        jit_subaccount,
        std::borrow::Cow::Borrowed(&jit_account_data),
        false,
    );
    // Build base instructions once (no priority fee).
    tx_builder = tx_builder.update_amms(vec![intent.market_index]);
    tx_builder = tx_builder.proxy_jit(
        intent.market_index,
        intent.reference_price,
        intent.edge_ppm,
        &jit_stats,
        maker_accounts.as_slice(),
        maker_stats_vec.as_slice(),
        proxy_program_id,
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let latency_ms = now_ms.saturating_sub(intent.binance_ts_ms);
    log::info!(
        target: TARGET,
        "jit send: market={}, ref_px={}, edge_ppm={}, makers_bid={}, makers_ask={}, binance_ts_ms={}, send_ts_ms={}, latency_ms={}",
        intent.market_index,
        intent.reference_price,
        intent.edge_ppm,
        intent.makers_bid.len(),
        intent.makers_ask.len(),
        intent.binance_ts_ms,
        now_ms,
        latency_ms,
    );
    // Jito: no priority fee, only set CU limit + tip.
    let mut jito_builder = TransactionBuilder::new(
        drift.program_data(),
        jit_subaccount,
        std::borrow::Cow::Borrowed(&jit_account_data),
        false,
    )
    .add_ix(ComputeBudgetInstruction::set_compute_unit_limit(base_cu))
    .update_amms(vec![intent.market_index])
    .proxy_jit(
        intent.market_index,
        intent.reference_price,
        intent.edge_ppm,
        &jit_stats,
        maker_accounts.as_slice(),
        maker_stats_vec.as_slice(),
        proxy_program_id,
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

    // RPC: priority fee *10 (scaled earlier) + CU limit.
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
        TxIntentKind::Jit {
            market_index: intent.market_index,
            reference_price: intent.reference_price,
            edge_ppm: intent.edge_ppm,
        },
        base_cu as u64,
    );
}
