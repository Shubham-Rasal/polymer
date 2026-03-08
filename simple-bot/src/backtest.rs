//! Backtesting framework for Oracle Sniper and Bayesian strategies.
//!
//! Replays historical BTC 5-min markets using:
//! - Gamma API for market metadata + resolution (which side won)
//! - Binance REST API for historical BTC 1-min klines
//! - The same oracle model (normal CDF) used in live trading
//! - The same Bayesian model (sigmoid + sequential update) used in live trading
//!
//! Includes parameter sweep mode to find optimal strategy parameters.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use polymarket_client_sdk::gamma::Client as GammaClient;
use polymarket_client_sdk::gamma::types::request::EventsRequest;
use tracing::info;

use crate::cli::{BacktestStrategy, MarketWindow};

// ── Oracle model (same math as sniper.rs) ─────────────────────────────

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

fn compute_model_probability(delta: f64, remaining_secs: f64, volatility: f64, window_secs: f64) -> f64 {
    if remaining_secs <= 0.0 {
        return if delta > 0.0 { 0.99 } else { 0.01 };
    }
    let remaining_std = volatility * (remaining_secs / window_secs).sqrt();
    if remaining_std <= 0.0 {
        return if delta > 0.0 { 0.99 } else { 0.01 };
    }
    let z = delta / remaining_std;
    normal_cdf(z).clamp(0.01, 0.99)
}

// ── Strategy parameters (all configurable for sweep) ──────────────────

#[derive(Debug, Clone, Copy)]
struct StrategyParams {
    volatility: f64,
    min_confidence: f64,
    max_ask: f64,
    min_elapsed_secs: i64,   // only trade after this many seconds into the window
    market_lag: f64,          // assumed lag of market behind oracle (0.01 = 1%)
    size: f64,
}

impl StrategyParams {
    fn default_with(volatility: f64, size: f64) -> Self {
        Self {
            volatility,
            min_confidence: 0.90,
            max_ask: 0.95,
            min_elapsed_secs: 0,
            market_lag: 0.03,
            size,
        }
    }
}

// ── Bayesian model (same math as bayesian.rs) ───────────────────────────

fn bayesian_price_evidence(start_price: f64, current_price: f64, sensitivity: f64) -> (f64, f64) {
    if start_price <= 0.0 {
        return (0.5, 0.5);
    }
    let change_pct = (current_price - start_price) / start_price * 100.0;
    let up_score = 1.0 / (1.0 + (-sensitivity * change_pct).exp());
    let down_score = 1.0 - up_score;
    let sum = up_score + down_score;
    ((up_score / sum).max(0.01), (down_score / sum).max(0.01))
}

fn bayesian_posterior(prior: f64, evidences: &[(f64, f64)]) -> f64 {
    let mut posterior = prior.clamp(0.01, 0.99);
    for &(lh, lnh) in evidences {
        let p_not_h = 1.0 - posterior;
        let p_e = lh * posterior + lnh * p_not_h;
        if p_e > 0.0 {
            posterior = (lh * posterior) / p_e;
            posterior = posterior.clamp(0.01, 0.99);
        }
    }
    posterior
}

fn kelly_binary(our_prob: f64, market_price: f64) -> f64 {
    if market_price <= 0.0 || market_price >= 1.0 {
        return 0.0;
    }
    let odds = (1.0 - market_price) / market_price;
    let q = 1.0 - our_prob;
    let f = (odds * our_prob - q) / odds;
    f.max(0.0).min(1.0)
}

// ── Bayesian strategy parameters ─────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct BayesianParams {
    sensitivity: f64,      // sigmoid steepness (default 8)
    min_discrepancy: f64,  // min edge to trigger trade (default 0.03)
    kelly_fraction: f64,   // fraction of Kelly to use (default 0.25)
    min_elapsed_secs: i64, // only trade after this many seconds
    max_ask: f64,          // don't buy above this price
    market_lag: f64,       // simulated market lag behind model
    size: f64,             // base bet size (fixed for comparison)
}

impl BayesianParams {
    fn default_with(size: f64) -> Self {
        Self {
            sensitivity: 8.0,
            min_discrepancy: 0.03,
            kelly_fraction: 0.25,
            min_elapsed_secs: 0,
            max_ask: 0.95,
            market_lag: 0.03,
            size,
        }
    }
}

// ── Data types ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct MarketRecord {
    slug: String,
    start_ts: i64, // unix seconds
    up_won: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Candle {
    open_time_ms: i64,
    open: f64,
    close: f64,
}

#[derive(Debug, Clone)]
enum TradeOutcome {
    Win,
    Loss,
}

#[derive(Debug, Clone)]
struct SimTrade {
    market_slug: String,
    side_up: bool,
    model_prob: f64,
    buy_price: f64,
    edge: f64,
    entry_elapsed: i64,   // seconds elapsed when we entered
    outcome: TradeOutcome,
    pnl: f64,
}

struct SimResult {
    trade: Option<SimTrade>,
    model_correct_when_confident: Option<bool>,
}

// ── Model accuracy record (no trading, just measures model quality) ───

struct AccuracyRecord {
    elapsed_secs: i64,
    model_prob: f64,   // confidence (always >= 0.5)
    correct: bool,
}

// ── Data fetching ─────────────────────────────────────────────────────

async fn fetch_markets(gamma: &GammaClient, days: u64, market: MarketWindow) -> anyhow::Result<Vec<MarketRecord>> {
    let window_secs = market.secs() as u64;
    let slug_prefix = market.slug_prefix();
    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let start_ts = now_ts - days * 86400;
    let start_ts = start_ts - (start_ts % window_secs);
    let windows_per_day = 86400 / window_secs;

    info!(
        "[Backtest] Fetching {} markets from last {} days ({} windows)...",
        slug_prefix,
        days,
        days * windows_per_day,
    );

    let mut markets = Vec::new();
    let mut slugs_batch = Vec::new();
    let mut ts = start_ts;

    let mut all_timestamps = Vec::new();
    while ts < now_ts {
        all_timestamps.push(ts);
        ts += window_secs;
    }

    for chunk in all_timestamps.chunks(50) {
        slugs_batch.clear();
        for &t in chunk {
            slugs_batch.push(format!("{}-{}", slug_prefix, t));
        }

        let req = EventsRequest::builder()
            .slug(slugs_batch.clone())
            .closed(true)
            .limit(100)
            .build();

        match gamma.events(&req).await {
            Ok(events) => {
                for event in &events {
                    let event_slug = event.slug.as_deref().unwrap_or("");
                    let event_markets = match &event.markets {
                        Some(m) => m,
                        None => continue,
                    };
                    let window_ts: i64 = event_slug
                        .rsplit('-')
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    if window_ts == 0 {
                        continue;
                    }

                    for m in event_markets {
                        let prices = match m.outcome_prices.as_ref() {
                            Some(p) if p.len() >= 2 => p,
                            _ => continue,
                        };
                        let p0: f64 = prices[0].to_string().parse().unwrap_or(0.0);
                        let p1: f64 = prices[1].to_string().parse().unwrap_or(0.0);
                        if p0 < 0.9 && p1 < 0.9 {
                            continue;
                        }

                        let outcomes = m.outcomes.as_deref().unwrap_or(&[]);
                        let up_idx = outcomes
                            .iter()
                            .position(|o| o.to_lowercase().contains("up"))
                            .unwrap_or(0);
                        let up_price: f64 = prices[up_idx].to_string().parse().unwrap_or(0.0);
                        let up_won = up_price > 0.9;

                        markets.push(MarketRecord {
                            slug: event_slug.to_string(),
                            start_ts: window_ts,
                            up_won,
                        });
                    }
                }
            }
            Err(_) => {}
        }
    }

    markets.sort_by_key(|m| m.start_ts);
    markets.dedup_by_key(|m| m.slug.clone());

    info!("[Backtest] Found {} resolved markets", markets.len());
    Ok(markets)
}

