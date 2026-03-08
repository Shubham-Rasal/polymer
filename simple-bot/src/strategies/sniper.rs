//! Oracle Sniper: buy when our BTC price model diverges from the market.
//!
//! Strategy:
//! - Subscribe to Binance BTC/USDT real-time feed for ground-truth price
//! - Record BTC start_price when entering a market
//! - Compute true P(UP) = Φ(delta / (σ × √(τ/300))) using normal CDF
//! - Compare model probability vs market orderbook price
//! - Trade when: confidence > 90%, edge > 5%, ask < 95%, time > 5s remaining
//! - After market ends, resolve position and move to next market

use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{B256, U256};
use alloy::signers::local::PrivateKeySigner;
use chrono::{self, Utc};
use futures::StreamExt;
use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use polymarket_client_sdk::clob::ws::Client as WsClient;
use polymarket_client_sdk::gamma;
use polymarket_client_sdk::gamma::types::request::EventBySlugRequest;
use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::types::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use crate::auth::AuthenticatedClob;
use crate::feeds::{spawn_feed_collector, SharedFeedState};
use crate::tracker::SharedTracker;
use crate::trading::execute_order;

// ── Oracle risk gates (configurable) ──────────────────────────────────

/// Minimum model probability to consider trading.
const MIN_CONFIDENCE: f64 = 0.90;

/// Minimum edge (model_prob - market_price) to trigger a trade.
const MIN_EDGE: f64 = 0.01;

/// Maximum edge — if our model diverges more than this from the market,
/// the market probably knows something we don't. Skip the trade.
const MAX_EDGE: f64 = 0.15;

/// Don't buy above this price (need profit room: payout is $1).
const MAX_ASK: f64 = 0.95;

/// Don't trade in the final N seconds (execution risk).
const MIN_REMAINING_SECS: i64 = 5;

/// Don't trade until at least this many seconds have elapsed in the window.
/// Backtested: waiting 120s+ dramatically improves accuracy (97%+ at vol=200).
const MIN_ELAPSED_SECS: i64 = 120;

/// BTC volatility parameter per 5-min window (in USD).
/// Higher = more conservative model = only trades on large moves.
/// Backtested optimal: $200 gives 97% accuracy when confident (>90%).
/// At vol=$50, accuracy was only ~88% — insufficient for the payoff structure.
const BTC_5M_VOLATILITY: f64 = 200.0;

/// Sell early if best bid >= buy_price * this threshold (10% profit).
const PROFIT_TAKE_THRESHOLD: f64 = 1.10;

/// How often to log status (seconds).
const STATUS_LOG_INTERVAL_SECS: u64 = 10;

/// Paper mode virtual wallet size (USDC).
const PAPER_WALLET_USDC: f64 = 100.0;

// ── Normal CDF approximation (Abramowitz & Stegun 7.1.26) ────────────

fn erf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    sign * y
}

fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

/// Compute P(UP) given BTC delta, seconds remaining, and volatility.
///
/// P(UP) = Φ(d / (σ × √(τ/300)))
///
/// Clamped to [0.01, 0.99] to avoid degenerate probabilities.
fn compute_model_probability(delta: f64, remaining_secs: f64, volatility: f64) -> f64 {
    if remaining_secs <= 0.0 {
        return if delta > 0.0 { 0.99 } else { 0.01 };
    }
    let remaining_std = volatility * (remaining_secs / 300.0).sqrt();
    if remaining_std <= 0.0 {
        return if delta > 0.0 { 0.99 } else { 0.01 };
    }
    let z = delta / remaining_std;
    normal_cdf(z).clamp(0.01, 0.99)
}

// ── Scanner ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct TargetMarket {
    condition_id: B256,
    yes_token_id: U256,
    no_token_id: U256,
    end_date: Option<chrono::DateTime<Utc>>,
    question: String,
}

fn current_5m_window_ts() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    now - (now % 300)
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
        let end = m.end_date;
        let q = m.question.as_deref().unwrap_or("");
        let outcomes = m.outcomes.as_deref().unwrap_or(&[]);
        let up_idx = outcomes
            .iter()
            .position(|o| o.to_lowercase().contains("up"))
            .unwrap_or(0);
        let yes_token = token_ids[up_idx];
        let no_token = token_ids[1 - up_idx];
        let cond = m.condition_id.unwrap_or_default();
        return Some(TargetMarket {
            condition_id: cond,
            yes_token_id: yes_token,
            no_token_id: no_token,
            end_date: end,
            question: q.to_string(),
        });
    }
    None
}

