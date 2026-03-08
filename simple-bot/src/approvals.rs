#![allow(clippy::exhaustive_enums)]
#![allow(clippy::exhaustive_structs)]

use std::str::FromStr as _;

use alloy::primitives::U256;
use alloy::providers::ProviderBuilder;
use alloy::signers::Signer as _;
use alloy::signers::local::LocalSigner;
use alloy::sol;
use anyhow::{Context, Result};
use polymarket_client_sdk::types::{Address, address};
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR, contract_config};

const RPC_URL: &str = "https://polygon-rpc.com";
const USDC_ADDRESS: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 value) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IERC1155 {
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }
}

fn format_allowance(allowance: U256) -> String {
    if allowance == U256::MAX {
        "MAX (unlimited)".to_owned()
    } else if allowance == U256::ZERO {
        "0".to_owned()
    } else {
        let usdc_decimals = U256::from(1_000_000);
        let whole = allowance / usdc_decimals;
        format!("{whole} USDC")
    }
}

pub async fn run_approvals() -> Result<()> {
    let private_key = std::env::var(PRIVATE_KEY_VAR)
        .context("Set POLYMARKET_PRIVATE_KEY env var")?;

    let signer = LocalSigner::from_str(&private_key)?
        .with_chain_id(Some(POLYGON));
    let owner = signer.address();

    println!("Wallet: {owner}");

    let config = contract_config(POLYGON, false).unwrap();
    let neg_risk_config = contract_config(POLYGON, true).unwrap();

    let mut targets: Vec<(&str, Address)> = vec![
        ("CTF Exchange", config.exchange),
        ("Neg Risk CTF Exchange", neg_risk_config.exchange),
    ];
    if let Some(adapter) = neg_risk_config.neg_risk_adapter {
        targets.push(("Neg Risk Adapter", adapter));
    }

    let read_provider = ProviderBuilder::new().connect(RPC_URL).await?;
    let usdc_read = IERC20::new(USDC_ADDRESS, read_provider.clone());
    let ctf_read = IERC1155::new(config.conditional_tokens, read_provider.clone());

    println!("\n=== Current Approval Status ===\n");

    let mut needs_approval = false;

    for (name, target) in &targets {
        let usdc_allowance = usdc_read.allowance(owner, *target).call().await?;
        let ctf_approved = ctf_read.isApprovedForAll(owner, *target).call().await?;

        let usdc_ok = usdc_allowance > U256::ZERO;
        let ctf_ok = ctf_approved;

        println!(
            "{name}:\n  USDC allowance: {} ({})\n  CTF approved:   {} ({})",
            format_allowance(usdc_allowance),
            if usdc_ok { "OK" } else { "NEEDS APPROVAL" },
            ctf_approved,
            if ctf_ok { "OK" } else { "NEEDS APPROVAL" },
        );

        if !usdc_ok || !ctf_ok {
            needs_approval = true;
        }
    }

    if !needs_approval {
        println!("\nAll approvals are already set. You're ready to trade!");
        return Ok(());
    }

    println!("\nSetting missing approvals...\n");

    let write_provider = ProviderBuilder::new()
        .wallet(signer)
        .connect(RPC_URL)
        .await?;
    let usdc_write = IERC20::new(USDC_ADDRESS, write_provider.clone());
    let ctf_write = IERC1155::new(config.conditional_tokens, write_provider.clone());

    for (name, target) in &targets {
        let usdc_allowance = usdc_read.allowance(owner, *target).call().await?;
        if usdc_allowance == U256::ZERO {
            print!("{name}: approving USDC... ");
            let tx_hash = usdc_write
                .approve(*target, U256::MAX)
                .send()
                .await?
                .watch()
                .await?;
            println!("done (tx: {tx_hash})");
        }

        let ctf_approved = ctf_read.isApprovedForAll(owner, *target).call().await?;
        if !ctf_approved {
            print!("{name}: approving CTF... ");
            let tx_hash = ctf_write
                .setApprovalForAll(*target, true)
                .send()
                .await?
                .watch()
                .await?;
            println!("done (tx: {tx_hash})");
        }
    }

    println!("\n=== Verification ===\n");

    for (name, target) in &targets {
        let usdc_allowance = usdc_read.allowance(owner, *target).call().await?;
        let ctf_approved = ctf_read.isApprovedForAll(owner, *target).call().await?;
        println!(
            "{name}: USDC={}, CTF={}",
            format_allowance(usdc_allowance),
            ctf_approved,
        );
    }

    println!("\nAll approvals complete. You're ready to trade!");

    Ok(())
}
