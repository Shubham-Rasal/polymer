use std::sync::Arc;

use alloy::primitives::U256;
use dashmap::DashMap;
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use polymarket_client_sdk::types::Decimal;
use tracing::debug;

#[derive(Debug, Clone, Default)]
pub struct BookLevels {
    pub best_bid: Option<(f64, f64)>,
    pub best_ask: Option<(f64, f64)>,
}

pub type OrderbookState = Arc<DashMap<U256, BookLevels>>;

pub fn new_state() -> OrderbookState {
    Arc::new(DashMap::new())
}

pub fn apply_update(state: &OrderbookState, book: &BookUpdate) {
    let best_bid = book
        .bids
        .first()
        .map(|l| (decimal_to_f64(l.price), decimal_to_f64(l.size)));
    let best_ask = book
        .asks
        .first()
        .map(|l| (decimal_to_f64(l.price), decimal_to_f64(l.size)));
    let levels = BookLevels {
        best_bid,
        best_ask,
    };
    state.insert(book.asset_id, levels);
    debug!(
        asset_id = %book.asset_id,
        best_bid = ?best_bid,
        best_ask = ?best_ask,
        "[Orderbook] update"
    );
}

fn decimal_to_f64(d: Decimal) -> f64 {
    d.to_string().parse().unwrap_or(0.0)
}

pub fn get_best_bid(state: &OrderbookState, token: U256) -> Option<(f64, f64)> {
    state.get(&token).and_then(|g| g.best_bid)
}

pub fn get_best_ask(state: &OrderbookState, token: U256) -> Option<(f64, f64)> {
    state.get(&token).and_then(|g| g.best_ask)
}