async fn fetch_btc_klines(start_ts_secs: i64) -> anyhow::Result<Vec<Candle>> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let start_ms = (start_ts_secs - 900) * 1000;

    let total_minutes = (now_ms - start_ms) / 60_000;
    info!(
        "[Backtest] Fetching BTC 1-min klines from Binance (~{} candles)...",
        total_minutes,
    );

    let client = reqwest::Client::new();
    let mut candles = Vec::new();
    let mut cursor = start_ms;

    while cursor < now_ms {
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&startTime={}&limit=1000",
            cursor
        );

        let resp: Vec<Vec<serde_json::Value>> = client
            .get(&url)
            .send()
            .await?
            .json()
            .await?;

        if resp.is_empty() {
            break;
        }

        for kline in &resp {
            let open_time_ms = kline[0].as_i64().unwrap_or(0);
            let open: f64 = kline[1]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            let close: f64 = kline[4]
                .as_str()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0.0);
            candles.push(Candle {
                open_time_ms,
                open,
                close,
            });
        }

        cursor = candles.last().map(|c| c.open_time_ms + 60_000).unwrap_or(now_ms);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    info!("[Backtest] Fetched {} candles", candles.len());
    Ok(candles)
}

// ── Simulation ────────────────────────────────────────────────────────

fn build_candle_index(candles: &[Candle]) -> HashMap<i64, usize> {
    let mut map = HashMap::with_capacity(candles.len());
    for (i, c) in candles.iter().enumerate() {
        let ts_secs = c.open_time_ms / 1000;
        map.insert(ts_secs, i);
    }
    map
}

fn price_at(candles: &[Candle], index: &HashMap<i64, usize>, ts_secs: i64) -> Option<f64> {
    let aligned = ts_secs - (ts_secs % 60);
    if let Some(&i) = index.get(&aligned) {
        return Some(candles[i].close);
    }
    if let Some(&i) = index.get(&(aligned - 60)) {
        return Some(candles[i].close);
    }
    None
}

/// Collect model accuracy data at every check point for a market (no trading logic).
fn collect_accuracy(
    market: &MarketRecord,
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    volatility: f64,
    window_secs: i64,
) -> Vec<AccuracyRecord> {
    let start_price = match price_at(candles, candle_index, market.start_ts) {
        Some(p) => p,
        None => return vec![],
    };

    let mut records = Vec::new();
    // Check every 30 seconds for finer granularity
    for elapsed in (30..=(window_secs - 5)).step_by(30) {
        let check_ts = market.start_ts + elapsed as i64;
        let remaining = window_secs as f64 - elapsed as f64;
        if remaining < 5.0 {
            break;
        }

        let current_price = match price_at(candles, candle_index, check_ts) {
            Some(p) => p,
            None => continue,
        };

        let delta = current_price - start_price;
        let model_prob_up = compute_model_probability(delta, remaining, volatility, window_secs as f64);

        let (favors_up, our_prob) = if model_prob_up > 0.5 {
            (true, model_prob_up)
        } else {
            (false, 1.0 - model_prob_up)
        };

        let correct = favors_up == market.up_won;
        records.push(AccuracyRecord {
            elapsed_secs: elapsed as i64,
            model_prob: our_prob,
            correct,
        });
    }
    records
}

/// Simulate the oracle model on one market with given parameters.
fn simulate_market(
    market: &MarketRecord,
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    params: &StrategyParams,
    window_secs: i64,
) -> SimResult {
    let start_price = match price_at(candles, candle_index, market.start_ts) {
        Some(p) => p,
        None => return SimResult { trade: None, model_correct_when_confident: None },
    };

    let mut best_confident_correct: Option<bool> = None;

    // Check every 30 seconds for finer resolution
    let check_times: Vec<i64> = (30..=(window_secs - 5)).step_by(30).collect();

    for &elapsed in &check_times {
        let remaining = window_secs as f64 - elapsed as f64;

        // Don't trade before min_elapsed
        if elapsed < params.min_elapsed_secs {
            continue;
        }
        // Don't trade in final 5 seconds
        if remaining < 5.0 {
            break;
        }

        let check_ts = market.start_ts + elapsed;
        let current_price = match price_at(candles, candle_index, check_ts) {
            Some(p) => p,
            None => continue,
        };

        let delta = current_price - start_price;
        let model_prob_up = compute_model_probability(delta, remaining, params.volatility, window_secs as f64);

        let (favors_up, our_prob) = if model_prob_up > 0.5 {
            (true, model_prob_up)
        } else {
            (false, 1.0 - model_prob_up)
        };

        // Track model accuracy when confident
        if our_prob >= params.min_confidence {
            let model_correct = favors_up == market.up_won;
            best_confident_correct = Some(model_correct);
        }

        // Confidence gate
        if our_prob < params.min_confidence {
            continue;
        }

        // Simulate market price: market trails our model by `market_lag`
        // Lag decreases as we get closer to expiry (market catches up)
        let time_factor = remaining / window_secs as f64;
        let lag = params.market_lag * time_factor;
        let simulated_ask = (our_prob - lag).clamp(0.01, 0.99);

        // Max ask gate
        if simulated_ask >= params.max_ask {
            continue;
        }

        // Edge: must be positive (we're getting a discount)
        let edge = our_prob - simulated_ask;
        if edge < 0.005 {
            continue; // need at least tiny positive edge
        }

        // Trade triggered
        let we_won = favors_up == market.up_won;
        let pnl = if we_won {
            (params.size / simulated_ask) - params.size
        } else {
            -params.size
        };

        let outcome = if we_won {
            TradeOutcome::Win
        } else {
            TradeOutcome::Loss
        };

        return SimResult {
            trade: Some(SimTrade {
                market_slug: market.slug.clone(),
                side_up: favors_up,
                model_prob: our_prob,
                buy_price: simulated_ask,
                edge,
                entry_elapsed: elapsed,
                outcome,
                pnl,
            }),
            model_correct_when_confident: best_confident_correct,
        };
    }

    SimResult {
        trade: None,
        model_correct_when_confident: best_confident_correct,
    }
}

