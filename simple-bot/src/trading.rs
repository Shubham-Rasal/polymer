use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side};
use polymarket_client_sdk::types::{Decimal, U256};
use tracing::{info, warn};

use crate::auth::AuthenticatedClob;

/// Execute an order. Returns true if the order was successfully placed (or simulated in paper mode).
pub async fn execute_order(
    signer: Option<&PrivateKeySigner>,
    client: Option<&AuthenticatedClob>,
    token_id: U256,
    amount: Decimal,
    side: Side,
    order_type: OrderType,
) -> bool {
    let (Some(s), Some(c)) = (signer, client) else {
        let side_str = match side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
            _ => "?",
        };
        info!("[Paper] {} {} USDC token={}", side_str, amount, token_id);
        return true;
    };

    let amount_val = match &side {
        Side::Sell => match Amount::shares(amount) {
            Ok(a) => a,
            Err(e) => {
                warn!("Invalid shares amount: {}", e);
                return false;
            }
        },
        _ => match Amount::usdc(amount) {
            Ok(a) => a,
            Err(e) => {
                warn!("Invalid USDC amount: {}", e);
                return false;
            }
        },
    };

    let order = match c
        .market_order()
        .token_id(token_id)
        .amount(amount_val)
        .side(side.clone())
        .order_type(order_type)
        .build()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            warn!("Failed to build order: {}", e);
            return false;
        }
    };

    let signed = match c.sign(s, order).await {
        Ok(o) => o,
        Err(e) => {
            warn!("Failed to sign order: {}", e);
            return false;
        }
    };

    match c.post_order(signed).await {
        Ok(r) => {
            info!("Order posted: order_id={:?}, success={}", r.order_id, r.success);
            r.success
        }
        Err(e) => {
            warn!("Order failed: {}", e);
            false
        }
    }
}
