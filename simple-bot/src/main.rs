mod approvals;
mod auth;
mod backtest;
mod cli;
mod feeds;
mod orderbook;
mod redeem;
mod strategies;
mod tracker;
mod trading;

use std::fs::File;

use anyhow::Result;
use clap::Parser;
use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, Strategy};

const DEFAULT_LOG_FILE: &str = "bot.log";

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("rustls crypto provider");

    let cli = Cli::parse();
    let _ = dotenvy::dotenv();

    let env_filter = EnvFilter::from_default_env()
        .add_directive("simple_bot=info".parse().unwrap());
    let log_path = std::env::var("LOG_FILE").unwrap_or_else(|_| DEFAULT_LOG_FILE.to_string());
    let file = File::create(&log_path).expect("create log file");

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_ansi(true),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_ansi(false),
        )
        .init();

    info!("Log file: {}", log_path);

    match cli.command {
        Command::Approve => {
            approvals::run_approvals().await?;
        }
        Command::Redeem { condition_id } => {
            redeem::run_redeem(&condition_id).await?;
        }
        Command::Balance => {
            run_balance().await?;
        }
        Command::Backtest {
            days,
            size,
            volatility,
            strategy,
            market,
        } => {
            backtest::run(days, size, volatility, strategy, market).await?;
        }
        Command::Run {
            strategy,
            paper,
            size,
            event,
            dry_run,
        } => {
            let (signer, auth_clob) = auth::authenticate(paper).await;
            let tracker = tracker::create_tracker();

            match strategy {
                Strategy::Bayesian => {
                    strategies::bayesian::run(signer, auth_clob, tracker).await?;
                }
                Strategy::Sniper => {
                    strategies::sniper::run(dry_run, size, event, signer, auth_clob, tracker)
                        .await?;
                }
                Strategy::Arbitrage => {
                    strategies::arbitrage::run(signer, auth_clob, tracker).await?;
                }
                Strategy::Frontload => {
                    strategies::frontload::run(dry_run, size, signer, auth_clob, tracker)
                        .await?;
                }
            }
        }
    }

    Ok(())
}

async fn run_balance() -> Result<()> {
    let (_, auth_clob) = auth::authenticate(false).await;

    match auth_clob {
        Some(client) => {
            let resp = client
                .balance_allowance(BalanceAllowanceRequest::default())
                .await?;
            let usdc = resp.balance / rust_decimal::Decimal::from(1_000_000);
            println!("USDC balance: {}", usdc.round_dp(6));
        }
        None => {
            println!("Cannot check balance without POLYMARKET_PRIVATE_KEY");
        }
    }

    Ok(())
}
