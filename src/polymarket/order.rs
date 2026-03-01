/// Polymarket order placement wrapper — official polymarket-client-sdk 0.4.
///
/// `post_order()` posts a market buy and returns the order ID immediately.
/// `wait_for_fill()` polls until the order fills or timeout elapses.
/// `buy_position()` combines both for the common case.
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
    pub shares_filled: f64,
    pub avg_price: f64,
}

pub struct PolymarketOrderClient {
    clob_url: String,
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

    /// Post a market buy order and return the order ID immediately.
    ///
    /// Does NOT wait for the order to fill.  Call `wait_for_fill()` to poll
    /// until the order is matched.
    pub async fn post_order(
        &self,
        token_id: &str,
        usdc_amount: u64,
        outcome: Outcome,
    ) -> Result<String> {
        let signer = PrivateKeySigner::from_str(&self.private_key_hex)
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        let client = Client::new(&self.clob_url, Config::default())
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?
            .authentication_builder(&signer)
            .authenticate()
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        let usdc_decimal = Decimal::new(usdc_amount as i64, 6);

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
        Ok(order_id)
    }

    /// Poll the CLOB for an order's fill status.
    ///
    /// Blocks until the order is filled or `fill_timeout_secs` elapses.
    /// Returns `OrderResult` on fill, or `RouterError::PolyFillTimeout` on timeout.
    pub async fn wait_for_fill(&self, order_id: &str) -> Result<OrderResult> {
        let signer = PrivateKeySigner::from_str(&self.private_key_hex)
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        let client = Client::new(&self.clob_url, Config::default())
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?
            .authentication_builder(&signer)
            .authenticate()
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        let deadline = Instant::now() + Duration::from_secs(self.fill_timeout_secs);

        loop {
            let resp = client
                .order(order_id)
                .await
                .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

            let status = format!("{:?}", resp.status);
            debug!("order {order_id} status: {status}");

            if status.contains("Matched") || status.contains("Filled") {
                let shares = resp.size_matched.to_string().parse::<f64>().unwrap_or(0.0);
                let price = resp.price.to_string().parse::<f64>().unwrap_or(0.0);

                info!("Order {order_id} filled: {shares} shares @ {price}");
                return Ok(OrderResult {
                    order_id: order_id.to_string(),
                    shares_filled: shares,
                    avg_price: price,
                });
            }

            if Instant::now() >= deadline {
                let _ = client.cancel_order(order_id).await;
                return Err(RouterError::PolyFillTimeout {
                    secs: self.fill_timeout_secs,
                    order_id: order_id.to_string(),
                });
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Convenience: post and wait for fill in one call.
    pub async fn buy_position(
        &self,
        token_id: &str,
        usdc_amount: u64,
        outcome: Outcome,
    ) -> Result<OrderResult> {
        let order_id = self.post_order(token_id, usdc_amount, outcome).await?;
        self.wait_for_fill(&order_id).await
    }
}
