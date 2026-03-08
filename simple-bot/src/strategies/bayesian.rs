use std::collections::HashSet;
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use alloy::primitives::B256;
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::gamma::types::request::{
    EventsRequest, MarketsRequest, SearchRequest, SeriesListRequest,
};
use polymarket_client_sdk::gamma::Client as GammaClient;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::info;

use crate::auth::AuthenticatedClob;
use crate::feeds::{spawn_feed_collector, FeedPrices};
use crate::strategies::TradeSignal;
use crate::tracker::SharedTracker;
use crate::trading::execute_order;

// ── Bayesian math ──────────────────────────────────────────────────

struct BayesianUpdater {
    prior: f64,
    min_prob: f64,
    max_prob: f64,
}

impl BayesianUpdater {
    fn new(prior: f64) -> Self {
        Self {
            prior: prior.clamp(0.0, 1.0),
            min_prob: 0.01,
            max_prob: 0.99,
        }
    }

    fn update_sequential(&self, evidences: &[(f64, f64)]) -> f64 {
        let mut posterior = self.prior;
        for (lh, lnh) in evidences {
            let p_not_h = 1.0 - posterior;
            let p_e = lh * posterior + lnh * p_not_h;
            if p_e > 0.0 {
                posterior = (lh * posterior) / p_e;
                posterior = posterior.clamp(self.min_prob, self.max_prob);
            }
        }
        posterior
    }

    fn price_evidence(start_price: f64, current_price: f64) -> (f64, f64) {
        if start_price <= 0.0 {
            return (0.5, 0.5);
        }
        let change_pct = (current_price - start_price) / start_price * 100.0;
        let sensitivity = 8.0;
        let up_score = 1.0 / (1.0 + (-sensitivity * change_pct).exp());
        let down_score = 1.0 - up_score;
        let sum = up_score + down_score;
        ((up_score / sum).max(0.01), (down_score / sum).max(0.01))
    }
}

fn posterior_from_feeds(prior: f64, feed_prices: &[(f64, f64)]) -> f64 {
    let updater = BayesianUpdater::new(prior);
    let evidences: Vec<(f64, f64)> = feed_prices
        .iter()
        .map(|(start, current)| BayesianUpdater::price_evidence(*start, *current))
        .collect();
    updater.update_sequential(&evidences)
}

// ── Kelly criterion ────────────────────────────────────────────────

fn kelly_binary(our_prob: f64, market_price: f64) -> f64 {
    if market_price <= 0.0 || market_price >= 1.0 {
        return 0.0;
    }
    let odds = (1.0 - market_price) / market_price;
    let q = 1.0 - our_prob;
    let f = (odds * our_prob - q) / odds;
    f.max(0.0).min(1.0)
}

fn fractional_kelly(full_kelly: f64, fraction: f64) -> f64 {
    (full_kelly * fraction).min(1.0).max(0.0)
}

fn kelly_to_amount(kelly_frac: f64, bankroll: Decimal, min_bet: Decimal, max_bet: Decimal) -> Decimal {
    let amount = bankroll * Decimal::try_from(kelly_frac).unwrap_or(dec!(0));
    amount.max(min_bet).min(max_bet).min(bankroll)
}

// ── Strategy constants ─────────────────────────────────────────────

const MIN_DISCREPANCY: f64 = 0.03;
const KELLY_FRACTION: f64 = 0.25;
const MIN_MARKET_PRICE: f64 = 0.05;
const MAX_BUY_PRICE: f64 = 0.95;
const MAX_SIGNALS_PER_ITERATION: usize = 2;
const MAX_POSITION_FRACTION: f64 = 0.10;
const MIN_REQUIRED_FEEDS: usize = 1;
const CRYPTO_SEARCH_TERMS: &[&str] = &["bitcoin", "btc", "btc-updown-5m", "btc-updown-15m"];
const SCAN_INTERVAL_SECS: u64 = 10;
const START_PRICE_RESET_SECS: u64 = 300;
const PROFIT_THRESHOLD: f64 = 1.02;

