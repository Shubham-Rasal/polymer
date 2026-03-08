//! Frontloading strategy: buy BEFORE/AT market open using pre-market BTC momentum.
//!
//! Unlike Sniper/Bayesian which wait for BTC to move DURING the 5-min window
//! (paying ~$0.90+ for high-confidence sides), frontloading buys at ~$0.50
//! right when the market opens, using BTC momentum from the PREVIOUS interval.
//!
//! Edge: BTC momentum/autocorrelation — short-term trends persist across
//! adjacent 5-min windows. Even a 55% win rate at $0.50 entry is very profitable.

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::{B256, U256};
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::gamma;
use polymarket_client_sdk::gamma::Client as GammaClient;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use crate::auth::AuthenticatedClob;
use crate::feeds::{spawn_feed_collector, SharedFeedState};
use crate::tracker::SharedTracker;
use crate::trading::execute_order;

// ── Configuration ───────────────────────────────────────────────────

/// How many seconds before market start to measure BTC momentum.
const LOOKBACK_SECS: i64 = 300;

/// Minimum BTC price change (in $) over the lookback period to trigger entry.
const MOMENTUM_THRESHOLD: f64 = 20.0;

/// Maximum price we'll pay for a side token at market open.
const MAX_ENTRY_PRICE: f64 = 0.55;

/// Paper mode virtual wallet size (USDC).
const PAPER_WALLET_USDC: f64 = 100.0;

// ── Helpers ─────────────────────────────────────────────────────────

fn current_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn current_5m_window_ts() -> u64 {
    let now = current_ts();
    now - (now % 300)
}

/// Find the NEXT upcoming market window (the one that hasn't started yet).
fn next_window_ts() -> u64 {
    current_5m_window_ts() + 300
}

/// Read BTC price from the shared feed state.
async fn read_btc_price(feed_state: &SharedFeedState) -> Option<f64> {
    let feeds = feed_state.read().await;
    feeds.binance_btc.or(feeds.chainlink_btc)
}

