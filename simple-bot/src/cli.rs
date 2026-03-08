use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "simple-bot", about = "Polymarket trading bot")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a trading strategy
    Run {
        /// Strategy to run
        #[arg(long, default_value = "bayesian")]
        strategy: Strategy,
        /// Paper mode (simulated trading, no real orders)
        #[arg(long)]
        paper: bool,
        /// Order size in USDC
        #[arg(long, default_value = "5")]
        size: f64,
        /// Target event by slug (e.g. btc-updown-5m-1770978300)
        #[arg(long)]
        event: Option<String>,
        /// Dry run (log signals only, no orders)
        #[arg(long)]
        dry_run: bool,
    },
    /// Set token approvals for CLOB trading
    Approve,
    /// Redeem winning tokens from resolved markets
    Redeem {
        /// Condition ID (32-byte hex, e.g. 0x...)
        #[arg(long)]
        condition_id: String,
    },
    /// Check wallet USDC balance
    Balance,
    /// Backtest strategies on historical data
    Backtest {
        /// Number of days to backtest
        #[arg(long, default_value = "7")]
        days: u64,
        /// Order size in USDC for P&L calculation
        #[arg(long, default_value = "5")]
        size: f64,
        /// BTC volatility parameter ($/5min window)
        #[arg(long, default_value = "50")]
        volatility: f64,
        /// Which strategy to backtest
        #[arg(long, default_value = "all")]
        strategy: BacktestStrategy,
        /// Market window to backtest (5m or 15m)
        #[arg(long, default_value = "5m")]
        market: MarketWindow,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum MarketWindow {
    /// 5-minute BTC up/down markets
    #[value(name = "5m")]
    FiveMin,
    /// 15-minute BTC up/down markets
    #[value(name = "15m")]
    FifteenMin,
}

impl MarketWindow {
    pub fn secs(self) -> i64 {
        match self {
            Self::FiveMin => 300,
            Self::FifteenMin => 900,
        }
    }
    pub fn slug_prefix(self) -> &'static str {
        match self {
            Self::FiveMin => "btc-updown-5m",
            Self::FifteenMin => "btc-updown-15m",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BacktestStrategy {
    /// Oracle sniper only
    Sniper,
    /// Bayesian only
    Bayesian,
    /// Frontloading: buy at market open using pre-market BTC momentum
    Frontload,
    /// All strategies + head-to-head comparison
    All,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Strategy {
    /// Bayesian: 4 feeds, posterior, >3% edge, quarter-Kelly sizing
    Bayesian,
    /// Sniper: buy winning side of 5/15-min markets in last second
    Sniper,
    /// Arbitrage: YES+NO != $1, parallel FOK execution
    Arbitrage,
    /// Frontload: buy at market open using pre-market BTC momentum
    Frontload,
}