// ── Market discovery ───────────────────────────────────────────────

fn filter_btc_updown_market(m: &polymarket_client_sdk::gamma::types::response::Market) -> bool {
    if m.clob_token_ids.is_none() || m.clob_token_ids.as_ref().unwrap().is_empty() {
        return false;
    }
    let q = m.question.as_deref().unwrap_or("");
    let slug = m.slug.as_deref().unwrap_or("");
    let is_btc = q.to_lowercase().contains("bitcoin") || q.to_lowercase().contains("btc");
    let is_5m_or_15m = q.to_lowercase().contains("5 min")
        || q.to_lowercase().contains("5-min")
        || q.to_lowercase().contains("5m")
        || q.to_lowercase().contains("15 min")
        || q.to_lowercase().contains("15-min")
        || q.to_lowercase().contains("15m")
        || slug.to_lowercase().contains("5m")
        || slug.to_lowercase().contains("15m");
    is_btc && is_5m_or_15m
}

fn collect_from_events(
    events: &[polymarket_client_sdk::gamma::types::response::Event],
    all_markets: &mut Vec<polymarket_client_sdk::gamma::types::response::Market>,
    require_btc_updown_slug: bool,
) {
    for event in events {
        let event_slug = event.slug.as_deref().unwrap_or("");
        if require_btc_updown_slug && !event_slug.contains("btc-updown") {
            continue;
        }
        if let Some(markets) = &event.markets {
            for m in markets {
                if m.closed.unwrap_or(true) {
                    continue;
                }
                if filter_btc_updown_market(m) && !all_markets.iter().any(|x| x.id == m.id) {
                    all_markets.push(m.clone());
                }
            }
        }
    }
}

async fn find_crypto_markets(
    gamma: &GammaClient,
) -> anyhow::Result<Vec<polymarket_client_sdk::gamma::types::response::Market>> {
    use polymarket_client_sdk::gamma::types::response::Market;
    let mut all_markets: Vec<Market> = Vec::new();

    for series_slug in &["btc-updown-5m", "btc-updown-15m"] {
        let req = SeriesListRequest::builder()
            .slug(vec![(*series_slug).to_string()])
            .closed(false)
            .limit(5)
            .build();
        if let Ok(series_list) = gamma.series(&req).await {
            for s in series_list {
                if let Some(events) = &s.events {
                    collect_from_events(events, &mut all_markets, false);
                }
            }
        }
    }

    for tag in &["5M", "15M", "btc-updown", "crypto"] {
        let req = EventsRequest::builder()
            .tag_slug(tag.to_string())
            .closed(false)
            .limit(50)
            .build();
        if let Ok(events) = gamma.events(&req).await {
            collect_from_events(&events, &mut all_markets, true);
        }
    }

    for term in CRYPTO_SEARCH_TERMS {
        let search = SearchRequest::builder().q(term.to_string()).build();
        if let Ok(results) = gamma.search(&search).await {
            if let Some(events) = results.events {
                collect_from_events(&events, &mut all_markets, true);
            }
        }
    }

    let req = MarketsRequest::builder().closed(false).limit(200).build();
    if let Ok(markets) = gamma.markets(&req).await {
        for m in markets {
            if filter_btc_updown_market(&m) && !all_markets.iter().any(|x| x.id == m.id) {
                all_markets.push(m);
            }
        }
    }

    Ok(all_markets)
}

// ── Signal evaluation ──────────────────────────────────────────────

