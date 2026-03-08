use std::time::Duration;

use alloy::primitives::U256;
use alloy::signers::local::PrivateKeySigner;
use futures::StreamExt;
use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::ws::Client as WsClient;
use polymarket_client_sdk::gamma::types::request::MarketsRequest;
use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::types::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use crate::auth::AuthenticatedClob;
use crate::orderbook::{self, OrderbookState};
use crate::tracker::SharedTracker;
use crate::trading::execute_order;

// ── Detector constants ─────────────────────────────────────────────

const ARB_THRESHOLD: f64 = 0.02;
const MIN_POSITION_USDC: f64 = 5.0;
const MAX_POSITION_USDC: f64 = 100.0;
const LIQUIDITY_CAP_FRAC: f64 = 0.5;

#[derive(Debug, Clone)]
enum ArbitrageSide {
    BuyBoth,
    SellBoth,
}

#[derive(Debug, Clone)]
struct ArbitrageSignal {
    side: ArbitrageSide,
    yes_token: U256,
    no_token: U256,
    size_usdc: Decimal,
    profit_per_unit: f64,
    market_id: String,
}

fn check_arbitrage(
    state: &OrderbookState,
    yes_token: U256,
    no_token: U256,
    market_id: &str,
) -> Option<ArbitrageSignal> {
    let ask_yes = orderbook::get_best_ask(state, yes_token);
    let ask_no = orderbook::get_best_ask(state, no_token);
    let bid_yes = orderbook::get_best_bid(state, yes_token);
    let bid_no = orderbook::get_best_bid(state, no_token);

    if let (Some((ask_y, size_y)), Some((ask_n, size_n))) = (ask_yes, ask_no) {
        let sum = ask_y + ask_n;
        if sum < 1.0 - ARB_THRESHOLD {
            let cost_yes: f64 = size_y * ask_y;
            let cost_no: f64 = size_n * ask_n;
            let max_usdc = cost_yes.min(cost_no) * LIQUIDITY_CAP_FRAC;
            let size_usdc = max_usdc.min(MAX_POSITION_USDC).max(MIN_POSITION_USDC);
            if size_usdc >= MIN_POSITION_USDC {
                let profit_per_unit = 1.0 - sum;
                info!(
                    "[Signal] Buy both arbitrage: market={}, ask_yes={}, ask_no={}, sum={}, profit={}",
                    market_id, ask_y, ask_n, sum, profit_per_unit
                );
                return Some(ArbitrageSignal {
                    side: ArbitrageSide::BuyBoth,
                    yes_token,
                    no_token,
                    size_usdc: Decimal::try_from(size_usdc).unwrap_or(dec!(5)),
                    profit_per_unit,
                    market_id: market_id.to_string(),
                });
            }
        }
    }

    if let (Some((bid_y, size_y)), Some((bid_n, size_n))) = (bid_yes, bid_no) {
        let sum = bid_y + bid_n;
        if sum > 1.0 + ARB_THRESHOLD {
            let value_yes: f64 = size_y * bid_y;
            let value_no: f64 = size_n * bid_n;
            let max_usdc = value_yes.min(value_no) * LIQUIDITY_CAP_FRAC;
            let size_usdc = max_usdc.min(MAX_POSITION_USDC).max(MIN_POSITION_USDC);
            let profit_per_unit = sum - 1.0;
            info!(
                "[Signal] Sell both arbitrage: market={}, bid_yes={}, bid_no={}, sum={}, profit={}",
                market_id, bid_y, bid_n, sum, profit_per_unit
            );
            return Some(ArbitrageSignal {
                side: ArbitrageSide::SellBoth,
                yes_token,
                no_token,
                size_usdc: Decimal::try_from(size_usdc).unwrap_or(dec!(5)),
                profit_per_unit,
                market_id: market_id.to_string(),
            });
        }
    }

    None
}

// ── Executor ───────────────────────────────────────────────────────

