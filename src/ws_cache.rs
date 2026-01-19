use std::sync::Arc;

use dashmap::DashMap;
use drift_rs::{
    constants::PROGRAM_ID,
    dlob::DLOBNotifier,
    memcmp::{get_non_idle_user_filter, get_user_filter, get_user_stats_filter},
    types::{
        accounts::{User, UserStats},
        SdkResult,
    },
    DriftClient, Pubkey,
};
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};

#[derive(Clone, Copy, Debug)]
struct UserEntry {
    user: User,
    slot: u64,
}

#[derive(Clone, Copy, Debug)]
struct UserStatsEntry {
    stats: UserStats,
    slot: u64,
}

#[derive(Clone, Default)]
pub struct WsAccountCache {
    users: Arc<DashMap<Pubkey, UserEntry>>,
    stats: Arc<DashMap<Pubkey, UserStatsEntry>>,
}

impl WsAccountCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_user_update(
        &self,
        pubkey: Pubkey,
        user: User,
        slot: u64,
        dlob_notifier: Option<&DLOBNotifier>,
    ) {
        let mut prev_user = None;
        if let Some(existing) = self.users.get(&pubkey) {
            if existing.slot > slot {
                log::warn!(
                    target: "ws_cache",
                    "skip out-of-order user update: {} > {}",
                    existing.slot,
                    slot
                );
                return;
            }
            prev_user = Some(existing.user);
        }

        if let Some(dlob_notifier) = dlob_notifier {
            match prev_user.as_ref() {
                Some(prev) => dlob_notifier.user_update(pubkey, Some(prev), &user, slot),
                None => dlob_notifier.user_update(pubkey, None, &user, slot),
            }
        }

        self.users.insert(pubkey, UserEntry { user, slot });
    }

    pub fn upsert_stats(&self, pubkey: Pubkey, stats: UserStats, slot: u64) {
        if let Some(existing) = self.stats.get(&pubkey) {
            if existing.slot > slot {
                log::warn!(
                    target: "ws_cache",
                    "skip out-of-order user stats update: {} > {}",
                    existing.slot,
                    slot
                );
                return;
            }
        }

        self.stats.insert(pubkey, UserStatsEntry { stats, slot });
    }

    pub fn get_user(&self, pubkey: &Pubkey) -> Option<User> {
        self.users.get(pubkey).map(|entry| entry.user)
    }

    pub fn get_stats(&self, pubkey: &Pubkey) -> Option<UserStats> {
        self.stats.get(pubkey).map(|entry| entry.stats)
    }

    pub fn snapshot_users(&self) -> Vec<(Pubkey, User, u64)> {
        self.users
            .iter()
            .map(|entry| (*entry.key(), entry.value().user, entry.value().slot))
            .collect()
    }

    pub async fn get_user_or_fetch(
        &self,
        drift: &DriftClient,
        pubkey: &Pubkey,
    ) -> SdkResult<User> {
        if let Some(user) = self.get_user(pubkey) {
            return Ok(user);
        }

        let user = drift.get_account_value::<User>(pubkey).await?;
        self.users.insert(*pubkey, UserEntry { user, slot: 0 });
        Ok(user)
    }

    pub async fn get_stats_or_fetch(
        &self,
        drift: &DriftClient,
        pubkey: &Pubkey,
    ) -> SdkResult<UserStats> {
        if let Some(stats) = self.get_stats(pubkey) {
            return Ok(stats);
        }

        let stats = drift.get_account_value::<UserStats>(pubkey).await?;
        self.stats
            .insert(*pubkey, UserStatsEntry { stats, slot: 0 });
        Ok(stats)
    }
}

pub async fn sync_stats_accounts_ws(
    drift: &DriftClient,
    cache: &WsAccountCache,
) -> Result<(), solana_rpc_client_api::client_error::Error> {
    let stats_sync_result = drift
        .rpc()
        .get_program_accounts_with_config(
            &PROGRAM_ID,
            RpcProgramAccountsConfig {
                filters: Some(vec![get_user_stats_filter()]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64Zstd),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await;

    match stats_sync_result {
        Ok(accounts) => {
            for (pubkey, account) in accounts {
                let stats: &UserStats = drift_rs::utils::deser_zero_copy(&account.data);
                cache.upsert_stats(pubkey, *stats, 0);
            }
            log::info!(target: "dlob", "syncd stats accounts");
            Ok(())
        }
        Err(err) => {
            log::error!(target: "dlob", "dlob sync error: {err:?}");
            Err(err)
        }
    }
}

pub async fn sync_user_accounts_ws(
    drift: &DriftClient,
    dlob_notifier: &DLOBNotifier,
    cache: &WsAccountCache,
) -> Result<(), solana_rpc_client_api::client_error::Error> {
    let sync_result = drift
        .rpc()
        .get_program_accounts_with_config(
            &PROGRAM_ID,
            RpcProgramAccountsConfig {
                filters: Some(vec![get_non_idle_user_filter(), get_user_filter()]),
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64Zstd),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await;

    match sync_result {
        Ok(accounts) => {
            for (pubkey, account) in accounts {
                let user: &User = drift_rs::utils::deser_zero_copy(&account.data);
                cache.apply_user_update(pubkey, *user, 0, Some(dlob_notifier));
            }
            log::info!(target: "dlob", "synced initial orders");
            Ok(())
        }
        Err(err) => {
            log::error!(target: "dlob", "dlob sync error: {err:?}");
            Err(err)
        }
    }
}
