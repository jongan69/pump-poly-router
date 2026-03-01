/// Jupiter v6 DEX aggregator client.
///
/// Handles SPL-token-to-USDC (and USDC-to-SOL) swap quoting and execution
/// without any dependency on Pump.fun's bonding curve — all routing is
/// delegated to Jupiter's solver network.
use crate::error::{Result, RouterError};
use base64::Engine;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::VersionedTransaction,
};

// ── Request / response types ──────────────────────────────────────────────────

/// Response from Jupiter `/v6/quote`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u16,
    /// Nested route plan (opaque; forwarded as-is to /swap).
    pub route_plan: serde_json::Value,
    /// Price impact as a percentage string (e.g. "0.01").
    #[serde(default)]
    pub price_impact_pct: String,
}

impl QuoteResponse {
    /// Output amount as u64 (native units of output mint).
    pub fn out_amount_u64(&self) -> Result<u64> {
        self.out_amount
            .parse::<u64>()
            .map_err(|_| RouterError::JupiterQuote("invalid out_amount".to_string()))
    }
}

/// Request body for Jupiter `/v6/swap`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwapRequest<'a> {
    quote_response: &'a QuoteResponse,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    /// Compute unit price in micro-lamports (for priority fee).
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_unit_price_micro_lamports: Option<u64>,
    /// Set true to get a VersionedTransaction.
    as_legacy_transaction: bool,
}

/// Response from Jupiter `/v6/swap`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    /// Base64-encoded serialised `VersionedTransaction` (unsigned).
    swap_transaction: String,
}

// ── Client ────────────────────────────────────────────────────────────────────

pub struct JupiterClient {
    base_url: String,
    http: HttpClient,
}

impl JupiterClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        JupiterClient { base_url: base_url.into(), http: HttpClient::new() }
    }

    /// Get a swap quote from Jupiter.
    ///
    /// - `input_mint` / `output_mint`: base58 pubkeys
    /// - `amount`: native units of the input token
    /// - `slippage_bps`: e.g. 50 = 0.5 %
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse> {
        let url = format!(
            "{}/v6/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.base_url, input_mint, output_mint, amount, slippage_bps
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| RouterError::JupiterQuote(e.to_string()))?;

        let quote: QuoteResponse = resp
            .json()
            .await
            .map_err(|e| RouterError::JupiterQuote(e.to_string()))?;
        Ok(quote)
    }

    /// Fetch the swap transaction from Jupiter for the given quote.
    ///
    /// Returns a base64-encoded, unsigned `VersionedTransaction`.
    pub async fn get_swap_transaction(
        &self,
        quote: &QuoteResponse,
        user_pubkey: &Pubkey,
        priority_fee_micro_lamports: Option<u64>,
    ) -> Result<String> {
        let body = SwapRequest {
            quote_response: quote,
            user_public_key: user_pubkey.to_string(),
            wrap_and_unwrap_sol: true,
            compute_unit_price_micro_lamports: priority_fee_micro_lamports,
            as_legacy_transaction: false,
        };

        let url = format!("{}/v6/swap", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| RouterError::JupiterSwap(e.to_string()))?;

        let swap: SwapResponse = resp
            .json()
            .await
            .map_err(|e| RouterError::JupiterSwap(e.to_string()))?;
        Ok(swap.swap_transaction)
    }

    /// Sign and submit a Jupiter swap transaction.
    ///
    /// Returns the Solana transaction signature.
    pub async fn execute_swap(
        &self,
        tx_b64: &str,
        payer: &Keypair,
        rpc: &RpcClient,
    ) -> Result<Signature> {
        // Decode base64 → raw bytes
        let tx_bytes = base64::engine::general_purpose::STANDARD
            .decode(tx_b64)
            .map_err(|e| RouterError::JupiterSwap(format!("base64 decode: {e}")))?;

        // Deserialize as VersionedTransaction
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)
            .map_err(|e| RouterError::JupiterSwap(format!("tx deserialise: {e}")))?;

        // Sign
        let recent_blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| RouterError::JupiterSwap(format!("get blockhash: {e}")))?;

        tx.message.set_recent_blockhash(recent_blockhash);

        let sig = payer.sign_message(tx.message.serialize().as_slice());
        if tx.signatures.is_empty() {
            tx.signatures.push(sig);
        } else {
            tx.signatures[0] = sig;
        }

        // Send and confirm
        let signature = rpc
            .send_and_confirm_transaction(&tx)
            .await
            .map_err(|e| RouterError::JupiterSwap(format!("send_and_confirm: {e}")))?;
        Ok(signature)
    }

    /// Convenience: quote + execute in one call.
    pub async fn swap(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
        payer: &Keypair,
        rpc: &RpcClient,
    ) -> Result<(Signature, u64)> {
        let quote = self.get_quote(input_mint, output_mint, amount, slippage_bps).await?;
        let out_amount = quote.out_amount_u64()?;
        let tx_b64 = self.get_swap_transaction(&quote, &payer.pubkey(), None).await?;
        let sig = self.execute_swap(&tx_b64, payer, rpc).await?;
        Ok((sig, out_amount))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_out_amount() {
        let q = QuoteResponse {
            input_mint: "a".into(),
            in_amount: "1000000".into(),
            output_mint: "b".into(),
            out_amount: "999000".into(),
            other_amount_threshold: "950000".into(),
            swap_mode: "ExactIn".into(),
            slippage_bps: 50,
            route_plan: serde_json::Value::Null,
            price_impact_pct: "0.01".into(),
        };
        assert_eq!(q.out_amount_u64().unwrap(), 999_000);
    }
}
