use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use polymarket_client_sdk::rtds::Client as RtdsClient;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

pub const BINANCE_BTC: &str = "btcusdt";
pub const BINANCE_ETH: &str = "ethusdt";
pub const CHAINLINK_BTC: &str = "btc/usd";
pub const CHAINLINK_ETH: &str = "eth/usd";

#[derive(Debug, Clone, Default)]
pub struct FeedPrices {
    pub binance_btc: Option<f64>,
    pub binance_eth: Option<f64>,
    pub chainlink_btc: Option<f64>,
    pub chainlink_eth: Option<f64>,
}

impl FeedPrices {
    pub fn as_evidence_btc_only(&self, start_prices: &FeedPrices) -> Vec<(f64, f64)> {
        let mut evidence = Vec::with_capacity(2);
        if let (Some(s), Some(c)) = (start_prices.binance_btc, self.binance_btc) {
            evidence.push((s, c));
        }
        if let (Some(s), Some(c)) = (start_prices.chainlink_btc, self.chainlink_btc) {
            evidence.push((s, c));
        }
        evidence
    }

    pub fn has_minimum_data(&self) -> bool {
        let has_btc = self.binance_btc.is_some() || self.chainlink_btc.is_some();
        let has_eth = self.binance_eth.is_some() || self.chainlink_eth.is_some();
        has_btc && has_eth
    }

    pub fn feed_count(&self) -> usize {
        [
            self.binance_btc,
            self.binance_eth,
            self.chainlink_btc,
            self.chainlink_eth,
        ]
        .iter()
        .filter(|p| p.is_some())
        .count()
    }
}

pub type SharedFeedState = Arc<RwLock<FeedPrices>>;

pub async fn spawn_feed_collector() -> SharedFeedState {
    let state: SharedFeedState = Arc::new(RwLock::new(FeedPrices::default()));

    let s = Arc::clone(&state);
    tokio::spawn(async move {
        let rtds = RtdsClient::default();
        let stream = match rtds.subscribe_crypto_prices(Some(vec![BINANCE_BTC.to_owned()])) {
            Ok(s) => s,
            Err(e) => { warn!(error = %e, "[Feed] Failed to subscribe to Binance BTC"); return; }
        };
        info!("[Feed] Binance BTC subscription connected");
        let mut stream = Box::pin(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(price) => {
                    let value = price.value.to_string().parse().unwrap_or(0.0);
                    s.write().await.binance_btc = Some(value);
                    debug!(symbol = %price.symbol, value = %price.value, "Binance BTC");
                }
                Err(e) => warn!(error = %e, "Binance BTC feed error"),
            }
        }
        warn!("[Feed] Binance BTC stream ended");
    });

    let s = Arc::clone(&state);
    tokio::spawn(async move {
        let rtds = RtdsClient::default();
        let stream = match rtds.subscribe_crypto_prices(Some(vec![BINANCE_ETH.to_owned()])) {
            Ok(s) => s,
            Err(e) => { warn!(error = %e, "[Feed] Failed to subscribe to Binance ETH"); return; }
        };
        info!("[Feed] Binance ETH subscription connected");
        let mut stream = Box::pin(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(price) => {
                    let value = price.value.to_string().parse().unwrap_or(0.0);
                    s.write().await.binance_eth = Some(value);
                    debug!(symbol = %price.symbol, value = %price.value, "Binance ETH");
                }
                Err(e) => warn!(error = %e, "Binance ETH feed error"),
            }
        }
        warn!("[Feed] Binance ETH stream ended");
    });

    let s = Arc::clone(&state);
    tokio::spawn(async move {
        let rtds = RtdsClient::default();
        let stream = match rtds.subscribe_chainlink_prices(Some(CHAINLINK_BTC.to_owned())) {
            Ok(s) => s,
            Err(e) => { warn!(error = %e, "[Feed] Failed to subscribe to Chainlink BTC"); return; }
        };
        info!("[Feed] Chainlink BTC subscription connected");
        let mut stream = Box::pin(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(price) => {
                    if let Ok(v) = price.value.to_string().parse::<f64>() {
                        s.write().await.chainlink_btc = Some(v);
                    }
                    debug!(symbol = %price.symbol, "Chainlink BTC");
                }
                Err(e) => warn!(error = %e, "Chainlink BTC feed error"),
            }
        }
        warn!("[Feed] Chainlink BTC stream ended");
    });

    let s = Arc::clone(&state);
    tokio::spawn(async move {
        let rtds = RtdsClient::default();
        let stream = match rtds.subscribe_chainlink_prices(Some(CHAINLINK_ETH.to_owned())) {
            Ok(s) => s,
            Err(e) => { warn!(error = %e, "[Feed] Failed to subscribe to Chainlink ETH"); return; }
        };
        info!("[Feed] Chainlink ETH subscription connected");
        let mut stream = Box::pin(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(price) => {
                    if let Ok(v) = price.value.to_string().parse::<f64>() {
                        s.write().await.chainlink_eth = Some(v);
                    }
                    debug!(symbol = %price.symbol, "Chainlink ETH");
                }
                Err(e) => warn!(error = %e, "Chainlink ETH feed error"),
            }
        }
        warn!("[Feed] Chainlink ETH stream ended");
    });

    tokio::time::sleep(Duration::from_secs(3)).await;

    let initial = state.read().await;
    info!(
        "Feed collector: 4 subscriptions started — Binance BTC={} ETH={}, Chainlink BTC={} ETH={}",
        if initial.binance_btc.is_some() { "OK" } else { "WAITING" },
        if initial.binance_eth.is_some() { "OK" } else { "WAITING" },
        if initial.chainlink_btc.is_some() { "OK" } else { "WAITING" },
        if initial.chainlink_eth.is_some() { "OK" } else { "WAITING" },
    );
    drop(initial);

    state
}
