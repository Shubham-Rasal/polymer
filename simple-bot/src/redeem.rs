use std::str::FromStr as _;

use alloy::primitives::B256;
use alloy::providers::ProviderBuilder;
use alloy::signers::Signer as _;
use alloy::signers::local::LocalSigner;
use anyhow::{Context, Result};
use polymarket_client_sdk::ctf;
use polymarket_client_sdk::ctf::types::RedeemPositionsRequest;
use polymarket_client_sdk::types::address;
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};

const RPC_URL: &str = "https://polygon-rpc.com";
const USDC: alloy::primitives::Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");

pub async fn run_redeem(condition_id_hex: &str) -> Result<()> {
    let private_key = std::env::var(PRIVATE_KEY_VAR)
        .context("Set POLYMARKET_PRIVATE_KEY env var")?;

    let condition_id = B256::from_str(condition_id_hex)
        .context("condition-id must be a valid 32-byte hex (0x...)")?;

    let signer = LocalSigner::from_str(&private_key)?
        .with_chain_id(Some(POLYGON));
    let wallet_address = signer.address();

    println!("Wallet:       {wallet_address}");
    println!("Condition ID: {condition_id}");

    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect(RPC_URL)
        .await?;

    let client = ctf::Client::new(provider, POLYGON)?;

    println!("\nRedeeming winning tokens...");

    let redeem_req = RedeemPositionsRequest::for_binary_market(USDC, condition_id);

    let resp = client
        .redeem_positions(&redeem_req)
        .await
        .context("Redeem failed — is the market resolved and do you hold winning tokens?")?;

    println!("Redeem successful!");
    println!("  Tx hash: {}", resp.transaction_hash);
    println!("  Block:   {}", resp.block_number);

    let read_provider = alloy::providers::ProviderBuilder::new()
        .connect(RPC_URL)
        .await?;

    let usdc_contract = {
        alloy::sol! {
            #[sol(rpc)]
            interface IERC20 {
                function balanceOf(address account) external view returns (uint256);
            }
        }
        IERC20::new(USDC, read_provider)
    };

    let balance = usdc_contract.balanceOf(wallet_address).call().await?;
    let usdc_balance = balance.to::<u128>() as f64 / 1e6;
    println!("\n  USDC.e balance: {usdc_balance:.6}");

    Ok(())
}