fn evaluate_markets(
    markets: &[polymarket_client_sdk::gamma::types::response::Market],
    current: &FeedPrices,
    start: &FeedPrices,
    bankroll: Decimal,
    held_condition_ids: &HashSet<B256>,
) -> Vec<TradeSignal> {
    let mut signals = Vec::new();
    let evidence = current.as_evidence_btc_only(start);
    if evidence.len() < MIN_REQUIRED_FEEDS {
        info!(
            "Skipping evaluation: need >= {} feeds (have {})",
            MIN_REQUIRED_FEEDS,
            evidence.len()
        );
        return signals;
    }

    let per_market_max = Decimal::try_from(MAX_POSITION_FRACTION).unwrap_or(dec!(0.1)) * bankroll;

    for market in markets {
        if signals.len() >= MAX_SIGNALS_PER_ITERATION {
            break;
        }

        let token_ids = match &market.clob_token_ids {
            Some(ids) if ids.len() >= 2 => ids,
            _ => continue,
        };

        if let Some(cid) = market.condition_id {
            if held_condition_ids.contains(&cid) {
                continue;
            }
        }

        let outcome_prices = market.outcome_prices.as_deref().unwrap_or(&[]);
        let best_bid = market.best_bid.and_then(|d| d.to_string().parse::<f64>().ok());
        let outcomes = market.outcomes.as_deref().unwrap_or(&[]);

        let (up_idx, yes_token_id, yes_price) = if outcomes.len() >= 2 {
            let idx = outcomes
                .iter()
                .position(|o| o.to_lowercase().contains("up"))
                .unwrap_or(0);
            let price = outcome_prices
                .get(idx)
                .and_then(|d| d.to_string().parse::<f64>().ok())
                .or(best_bid)
                .or_else(|| outcome_prices.first().and_then(|d| d.to_string().parse::<f64>().ok()))
                .unwrap_or(0.5);
            (idx, token_ids[idx], price)
        } else {
            (
                0,
                token_ids[0],
                outcome_prices
                    .first()
                    .and_then(|d| d.to_string().parse::<f64>().ok())
                    .unwrap_or(0.5),
            )
        };

        let market_price = yes_price.min(1.0).max(0.0);
        if market_price < MIN_MARKET_PRICE || market_price > MAX_BUY_PRICE {
            continue;
        }

        let prior = market_price;
        let our_prob = posterior_from_feeds(prior, &evidence);

        if signals.is_empty() {
            info!(
                "Bayesian posterior P(UP|evidence) = {:.1}% (prior/market was {:.1}%, {} feeds)",
                our_prob * 100.0,
                prior * 100.0,
                evidence.len()
            );
        }

        let discrepancy = our_prob - market_price;
        if discrepancy > MIN_DISCREPANCY {
            let kelly = kelly_binary(our_prob, market_price);
            let kelly_frac = fractional_kelly(kelly, KELLY_FRACTION);
            let amount = kelly_to_amount(kelly_frac, bankroll, dec!(1), per_market_max);
            if amount >= dec!(1) {
                info!(
                    "  → BUY UP: posterior={:.1}% vs market={:.1}% (edge={:.1}%), kelly={:.2}%, amount={} USDC",
                    our_prob * 100.0, market_price * 100.0, discrepancy * 100.0, kelly_frac * 100.0, amount
                );
                signals.push(TradeSignal {
                    token_id: yes_token_id,
                    condition_id: market.condition_id,
                    market_question: market.question.clone().unwrap_or_else(|| "?".to_string()),
                    side: Side::Buy,
                    bought_up: true,
                    our_prob,
                    market_price,
                    discrepancy_pct: discrepancy * 100.0,
                    kelly_fraction: kelly_frac,
                    amount_usdc: amount,
                });
            }
        } else if -discrepancy > MIN_DISCREPANCY {
            let down_prob = 1.0 - our_prob;
            let down_price = 1.0 - market_price;
            let no_token_id = if outcomes.len() >= 2 {
                token_ids[1 - up_idx]
            } else {
                token_ids[1]
            };
            let kelly = kelly_binary(down_prob, down_price);
            let kelly_frac = fractional_kelly(kelly, KELLY_FRACTION);
            let amount = kelly_to_amount(kelly_frac, bankroll, dec!(1), per_market_max);
            if amount >= dec!(1) {
                info!(
                    "  → BUY DOWN: posterior_down={:.1}% vs market_down={:.1}% (edge={:.1}%), kelly={:.2}%, amount={} USDC",
                    down_prob * 100.0, down_price * 100.0, (-discrepancy) * 100.0, kelly_frac * 100.0, amount
                );
                signals.push(TradeSignal {
                    token_id: no_token_id,
                    condition_id: market.condition_id,
                    market_question: market.question.clone().unwrap_or_else(|| "?".to_string()),
                    side: Side::Buy,
                    bought_up: false,
                    our_prob: down_prob,
                    market_price: down_price,
                    discrepancy_pct: (-discrepancy) * 100.0,
                    kelly_fraction: kelly_frac,
                    amount_usdc: amount,
                });
            }
        }
    }
    signals
}