async fn find_target_by_slug(
    gamma: &GammaClient,
    slug: &str,
) -> anyhow::Result<Option<TargetMarket>> {
    info!("[Scanner] Looking up slug: {}", slug);
    let req = EventBySlugRequest::builder().slug(slug).build();
    let event = match gamma.event_by_slug(&req).await {
        Ok(e) => e,
        Err(e) => {
            warn!("[Scanner] event_by_slug({}) failed: {}", slug, e);
            return Ok(None);
        }
    };
    Ok(extract_target(&event))
}

async fn find_target_market(gamma: &GammaClient) -> anyhow::Result<Option<TargetMarket>> {
    let window_ts = current_5m_window_ts();

    for offset in [0u64, 300] {
        let ts = window_ts + offset;
        let slug = format!("btc-updown-5m-{ts}");
        info!("[Scanner] Trying slug: {}", slug);

        let req = gamma::types::request::EventsRequest::builder()
            .slug(vec![slug.clone()])
            .build();
        match gamma.events(&req).await {
            Ok(events) => {
                for event in &events {
                    if let Some(target) = extract_target(event) {
                        info!(
                            "[Scanner] Found BTC 5-min market: {} (ends {:?})",
                            target.question, target.end_date
                        );
                        return Ok(Some(target));
                    }
                }
            }
            Err(e) => {
                info!("[Scanner] Slug {} not found: {}", slug, e);
            }
        }
    }

    Ok(None)
}

// ── Orderbook helpers ─────────────────────────────────────────────────

fn update_book_state(
    book: &BookUpdate,
    target: &TargetMarket,
    best_bid_yes: &mut Option<f64>,
    best_ask_yes: &mut Option<f64>,
    best_bid_no: &mut Option<f64>,
    best_ask_no: &mut Option<f64>,
) {
    // Bids are sorted low-to-high, best bid = last (highest price)
    let new_bid = book
        .bids
        .last()
        .and_then(|l| l.price.to_string().parse::<f64>().ok());
    // Asks are sorted high-to-low, best ask = last (lowest price)
    let new_ask = book
        .asks
        .last()
        .and_then(|l| l.price.to_string().parse::<f64>().ok());
    if book.asset_id == target.yes_token_id {
        if new_bid.is_some() {
            *best_bid_yes = new_bid;
        }
        if new_ask.is_some() {
            *best_ask_yes = new_ask;
        }
    } else if book.asset_id == target.no_token_id {
        if new_bid.is_some() {
            *best_bid_no = new_bid;
        }
        if new_ask.is_some() {
            *best_ask_no = new_ask;
        }
    }
}

fn infer_winning_side(
    best_bid_yes: Option<f64>,
    best_bid_no: Option<f64>,
    best_ask_yes: Option<f64>,
    best_ask_no: Option<f64>,
) -> bool {
    let mid_yes = best_bid_yes
        .zip(best_ask_yes)
        .map(|(b, a)| (b + a) / 2.0)
        .or(best_bid_yes)
        .or(best_ask_yes)
        .unwrap_or(0.0);
    let mid_no = best_bid_no
        .zip(best_ask_no)
        .map(|(b, a)| (b + a) / 2.0)
        .or(best_bid_no)
        .or(best_ask_no)
        .unwrap_or(0.0);
    mid_yes >= mid_no
}

// ── BTC price helpers ─────────────────────────────────────────────────

/// Read the best available BTC price: prefer Binance, fall back to Chainlink.
async fn read_btc_price(feed_state: &SharedFeedState) -> Option<f64> {
    let feeds = feed_state.read().await;
    feeds.binance_btc.or(feeds.chainlink_btc)
}