// ── Sweep result ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SweepResult {
    params: StrategyParams,
    trades: usize,
    wins: usize,
    win_rate: f64,
    net_pnl: f64,
    avg_buy_price: f64,
    pnl_per_trade: f64,
}

fn run_single_config(
    markets: &[MarketRecord],
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    params: &StrategyParams,
    window_secs: i64,
) -> SweepResult {
    let mut total_trades = 0usize;
    let mut wins = 0usize;
    let mut net_pnl = 0.0f64;
    let mut sum_buy_price = 0.0f64;

    for market in markets {
        if price_at(candles, candle_index, market.start_ts).is_none() {
            continue;
        }

        let result = simulate_market(market, candles, candle_index, params, window_secs);
        if let Some(trade) = result.trade {
            total_trades += 1;
            sum_buy_price += trade.buy_price;
            net_pnl += trade.pnl;
            if matches!(trade.outcome, TradeOutcome::Win) {
                wins += 1;
            }
        }
    }

    let win_rate = if total_trades > 0 { wins as f64 / total_trades as f64 } else { 0.0 };
    let avg_buy_price = if total_trades > 0 { sum_buy_price / total_trades as f64 } else { 0.0 };
    let pnl_per_trade = if total_trades > 0 { net_pnl / total_trades as f64 } else { 0.0 };

    SweepResult {
        params: *params,
        trades: total_trades,
        wins,
        win_rate,
        net_pnl,
        avg_buy_price,
        pnl_per_trade,
    }
}

// ── Bayesian simulation ──────────────────────────────────────────────

fn simulate_market_bayesian(
    market: &MarketRecord,
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    params: &BayesianParams,
    window_secs: i64,
) -> Option<SimTrade> {
    let start_price = price_at(candles, candle_index, market.start_ts)?;

    // Collect all evidence observations up front (we'll build cumulatively)
    let check_times: Vec<i64> = (30..=(window_secs - 5)).step_by(30).collect();

    let mut all_evidences: Vec<(f64, f64)> = Vec::new();

    for &elapsed in &check_times {
        let remaining = window_secs as f64 - elapsed as f64;

        if remaining < 5.0 {
            break;
        }

        let check_ts = market.start_ts + elapsed;
        let current_price = match price_at(candles, candle_index, check_ts) {
            Some(p) => p,
            None => continue,
        };

        // Add new evidence from this time step
        let evidence = bayesian_price_evidence(start_price, current_price, params.sensitivity);
        all_evidences.push(evidence);

        // Don't trade before min_elapsed
        if elapsed < params.min_elapsed_secs {
            continue;
        }

        // Compute posterior using all accumulated evidence
        // Simulated prior = simulated market price (lagged)
        let time_factor = remaining / window_secs as f64;
        let lag = params.market_lag * time_factor;

        // Use 0.5 as the uninformed prior (50/50)
        let posterior_up = bayesian_posterior(0.5, &all_evidences);

        // Determine which side the model favors
        let (favors_up, our_prob) = if posterior_up > 0.5 {
            (true, posterior_up)
        } else {
            (false, 1.0 - posterior_up)
        };

        // Simulated market price (trails our model by lag)
        let simulated_market = (our_prob - lag).clamp(0.01, 0.99);

        // Discrepancy check
        let discrepancy = our_prob - simulated_market;
        if discrepancy < params.min_discrepancy {
            continue;
        }

        // Max ask gate
        if simulated_market >= params.max_ask {
            continue;
        }

        // Kelly sizing for P&L calculation
        let kelly = kelly_binary(our_prob, simulated_market);
        let kelly_frac = (kelly * params.kelly_fraction).max(0.0).min(1.0);

        if kelly_frac < 0.01 {
            continue;
        }

        // Trade triggered — use fixed size for P&L comparison
        let we_won = favors_up == market.up_won;
        let pnl = if we_won {
            (params.size / simulated_market) - params.size
        } else {
            -params.size
        };

        let outcome = if we_won {
            TradeOutcome::Win
        } else {
            TradeOutcome::Loss
        };

        return Some(SimTrade {
            market_slug: market.slug.clone(),
            side_up: favors_up,
            model_prob: our_prob,
            buy_price: simulated_market,
            edge: discrepancy,
            entry_elapsed: elapsed,
            outcome,
            pnl,
        });
    }

    None
}

// ── Bayesian sweep result ────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BayesianSweepResult {
    params: BayesianParams,
    trades: usize,
    wins: usize,
    win_rate: f64,
    net_pnl: f64,
    avg_buy_price: f64,
    pnl_per_trade: f64,
}

fn run_single_bayesian_config(
    markets: &[MarketRecord],
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    params: &BayesianParams,
    window_secs: i64,
) -> BayesianSweepResult {
    let mut total_trades = 0usize;
    let mut wins = 0usize;
    let mut net_pnl = 0.0f64;
    let mut sum_buy_price = 0.0f64;

    for market in markets {
        if price_at(candles, candle_index, market.start_ts).is_none() {
            continue;
        }

        if let Some(trade) = simulate_market_bayesian(market, candles, candle_index, params, window_secs) {
            total_trades += 1;
            sum_buy_price += trade.buy_price;
            net_pnl += trade.pnl;
            if matches!(trade.outcome, TradeOutcome::Win) {
                wins += 1;
            }
        }
    }

    let win_rate = if total_trades > 0 { wins as f64 / total_trades as f64 } else { 0.0 };
    let avg_buy_price = if total_trades > 0 { sum_buy_price / total_trades as f64 } else { 0.0 };
    let pnl_per_trade = if total_trades > 0 { net_pnl / total_trades as f64 } else { 0.0 };

    BayesianSweepResult {
        params: *params,
        trades: total_trades,
        wins,
        win_rate,
        net_pnl,
        avg_buy_price,
        pnl_per_trade,
    }
}

