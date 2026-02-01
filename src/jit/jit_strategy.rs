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
    pub binance_ts_ms: u64,
    pub best_bid_price: u64,
    pub best_ask_price: u64,
    pub makers_bid: Vec<L3Order>,
    pub makers_ask: Vec<L3Order>,
    pub drift_mid: i64,
    pub binance_mid: i64,
    pub basis_ema: f64,
    pub spread: f64,
    pub oracle_price: u64,
    pub trigger_price: u64,
    pub dlob_slot: u64,
}

#[derive(Clone, Debug)]
pub struct JitStrategy {
    pub edge_ppm: i64,
    pub cooldown_ms: u64,
    pub staleness_ms: u64,
    pub max_makers_per_side: usize,
    start_time_ms: u64,
    warmup_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct JitMarketState {
    pub last_fire_ms: u64,
    pub basis_ema: f64,
    pub last_basis_sec: u64,
    pub last_dlob_mid: i64,
    pub last_dlob_ts_ms: u64,
}

impl JitStrategy {
    pub fn update_basis_ema(
        &self,
        state: &mut JitMarketState,
        binance_mid: i64,
        basis_mid: i64,
        now_ms: u64,
    ) {
        let now_sec = now_ms / 1000;
        if now_sec == state.last_basis_sec {
            return;
        }
        let spread = (binance_mid as f64) - (basis_mid as f64);
        let prev = if state.last_basis_sec == 0 {
            spread
        } else {
            state.basis_ema
        };
        let ema = prev + BASIS_EMA_ALPHA * (spread - prev);
        state.basis_ema = ema;
        state.last_basis_sec = now_sec;
    }

    pub fn new(
        edge_ppm: i64,
        cooldown_ms: u64,
        staleness_ms: u64,
        max_makers_per_side: usize,
        start_time_ms: u64,
        warmup_ms: u64,
    ) -> Self {
        Self {
            edge_ppm,
            cooldown_ms,
            staleness_ms,
            max_makers_per_side,
            start_time_ms,
            warmup_ms,
        }
    }

    pub fn maybe_intent(
        &self,
        state: &mut JitMarketState,
        market_index: u16,
        binance_mid: i64,
        reference_ts_ms: u64,
        now_ms: u64,
        dlob: &DLOB,
        perp_market: &PerpMarket,
        oracle_price: u64,
        trigger_price: u64,
        user_cache: &WsAccountCache,
        last_dlob_update_ms: u64,
        official_mid: Option<(i64, u64)>,
        official_stale_ms: u64,
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
        if self.warmup_ms > 0 && now_ms.saturating_sub(self.start_time_ms) < self.warmup_ms {
            log::debug!(
                target: "filler",
                "jit skip: warmup market={}, elapsed_ms={}, warmup_ms={}",
                market_index,
                now_ms.saturating_sub(self.start_time_ms),
                self.warmup_ms
            );
            return None;
        }
        if last_dlob_update_ms == 0 {
            log::warn!(
                target: "filler",
                "jit skip: no DLOB updates yet market={}",
                market_index
            );
            return None;
        }
        const MAX_DLOB_STALE_MS: u64 = 5_000;
        if now_ms.saturating_sub(last_dlob_update_ms) > MAX_DLOB_STALE_MS {
            log::warn!(
                target: "filler",
                "jit skip: stale DLOB market={}, age_ms={}, max_ms={}",
                market_index,
                now_ms.saturating_sub(last_dlob_update_ms),
                MAX_DLOB_STALE_MS
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
        if state.last_fire_ms > 0
            && now_ms.saturating_sub(state.last_fire_ms) < self.cooldown_ms
        {
            log::debug!(
                target: "filler",
                "jit skip: cooldown market={}, since_last_ms={}, cooldown_ms={}",
                market_index,
                now_ms.saturating_sub(state.last_fire_ms),
                self.cooldown_ms
            );
            return None;
        }

        let (best_bid, best_ask, dlob_slot) = match best_levels_with_makers(
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
        let drift_mid_i64 = drift_mid as i64;
        state.last_dlob_mid = drift_mid_i64;
        state.last_dlob_ts_ms = now_ms;
        let basis_mid = match official_mid {
            Some((mid, ts_ms)) if now_ms.saturating_sub(ts_ms) <= official_stale_ms => mid,
            _ => drift_mid_i64,
        };
        let basis = state.basis_ema;
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

        state.last_fire_ms = now_ms;

        Some(JitIntent {
            market_index,
            reference_price,
            edge_ppm: self.edge_ppm,
            binance_ts_ms: reference_ts_ms,
            best_bid_price: best_bid.price,
            best_ask_price: best_ask.price,
            makers_bid: best_bid.makers,
            makers_ask: best_ask.makers,
            drift_mid: drift_mid_i64,
            binance_mid,
            basis_ema: basis,
            spread: (binance_mid as f64) - (basis_mid as f64),
            oracle_price,
            trigger_price,
            dlob_slot,
        })
    }
}
