pub mod arbitrage;
pub mod bayesian;
pub mod frontload;
pub mod sniper;

use alloy::primitives::{B256, U256};
use polymarket_client_sdk::clob::types::Side;
use rust_decimal::Decimal;

#[derive(Debug)]
#[allow(dead_code)]
pub struct TradeSignal {
    pub token_id: U256,
    pub condition_id: Option<B256>,
    pub market_question: String,
    pub side: Side,
    pub bought_up: bool,
    pub our_prob: f64,
    pub market_price: f64,
    pub discrepancy_pct: f64,
    pub kelly_fraction: f64,
    pub amount_usdc: Decimal,
}