fn run_bayesian_backtest(
    markets: &[MarketRecord],
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    markets_with_data: usize,
    size: f64,
    window_secs: i64,
) -> Option<BayesianSweepResult> {
    info!("");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  BAYESIAN STRATEGY BACKTEST + PARAMETER SWEEP");
    info!("═══════════════════════════════════════════════════════════════");

    // ── Phase 1: Default config baseline ──────────────────────────
    let default_params = BayesianParams::default_with(size);
    let default_result = run_single_bayesian_config(markets, candles, candle_index, &default_params, window_secs);
    info!("");
    info!("  ── DEFAULT CONFIG BASELINE ──");
    info!("  sensitivity={}, min_discrepancy={:.0}%, kelly_frac={:.0}%, max_ask={:.0}%",
        default_params.sensitivity, default_params.min_discrepancy * 100.0,
        default_params.kelly_fraction * 100.0, default_params.max_ask * 100.0);
    info!("  Trades: {} | Win rate: {:.1}% | Net P&L: {:+.2} | $/trade: {:+.4}",
        default_result.trades, default_result.win_rate * 100.0,
        default_result.net_pnl, default_result.pnl_per_trade);

    // ── Phase 2: Parameter sweep ──────────────────────────────────
    info!("");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  BAYESIAN PARAMETER SWEEP");
    info!("═══════════════════════════════════════════════════════════════");

    let sensitivities = [4.0, 8.0, 12.0, 16.0, 20.0];
    let min_discrepancies = [0.01, 0.03, 0.05, 0.08];
    let min_elapsed_options: [i64; 4] = [0, 120, 180, 240];
    let market_lags = [0.02, 0.03, 0.05];
    let max_asks = [0.85, 0.90, 0.95];

    let total_combos = sensitivities.len() * min_discrepancies.len()
        * min_elapsed_options.len() * market_lags.len() * max_asks.len();
    info!("  Testing {} parameter combinations...", total_combos);

    let mut results: Vec<BayesianSweepResult> = Vec::new();

    for &sens in &sensitivities {
        for &disc in &min_discrepancies {
            for &min_elapsed in &min_elapsed_options {
                for &lag in &market_lags {
                    for &max_ask in &max_asks {
                        let params = BayesianParams {
                            sensitivity: sens,
                            min_discrepancy: disc,
                            kelly_fraction: 0.25, // fixed for sweep
                            min_elapsed_secs: min_elapsed,
                            max_ask,
                            market_lag: lag,
                            size,
                        };

                        let result = run_single_bayesian_config(markets, candles, candle_index, &params, window_secs);

                        if result.trades >= 20 {
                            results.push(result);
                        }
                    }
                }
            }
        }
    }

    // Sort by net P&L descending
    results.sort_by(|a, b| b.net_pnl.partial_cmp(&a.net_pnl).unwrap());

    info!("");
    info!("  ── TOP 20 BAYESIAN CONFIGURATIONS BY NET P&L ──");
    info!("  {:>5} {:>5} {:>5} {:>5} {:>5} | {:>5} {:>5} {:>8} {:>8} {:>8}",
        "sens", "disc", "maxA", "minT", "lag", "trd", "win%", "avgPrc", "netPnL", "$/trd");
    info!("  {}", "-".repeat(85));

    for r in results.iter().take(20) {
        info!(
            "  {:>5.0} {:>4.0}% {:>4.0}% {:>4}s {:>4.0}% | {:>5} {:>4.1}% {:>8.4} {:>+8.2} {:>+8.4}",
            r.params.sensitivity,
            r.params.min_discrepancy * 100.0,
            r.params.max_ask * 100.0,
            r.params.min_elapsed_secs,
            r.params.market_lag * 100.0,
            r.trades,
            r.win_rate * 100.0,
            r.avg_buy_price,
            r.net_pnl,
            r.pnl_per_trade,
        );
    }

    // Bottom 5
    info!("");
    info!("  ── BOTTOM 5 (worst) ──");
    for r in results.iter().rev().take(5) {
        info!(
            "  {:>5.0} {:>4.0}% {:>4.0}% {:>4}s {:>4.0}% | {:>5} {:>4.1}% {:>8.4} {:>+8.2} {:>+8.4}",
            r.params.sensitivity,
            r.params.min_discrepancy * 100.0,
            r.params.max_ask * 100.0,
            r.params.min_elapsed_secs,
            r.params.market_lag * 100.0,
            r.trades,
            r.win_rate * 100.0,
            r.avg_buy_price,
            r.net_pnl,
            r.pnl_per_trade,
        );
    }

    // ── Phase 3: Detailed report for best config ──────────────────
    if let Some(best) = results.first() {
        info!("");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  BEST BAYESIAN CONFIGURATION — DETAILED REPORT");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  sensitivity={}, min_discrepancy={:.0}%, max_ask={:.0}%, min_elapsed={}s, lag={:.0}%",
            best.params.sensitivity,
            best.params.min_discrepancy * 100.0,
            best.params.max_ask * 100.0,
            best.params.min_elapsed_secs,
            best.params.market_lag * 100.0,
        );

        // Re-run to get individual trades
        let mut trades: Vec<SimTrade> = Vec::new();
        for market in markets {
            if price_at(candles, candle_index, market.start_ts).is_none() {
                continue;
            }
            if let Some(trade) = simulate_market_bayesian(market, candles, candle_index, &best.params, window_secs) {
                trades.push(trade);
            }
        }

        let wins: Vec<&SimTrade> = trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Win)).collect();
        let losses: Vec<&SimTrade> = trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Loss)).collect();
        let gross_profit: f64 = wins.iter().map(|t| t.pnl).sum();
        let gross_loss: f64 = losses.iter().map(|t| t.pnl).sum();

        info!("");
        info!("  Markets traded:    {:>6}  ({:.1}% of {})",
            trades.len(),
            trades.len() as f64 / markets_with_data as f64 * 100.0,
            markets_with_data);
        info!("  Wins:              {:>6}  ({:.1}%)", wins.len(), best.win_rate * 100.0);
        info!("  Losses:            {:>6}  ({:.1}%)", losses.len(), (1.0 - best.win_rate) * 100.0);
        info!("");
        info!("  Avg buy price:     {:.4}", best.avg_buy_price);
        info!("  Gross profit:     {:>+10.2}", gross_profit);
        info!("  Gross loss:       {:>+10.2}", gross_loss);
        info!("  Net P&L:          {:>+10.2}  (${:+.4}/trade)", best.net_pnl, best.pnl_per_trade);

        // Entry timing distribution
        let mut timing_dist: HashMap<i64, (usize, usize)> = HashMap::new();
        for t in &trades {
            let bucket = (t.entry_elapsed / 60) * 60;
            let entry = timing_dist.entry(bucket).or_insert((0, 0));
            entry.0 += 1;
            if matches!(t.outcome, TradeOutcome::Win) {
                entry.1 += 1;
            }
        }
        let mut timing_keys: Vec<i64> = timing_dist.keys().copied().collect();
        timing_keys.sort();

        info!("");
        info!("  ── Entry Timing Distribution ──");
        for &k in &timing_keys {
            let (total, w) = timing_dist[&k];
            let wr = if total > 0 { w as f64 / total as f64 * 100.0 } else { 0.0 };
            info!("  t={}s: {} trades, {:.1}% win rate", k, total, wr);
        }

        // Sample trades
        info!("");
        info!("  ── Sample Trades (first 15) ──");
        for (i, t) in trades.iter().take(15).enumerate() {
            let side = if t.side_up { "UP  " } else { "DOWN" };
            let result = match t.outcome {
                TradeOutcome::Win => "WIN ",
                TradeOutcome::Loss => "LOSS",
            };
            info!(
                "  {:>2}. {} {} model={:.1}% ask={:.2} edge={:.1}% t={}s → {} pnl={:+.2}",
                i + 1,
                &t.market_slug[..t.market_slug.len().min(30)],
                side,
                t.model_prob * 100.0,
                t.buy_price,
                t.edge * 100.0,
                t.entry_elapsed,
                result,
                t.pnl,
            );
        }

        // Suggested constants for bayesian.rs
        info!("");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  SUGGESTED BAYESIAN.RS CONSTANTS");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  const SENSITIVITY: f64 = {:.1};  // (was 8.0)", best.params.sensitivity);
        info!("  const MIN_DISCREPANCY: f64 = {:.2};  // (was 0.03)", best.params.min_discrepancy);
        info!("  const MAX_BUY_PRICE: f64 = {:.2};  // (was 0.95)", best.params.max_ask);
        info!("═══════════════════════════════════════════════════════════════");
    }

    results.into_iter().next()
}