// ── Resolution / profitable-sell helpers ───────────────────────────

async fn check_resolutions(gamma: &GammaClient, tracker: &SharedTracker) {
    let ids: Vec<_> = tracker.read().await.open_condition_ids();
    if ids.is_empty() {
        return;
    }
    let req = MarketsRequest::builder()
        .condition_ids(ids.clone())
        .closed(true)
        .limit(ids.len() as i32)
        .build();
    let Ok(markets) = gamma.markets(&req).await else {
        return;
    };
    for m in markets {
        let Some(cond) = m.condition_id else { continue };
        let Some(prices) = m.outcome_prices.as_ref() else {
            continue;
        };
        if prices.len() < 2 {
            continue;
        }
        let p0: f64 = prices[0].to_string().parse().unwrap_or(0.0);
        let p1: f64 = prices[1].to_string().parse().unwrap_or(0.0);
        let up_won = p0 > 0.9;
        let down_won = p1 > 0.9;
        if up_won || down_won {
            tracker.write().await.resolve_market(cond, up_won);
        }
    }
}

async fn check_profitable_sells(
    markets: &[polymarket_client_sdk::gamma::types::response::Market],
    tracker: &SharedTracker,
    signer: &Option<PrivateKeySigner>,
    auth_clob: &Option<AuthenticatedClob>,
) {
    let positions = tracker.read().await.open_positions();
    if positions.is_empty() {
        return;
    }

    for pos in positions {
        let market = match markets
            .iter()
            .find(|m| m.condition_id == Some(pos.condition_id))
        {
            Some(m) => m,
            None => continue,
        };
        let prices = match market.outcome_prices.as_ref() {
            Some(p) if p.len() >= 2 => p,
            _ => continue,
        };
        let idx = if pos.bought_up { 0 } else { 1 };
        let current_price: f64 = prices[idx].to_string().parse().unwrap_or(0.0);
        if current_price < pos.price * PROFIT_THRESHOLD {
            continue;
        }

        let price_dec = Decimal::from_str(&pos.price.to_string()).unwrap_or(dec!(1));
        let shares = (pos.amount_usdc / price_dec).round_dp(6);
        if shares < Decimal::from_str("0.01").unwrap_or_default() {
            continue;
        }

        let executed = execute_order(
            signer.as_ref(),
            auth_clob.as_ref(),
            pos.token_id,
            shares,
            Side::Sell,
            OrderType::FOK,
        )
        .await;

        if executed {
            tracker
                .write()
                .await
                .close_position_sell(pos.condition_id, pos.token_id, current_price);
        }
    }
}

// ── Main loop ──────────────────────────────────────────────────────

async fn fetch_wallet_balance(auth_clob: &Option<AuthenticatedClob>) -> Option<Decimal> {
    let client = auth_clob.as_ref()?;
    let resp = client
        .balance_allowance(BalanceAllowanceRequest::default())
        .await
        .ok()?;
    // API returns micro-USDC (6 decimals), convert to USDC
    let usdc = resp.balance / Decimal::from(1_000_000);
    Some(usdc.round_dp(6))
}

