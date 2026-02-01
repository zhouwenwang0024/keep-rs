use std::collections::HashSet;

use drift_rs::{
    dlob::{L3Order, DLOB},
    types::{accounts::PerpMarket, MarketType},
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
) -> Option<(BestLevel, BestLevel, u64)> {
    let book = dlob.get_l3_snapshot(market_index, market_type);

    let mut best_bid_price: Option<u64> = None;
    let mut best_ask_price: Option<u64> = None;
    let mut bid_makers: Vec<L3Order> = Vec::new();
    let mut ask_makers: Vec<L3Order> = Vec::new();
    let mut seen_bids = HashSet::new();
    let mut seen_asks = HashSet::new();

    for bid in book.bids_with_price(Some(oracle_price), Some(perp_market), Some(trigger_price)) {
        let order = bid.order;
        let price = bid.price;
        if is_invalid_reduce_only(order, market_index, user_cache) {
            continue;
        }
        if best_bid_price.is_none() {
            best_bid_price = Some(price);
        }
        if Some(price) != best_bid_price {
            break;
        }
        if bid_makers.len() < max_makers && seen_bids.insert(order.user) {
            bid_makers.push(order.clone());
        }
        if bid_makers.len() >= max_makers {
            break;
        }
    }

    for ask in book.asks_with_price(Some(oracle_price), Some(perp_market), Some(trigger_price)) {
        let order = ask.order;
        let price = ask.price;
        if is_invalid_reduce_only(order, market_index, user_cache) {
            continue;
        }
        if best_ask_price.is_none() {
            best_ask_price = Some(price);
        }
        if Some(price) != best_ask_price {
            break;
        }
        if ask_makers.len() < max_makers && seen_asks.insert(order.user) {
            ask_makers.push(order.clone());
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
        book.slot,
    ))
}