// ── Frontload simulation ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct FrontloadParams {
    lookback_secs: i64,      // how far back to measure momentum (60-600)
    momentum_threshold: f64, // min BTC $ change to trigger entry (5-100)
    entry_price: f64,        // simulated entry price near market open (0.48-0.55)
    size: f64,               // bet size
}

fn simulate_market_frontload(
    market: &MarketRecord,
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    params: &FrontloadParams,
) -> Option<SimTrade> {
    // Get BTC price at market start
    let price_at_start = price_at(candles, candle_index, market.start_ts)?;

    // Get BTC price lookback_secs before market start
    let lookback_ts = market.start_ts - params.lookback_secs;
    let price_at_lookback = price_at(candles, candle_index, lookback_ts)?;

    // Compute momentum delta
    let momentum = price_at_start - price_at_lookback;

    // Only trade if momentum exceeds threshold
    if momentum.abs() < params.momentum_threshold {
        return None;
    }

    // Buy the direction of momentum
    let buy_up = momentum > 0.0;

    // Evaluate against actual outcome
    let we_won = buy_up == market.up_won;
    let pnl = if we_won {
        (params.size / params.entry_price) - params.size
    } else {
        -params.size
    };

    let outcome = if we_won {
        TradeOutcome::Win
    } else {
        TradeOutcome::Loss
    };

    Some(SimTrade {
        market_slug: market.slug.clone(),
        side_up: buy_up,
        model_prob: 0.50, // no model — pure momentum
        buy_price: params.entry_price,
        edge: 0.0, // not applicable for frontloading
        entry_elapsed: 0, // entry at market open
        outcome,
        pnl,
    })
}

#[derive(Debug, Clone)]
struct FrontloadSweepResult {
    params: FrontloadParams,
    trades: usize,
    wins: usize,
    win_rate: f64,
    net_pnl: f64,
    pnl_per_trade: f64,
}

fn run_single_frontload_config(
    markets: &[MarketRecord],
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    params: &FrontloadParams,
) -> FrontloadSweepResult {
    let mut total_trades = 0usize;
    let mut wins = 0usize;
    let mut net_pnl = 0.0f64;

    for market in markets {
        if let Some(trade) = simulate_market_frontload(market, candles, candle_index, params) {
            total_trades += 1;
            net_pnl += trade.pnl;
            if matches!(trade.outcome, TradeOutcome::Win) {
                wins += 1;
            }
        }
    }

    let win_rate = if total_trades > 0 { wins as f64 / total_trades as f64 } else { 0.0 };
    let pnl_per_trade = if total_trades > 0 { net_pnl / total_trades as f64 } else { 0.0 };

    FrontloadSweepResult {
        params: *params,
        trades: total_trades,
        wins,
        win_rate,
        net_pnl,
        pnl_per_trade,
    }
}

