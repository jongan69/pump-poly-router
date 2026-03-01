/// Polymarket order placement wrapper.
///
/// Wraps `polymarket_client_sdk::clob::Client` to provide a simpler interface
/// for buying YES/NO outcome position tokens through the CLOB API.
use crate::{
    error::{Result, RouterError},
    types::Outcome,
};
use polymarket_client_sdk::{
    auth::LocalSigner,
    clob::{
        builders::MarketOrderBuilder,
        types::{Amount, Side},
        Client as ClobClient,
    },
    types::Decimal,
};
use serde::{Deserialize, Serialize};
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
    clob: ClobClient,
    signer: LocalSigner,
    fill_timeout_secs: u64,
}

impl PolymarketOrderClient {
    /// Construct from a pre-configured `ClobClient` and signer.
    pub fn new(clob: ClobClient, signer: LocalSigner, fill_timeout_secs: u64) -> Self {
        PolymarketOrderClient { clob, signer, fill_timeout_secs }
    }

    /// Place a market buy order for the given outcome token.
    ///
    /// - `token_id`: ERC-1155 position token ID (from the market's outcome).
    /// - `usdc_amount`: amount in **micro-USDC** (6 decimals) to spend.
    /// - `outcome`: for logging / verification only.
    ///
    /// Polls until filled or `fill_timeout_secs` elapses.
    pub async fn buy_position(
        &self,
        token_id: &str,
        usdc_amount: u64,
        outcome: Outcome,
    ) -> Result<OrderResult> {
        // Convert micro-USDC to a Decimal value the SDK expects (whole USDC).
        let usdc_decimal: Decimal = usdc_amount as f64 / 1_000_000.0;

        let order = MarketOrderBuilder::new()
            .token_id(token_id)
            .side(Side::Buy)
            .amount(Amount::usdc(usdc_decimal))
            .build()
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        // Sign the order
        let order = self
            .clob
            .sign(&self.signer, order)
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        info!("Posting Polymarket {} order for {} USDC on token {}", outcome, usdc_decimal, token_id);

        let post_resp = self
            .clob
            .post_order(&order)
            .await
            .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

        if !post_resp.success {
            return Err(RouterError::PolyOrderPost(format!(
                "order post not accepted: id={}",
                post_resp.order_id
            )));
        }

        let order_id = post_resp.order_id;
        info!("Polymarket order posted: {order_id}");

        // Poll until filled
        self.wait_for_fill(&order_id).await
    }

    /// Poll the CLOB API until the order is filled or the timeout is reached.
    async fn wait_for_fill(&self, order_id: &str) -> Result<OrderResult> {
        let deadline = Instant::now() + Duration::from_secs(self.fill_timeout_secs);

        loop {
            let resp = self
                .clob
                .order(order_id)
                .await
                .map_err(|e| RouterError::PolyOrderPost(e.to_string()))?;

            // The generic response wraps raw JSON; parse what we need.
            let v: serde_json::Value = serde_json::from_str(&resp.0)
                .unwrap_or(serde_json::Value::Null);

            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("unknown");
            debug!("order {order_id} status: {status}");

            if status == "MATCHED" || status == "FILLED" {
                let size_matched = v
                    .get("size_matched")
                    .and_then(|s| s.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let avg_price = v
                    .get("average_price")
                    .and_then(|s| s.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);

                info!("Order {order_id} filled: {size_matched} shares @ {avg_price}");
                return Ok(OrderResult {
                    order_id: order_id.to_string(),
                    shares_filled: size_matched,
                    avg_price,
                });
            }

            if Instant::now() >= deadline {
                // Cancel the unfilled order to free up collateral.
                let _ = self.clob.cancel_order(order_id).await;
                return Err(RouterError::PolyFillTimeout {
                    secs: self.fill_timeout_secs,
                    order_id: order_id.to_string(),
                });
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
