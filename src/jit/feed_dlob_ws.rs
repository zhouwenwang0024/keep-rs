use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc::{self, Receiver};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Clone, Debug)]
pub struct DriftL2Update {
    pub market_index: u16,
    pub bid: i64,
    pub ask: i64,
    pub mid: i64,
    pub ts_ms: u64,
}

const RECONNECT_MAX_SECS: u64 = 30;

pub fn spawn_dlob_l2_feed(
    markets: Vec<(u16, String)>,
    ws_url: String,
) -> Receiver<DriftL2Update> {
    let (tx, rx) = mpsc::channel(1024);
    tokio::spawn(async move {
        log::info!(
            target: "filler",
            "[DLOB_WS] starting url={}, markets={}",
            ws_url,
            markets.len()
        );
        let mut backoff_secs = 1u64;
        let mut market_map = HashMap::<String, u16>::new();
        for (market_index, market_name) in markets.iter() {
            market_map.insert(market_name.to_ascii_uppercase(), *market_index);
        }

        loop {
            log::info!(
                target: "filler",
                "[DLOB_WS] connecting url={}",
                ws_url
            );
            match connect_async(ws_url.as_str()).await {
                Ok((ws, _)) => {
                    log::info!(
                        target: "filler",
                        "[DLOB_WS] connected url={}, markets={}",
                        ws_url,
                        markets.len()
                    );
                    backoff_secs = 1;
                    let (mut write, mut read) = ws.split();
                    for (_, market_name) in markets.iter() {
                        let msg = serde_json::json!({
                            "type": "subscribe",
                            "marketType": "perp",
                            "channel": "orderbook",
                            "market": market_name,
                        });
                        log::info!(
                            target: "filler",
                            "[DLOB_WS] subscribe payload: {}",
                            msg.to_string()
                        );
                        if let Err(err) = write.send(Message::Text(msg.to_string())).await {
                            log::warn!(
                                target: "filler",
                                "[DLOB_WS] subscribe failed url={}, market={}, err={:?}",
                                ws_url,
                                market_name,
                                err
                            );
                        }
                    }
                    log::info!(
                        target: "filler",
                        "[DLOB_WS] subscribe sent count={}",
                        markets.len()
                    );

                    while let Some(msg) = read.next().await {
                        let msg = match msg {
                            Ok(m) => m,
                            Err(err) => {
                                log::warn!(
                                    target: "filler",
                                    "[DLOB_WS] read failed url={}, err={:?}",
                                    ws_url,
                                    err
                                );
                                break;
                            }
                        };

                        let text = match msg {
                            Message::Text(t) => t,
                            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                            Message::Close(frame) => {
                                log::warn!(
                                    target: "filler",
                                    "[DLOB_WS] closed url={}, frame={:?}",
                                    ws_url,
                                    frame
                                );
                                break;
                            }
                            Message::Ping(_) | Message::Pong(_) => {
                                continue;
                            }
                            _ => continue,
                        };

                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let parsed_market = parsed
                            .get("market")
                            .and_then(|v| v.as_str())
                            .map(|v| v.to_ascii_uppercase());
                        let mut data = parsed.get("data").cloned().unwrap_or(parsed);
                        if let Some(s) = data.as_str() {
                            if let Ok(v) = serde_json::from_str::<Value>(s) {
                                data = v;
                            }
                        }
                        let data_obj = match data.as_object() {
                            Some(obj) => obj,
                            None => continue,
                        };
                        let data_market_name = data_obj
                            .get("market")
                            .and_then(|v| v.as_str())
                            .map(|v| v.to_ascii_uppercase())
                            .or_else(|| {
                                data_obj
                                    .get("marketName")
                                    .and_then(|v| v.as_str())
                                    .map(|v| v.to_ascii_uppercase())
                            });
                        let data_market_index =
                            data_obj.get("marketIndex").and_then(|v| v.as_u64());
                        let market_index = if let Some(idx) = data_market_index {
                            idx as u16
                        } else {
                            let market_name = parsed_market.or(data_market_name);
                            match market_name.as_ref().and_then(|m| market_map.get(m)) {
                                Some(idx) => *idx,
                                None => {
                                    if market_map.len() == 1 {
                                        *market_map.values().next().unwrap()
                                    } else {
                                        continue;
                                    }
                                }
                            }
                        };

                        let best_bid = data_obj.get("bestBidPrice").and_then(parse_f64);
                        let best_ask = data_obj.get("bestAskPrice").and_then(parse_f64);
                        let bids = data_obj.get("bids");
                        let asks = data_obj.get("asks");
                        let bid = best_bid.or_else(|| bids.and_then(extract_best_px));
                        let ask = best_ask.or_else(|| asks.and_then(extract_best_px));
                        let (bid, ask) = match (bid, ask) {
                            (Some(b), Some(a)) if b > 0.0 && a > 0.0 => (b, a),
                            _ => continue,
                        };
                        let mid = 0.5 * (bid + ask);
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        let ts_ms = data_obj.get("ts").and_then(parse_u64).unwrap_or(now_ms);
                        let update = DriftL2Update {
                            market_index,
                            bid: bid.round() as i64,
                            ask: ask.round() as i64,
                            mid: mid.round() as i64,
                            ts_ms,
                        };
                        let _ = tx.send(update).await;
                    }
                    log::warn!(
                        target: "filler",
                        "[DLOB_WS] read loop ended url={}, reconnecting",
                        ws_url
                    );
                }
                Err(err) => {
                    log::warn!(
                        target: "filler",
                        "[DLOB_WS] connect failed url={}, err={:?}",
                        ws_url,
                        err
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            backoff_secs = (backoff_secs * 2).min(RECONNECT_MAX_SECS);
        }
    });

    rx
}

fn extract_best_px(levels: &Value) -> Option<f64> {
    let arr = levels.as_array()?;
    for item in arr.iter() {
        if let Some(px) = extract_price_value(item) {
            return Some(px);
        }
    }
    None
}

fn extract_price_value(item: &Value) -> Option<f64> {
    if let Some(arr) = item.as_array() {
        if let Some(px) = arr.first() {
            return parse_f64(px);
        }
    }
    if let Some(obj) = item.as_object() {
        for key in ["price", "px", "p"] {
            if let Some(px) = obj.get(key) {
                if let Some(v) = parse_f64(px) {
                    return Some(v);
                }
            }
        }
    }
    parse_f64(item)
}

fn parse_f64(val: &Value) -> Option<f64> {
    match val {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_u64(val: &Value) -> Option<u64> {
    match val {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}