fn run_frontload_backtest(
    markets: &[MarketRecord],
    candles: &[Candle],
    candle_index: &HashMap<i64, usize>,
    markets_with_data: usize,
    size: f64,
) -> Option<FrontloadSweepResult> {
    info!("");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  FRONTLOAD STRATEGY BACKTEST + PARAMETER SWEEP");
    info!("═══════════════════════════════════════════════════════════════");

    // Default config baseline
    let default_params = FrontloadParams {
        lookback_secs: 300,
        momentum_threshold: 20.0,
        entry_price: 0.50,
        size,
    };
    let default_result = run_single_frontload_config(markets, candles, candle_index, &default_params);
    info!("");
    info!("  ── DEFAULT CONFIG BASELINE ──");
    info!("  lookback={}s, threshold=${:.0}, entry_price={:.2}",
        default_params.lookback_secs, default_params.momentum_threshold, default_params.entry_price);
    info!("  Trades: {} | Win rate: {:.1}% | Net P&L: {:+.2} | $/trade: {:+.4}",
        default_result.trades, default_result.win_rate * 100.0,
        default_result.net_pnl, default_result.pnl_per_trade);

    // Parameter sweep
    info!("");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  FRONTLOAD PARAMETER SWEEP");
    info!("═══════════════════════════════════════════════════════════════");

    let lookbacks: [i64; 5] = [60, 120, 180, 300, 600];
    let thresholds = [5.0, 10.0, 20.0, 50.0, 100.0];
    let entry_prices = [0.48, 0.50, 0.52, 0.55];

    let total_combos = lookbacks.len() * thresholds.len() * entry_prices.len();
    info!("  Testing {} parameter combinations...", total_combos);

    let mut results: Vec<FrontloadSweepResult> = Vec::new();

    for &lookback in &lookbacks {
        for &threshold in &thresholds {
            for &entry_price in &entry_prices {
                let params = FrontloadParams {
                    lookback_secs: lookback,
                    momentum_threshold: threshold,
                    entry_price,
                    size,
                };

                let result = run_single_frontload_config(markets, candles, candle_index, &params);

                if result.trades >= 10 {
                    results.push(result);
                }
            }
        }
    }

    // Sort by net P&L descending
    results.sort_by(|a, b| b.net_pnl.partial_cmp(&a.net_pnl).unwrap());

    info!("");
    info!("  ── TOP 20 FRONTLOAD CONFIGURATIONS BY NET P&L ──");
    info!("  {:>6} {:>6} {:>6} | {:>5} {:>5} {:>8} {:>8}",
        "look", "thres", "entry", "trd", "win%", "netPnL", "$/trd");
    info!("  {}", "-".repeat(65));

    for r in results.iter().take(20) {
        info!(
            "  {:>5}s {:>5.0} {:>6.2} | {:>5} {:>4.1}% {:>+8.2} {:>+8.4}",
            r.params.lookback_secs,
            r.params.momentum_threshold,
            r.params.entry_price,
            r.trades,
            r.win_rate * 100.0,
            r.net_pnl,
            r.pnl_per_trade,
        );
    }

    // Bottom 5
    info!("");
    info!("  ── BOTTOM 5 (worst) ──");
    for r in results.iter().rev().take(5) {
        info!(
            "  {:>5}s {:>5.0} {:>6.2} | {:>5} {:>4.1}% {:>+8.2} {:>+8.4}",
            r.params.lookback_secs,
            r.params.momentum_threshold,
            r.params.entry_price,
            r.trades,
            r.win_rate * 100.0,
            r.net_pnl,
            r.pnl_per_trade,
        );
    }

    // Detailed report for best config
    if let Some(best) = results.first() {
        info!("");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  BEST FRONTLOAD CONFIGURATION — DETAILED REPORT");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  lookback={}s, threshold=${:.0}, entry_price={:.2}",
            best.params.lookback_secs, best.params.momentum_threshold, best.params.entry_price);

        // Re-run to get individual trades
        let mut trades: Vec<SimTrade> = Vec::new();
        for market in markets {
            if let Some(trade) = simulate_market_frontload(market, candles, candle_index, &best.params) {
                trades.push(trade);
            }
        }

        let wins: Vec<&SimTrade> = trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Win)).collect();
        let losses: Vec<&SimTrade> = trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Loss)).collect();
        let gross_profit: f64 = wins.iter().map(|t| t.pnl).sum();
        let gross_loss: f64 = losses.iter().map(|t| t.pnl).sum();

        info!("");
        info!("  Markets traded:    {:>6}  ({:.1}% of {})",
            trades.len(),
            trades.len() as f64 / markets_with_data as f64 * 100.0,
            markets_with_data);
        info!("  Wins:              {:>6}  ({:.1}%)", wins.len(), best.win_rate * 100.0);
        info!("  Losses:            {:>6}  ({:.1}%)", losses.len(), (1.0 - best.win_rate) * 100.0);
        info!("");
        info!("  Entry price:       {:.2}", best.params.entry_price);
        info!("  Gross profit:     {:>+10.2}", gross_profit);
        info!("  Gross loss:       {:>+10.2}", gross_loss);
        info!("  Net P&L:          {:>+10.2}  (${:+.4}/trade)", best.net_pnl, best.pnl_per_trade);

        // Momentum direction distribution
        let up_trades: Vec<&SimTrade> = trades.iter().filter(|t| t.side_up).collect();
        let down_trades: Vec<&SimTrade> = trades.iter().filter(|t| !t.side_up).collect();
        let up_wins = up_trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Win)).count();
        let down_wins = down_trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Win)).count();

        info!("");
        info!("  ── Momentum Direction Distribution ──");
        info!("  UP momentum:   {} trades, {:.1}% win rate",
            up_trades.len(),
            if up_trades.is_empty() { 0.0 } else { up_wins as f64 / up_trades.len() as f64 * 100.0 });
        info!("  DOWN momentum: {} trades, {:.1}% win rate",
            down_trades.len(),
            if down_trades.is_empty() { 0.0 } else { down_wins as f64 / down_trades.len() as f64 * 100.0 });

        // Sample trades
        info!("");
        info!("  ── Sample Trades (first 15) ──");
        for (i, t) in trades.iter().take(15).enumerate() {
            let side = if t.side_up { "UP  " } else { "DOWN" };
            let result = match t.outcome {
                TradeOutcome::Win => "WIN ",
                TradeOutcome::Loss => "LOSS",
            };
            info!(
                "  {:>2}. {} {} entry={:.2} → {} pnl={:+.2}",
                i + 1,
                &t.market_slug[..t.market_slug.len().min(30)],
                side,
                t.buy_price,
                result,
                t.pnl,
            );
        }

        // Suggested constants
        info!("");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  SUGGESTED FRONTLOAD.RS CONSTANTS");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  const LOOKBACK_SECS: i64 = {};", best.params.lookback_secs);
        info!("  const MOMENTUM_THRESHOLD: f64 = {:.1};", best.params.momentum_threshold);
        info!("  const ENTRY_PRICE: f64 = {:.2};", best.params.entry_price);
        info!("═══════════════════════════════════════════════════════════════");
    }

    results.into_iter().next()
}

// ── Head-to-head comparison ──────────────────────────────────────────

fn print_comparison(
    sniper_best: Option<&SweepResult>,
    bayesian_best: Option<&BayesianSweepResult>,
    bayesian_default: &BayesianSweepResult,
    sniper_current: &SweepResult,
    frontload_best: Option<&FrontloadSweepResult>,
    days: u64,
) {
    info!("");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  HEAD-TO-HEAD COMPARISON ({}-day dataset)", days);
    info!("═══════════════════════════════════════════════════════════════");
    info!("  {:>22} | {:>5} {:>6} {:>+9} {:>+9} {:>8}",
        "Strategy", "Trd", "Win%", "Net P&L", "$/trade", "AvgPrc");
    info!("  {}", "-".repeat(72));

    if let Some(s) = sniper_best {
        info!("  {:>22} | {:>5} {:>5.1}% {:>+9.2} {:>+9.4} {:>8.4}",
            "Sniper (best)", s.trades, s.win_rate * 100.0,
            s.net_pnl, s.pnl_per_trade, s.avg_buy_price);
    }

    info!("  {:>22} | {:>5} {:>5.1}% {:>+9.2} {:>+9.4} {:>8.4}",
        "Sniper (current)", sniper_current.trades, sniper_current.win_rate * 100.0,
        sniper_current.net_pnl, sniper_current.pnl_per_trade, sniper_current.avg_buy_price);

    if let Some(b) = bayesian_best {
        info!("  {:>22} | {:>5} {:>5.1}% {:>+9.2} {:>+9.4} {:>8.4}",
            "Bayesian (best)", b.trades, b.win_rate * 100.0,
            b.net_pnl, b.pnl_per_trade, b.avg_buy_price);
    }

    info!("  {:>22} | {:>5} {:>5.1}% {:>+9.2} {:>+9.4} {:>8.4}",
        "Bayesian (default)", bayesian_default.trades, bayesian_default.win_rate * 100.0,
        bayesian_default.net_pnl, bayesian_default.pnl_per_trade, bayesian_default.avg_buy_price);

    if let Some(f) = frontload_best {
        info!("  {:>22} | {:>5} {:>5.1}% {:>+9.2} {:>+9.4} {:>8.4}",
            "Frontload (best)", f.trades, f.win_rate * 100.0,
            f.net_pnl, f.pnl_per_trade, f.params.entry_price);
    }

    info!("");
    info!("  Note: Arbitrage strategy requires historical orderbook snapshots");
    info!("  and cannot be backtested with available data.");
    info!("═══════════════════════════════════════════════════════════════");
}

// ── Entry point ───────────────────────────────────────────────────────

