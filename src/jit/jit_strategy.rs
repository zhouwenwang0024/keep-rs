use std::collections::BTreeMap;

use drift_rs::{
    dlob::{L3Order, DLOB},
    types::{accounts::PerpMarket, MarketType},
};

use crate::{jit::maker_select::best_levels_with_makers, ws_cache::WsAccountCache};

const BASIS_EMA_POINTS: i64 = 300;
const BASIS_EMA_ALPHA: f64 = 2.0 / (BASIS_EMA_POINTS as f64 + 1.0);
const BASIS_ALIGN_MAX_MS: u64 = 500;

#[derive(Clone, Debug)]
pub struct JitIntent {
    pub market_index: u16,
    pub reference_price: i64,
    pub edge_ppm: i64,
    pub best_bid_price: u64,
    pub best_ask_price: u64,
    pub makers_bid: Vec<L3Order>,
    pub makers_ask: Vec<L3Order>,
}

#[derive(Clone, Debug)]
pub struct JitStrategy {
    pub edge_ppm: i64,
    pub cooldown_ms: u64,
    pub staleness_ms: u64,
    pub max_makers_per_side: usize,
    last_fire_ms: BTreeMap<u16, u64>,
    basis_ema: BTreeMap<u16, f64>,
    last_basis_sec: BTreeMap<u16, u64>,
}

impl JitStrategy {
    pub fn new(edge_ppm: i64, cooldown_ms: u64, staleness_ms: u64, max_makers_per_side: usize) -> Self {
        Self {
            edge_ppm,
            cooldown_ms,
            staleness_ms,
            max_makers_per_side,
            last_fire_ms: BTreeMap::new(),
            basis_ema: BTreeMap::new(),
            last_basis_sec: BTreeMap::new(),
        }
    }

    pub fn maybe_intent(
        &mut self,
        market_index: u16,
        binance_mid: i64,
        reference_ts_ms: u64,
        now_ms: u64,
        dlob: &DLOB,
        perp_market: &PerpMarket,
        oracle_price: u64,
        trigger_price: u64,
        user_cache: &WsAccountCache,
    ) -> Option<JitIntent> {
        if binance_mid <= 0 {
            log::trace!(
                target: "filler",
                "jit skip: invalid binance_mid market={}, binance_mid={}",
                market_index,
                binance_mid
            );
            return None;
        }
        if now_ms.saturating_sub(reference_ts_ms) > self.staleness_ms {
            log::debug!(
                target: "filler",
                "jit skip: stale ref price market={}, age_ms={}, staleness_ms={}",
                market_index,
                now_ms.saturating_sub(reference_ts_ms),
                self.staleness_ms
            );
            return None;
        }
        if now_ms.saturating_sub(reference_ts_ms) > BASIS_ALIGN_MAX_MS {
            log::debug!(
                target: "filler",
                "jit skip: basis align too old market={}, age_ms={}, max_ms={}",
                market_index,
                now_ms.saturating_sub(reference_ts_ms),
                BASIS_ALIGN_MAX_MS
            );
            return None;
        }
        if let Some(last) = self.last_fire_ms.get(&market_index) {
            if now_ms.saturating_sub(*last) < self.cooldown_ms {
                log::debug!(
                    target: "filler",
                    "jit skip: cooldown market={}, since_last_ms={}, cooldown_ms={}",
                    market_index,
                    now_ms.saturating_sub(*last),
                    self.cooldown_ms
                );
                return None;
            }
        }

        let (best_bid, best_ask) = match best_levels_with_makers(
            dlob,
            market_index,
            MarketType::Perp,
            oracle_price,
            perp_market,
            trigger_price,
            self.max_makers_per_side,
            user_cache,
        ) {
            Some(levels) => levels,
            None => {
                log::debug!(
                    target: "filler",
                    "jit skip: no best levels market={}",
                    market_index
                );
                return None;
            }
        };

        let drift_mid = (best_bid.price as i128 + best_ask.price as i128) / 2;
        let now_sec = now_ms / 1000;
        let last_sec = *self.last_basis_sec.get(&market_index).unwrap_or(&0);
        if now_sec != last_sec {
            let spread = (binance_mid as f64) - (drift_mid as f64);
            let prev = *self.basis_ema.get(&market_index).unwrap_or(&spread);
            let ema = prev + BASIS_EMA_ALPHA * (spread - prev);
            self.basis_ema.insert(market_index, ema);
            self.last_basis_sec.insert(market_index, now_sec);
        }
        let basis = *self.basis_ema.get(&market_index).unwrap_or(&0.0);
        let reference_price = (binance_mid as f64 - 0.8 * basis).round() as i64;
        if reference_price <= 0 {
            log::debug!(
                target: "filler",
                "jit skip: invalid reference_price market={}, ref_px={}",
                market_index,
                reference_price
            );
            return None;
        }

        let best_bid_i128 = best_bid.price as i128;
        let best_ask_i128 = best_ask.price as i128;
        let ref_i128 = reference_price as i128;
        let sell_ok = best_bid_i128 > ref_i128;
        let buy_ok = best_ask_i128 < ref_i128;

        if !sell_ok && !buy_ok {
            log::debug!(
                target: "filler",
                "jit no cross: market={}, ref_px={}, best_bid={}, best_ask={}, sell_ok={}, buy_ok={}",
                market_index,
                reference_price,
                best_bid.price,
                best_ask.price,
                sell_ok,
                buy_ok
            );
            return None;
        }

        self.last_fire_ms.insert(market_index, now_ms);

        Some(JitIntent {
            market_index,
            reference_price,
            edge_ppm: self.edge_ppm,
            best_bid_price: best_bid.price,
            best_ask_price: best_ask.price,
            makers_bid: best_bid.makers,
            makers_ask: best_ask.makers,
        })
    }
}