pub async fn run(
    signer: Option<PrivateKeySigner>,
    auth_clob: Option<AuthenticatedClob>,
    tracker: SharedTracker,
) -> anyhow::Result<()> {
    info!("═══════════════════════════════════════════════════════════════");
    info!("  BAYESIAN TRADING BOT — Polymarket 5/15-min Bitcoin markets");
    info!("═══════════════════════════════════════════════════════════════");
    info!("Strategy: P(H|E) = [P(E|H) × P(H)] / P(E)");
    info!("  • Prior = market price, quarter-Kelly sizing");
    info!("  • Buy UP or DOWN when edge > 3%");
    info!("  • Max 2 trades/iteration, max 10% bankroll/market");
    info!("═══════════════════════════════════════════════════════════════");

    let feed_state = spawn_feed_collector().await;
    info!("Taking snapshot of starting prices...");
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut start_prices = feed_state.read().await.clone();
    let mut last_start_reset = Instant::now();

    let gamma = GammaClient::default();
    let mut iteration = 0u64;
    let mut live_balance_synced = false;

    loop {
        iteration += 1;
        let current = feed_state.read().await.clone();

        if last_start_reset.elapsed() >= Duration::from_secs(START_PRICE_RESET_SECS) {
            start_prices = current.clone();
            last_start_reset = Instant::now();
            info!(
                "[Iteration {}] Reference prices reset (every {}s)",
                iteration, START_PRICE_RESET_SECS
            );
        }

        if !current.has_minimum_data() {
            info!(
                "[Iteration {}] Waiting for feed data — have {} of 4 feeds",
                iteration,
                current.feed_count()
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        info!(
            "[Iteration {}] PRICE FEEDS: BTC {:?} {:?}, ETH {:?} {:?}",
            iteration,
            current.binance_btc,
            current.chainlink_btc,
            current.binance_eth,
            current.chainlink_eth
        );

        match find_crypto_markets(&gamma).await {
            Ok(markets) => {
                if markets.is_empty() && iteration % 5 == 1 {
                    info!(
                        "[Iteration {}] No btc-updown-5m/15m markets found",
                        iteration
                    );
                }

                let balance = if let Some(wallet_bal) = fetch_wallet_balance(&auth_clob).await {
                    if !live_balance_synced {
                        tracker.write().await.set_start_balance(wallet_bal);
                        live_balance_synced = true;
                        info!("[Live] Wallet balance: {} USDC (synced)", wallet_bal);
                    }
                    wallet_bal
                } else {
                    tracker.read().await.available_balance()
                };

                let held_ids: HashSet<_> =
                    tracker.read().await.open_condition_ids().into_iter().collect();
                let signals = evaluate_markets(&markets, &current, &start_prices, balance, &held_ids);

                let mut balance_remaining = balance;
                for sig in &signals {
                    let mut amount = sig.amount_usdc.round_dp(6);
                    if amount > balance_remaining {
                        amount = balance_remaining.round_dp(6);
                    }
                    if amount < dec!(1) {
                        continue;
                    }
                    info!(
                        "  ★★★ TRADE SIGNAL ★★★ Market: {} ({} USDC)",
                        sig.market_question, amount
                    );

                    let executed = execute_order(
                        signer.as_ref(),
                        auth_clob.as_ref(),
                        sig.token_id,
                        amount,
                        sig.side.clone(),
                        OrderType::FOK,
                    )
                    .await;

                    if executed {
                        balance_remaining -= amount;
                    }
                    tracker.write().await.record_trade(
                        "bayesian",
                        &sig.market_question,
                        sig.side.clone(),
                        amount,
                        sig.market_price,
                        executed && auth_clob.is_some(),
                        sig.condition_id,
                        sig.token_id,
                        sig.bought_up,
                    );
                }

                check_resolutions(&gamma, &tracker).await;
                check_profitable_sells(&markets, &tracker, &signer, &auth_clob).await;

                if signals.is_empty() {
                    info!(
                        "[Iteration {}] No signals (held={} positions)",
                        iteration,
                        held_ids.len()
                    );
                } else {
                    let s = tracker.read().await.summary();
                    info!(
                        "[Results] Session: {} trades, {} open, {} realized, {} balance",
                        s.trades_count, s.open_invested, s.realized_profit, s.balance_remaining
                    );
                }
            }
            Err(e) => info!("[Iteration {}] Gamma error: {}", iteration, e),
        }

        tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
    }
}