/// Wait for any BTC price feed to become available.
/// Polls every 200ms, times out after 15s.
async fn wait_for_btc_price(feed_state: &SharedFeedState) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(price) = read_btc_price(feed_state).await {
            let feeds = feed_state.read().await;
            let source = if feeds.binance_btc.is_some() { "Binance" } else { "Chainlink" };
            info!("[Sniper] BTC feed source: {}", source);
            return price;
        }
        if Instant::now() >= deadline {
            warn!("[Sniper] Timed out waiting for BTC price from any feed");
            return 0.0;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ── Monitor a single market with oracle model ─────────────────────────

async fn monitor_market(
    target: &TargetMarket,
    dry_run: bool,
    effective_size: f64,
    signer: &Option<PrivateKeySigner>,
    auth_clob: &Option<AuthenticatedClob>,
    tracker: &SharedTracker,
    feed_state: &SharedFeedState,
) -> anyhow::Result<()> {
    info!(
        "[Sniper] Target: {} (ends {:?})",
        target.question, target.end_date
    );
    info!(
        "[Sniper] UP token: {}, DOWN token: {}",
        target.yes_token_id, target.no_token_id
    );

    // Capture BTC start price for this market window
    let start_price = wait_for_btc_price(feed_state).await;
    info!(
        "[Sniper] BTC start price: ${:.2} (oracle reference)",
        start_price
    );

    let ws = WsClient::default();
    let asset_ids = vec![target.yes_token_id, target.no_token_id];
    let mut stream = Box::pin(ws.subscribe_orderbook(asset_ids)?);

    let mut best_ask_yes: Option<f64> = None;
    let mut best_ask_no: Option<f64> = None;
    let mut best_bid_yes: Option<f64> = None;
    let mut best_bid_no: Option<f64> = None;

    let end_time = target.end_date.unwrap_or_else(Utc::now);

    info!(
        "[Sniper] Oracle Sniper active — gates: confidence>{:.0}% edge={:.0}%-{:.0}% ask<{:.2} t>{}s elapsed>{}s vol=${:.0}",
        MIN_CONFIDENCE * 100.0,
        MIN_EDGE * 100.0,
        MAX_EDGE * 100.0,
        MAX_ASK,
        MIN_REMAINING_SECS,
        MIN_ELAPSED_SECS,
        BTC_5M_VOLATILITY,
    );
    if dry_run {
        info!("[Sniper] DRY_RUN — will log WOULD BUY only");
    }

    let bal = tracker.read().await.available_balance();
    info!("[Sniper] Current balance: {} USDC", bal);

    let mut last_status_log = Instant::now();
    let mut already_bought = false;
    let mut bought_up: Option<bool> = None;
    let mut bought_price: Option<f64> = None;
    let mut bought_token: Option<U256> = None;
    let mut bought_amount: Option<Decimal> = None;

    while let Some(book_result) = stream.next().await {
        let book = match book_result {
            Ok(b) => b,
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("lagged") {
                    warn!("[Sniper] WebSocket error: {}", e);
                }
                continue;
            }
        };

        let now = Utc::now();
        let remaining = (end_time - now).num_seconds();

        update_book_state(
            &book,
            target,
            &mut best_bid_yes,
            &mut best_ask_yes,
            &mut best_bid_no,
            &mut best_ask_no,
        );

        if remaining <= 0 {
            // Market ended — resolve position
            if already_bought {
                let final_up =
                    infer_winning_side(best_bid_yes, best_bid_no, best_ask_yes, best_ask_no);
                let our_side_won = bought_up.map_or(false, |b| b == final_up);
                let side_str = if bought_up.unwrap_or(true) { "UP" } else { "DOWN" };
                let result_str = if our_side_won { "WON" } else { "LOST" };
                info!(
                    "[Sniper] Market ended. We bought {} at {:.2} → {}",
                    side_str,
                    bought_price.unwrap_or(0.0),
                    result_str
                );
                tracker
                    .write()
                    .await
                    .resolve_market(target.condition_id, final_up);
            } else {
                info!(
                    "[Sniper] Market ended (no position): {}",
                    target.question
                );
            }

            let s = tracker.read().await.summary();
            info!(
                "[Sniper] Balance: {:.2} USDC | P&L: {:.2} USDC | Trades: {}",
                s.balance_remaining, s.realized_profit, s.trades_count
            );
            break;
        }

        // Read current BTC price from feed (Binance preferred, Chainlink fallback)
        let current_btc = match read_btc_price(feed_state).await {
            Some(p) => p,
            None => continue,
        };

        let delta = current_btc - start_price;
        let model_prob_up =
            compute_model_probability(delta, remaining as f64, BTC_5M_VOLATILITY);

        // Determine which side our model favors and the corresponding market ask
        let (model_favors_up, our_prob, market_ask, winning_token) = if model_prob_up > 0.5 {
            let ask = best_ask_yes.unwrap_or(1.0);
            (true, model_prob_up, ask, target.yes_token_id)
        } else {
            let ask = best_ask_no.unwrap_or(1.0);
            (false, 1.0 - model_prob_up, ask, target.no_token_id)
        };

        let edge = our_prob - market_ask;
        let side_str = if model_favors_up { "UP" } else { "DOWN" };

        // ── Periodic status log ───────────────────────────────────────
        if last_status_log.elapsed().as_secs() >= STATUS_LOG_INTERVAL_SECS {
            last_status_log = Instant::now();
            let bal = tracker.read().await.available_balance();
            let pos_str = if already_bought {
                format!(
                    " | HOLDING {}",
                    if bought_up.unwrap_or(true) { "UP" } else { "DOWN" }
                )
            } else {
                String::new()
            };
            info!(
                "[Sniper] t-{}s | btc=${:.2} delta={:+.2} | model={:.1}% market={:.1}% edge={:.1}% | side={} | bal={:.2}{}",
                remaining,
                current_btc,
                delta,
                our_prob * 100.0,
                market_ask * 100.0,
                edge * 100.0,
                side_str,
                bal,
                pos_str,
            );
        }

        // If we already bought, check for early profit-take (10%+)
        if already_bought {
            if let (Some(buy_price), Some(token), Some(amount)) =
                (bought_price, bought_token, bought_amount)
            {
                let current_bid = if bought_up.unwrap_or(true) {
                    best_bid_yes
                } else {
                    best_bid_no
                };
                if let Some(bid) = current_bid {
                    if bid >= buy_price * PROFIT_TAKE_THRESHOLD {
                        let profit_pct = (bid / buy_price - 1.0) * 100.0;
                        let side_label = if bought_up.unwrap_or(true) { "UP" } else { "DOWN" };
                        info!(
                            "[Sniper] PROFIT TAKE: {} bought@{:.2} now@{:.2} (+{:.1}%) — selling early",
                            side_label, buy_price, bid, profit_pct,
                        );

                        let price_dec =
                            Decimal::from_str(&buy_price.to_string()).unwrap_or(dec!(1));
                        let shares = (amount / price_dec).round_dp(6);

                        if dry_run {
                            info!(
                                "[Sniper] WOULD SELL {} shares at {:.2} (+{:.1}% profit)",
                                shares, bid, profit_pct,
                            );
                        } else {
                            let sold = execute_order(
                                signer.as_ref(),
                                auth_clob.as_ref(),
                                token,
                                shares,
                                Side::Sell,
                                OrderType::FOK,
                            )
                            .await;
                            if sold {
                                tracker.write().await.close_position_sell(
                                    target.condition_id,
                                    token,
                                    bid,
                                );
                                let s = tracker.read().await.summary();
                                info!(
                                    "[Sniper] Sold for profit. Balance: {:.2} USDC | P&L: {:.2}",
                                    s.balance_remaining, s.realized_profit,
                                );
                                break;
                            }
                        }
                    }
                }
            }
            continue;
        }

        // ── Risk gates ────────────────────────────────────────────────
        if our_prob < MIN_CONFIDENCE {
            continue;
        }
        if edge < MIN_EDGE {
            continue;
        }
        if edge > MAX_EDGE {
            // Edge too large — our simple model probably disagrees with
            // the market for a reason. Trust the market.
            continue;
        }
        if market_ask >= MAX_ASK || market_ask <= 0.0 {
            continue;
        }
        if remaining <= MIN_REMAINING_SECS {
            continue;
        }
        // Don't trade too early in the window — model needs time to observe BTC movement
        let elapsed = 300 - remaining;
        if elapsed < MIN_ELAPSED_SECS {
            continue;
        }

        // ── All gates passed — TRADE ──────────────────────────────────
        info!(
            "[Sniper] ORACLE TRIGGER: {} | model={:.1}% market={:.1}% edge={:.1}% | btc=${:.2} delta={:+.2} t-{}s",
            side_str,
            our_prob * 100.0,
            market_ask * 100.0,
            edge * 100.0,
            current_btc,
            delta,
            remaining,
        );

        let mut amount_dec = Decimal::from_str(&format!("{:.2}", effective_size))
            .unwrap_or(dec!(1))
            .round_dp(6);

        let balance = if let Some(client) = auth_clob {
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
            info!("[Sniper] Insufficient balance ({} USDC), skipping", balance);
            continue;
        }

        let executed = if dry_run {
            info!(
                "[Sniper] WOULD BUY {} at {:.2} — size={} USDC | model={:.1}% edge={:.1}%",
                side_str, market_ask, amount_dec, our_prob * 100.0, edge * 100.0,
            );
            false
        } else {
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
            "sniper",
            &target.question,
            Side::Buy,
            amount_dec,
            market_ask,
            executed && auth_clob.is_some(),
            Some(target.condition_id),
            winning_token,
            model_favors_up,
        );

        already_bought = true;
        bought_up = Some(model_favors_up);
        bought_price = Some(market_ask);
        bought_token = Some(winning_token);
        bought_amount = Some(amount_dec);

        let s = tracker.read().await.summary();
        info!(
            "[Sniper] Bought {} at {:.2}. Balance: {:.2} USDC | Trades: {}",
            side_str, market_ask, s.balance_remaining, s.trades_count
        );
    }

    Ok(())
}

