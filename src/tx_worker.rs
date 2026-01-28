//! 交易发送与确认逻辑（拆分自 filler.rs）
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use drift_rs::{
    event_subscriber::DriftEvent,
    types::{CommitmentConfig, RpcSendTransactionConfig, VersionedMessage},
    DriftClient,
};
use solana_rpc_client_api::config::RpcTransactionConfig;
use solana_sdk::{signature::Signature, transaction::TransactionError};
use solana_transaction_status_client_types::UiTransactionEncoding;
use tokio::{runtime::Handle, sync::RwLock};

use crate::{
    filler::TARGET,
    http::Metrics,
    jito_sender::JitoSender,
    util::{PendingTxMeta, PendingTxs, TxIntent},
};

struct JitoConfig {
    sender: JitoSender,
}

pub(crate) enum TxWork {
    Send {
        tx: VersionedMessage,
        jito_tx: Option<VersionedMessage>,
        ts: u64,
        intent: TxIntent,
        cu_limit: u64,
    },
    Confirm {
        tx: Signature,
        ts: u64,
    },
}

pub(crate) struct TxWorker {
    drift: &'static DriftClient,
    pending_txs: Arc<RwLock<PendingTxs<1024>>>,
    metrics: Arc<Metrics>,
    dry_run: bool,
    jito: Option<Arc<JitoConfig>>,
}

impl TxWorker {
    pub fn new(drift: DriftClient, metrics: Arc<Metrics>, dry_run: bool) -> Self {
        let jito_sender = JitoSender::from_env().map(|sender| JitoConfig { sender });
        Self {
            drift: Box::leak(Box::new(drift)),
            pending_txs: Arc::new(RwLock::new(PendingTxs::new())),
            metrics,
            dry_run,
            jito: jito_sender.map(Arc::new),
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
                        jito_tx,
                        ts: _,
                        intent,
                        cu_limit,
                    } => {
                        if self.dry_run {
                            log::debug!(target: TARGET, "skip tx dry run: {intent:?}");
                            continue;
                        }
                        self.send_tx(&rt, tx, jito_tx, intent, cu_limit);
                    }
                    TxWork::Confirm { tx, ts: _ } => {
                        self.confirm_tx(&rt, tx);
                    }
                }
            }
        });
        TxSender(tx)
    }
    fn send_tx(
        &self,
        rt: &Handle,
        tx: VersionedMessage,
        jito_tx: Option<VersionedMessage>,
        intent: TxIntent,
        cu_limit: u64,
    ) {
        log::debug!(target: TARGET, "txworker send tx: {intent:?}");
        let drift = self.drift;
        let pending_txs = Arc::clone(&self.pending_txs);
        let metrics = self.metrics.clone();
        let jito = self.jito.clone();
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
            let blockhash = match drift.rpc().get_latest_blockhash().await {
                Ok(v) => v,
                Err(err) => {
                    log::warn!(target: TARGET, "failed to get blockhash: {err:?}");
                    metrics
                        .tx_failed
                        .with_label_values(&[intent_label, "blockhash_error"])
                        .inc();
                    return;
                }
            };

            let rpc_fut = drift.sign_and_send_with_config(
                tx.clone(),
                Some(blockhash),
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    max_retries: Some(0),
                    ..Default::default()
                },
            );

            let jito_fut = async {
                if let Some(jito) = jito {
                    let jito_msg = jito_tx.unwrap_or_else(|| tx.clone());
                    let business_tx = match drift.wallet().sign_tx(jito_msg, blockhash) {
                        Ok(tx) => tx,
                        Err(err) => return Err(format!("sign business tx failed: {err:?}")),
                    };
                    let raw_business = bincode::serialize(&business_tx)
                        .map_err(|err| format!("encode business tx failed: {err:?}"))?;
                    jito.sender
                        .send_bundle_base64(&[raw_business])
                        .await
                        .map_err(|err| format!("jito send failed: {err}"))?;
                }
                Ok::<(), String>(())
            };

            let (rpc_res, jito_res) = tokio::join!(rpc_fut, jito_fut);

            if let Err(err) = jito_res {
                log::warn!(target: TARGET, "jito send error: {err}");
                metrics
                    .tx_failed
                    .with_label_values(&[intent_label, "jito_send_error"])
                    .inc();
            }

            match rpc_res {
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
                                        if let DriftEvent::OrderFill { .. } = event {
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
                                log::debug!(
                                    target: TARGET,
                                    "txworker: {tx:?} confirmed after {confirmation_slots} slots"
                                );
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
                                        metrics
                                            .liquidation_success
                                            .with_label_values(&["perp"])
                                            .inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics
                                            .liquidation_success
                                            .with_label_values(&["spot"])
                                            .inc();
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
                                    .with_label_values(&[intent_label, "insufficient_funds"])
                                    .inc();
                            }
                            Some(err) => {
                                log::warn!(target: TARGET, "tx failed: {err:?}");
                                // 交易失败
                                metrics
                                    .tx_failed
                                    .with_label_values(&[intent_label, &format!("{:?}", err)])
                                    .inc();
                                match intent {
                                    TxIntent::LiquidateWithFill { .. } => {
                                        metrics
                                            .liquidation_failed
                                            .with_label_values(&["perp"])
                                            .inc();
                                    }
                                    TxIntent::LiquidateSpot { .. } => {
                                        metrics
                                            .liquidation_failed
                                            .with_label_values(&["spot"])
                                            .inc();
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
                jito_tx: None,
                ts: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                intent,
                cu_limit,
            })
            .expect("sent");
    }

    pub fn send_tx_with_jito(
        &self,
        tx: VersionedMessage,
        jito_tx: Option<VersionedMessage>,
        intent: TxIntent,
        cu_limit: u64,
    ) {
        self.0
            .send(TxWork::Send {
                tx,
                jito_tx,
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
