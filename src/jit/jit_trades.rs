use std::collections::HashSet;

use drift_rs::{types::accounts::{User, UserStats}, DriftClient, Pubkey, TransactionBuilder, Wallet};
use solana_sdk::compute_budget::ComputeBudgetInstruction;

use crate::{
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
    let jit_account_data = match user_cache.get_user_or_fetch(drift, &jit_subaccount).await {
        Ok(user) => user,
        Err(err) => {
            log::warn!("jit missing subaccount: {err:?}");
            return;
        }
    };
    let jit_stats_pubkey = Wallet::derive_stats_account(&jit_account_data.authority);
    let jit_stats = match user_cache
        .get_stats_or_fetch(drift, &jit_stats_pubkey)
        .await
    {
        Ok(stats) => stats,
        Err(err) => {
            log::warn!("jit missing stats: {err:?}");
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
        match user_cache.get_user_or_fetch(drift, &order.user).await {
            Ok(user) => maker_accounts.push(user),
            Err(err) => {
                log::warn!("jit missing maker account: {err:?}");
            }
        }
    }
    let mut maker_stats_vec: Vec<UserStats> = Vec::new();
    for maker in &maker_accounts {
        let maker_stats_pubkey = Wallet::derive_stats_account(&maker.authority);
        match user_cache.get_stats_or_fetch(drift, &maker_stats_pubkey).await {
            Ok(stats) => maker_stats_vec.push(stats),
            Err(err) => {
                log::warn!("jit missing maker stats: {err:?}");
            }
        }
    }

    let base_cu = cu_limit.saturating_mul(3);
    let mut tx_builder = TransactionBuilder::new(
        drift.program_data(),
        jit_subaccount,
        std::borrow::Cow::Borrowed(&jit_account_data),
        false,
    )
    .with_priority_fee(priority_fee, Some(base_cu));

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

    if let Some(ix) = tx_builder.ixs().last() {
        if ix.accounts.len() >= 30 {
            tx_builder = tx_builder.set_ix(
                1,
                ComputeBudgetInstruction::set_compute_unit_limit(base_cu.saturating_mul(3)),
            );
        }
    }

    log::info!(
        "jit send: market={}, ref_px={}, edge_ppm={}, makers_bid={}, makers_ask={}",
        intent.market_index,
        intent.reference_price,
        intent.edge_ppm,
        intent.makers_bid.len(),
        intent.makers_ask.len(),
    );
    let rpc_tx = tx_builder.build();
    let jito_tx = jito_tip_ix(*drift.wallet().authority())
        .map(|ix| tx_builder.build_with_extra_ixs(&[ix]));
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