async fn execute_arbitrage(
    signer: Option<&PrivateKeySigner>,
    auth_clob: Option<&AuthenticatedClob>,
    signal: &ArbitrageSignal,
    tracker: &SharedTracker,
) {
    let half = (signal.size_usdc / dec!(2)).round_dp(6);
    let (side, side_str) = match signal.side {
        ArbitrageSide::BuyBoth => (Side::Buy, "BUY"),
        ArbitrageSide::SellBoth => (Side::Sell, "SELL"),
    };

    info!(
        "[Execute] {} both legs: {} USDC each, profit/unit={:.4}",
        side_str, half, signal.profit_per_unit
    );

    let (ok_yes, ok_no) = tokio::join!(
        execute_order(signer, auth_clob, signal.yes_token, half, side.clone(), OrderType::FOK),
        execute_order(signer, auth_clob, signal.no_token, half, side.clone(), OrderType::FOK),
    );

    let executed = ok_yes && ok_no && auth_clob.is_some();
    tracker.write().await.record_trade(
        "arbitrage",
        &signal.market_id,
        side.clone(),
        signal.size_usdc,
        signal.profit_per_unit,
        executed,
        None,
        signal.yes_token,
        true,
    );
}

// ── Main loop ──────────────────────────────────────────────────────

pub async fn run(
    signer: Option<PrivateKeySigner>,
    auth_clob: Option<AuthenticatedClob>,
    tracker: SharedTracker,
) -> anyhow::Result<()> {
    info!("═══════════════════════════════════════════════════════════════");
    info!("  ARBITRAGE BOT — Single-condition YES+NO != $1");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  • Threshold: {}% deviation from $1", ARB_THRESHOLD * 100.0);
    info!("  • Position: ${}-${} per signal", MIN_POSITION_USDC, MAX_POSITION_USDC);
    info!("  • Both legs submitted in parallel (FOK)");
    info!("═══════════════════════════════════════════════════════════════");

    if let Some(ref client) = auth_clob {
        if let Ok(resp) = client.balance_allowance(BalanceAllowanceRequest::default()).await {
            let bal = (resp.balance / Decimal::from(1_000_000)).round_dp(6);
            tracker.write().await.set_start_balance(bal);
            info!("[Live] Wallet balance: {} USDC (synced)", bal);
        }
    }

    let gamma = GammaClient::default();

    loop {
        info!("[Arbitrage] Scanning for binary markets...");
        let req = MarketsRequest::builder().closed(false).limit(100).build();
        let markets = match gamma.markets(&req).await {
            Ok(m) => m,
            Err(e) => {
                warn!("[Arbitrage] Gamma error: {}", e);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        let mut targets: Vec<(U256, U256, String)> = Vec::new();
        for m in &markets {
            let token_ids = match &m.clob_token_ids {
                Some(ids) if ids.len() >= 2 => ids,
                _ => continue,
            };
            let market_id = m.id.clone();
            targets.push((token_ids[0], token_ids[1], market_id));
        }

        if targets.is_empty() {
            info!("[Arbitrage] No binary markets found, retrying...");
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        }

        let asset_ids: Vec<U256> = targets
            .iter()
            .flat_map(|(y, n, _)| vec![*y, *n])
            .collect();

        info!("[Arbitrage] Monitoring {} markets via WebSocket", targets.len());
        let ws = WsClient::default();
        let mut stream = match ws.subscribe_orderbook(asset_ids) {
            Ok(s) => Box::pin(s),
            Err(e) => {
                warn!("[Arbitrage] WebSocket subscribe failed: {}", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let state = orderbook::new_state();
        let mut checks = 0u64;

        while let Some(book_result) = stream.next().await {
            let book = match book_result {
                Ok(b) => b,
                Err(e) => {
                    warn!("[Arbitrage] WebSocket error: {}", e);
                    continue;
                }
            };

            orderbook::apply_update(&state, &book);
            checks += 1;

            if checks % 100 == 0 {
                for (yes, no, mid) in &targets {
                    if let Some(signal) = check_arbitrage(&state, *yes, *no, mid) {
                        execute_arbitrage(
                            signer.as_ref(),
                            auth_clob.as_ref(),
                            &signal,
                            &tracker,
                        )
                        .await;
                    }
                }
            }
        }

        warn!("[Arbitrage] WebSocket stream ended, reconnecting...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
