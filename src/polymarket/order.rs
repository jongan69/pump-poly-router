/// Polymarket order placement wrapper — official polymarket-client-sdk 0.4.
///
/// Authenticates a fresh CLOB client per `buy_position()` call (avoids
/// storing the generic `Client<Authenticated<K>>` in a struct field).
use crate::{
    error::{Result, RouterError},
    types::Outcome,
};
use alloy::{primitives::U256, signers::local::PrivateKeySigner};
use polymarket_client_sdk::clob::{
    types::{Amount, Side},
    Client, Config,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Result of a successfully placed and filled Polymarket order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    pub order_id: String,
    /// Number of shares received.
    pub shares_filled: f64,
    /// Average fill price (0–1).
    pub avg_price: f64,
}

pub struct PolymarketOrderClient {
    clob_url: String,
    /// Hex private key used for L1 order signing and L2 API authentication.
    private_key_hex: String,
    fill_timeout_secs: u64,
}

impl PolymarketOrderClient {
    pub fn new(clob_url: &str, private_key_hex: &str, fill_timeout_secs: u64) -> Self {
        PolymarketOrderClient {
            clob_url: clob_url.to_string(),
            private_key_hex: private_key_hex.to_string(),
            fill_timeout_secs,
        }
    }

    /// Place a market buy order for the given outcome token.
    ///
    /// - `token_id`: ERC-1155 position token ID.
    /// - `usdc_amount`: amount in **micro-USDC** (6 decimals).
    /// - `outcome`: for logging only.
    ///
    /// Authenticates a fresh CLOB session, posts the order, then polls
    /// until filled or `fill_timeout_secs` elapses.
    pub async fn buy_position(
        &self,
        token_id: &str,
        usdc_amount: u64,
        outcome: Outcome,
    ) -> Result<OrderResult> {
        let signer = PrivateKeySigner::from_str(&self.private_key_hex)
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        let client = Client::new(&self.clob_url, Config::default())
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?
            .authentication_builder(&signer)
            .authenticate()
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        // Convert micro-USDC (6 decimal places) to rust_decimal::Decimal
        // e.g. 1_500_000 µUSDC → Decimal::new(1_500_000, 6) → 1.500000 USDC
        let usdc_decimal = Decimal::new(usdc_amount as i64, 6);

        // Parse token_id: Polymarket token IDs are U256 integers (decimal string)
        let token_u256 = U256::from_str_radix(token_id.trim_start_matches("0x"), 16)
            .or_else(|_| token_id.parse::<U256>())
            .map_err(|_| RouterError::PolyOrderPost(format!("invalid token_id: {token_id}")))?;

        let order = client
            .market_order()
            .token_id(token_u256)
            .side(Side::Buy)
            .amount(Amount::usdc(usdc_decimal).map_err(|e| RouterError::PolyOrderPost(e.to_string()))?)
            .build()
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        info!(
            "Posting Polymarket {} order for {} USDC on token {}",
            outcome, usdc_decimal, token_id
        );

        let signed_order = client
            .sign(&signer, order)
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        let post_resp = client
            .post_order(signed_order)
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        if !post_resp.success {
            return Err(RouterError::PolyOrderPost(format!(
                "order not accepted: order_id={}",
                post_resp.order_id
            )));
        }

        let order_id = post_resp.order_id.to_string();
        info!("Polymarket order posted: {order_id}");

        // Poll until filled
        let deadline = Instant::now() + Duration::from_secs(self.fill_timeout_secs);
        loop {
            let resp = client
                .order(&order_id)
                .await
                .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

            // Inspect status via debug representation until we know the exact enum variants
            let status = format!("{:?}", resp.status);
            debug!("order {order_id} status: {status}");

            if status.contains("Matched") || status.contains("Filled") {
                let shares = resp.size_matched.to_string().parse::<f64>().unwrap_or(0.0);
                let price = resp.price.to_string().parse::<f64>().unwrap_or(0.0);

                info!("Order {order_id} filled: {shares} shares @ {price}");
                return Ok(OrderResult {
                    order_id,
                    shares_filled: shares,
                    avg_price: price,
                });
            }

            if Instant::now() >= deadline {
                // Best-effort cancel to free collateral
                let _ = client.cancel_order(&order_id).await;
                return Err(RouterError::PolyFillTimeout {
                    secs: self.fill_timeout_secs,
                    order_id,
                });
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
