use std::fs::OpenOptions;
use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::{B256, U256};
use chrono::Utc;
use polymarket_client_sdk::clob::types::Side;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::RwLock;
use tracing::info;

const DEFAULT_RESULTS_FILE: &str = "trades.json";

#[derive(Debug, Clone, serde::Serialize)]
pub struct TradeRecord {
    pub timestamp: String,
    pub mode: String,
    pub market: String,
    pub side: String,
    pub amount_usdc: String,
    pub price: f64,
    pub executed: bool,
}

#[derive(Debug, Clone)]
pub struct Position {
    pub condition_id: B256,
    pub token_id: U256,
    pub market: String,
    pub amount_usdc: Decimal,
    pub price: f64,
    pub bought_up: bool,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub trades_count: u32,
    pub open_invested: Decimal,
    pub realized_profit: Decimal,
    pub start_balance: Decimal,
    pub balance_remaining: Decimal,
}

impl SessionSummary {
    pub fn pnl(&self) -> Decimal {
        self.realized_profit - self.open_invested
    }
}

pub struct SessionTracker {
    start_balance: Decimal,
    trades: Vec<TradeRecord>,
    positions: Vec<Position>,
    realized_profit: Decimal,
    results_file: String,
}

impl SessionTracker {
    pub fn new(start_balance: Decimal) -> Self {
        let results_file =
            std::env::var("RESULTS_FILE").unwrap_or_else(|_| DEFAULT_RESULTS_FILE.to_string());
        Self {
            start_balance,
            trades: Vec::new(),
            positions: Vec::new(),
            realized_profit: dec!(0),
            results_file,
        }
    }

    pub fn set_start_balance(&mut self, balance: Decimal) {
        self.start_balance = balance;
    }

    pub fn available_balance(&self) -> Decimal {
        let open_invested: Decimal = self.positions.iter().map(|p| p.amount_usdc).sum();
        self.start_balance + self.realized_profit - open_invested
    }

    pub fn record_trade(
        &mut self,
        mode: &str,
        market: &str,
        side: Side,
        amount_usdc: Decimal,
        price: f64,
        executed: bool,
        condition_id: Option<B256>,
        token_id: U256,
        bought_up: bool,
    ) {
        let side_str = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
            _ => "?",
        };
        let record = TradeRecord {
            timestamp: Utc::now().to_rfc3339(),
            mode: mode.to_string(),
            market: market.to_string(),
            side: side_str.to_string(),
            amount_usdc: amount_usdc.to_string(),
            price,
            executed,
        };
        self.trades.push(record.clone());

        if let Some(cid) = condition_id {
            self.positions.push(Position {
                condition_id: cid,
                token_id,
                market: market.to_string(),
                amount_usdc,
                price,
                bought_up,
            });
        }

        let open_invested: Decimal = self.positions.iter().map(|p| p.amount_usdc).sum();
        let balance = self.start_balance + self.realized_profit - open_invested;
        let pnl = self.realized_profit - open_invested;
        info!(
            "[Results] Trade #{}: {} {} {} USDC @ {:.2} | Open: {} | Realized: {} | Balance: {} | P&L: {} USDC",
            self.trades.len(), side_str, if executed { "EXECUTED" } else { "PAPER" },
            amount_usdc, price, open_invested, self.realized_profit, balance, pnl
        );

        self.persist_trade(&record);
    }

    fn persist_trade(&self, record: &TradeRecord) {
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.results_file)
        {
            if let Ok(json) = serde_json::to_string(record) {
                let _ = writeln!(f, "{}", json);
            }
        }
    }

    pub fn resolve_market(&mut self, condition_id: B256, up_won: bool) {
        let mut to_remove = Vec::new();
        for (i, pos) in self.positions.iter().enumerate() {
            if pos.condition_id == condition_id {
                to_remove.push(i);
                let won = (pos.bought_up && up_won) || (!pos.bought_up && !up_won);
                let price_dec = Decimal::from_str(&pos.price.to_string()).unwrap_or(dec!(1));
                let shares = pos.amount_usdc / price_dec;
                let payout = if won { shares } else { dec!(0) };
                let profit = payout - pos.amount_usdc;
                self.realized_profit += profit;
                info!(
                    "[Results] RESOLVED {}: {} USDC @ {:.2} → {} (profit: {} USDC)",
                    pos.market, pos.amount_usdc, pos.price, if won { "WON" } else { "LOST" }, profit
                );
            }
        }
        for i in to_remove.into_iter().rev() {
            self.positions.remove(i);
        }
    }

    pub fn close_position_sell(&mut self, condition_id: B256, token_id: U256, sell_price: f64) -> bool {
        if let Some(i) = self
            .positions
            .iter()
            .position(|p| p.condition_id == condition_id && p.token_id == token_id)
        {
            let pos = self.positions.remove(i);
            let price_dec = Decimal::from_str(&pos.price.to_string()).unwrap_or(dec!(1));
            let sell_dec = Decimal::from_str(&sell_price.to_string()).unwrap_or(dec!(0));
            let shares = pos.amount_usdc / price_dec;
            let proceeds = shares * sell_dec;
            let profit = proceeds - pos.amount_usdc;
            self.realized_profit += profit;
            info!(
                "[Results] SOLD {}: {} USDC @ {:.2} → {:.2} (profit: {} USDC)",
                pos.market, pos.amount_usdc, pos.price, sell_price, profit
            );
            true
        } else {
            false
        }
    }

    pub fn summary(&self) -> SessionSummary {
        let open_invested: Decimal = self.positions.iter().map(|p| p.amount_usdc).sum();
        let balance_remaining = self.start_balance + self.realized_profit - open_invested;
        SessionSummary {
            trades_count: self.trades.len() as u32,
            open_invested,
            realized_profit: self.realized_profit,
            start_balance: self.start_balance,
            balance_remaining,
        }
    }

    pub fn print_summary(&self) {
        let s = self.summary();
        info!("═══════════════════════════════════════════════════════════════");
        info!("  SESSION RESULTS");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  Trades:          {}", s.trades_count);
        info!("  Open invested:   {} USDC", s.open_invested);
        info!("  Realized profit: {} USDC", s.realized_profit);
        info!("  Start balance:   {} USDC", s.start_balance);
        info!("  Balance:         {} USDC", s.balance_remaining);
        info!("  Net P&L:         {} USDC", s.pnl());
        info!("  Results file:    {}", self.results_file);
        info!("═══════════════════════════════════════════════════════════════");
    }

    pub fn open_condition_ids(&self) -> Vec<B256> {
        self.positions.iter().map(|p| p.condition_id).collect()
    }

    pub fn open_positions(&self) -> Vec<Position> {
        self.positions.clone()
    }
}

pub type SharedTracker = Arc<RwLock<SessionTracker>>;

pub fn create_tracker() -> SharedTracker {
    Arc::new(RwLock::new(SessionTracker::new(dec!(100))))
}
