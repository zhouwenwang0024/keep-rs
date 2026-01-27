use std::time::Duration;

use futures_util::StreamExt;
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
}

const BINANCE_STALE_MS: u64 = 1000;
const BINANCE_WEIGHT_SPOT: f64 = 0.80;
const BINANCE_WEIGHT_FUT: f64 = 0.20;

pub fn spawn_binance_price_feed(markets: Vec<(u16, String)>) -> Receiver<BinancePriceUpdate> {
    let (tx, rx) = mpsc::channel(1024);
    for (market_index, symbol) in markets {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            let stream_name = symbol.to_ascii_lowercase();
            loop {
                let spot_url = format!(
                    "wss://stream.binance.com:9443/ws/{}@bookTicker",
                    stream_name
                );
                let fut_url = format!(
                    "wss://fstream.binance.com/ws/{}@bookTicker",
                    stream_name
                );
                match (connect_async(spot_url.as_str()).await, connect_async(fut_url.as_str()).await)
                {
                    (Ok((spot_ws, _)), Ok((fut_ws, _))) => {
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
                                        _ => break,
                                    };
                                    if let Ok(text) = msg.to_text() {
                                        if let Ok(parsed) = serde_json::from_str::<BookTickerMsg>(text) {
                                            if let (Ok(bid), Ok(ask)) = (parsed.bid.parse::<f64>(), parsed.ask.parse::<f64>()) {
                                                if bid > 0.0 && ask > 0.0 {
                                                    let mid = (bid + ask) * 0.5;
                                                    let ts_ms = std::time::SystemTime::now()
                                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_millis() as u64;
                                                    spot_mid = Some((mid, ts_ms));
                                                }
                                            }
                                        }
                                    }
                                }
                                msg = fut_read.next() => {
                                    let msg = match msg {
                                        Some(Ok(m)) => m,
                                        _ => break,
                                    };
                                    if let Ok(text) = msg.to_text() {
                                        if let Ok(parsed) = serde_json::from_str::<BookTickerMsg>(text) {
                                            if let (Ok(bid), Ok(ask)) = (parsed.bid.parse::<f64>(), parsed.ask.parse::<f64>()) {
                                                if bid > 0.0 && ask > 0.0 {
                                                    let mid = (bid + ask) * 0.5;
                                                    let ts_ms = std::time::SystemTime::now()
                                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                                        .unwrap()
                                                        .as_millis() as u64;
                                                    fut_mid = Some((mid, ts_ms));

                                                    if let (Some((spot_mid_v, spot_ts)), Some((fut_mid_v, fut_ts))) =
                                                        (spot_mid, fut_mid)
                                                    {
                                                        let now_ms = std::time::SystemTime::now()
                                                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                                            .unwrap()
                                                            .as_millis() as u64;
                                                        if now_ms.saturating_sub(spot_ts) <= BINANCE_STALE_MS
                                                            && now_ms.saturating_sub(fut_ts) <= BINANCE_STALE_MS
                                                        {
                                                            let mid = BINANCE_WEIGHT_SPOT * spot_mid_v
                                                                + BINANCE_WEIGHT_FUT * fut_mid_v;
                                                            let bin_mid = (mid * 1_000_000.0) as i64;
                                                            let _ = tx
                                                                .send(BinancePriceUpdate {
                                                                    market_index,
                                                                    reference_price: bin_mid,
                                                                    binance_mid: bin_mid,
                                                                    ts_ms: now_ms,
                                                                })
                                                                .await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });
    }
    rx
}
