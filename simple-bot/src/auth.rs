use std::str::FromStr as _;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer as _;
use polymarket_client_sdk::clob::types::SignatureType;
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::{derive_proxy_wallet, POLYGON, PRIVATE_KEY_VAR};
use tracing::info;

const WALLET_TYPE_VAR: &str = "POLYMARKET_WALLET_TYPE";
const FUNDER_ADDRESS_VAR: &str = "POLYMARKET_FUNDER_ADDRESS";

pub type AuthenticatedClob =
    Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>;

/// Attempt to authenticate with the CLOB. Returns None if no key or auth fails (paper mode).
pub async fn authenticate(force_paper: bool) -> (Option<PrivateKeySigner>, Option<AuthenticatedClob>) {
    if force_paper {
        info!("Paper mode requested — no live trading");
        return (None, None);
    }

    let private_key = match std::env::var(PRIVATE_KEY_VAR) {
        Ok(pk) => pk,
        Err(_) => {
            info!("No POLYMARKET_PRIVATE_KEY set — PAPER MODE (100 USDC wallet)");
            return (None, None);
        }
    };

    let signer = match PrivateKeySigner::from_str(&private_key) {
        Ok(s) => s.with_chain_id(Some(POLYGON)),
        Err(e) => {
            info!("Invalid private key ({}), PAPER MODE", e);
            return (None, None);
        }
    };

    let use_proxy = std::env::var(WALLET_TYPE_VAR)
        .map(|v| v.to_lowercase() == "proxy")
        .unwrap_or(false);
    let explicit_funder = std::env::var(FUNDER_ADDRESS_VAR)
        .ok()
        .and_then(|s| s.trim().trim_start_matches("0x").parse::<Address>().ok());

    let funder_addr = if let Some(addr) = explicit_funder {
        info!("Wallet type: Proxy (Magic/email)");
        info!("Funder address: {} (from {})", addr, FUNDER_ADDRESS_VAR);
        Some(addr)
    } else if use_proxy {
        if let Some(proxy_addr) = derive_proxy_wallet(signer.address(), POLYGON) {
            info!("Wallet type: Proxy (Magic/email)");
            info!("Funder address: {}", proxy_addr);
            Some(proxy_addr)
        } else {
            info!("Wallet address: {}", signer.address());
            None
        }
    } else {
        info!("Wallet address: {}", signer.address());
        None
    };

    let config = Config::builder().use_server_time(true).build();
    let mut auth_builder = match Client::new("https://clob.polymarket.com", config) {
        Ok(c) => c.authentication_builder(&signer),
        Err(e) => {
            info!("CLOB client error ({}), PAPER MODE", e);
            return (None, None);
        }
    };

    if use_proxy || funder_addr.is_some() {
        auth_builder = auth_builder.signature_type(SignatureType::Proxy);
        if let Some(addr) = funder_addr {
            auth_builder = auth_builder.funder(addr);
        }
    }

    match auth_builder.authenticate().await {
        Ok(client) => {
            info!("Authenticated — LIVE TRADING ENABLED");
            (Some(signer), Some(client))
        }
        Err(e) => {
            info!("Auth failed ({}), PAPER MODE (100 USDC wallet)", e);
            (None, None)
        }
    }
}
