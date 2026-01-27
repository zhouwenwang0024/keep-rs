use std::collections::HashSet;

use drift_rs::{
    dlob::{L3Order, DLOB},
    types::{accounts::PerpMarket, MarketType, PositionDirection},
};

use crate::{filler::is_invalid_reduce_only, ws_cache::WsAccountCache};

#[derive(Clone, Debug)]
pub struct BestLevel {
    pub price: u64,
    pub makers: Vec<L3Order>,
}

pub fn best_levels_with_makers(
    dlob: &DLOB,
    market_index: u16,
    market_type: MarketType,
    oracle_price: u64,
    perp_market: &PerpMarket,
    trigger_price: u64,
    max_makers: usize,
    user_cache: &WsAccountCache,
) -> Option<(BestLevel, BestLevel)> {
    let book = dlob.get_l3_snapshot(market_index, market_type);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let effective_price = |order: &L3Order, is_bid: bool| -> u64 {
        if let Some(price) = order.post_trigger_price(book.slot, oracle_price, perp_market) {
            return price;
        }
        if order.price == 0 {
            let dir = if is_bid {
                PositionDirection::Long
            } else {
                PositionDirection::Short
            };
            if let Ok(vamm_price) = perp_market.fallback_price(
                dir,
                oracle_price as i64,
                order.max_ts.saturating_sub(now) as i64,
            ) {
                return vamm_price;
            }
        }
        order.price
    };

    let mut best_bid_price: Option<u64> = None;
    let mut best_ask_price: Option<u64> = None;
    let mut bid_makers: Vec<L3Order> = Vec::new();
    let mut ask_makers: Vec<L3Order> = Vec::new();
    let mut seen_bids = HashSet::new();
    let mut seen_asks = HashSet::new();

    for bid in book.bids(Some(oracle_price), Some(perp_market), Some(trigger_price)) {
        if is_invalid_reduce_only(bid, market_index, user_cache) {
            continue;
        }
        let price = effective_price(bid, true);
        if best_bid_price.is_none() {
            best_bid_price = Some(price);
        }
        if Some(price) != best_bid_price {
            break;
        }
        if bid_makers.len() < max_makers && seen_bids.insert(bid.user) {
            bid_makers.push(bid.clone());
        }
        if bid_makers.len() >= max_makers {
            break;
        }
    }

    for ask in book.asks(Some(oracle_price), Some(perp_market), Some(trigger_price)) {
        if is_invalid_reduce_only(ask, market_index, user_cache) {
            continue;
        }
        let price = effective_price(ask, false);
        if best_ask_price.is_none() {
            best_ask_price = Some(price);
        }
        if Some(price) != best_ask_price {
            break;
        }
        if ask_makers.len() < max_makers && seen_asks.insert(ask.user) {
            ask_makers.push(ask.clone());
        }
        if ask_makers.len() >= max_makers {
            break;
        }
    }

    let best_bid_price = best_bid_price?;
    let best_ask_price = best_ask_price?;

    Some((
        BestLevel {
            price: best_bid_price,
            makers: bid_makers,
        },
        BestLevel {
            price: best_ask_price,
            makers: ask_makers,
        },
    ))
}