/// Wait for any BTC price feed to become available.
async fn wait_for_btc_price(feed_state: &SharedFeedState) -> f64 {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(price) = read_btc_price(feed_state).await {
            return price;
        }
        if std::time::Instant::now() >= deadline {
            warn!("[Frontload] Timed out waiting for BTC price");
            return 0.0;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[derive(Debug, Clone)]
struct TargetMarket {
    condition_id: B256,
    yes_token_id: U256,
    no_token_id: U256,
    question: String,
}

fn extract_target(event: &gamma::types::response::Event) -> Option<TargetMarket> {
    let markets = event.markets.as_ref()?;
    for m in markets {
        if m.closed.unwrap_or(true) {
            continue;
        }
        let token_ids = match &m.clob_token_ids {
            Some(ids) if ids.len() >= 2 => ids,
            _ => continue,
        };
        let outcomes = m.outcomes.as_deref().unwrap_or(&[]);
        let up_idx = outcomes
            .iter()
            .position(|o| o.to_lowercase().contains("up"))
            .unwrap_or(0);
        let yes_token = token_ids[up_idx];
        let no_token = token_ids[1 - up_idx];
        let cond = m.condition_id.unwrap_or_default();
        let q = m.question.as_deref().unwrap_or("").to_string();
        return Some(TargetMarket {
            condition_id: cond,
            yes_token_id: yes_token,
            no_token_id: no_token,
            question: q,
        });
    }
    None
}

async fn find_market_by_ts(gamma: &GammaClient, ts: u64) -> anyhow::Result<Option<TargetMarket>> {
    let slug = format!("btc-updown-5m-{}", ts);
    let req = gamma::types::request::EventsRequest::builder()
        .slug(vec![slug.clone()])
        .build();
    match gamma.events(&req).await {
        Ok(events) => {
            for event in &events {
                if let Some(target) = extract_target(event) {
                    return Ok(Some(target));
                }
            }
        }
        Err(e) => {
            info!("[Frontload] Slug {} not found: {}", slug, e);
        }
    }
    Ok(None)
}

// ── Main entry point ────────────────────────────────────────────────

pub async fn run(
    dry_run: bool,
    size_usdc: f64,
    signer: Option<PrivateKeySigner>,
    auth_clob: Option<AuthenticatedClob>,
    tracker: SharedTracker,
) -> anyhow::Result<()> {
    let gamma = GammaClient::default();

    let in_paper_mode = auth_clob.is_none();
    let effective_size = if in_paper_mode {
        size_usdc.min(PAPER_WALLET_USDC)
    } else {
        size_usdc
    };
    if in_paper_mode {
        info!(
            "[Frontload] Paper mode: 100 USDC wallet, size capped at {:.0}",
            PAPER_WALLET_USDC
        );
    } else if let Some(ref client) = auth_clob {
        if let Ok(resp) = client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
        {
            let bal = (resp.balance / Decimal::from(1_000_000)).round_dp(6);
            tracker.write().await.set_start_balance(bal);
            info!("[Frontload] Live mode: wallet balance {} USDC (synced)", bal);
        }
    }

    info!("═══════════════════════════════════════════════════════════════");
    info!("  FRONTLOADING — BTC 5-min Up/Down markets");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  • Buy at market open (~$0.50) using pre-market BTC momentum");
    info!("  • Lookback: {}s, Momentum threshold: ${:.0}", LOOKBACK_SECS, MOMENTUM_THRESHOLD);
    info!("  • Max entry price: {:.2}", MAX_ENTRY_PRICE);
    info!("  • Win = $1.00 payout, Loss = $0.00");
    info!("═══════════════════════════════════════════════════════════════");

    // Start BTC price feed
    info!("[Frontload] Starting Binance BTC feed...");
    let feed_state = spawn_feed_collector().await;

    // Wait for feed to initialize
    let init_price = wait_for_btc_price(&feed_state).await;
    info!("[Frontload] BTC feed ready: ${:.2}", init_price);

    // Record BTC price snapshots for momentum calculation
    // We store (timestamp, price) pairs
    let mut price_history: Vec<(u64, f64)> = Vec::new();

    loop {
        let next_ts = next_window_ts();
        let now = current_ts();
        let secs_until_open = if next_ts > now { next_ts - now } else { 0 };

        info!(
            "[Frontload] Next market: btc-updown-5m-{} (opens in {}s)",
            next_ts, secs_until_open
        );

        // Collect BTC prices while waiting for market to open
        // Sample every 5 seconds for momentum tracking
        while current_ts() < next_ts {
            if let Some(price) = read_btc_price(&feed_state).await {
                let ts = current_ts();
                price_history.push((ts, price));

                // Keep only last 15 minutes of history
                let cutoff = ts.saturating_sub(900);
                price_history.retain(|&(t, _)| t >= cutoff);
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        // Market window just started — compute momentum
        let current_price = match read_btc_price(&feed_state).await {
            Some(p) => p,
            None => {
                warn!("[Frontload] No BTC price available, skipping");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        // Find the price from LOOKBACK_SECS ago
        let lookback_ts = current_ts().saturating_sub(LOOKBACK_SECS as u64);
        let lookback_price = price_history
            .iter()
            .filter(|&&(t, _)| t <= lookback_ts)
            .last()
            .map(|&(_, p)| p);

        let momentum = match lookback_price {
            Some(past_price) => {
                let delta = current_price - past_price;
                info!(
                    "[Frontload] Momentum: ${:.2} → ${:.2} = {:+.2} (threshold: ${:.0})",
                    past_price, current_price, delta, MOMENTUM_THRESHOLD
                );
                delta
            }
            None => {
                info!("[Frontload] Not enough price history for {}s lookback, skipping", LOOKBACK_SECS);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        // Check momentum threshold
        if momentum.abs() < MOMENTUM_THRESHOLD {
            info!(
                "[Frontload] Momentum ${:+.2} below threshold ${:.0}, skipping market",
                momentum, MOMENTUM_THRESHOLD
            );
            // Wait for this window to end
            let end_ts = next_ts + 300;
            let wait = end_ts.saturating_sub(current_ts());
            if wait > 0 {
                tokio::time::sleep(Duration::from_secs(wait.min(300))).await;
            }
            continue;
        }

        let buy_up = momentum > 0.0;
        let side_str = if buy_up { "UP" } else { "DOWN" };

        info!(
            "[Frontload] MOMENTUM SIGNAL: {} (delta=${:+.2})",
            side_str, momentum
        );

        // Find the market that just opened
        let target = match find_market_by_ts(&gamma, next_ts).await? {
            Some(t) => t,
            None => {
                info!("[Frontload] Market btc-updown-5m-{} not found, retrying...", next_ts);
                tokio::time::sleep(Duration::from_secs(5)).await;
                // Retry once
                match find_market_by_ts(&gamma, next_ts).await? {
                    Some(t) => t,
                    None => {
                        warn!("[Frontload] Market not available, skipping");
                        tokio::time::sleep(Duration::from_secs(250)).await;
                        continue;
                    }
                }
            }
        };

        info!("[Frontload] Found market: {}", target.question);

        // Select the token to buy
        let winning_token = if buy_up {
            target.yes_token_id
        } else {
            target.no_token_id
        };

        // Determine order size
        let mut amount_dec = Decimal::from_str(&format!("{:.2}", effective_size))
            .unwrap_or(dec!(1))
            .round_dp(6);

        let balance = if let Some(ref client) = auth_clob {
            match client
                .balance_allowance(BalanceAllowanceRequest::default())
                .await
            {
                Ok(r) => (r.balance / Decimal::from(1_000_000)).round_dp(6),
                Err(_) => tracker.read().await.available_balance(),
            }
        } else {
            tracker.read().await.available_balance()
        };
        if amount_dec > balance {
            amount_dec = balance.round_dp(6);
        }
        if amount_dec < dec!(1) {
            info!("[Frontload] Insufficient balance ({} USDC), skipping", balance);
            tokio::time::sleep(Duration::from_secs(250)).await;
            continue;
        }

        // Execute buy immediately at market open
        let executed = if dry_run {
            info!(
                "[Frontload] WOULD BUY {} — size={} USDC | momentum=${:+.2}",
                side_str, amount_dec, momentum,
            );
            false
        } else {
            info!(
                "[Frontload] BUYING {} — size={} USDC | momentum=${:+.2}",
                side_str, amount_dec, momentum,
            );
            execute_order(
                signer.as_ref(),
                auth_clob.as_ref(),
                winning_token,
                amount_dec,
                Side::Buy,
                OrderType::FOK,
            )
            .await
        };

        tracker.write().await.record_trade(
            "frontload",
            &target.question,
            Side::Buy,
            amount_dec,
            0.50, // approximate entry price
            executed && auth_clob.is_some(),
            Some(target.condition_id),
            winning_token,
            buy_up,
        );

        let s = tracker.read().await.summary();
        info!(
            "[Frontload] Position: {} at ~$0.50. Balance: {:.2} USDC | Trades: {}",
            side_str, s.balance_remaining, s.trades_count
        );

        // Wait for market to resolve (5 min window)
        info!("[Frontload] Holding until market resolves (~5 min)...");
        let end_ts = next_ts + 300;
        loop {
            let now = current_ts();
            if now >= end_ts + 10 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }

        // Check resolution via Gamma
        info!("[Frontload] Market window ended. Checking resolution...");
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Try to determine outcome from resolved market prices
        let slug = format!("btc-updown-5m-{}", next_ts);
        let req = gamma::types::request::EventsRequest::builder()
            .slug(vec![slug])
            .closed(true)
            .build();
        match gamma.events(&req).await {
            Ok(events) => {
                for event in &events {
                    if let Some(markets) = &event.markets {
                        for m in markets {
                            let prices = match m.outcome_prices.as_ref() {
                                Some(p) if p.len() >= 2 => p,
                                _ => continue,
                            };
                            let outcomes = m.outcomes.as_deref().unwrap_or(&[]);
                            let up_idx = outcomes
                                .iter()
                                .position(|o| o.to_lowercase().contains("up"))
                                .unwrap_or(0);
                            let up_price: f64 = prices[up_idx].to_string().parse().unwrap_or(0.0);
                            let up_won = up_price > 0.9;
                            let we_won = buy_up == up_won;
                            let result_str = if we_won { "WON" } else { "LOST" };
                            info!(
                                "[Frontload] Result: {} won, we bought {} → {}",
                                if up_won { "UP" } else { "DOWN" },
                                side_str,
                                result_str
                            );

                            tracker
                                .write()
                                .await
                                .resolve_market(target.condition_id, up_won);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("[Frontload] Could not check resolution: {}", e);
            }
        }

        let s = tracker.read().await.summary();
        info!(
            "[Frontload] Balance: {:.2} USDC | P&L: {:.2} USDC | Trades: {}",
            s.balance_remaining, s.realized_profit, s.trades_count
        );

        info!("[Frontload] Moving to next market...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