pub async fn run(days: u64, size: f64, volatility: f64, strategy: BacktestStrategy, market: MarketWindow) -> anyhow::Result<()> {
    let run_sniper = matches!(strategy, BacktestStrategy::Sniper | BacktestStrategy::All);
    let run_bayesian = matches!(strategy, BacktestStrategy::Bayesian | BacktestStrategy::All);
    let run_frontload = matches!(strategy, BacktestStrategy::Frontload | BacktestStrategy::All);
    let window_secs = market.secs();

    info!("═══════════════════════════════════════════════════════════════");
    info!("  STRATEGY BACKTEST + PARAMETER SWEEP");
    info!("═══════════════════════════════════════════════════════════════");
    info!("  Strategy:    {:?}", strategy);
    info!("  Market:      {} ({}s windows)", market.slug_prefix(), window_secs);
    info!("  Period:      last {} days", days);
    info!("  Bet size:    ${:.2}", size);
    info!("  Base vol:    ${:.0}/{}s", volatility, window_secs);
    info!("═══════════════════════════════════════════════════════════════");

    let gamma = GammaClient::default();

    // Fetch data
    let markets = fetch_markets(&gamma, days, market).await?;
    let earliest_ts = markets.iter().map(|m| m.start_ts).min().unwrap_or(0);
    let candles = fetch_btc_klines(earliest_ts).await?;

    if markets.is_empty() {
        info!("[Backtest] No markets found. Exiting.");
        return Ok(());
    }
    if candles.is_empty() {
        info!("[Backtest] No BTC price data found. Exiting.");
        return Ok(());
    }

    let candle_index = build_candle_index(&candles);

    // Count markets with price data
    let markets_with_data: usize = markets.iter()
        .filter(|m| price_at(&candles, &candle_index, m.start_ts).is_some())
        .count();
    let up_won: usize = markets.iter()
        .filter(|m| price_at(&candles, &candle_index, m.start_ts).is_some() && m.up_won)
        .count();

    info!("");
    info!("  Markets with data: {} (UP won: {:.1}%, DOWN won: {:.1}%)",
        markets_with_data,
        up_won as f64 / markets_with_data as f64 * 100.0,
        (markets_with_data - up_won) as f64 / markets_with_data as f64 * 100.0,
    );

    // ── Sniper backtest ──────────────────────────────────────────────
    let mut sniper_best: Option<SweepResult> = None;
    let mut sniper_current = SweepResult {
        params: StrategyParams::default_with(volatility, size),
        trades: 0, wins: 0, win_rate: 0.0, net_pnl: 0.0, avg_buy_price: 0.0, pnl_per_trade: 0.0,
    };

    if run_sniper {
        // Phase 1: Model accuracy analysis
        info!("");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  SNIPER — PHASE 1: MODEL ACCURACY ANALYSIS");
        info!("═══════════════════════════════════════════════════════════════");

        for &vol in &[30.0, 50.0, 80.0, 120.0, 200.0] {
            let mut all_records: Vec<AccuracyRecord> = Vec::new();
            for market in &markets {
                all_records.extend(collect_accuracy(market, &candles, &candle_index, vol, window_secs));
            }

            info!("");
            info!("  ── vol=${:.0} ──", vol);

            for &min_elapsed in &[60, 120, 180, 240] {
                for &min_conf in &[0.90, 0.95, 0.97, 0.99] {
                    let filtered: Vec<&AccuracyRecord> = all_records.iter()
                        .filter(|r| r.elapsed_secs >= min_elapsed && r.model_prob >= min_conf)
                        .collect();

                    if filtered.is_empty() {
                        continue;
                    }

                    let correct = filtered.iter().filter(|r| r.correct).count();
                    let total = filtered.len();
                    let accuracy = correct as f64 / total as f64;

                    if total >= 20 {
                        let breakeven_price = accuracy;
                        let ev_if_buy_at_90 = accuracy * (1.0/0.90 - 1.0) * size - (1.0 - accuracy) * size;
                        info!(
                            "    t>={}s conf>={:.0}%: accuracy={:.1}% ({}/{}) | breakeven_price={:.3} | EV@0.90=${:+.2}",
                            min_elapsed, min_conf * 100.0,
                            accuracy * 100.0, correct, total,
                            breakeven_price,
                            ev_if_buy_at_90,
                        );
                    }
                }
            }
        }

        // Phase 2: Parameter sweep
        info!("");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  SNIPER — PHASE 2: PARAMETER SWEEP");
        info!("═══════════════════════════════════════════════════════════════");

        let volatilities = [30.0, 50.0, 80.0, 120.0, 200.0];
        let min_confidences = [0.90, 0.93, 0.95, 0.97, 0.99];
        let max_asks = [0.85, 0.88, 0.90, 0.92, 0.95];
        let min_elapsed_options = [0, 120, 180, 210, 240];
        let market_lags = [0.02, 0.03, 0.05];

        let mut results: Vec<SweepResult> = Vec::new();

        let total_combos = volatilities.len() * min_confidences.len() * max_asks.len()
            * min_elapsed_options.len() * market_lags.len();
        info!("  Testing {} parameter combinations...", total_combos);

        for &vol in &volatilities {
            for &min_conf in &min_confidences {
                for &max_ask in &max_asks {
                    for &min_elapsed in &min_elapsed_options {
                        for &lag in &market_lags {
                            let params = StrategyParams {
                                volatility: vol,
                                min_confidence: min_conf,
                                max_ask,
                                min_elapsed_secs: min_elapsed,
                                market_lag: lag,
                                size,
                            };

                            let result = run_single_config(&markets, &candles, &candle_index, &params, window_secs);

                            if result.trades >= 20 {
                                results.push(result);
                            }
                        }
                    }
                }
            }
        }

        results.sort_by(|a, b| b.net_pnl.partial_cmp(&a.net_pnl).unwrap());

        info!("");
        info!("  ── TOP 20 SNIPER CONFIGURATIONS BY NET P&L ──");
        info!("  {:>5} {:>5} {:>5} {:>5} {:>5} | {:>5} {:>5} {:>8} {:>8} {:>8}",
            "vol", "conf", "maxA", "minT", "lag", "trd", "win%", "avgPrc", "netPnL", "$/trd");
        info!("  {}", "-".repeat(85));

        for r in results.iter().take(20) {
            info!(
                "  {:>5.0} {:>4.0}% {:>4.0}% {:>4}s {:>4.0}% | {:>5} {:>4.1}% {:>8.4} {:>+8.2} {:>+8.4}",
                r.params.volatility,
                r.params.min_confidence * 100.0,
                r.params.max_ask * 100.0,
                r.params.min_elapsed_secs,
                r.params.market_lag * 100.0,
                r.trades,
                r.win_rate * 100.0,
                r.avg_buy_price,
                r.net_pnl,
                r.pnl_per_trade,
            );
        }

        info!("");
        info!("  ── BOTTOM 5 (worst) ──");
        for r in results.iter().rev().take(5) {
            info!(
                "  {:>5.0} {:>4.0}% {:>4.0}% {:>4}s {:>4.0}% | {:>5} {:>4.1}% {:>8.4} {:>+8.2} {:>+8.4}",
                r.params.volatility,
                r.params.min_confidence * 100.0,
                r.params.max_ask * 100.0,
                r.params.min_elapsed_secs,
                r.params.market_lag * 100.0,
                r.trades,
                r.win_rate * 100.0,
                r.avg_buy_price,
                r.net_pnl,
                r.pnl_per_trade,
            );
        }

        // Detailed report for best config
        if let Some(best) = results.first() {
            info!("");
            info!("═══════════════════════════════════════════════════════════════");
            info!("  BEST SNIPER CONFIGURATION — DETAILED REPORT");
            info!("═══════════════════════════════════════════════════════════════");
            info!("  vol=${:.0}, confidence>{:.0}%, max_ask={:.0}%, min_elapsed={}s, lag={:.0}%",
                best.params.volatility,
                best.params.min_confidence * 100.0,
                best.params.max_ask * 100.0,
                best.params.min_elapsed_secs,
                best.params.market_lag * 100.0,
            );

            let mut trades: Vec<SimTrade> = Vec::new();
            for market in &markets {
                if price_at(&candles, &candle_index, market.start_ts).is_none() {
                    continue;
                }
                let result = simulate_market(market, &candles, &candle_index, &best.params, window_secs);
                if let Some(trade) = result.trade {
                    trades.push(trade);
                }
            }

            let wins: Vec<&SimTrade> = trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Win)).collect();
            let losses: Vec<&SimTrade> = trades.iter().filter(|t| matches!(t.outcome, TradeOutcome::Loss)).collect();
            let gross_profit: f64 = wins.iter().map(|t| t.pnl).sum();
            let gross_loss: f64 = losses.iter().map(|t| t.pnl).sum();

            info!("");
            info!("  Markets traded:    {:>6}  ({:.1}% of {})",
                trades.len(),
                trades.len() as f64 / markets_with_data as f64 * 100.0,
                markets_with_data);
            info!("  Wins:              {:>6}  ({:.1}%)", wins.len(), best.win_rate * 100.0);
            info!("  Losses:            {:>6}  ({:.1}%)", losses.len(), (1.0 - best.win_rate) * 100.0);
            info!("");
            info!("  Avg buy price:     {:.4}", best.avg_buy_price);
            info!("  Gross profit:     {:>+10.2}", gross_profit);
            info!("  Gross loss:       {:>+10.2}", gross_loss);
            info!("  Net P&L:          {:>+10.2}  (${:+.4}/trade)", best.net_pnl, best.pnl_per_trade);

            let mut timing_dist: HashMap<i64, (usize, usize)> = HashMap::new();
            for t in &trades {
                let bucket = (t.entry_elapsed / 60) * 60;
                let entry = timing_dist.entry(bucket).or_insert((0, 0));
                entry.0 += 1;
                if matches!(t.outcome, TradeOutcome::Win) {
                    entry.1 += 1;
                }
            }
            let mut timing_keys: Vec<i64> = timing_dist.keys().copied().collect();
            timing_keys.sort();

            info!("");
            info!("  ── Entry Timing Distribution ──");
            for &k in &timing_keys {
                let (total, w) = timing_dist[&k];
                let wr = if total > 0 { w as f64 / total as f64 * 100.0 } else { 0.0 };
                info!("  t={}s: {} trades, {:.1}% win rate", k, total, wr);
            }

            info!("");
            info!("  ── Sample Trades (first 15) ──");
            for (i, t) in trades.iter().take(15).enumerate() {
                let side = if t.side_up { "UP  " } else { "DOWN" };
                let result = match t.outcome {
                    TradeOutcome::Win => "WIN ",
                    TradeOutcome::Loss => "LOSS",
                };
                info!(
                    "  {:>2}. {} {} model={:.1}% ask={:.2} edge={:.1}% t={}s → {} pnl={:+.2}",
                    i + 1,
                    &t.market_slug[..t.market_slug.len().min(30)],
                    side,
                    t.model_prob * 100.0,
                    t.buy_price,
                    t.edge * 100.0,
                    t.entry_elapsed,
                    result,
                    t.pnl,
                );
            }

            info!("");
            info!("═══════════════════════════════════════════════════════════════");
            info!("  SUGGESTED SNIPER.RS CONSTANTS");
            info!("═══════════════════════════════════════════════════════════════");
            info!("  const MIN_CONFIDENCE: f64 = {:.2};", best.params.min_confidence);
            info!("  const MAX_ASK: f64 = {:.2};", best.params.max_ask);
            info!("  const MIN_REMAINING_SECS: i64 = {};", 300 - best.params.min_elapsed_secs);
            info!("  const BTC_5M_VOLATILITY: f64 = {:.1};", best.params.volatility);
            info!("═══════════════════════════════════════════════════════════════");

            sniper_best = Some(best.clone());
        }

        // Current config for comparison
        let current_params = StrategyParams::default_with(volatility, size);
        sniper_current = run_single_config(&markets, &candles, &candle_index, &current_params, window_secs);

        info!("");
        info!("═══════════════════════════════════════════════════════════════");
        info!("  SNIPER CURRENT CONFIG (vol=${:.0}) FOR COMPARISON", volatility);
        info!("═══════════════════════════════════════════════════════════════");
        info!(
            "  Trades: {} | Win rate: {:.1}% | Avg price: {:.4} | Net P&L: {:+.2} | $/trade: {:+.4}",
            sniper_current.trades, sniper_current.win_rate * 100.0,
            sniper_current.avg_buy_price, sniper_current.net_pnl, sniper_current.pnl_per_trade,
        );
    }

    // ── Bayesian backtest ────────────────────────────────────────────
    let mut bayesian_best: Option<BayesianSweepResult> = None;
    let bayesian_default = run_single_bayesian_config(
        &markets, &candles, &candle_index, &BayesianParams::default_with(size), window_secs,
    );

    if run_bayesian {
        bayesian_best = run_bayesian_backtest(
            &markets, &candles, &candle_index, markets_with_data, size, window_secs,
        );
    }

    // ── Frontload backtest ───────────────────────────────────────────
    let mut frontload_best: Option<FrontloadSweepResult> = None;

    if run_frontload {
        frontload_best = run_frontload_backtest(
            &markets, &candles, &candle_index, markets_with_data, size,
        );
    }

    // ── Head-to-head comparison (only in "all" mode) ─────────────────
    if matches!(strategy, BacktestStrategy::All) {
        print_comparison(
            sniper_best.as_ref(),
            bayesian_best.as_ref(),
            &bayesian_default,
            &sniper_current,
            frontload_best.as_ref(),
            days,
        );
    }

    Ok(())
}