// ── Main entry point: continuous loop across markets ──────────────────

pub async fn run(
    dry_run: bool,
    size_usdc: f64,
    event_slug: Option<String>,
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
            "[Sniper] Paper mode: 100 USDC wallet, size capped at {:.0}",
            PAPER_WALLET_USDC
        );
    } else if let Some(ref client) = auth_clob {
        if let Ok(resp) = client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
        {
            let bal = (resp.balance / Decimal::from(1_000_000)).round_dp(6);
            tracker.write().await.set_start_balance(bal);
            info!("[Sniper] Live mode: wallet balance {} USDC (synced)", bal);
        }
    }

    info!("═══════════════════════════════════════════════════════════════");
    info!("  ORACLE SNIPER — BTC 5-min Up/Down markets");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  • Model: P(UP) = Φ(delta / (σ × √(τ/300)))");
    info!("  • Trade when: confidence > {:.0}% AND edge {:.0}%-{:.0}%", MIN_CONFIDENCE * 100.0, MIN_EDGE * 100.0, MAX_EDGE * 100.0);
    info!("  • Risk gates: ask < {:.2}, remaining > {}s, elapsed > {}s, vol = ${:.0}/5min", MAX_ASK, MIN_REMAINING_SECS, MIN_ELAPSED_SECS, BTC_5M_VOLATILITY);
    info!("  • Continuously moves to next market after each ends");
    info!("  • Status updates every {}s", STATUS_LOG_INTERVAL_SECS);
    info!("═══════════════════════════════════════════════════════════════");

    // Start BTC price feed (shared across all market cycles)
    info!("[Sniper] Starting Binance BTC feed for oracle model...");
    let feed_state = spawn_feed_collector().await;

    // If user specified a slug, run that one market and exit
    if let Some(ref slug) = event_slug {
        let target = find_target_by_slug(&gamma, slug).await?;
        match target {
            Some(t) => {
                monitor_market(
                    &t,
                    dry_run,
                    effective_size,
                    &signer,
                    &auth_clob,
                    &tracker,
                    &feed_state,
                )
                .await?;
            }
            None => {
                info!("[Sniper] No market found for slug: {}", slug);
            }
        }
        tracker.read().await.print_summary();
        return Ok(());
    }

    // Continuous loop: discover market → monitor → market ends → discover next
    loop {
        info!("[Sniper] Scanning for BTC 5-min market (btc-updown-5m-*)...");
        let target = find_target_market(&gamma).await?;

        match target {
            Some(t) => {
                monitor_market(
                    &t,
                    dry_run,
                    effective_size,
                    &signer,
                    &auth_clob,
                    &tracker,
                    &feed_state,
                )
                .await?;
                info!("[Sniper] Market finished. Looking for next market...");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            None => {
                info!("[Sniper] No active market found. Retrying in 10s...");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}
