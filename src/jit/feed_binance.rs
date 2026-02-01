use std::time::Duration;

use futures_util::StreamExt;
use log;
use serde::Deserialize;
use tokio::sync::mpsc::{self, Receiver};
use tokio_tungstenite::connect_async;

#[derive(Clone, Debug)]
pub struct BinancePriceUpdate {
    pub market_index: u16,
    pub reference_price: i64,
    pub binance_mid: i64,
    pub ts_ms: u64,
}

#[derive(Deserialize)]
struct BookTickerMsg {
    #[serde(rename = "b")]
    bid: String,
    #[serde(rename = "a")]
    ask: String,
    #[serde(rename = "E")]
    event_time: Option<u64>,
}

const BINANCE_STALE_MS: u64 = 100;
const BINANCE_WEIGHT_SPOT: f64 = 0.80;
const BINANCE_WEIGHT_FUT: f64 = 0.20;

pub fn spawn_binance_price_feed(markets: Vec<(u16, String)>) -> Receiver<BinancePriceUpdate> {
    let (tx, rx) = mpsc::channel(1024);
    for (market_index, symbol) in markets {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            let stream_name = symbol.to_ascii_lowercase();
            let mut last_state_log_ms: u64 = 0;
            let mut last_spot_ts_ms: Option<u64> = None;
            let mut last_fut_ts_ms: Option<u64> = None;
            loop {
                let spot_url = format!(
                    "wss://stream.binance.com:9443/ws/{}@bookTicker",
                    stream_name
                );
                let fut_url = format!(
                    "wss://fstream.binance.com/ws/{}@bookTicker",
                    stream_name
                );
                let spot_res = connect_async(spot_url.as_str()).await;
                let fut_res = connect_async(fut_url.as_str()).await;
                match (spot_res, fut_res) {
                    (Ok((spot_ws, _)), Ok((fut_ws, _))) => {
                        log::info!(
                            target: "filler",
                            "[BINANCE_FEED] connected market={} symbol={} spot_url={} fut_url={}",
                            market_index, stream_name, spot_url, fut_url
                        );
                        backoff_secs = 1;
                        let (_, mut spot_read) = spot_ws.split();
                        let (_, mut fut_read) = fut_ws.split();
                        let mut spot_mid: Option<(f64, u64)> = None;
                        let mut fut_mid: Option<(f64, u64)> = None;
                        loop {
                            tokio::select! {
                                msg = spot_read.next() => {
                                    let msg = match msg {
                                        Some(Ok(m)) => m,
                                        other => {
                                            log::warn!(
                                                target: "filler",
                                                "[BINANCE_FEED] spot stream closed market={} symbol={} msg={:?}",
                                                market_index, stream_name, other
                                            );
                                            break;
                                        }
                                    };
                                    if let Ok(text) = msg.to_text() {
                                        if let Ok(parsed) = serde_json::from_str::<BookTickerMsg>(text) {
                                            if let (Ok(bid), Ok(ask)) = (parsed.bid.parse::<f64>(), parsed.ask.parse::<f64>()) {
                                                if bid > 0.0 && ask > 0.0 {
                                                    let mid = (bid + ask) * 0.5;
                                                    let now_ms = std::time::SystemTime::now()
                                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_millis() as u64;
                                                    let ts_ms = parsed.event_time.unwrap_or(now_ms);
                                                    spot_mid = Some((mid, ts_ms));
                                                    last_spot_ts_ms = Some(ts_ms);
                                                }
                                            }
                                        }
                                    }
                                }
                                msg = fut_read.next() => {
                                    let msg = match msg {
                                        Some(Ok(m)) => m,
                                        other => {
                                            log::warn!(
                                                target: "filler",
                                                "[BINANCE_FEED] futures stream closed market={} symbol={} msg={:?}",
                                                market_index, stream_name, other
                                            );
                                            break;
                                        }
                                    };
                                    if let Ok(text) = msg.to_text() {
                                        if let Ok(parsed) = serde_json::from_str::<BookTickerMsg>(text) {
                                            if let (Ok(bid), Ok(ask)) = (parsed.bid.parse::<f64>(), parsed.ask.parse::<f64>()) {
                                                if bid > 0.0 && ask > 0.0 {
                                                    let mid = (bid + ask) * 0.5;
                                                    let now_ms = std::time::SystemTime::now()
                                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_millis() as u64;
                                                    let ts_ms = parsed.event_time.unwrap_or(now_ms);
                                                    fut_mid = Some((mid, ts_ms));
                                                    last_fut_ts_ms = Some(ts_ms);

                                                    if let (Some((spot_mid_v, spot_ts)), Some((fut_mid_v, fut_ts))) =
                                                        (spot_mid, fut_mid)
                                                    {
                                                        let now_ms = std::time::SystemTime::now()
                                                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                                            .unwrap()
                                                            .as_millis() as u64;
                                                        let skew_ms = if spot_ts >= fut_ts {
                                                            spot_ts - fut_ts
                                                        } else {
                                                            fut_ts - spot_ts
                                                        };
                                                        if now_ms.saturating_sub(spot_ts) <= BINANCE_STALE_MS
                                                            && now_ms.saturating_sub(fut_ts) <= BINANCE_STALE_MS
                                                            && skew_ms <= BINANCE_STALE_MS
                                                        {
                                                            let mid = BINANCE_WEIGHT_SPOT * spot_mid_v
                                                                + BINANCE_WEIGHT_FUT * fut_mid_v;
                                                            let bin_mid = (mid * 1_000_000.0) as i64;
                                                            let _ = tx
                                                                .send(BinancePriceUpdate {
                                                                    market_index,
                                                                    reference_price: bin_mid,
                                                                    binance_mid: bin_mid,
                                                                    ts_ms: fut_ts,
                                                                })
                                                                .await;
                                                            log::debug!(
                                                                target: "filler",
                                                                "[BINANCE_FEED] update market={} symbol={} mid={} ts_ms={} skew_ms={}",
                                                                market_index, stream_name, bin_mid, now_ms, skew_ms
                                                            );
                                                        }
                                                    }
                                                    let now_ms = std::time::SystemTime::now()
                                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_millis() as u64;
                                                    if now_ms.saturating_sub(last_state_log_ms) >= 30_000 {
                                                        last_state_log_ms = now_ms;
                                                        let spot_age_ms = last_spot_ts_ms
                                                            .map(|ts| now_ms.saturating_sub(ts))
                                                            .unwrap_or(u64::MAX);
                                                        let fut_age_ms = last_fut_ts_ms
                                                            .map(|ts| now_ms.saturating_sub(ts))
                                                            .unwrap_or(u64::MAX);
                                                        log::info!(
                                                            target: "filler",
                                                            "[BINANCE_FEED] state market={} symbol={} spot_age_ms={} fut_age_ms={}",
                                                            market_index, stream_name, spot_age_ms, fut_age_ms
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    (spot_res, fut_res) => {
                        log::warn!(
                            target: "filler",
                            "[BINANCE_FEED] connect failed market={} symbol={} spot_err={:?} fut_err={:?} backoff_s={}",
                            market_index, stream_name, spot_res.err(), fut_res.err(), backoff_secs
                        );
                    }
                }
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });
    }
    rx
}
