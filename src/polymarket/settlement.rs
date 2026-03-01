/// Polymarket settlement — redeem winning positions and initiate the return bridge.
///
/// After a market resolves in the user's favour, this module:
///   1. Calls `redeemPositions` on the CTF (ConditionalTokens) contract,
///      converting winning ERC-1155 position tokens back to USDC.e.
///   2. (The return bridge is then initiated from executor.rs via CctpPolygonClient.)
///
/// CTF `redeemPositions` ABI:
///   redeemPositions(
///       address collateralToken,
///       bytes32 parentCollectionId,    // 0x000...000 for top-level
///       bytes32 conditionId,
///       uint256[] indexSets
///   )
use crate::{error::{Result, RouterError}, evm::EvmWallet};
use reqwest::Client as HttpClient;
use serde_json::json;
use tracing::info;

const REDEEM_POSITIONS_SELECTOR: &str = "8c5d7d37";

pub struct SettlementClient {
    polygon_rpc: String,
    ctf_address: String,
    wallet: EvmWallet,
    http: HttpClient,
}

impl SettlementClient {
    pub fn new(
        polygon_rpc: impl Into<String>,
        ctf_address: impl Into<String>,
        executor_private_key: &str,
    ) -> Result<Self> {
        let wallet = EvmWallet::new(executor_private_key, 137)?;
        Ok(SettlementClient {
            polygon_rpc: polygon_rpc.into(),
            ctf_address: ctf_address.into(),
            wallet,
            http: HttpClient::new(),
        })
    }

    /// Redeem winning position tokens on Polygon.
    pub async fn redeem_positions(
        &self,
        collateral_token: &str,
        condition_id: &str,
        index_sets: &[u64],
    ) -> Result<String> {
        let calldata = encode_redeem_positions(collateral_token, condition_id, index_sets)?;
        let ctf_address = self.ctf_address.clone();
        let tx_hash = self
            .wallet
            .send_transaction(&self.polygon_rpc, &ctf_address, &calldata, 0)
            .await
            .map_err(|e| RouterError::CtfRedeem(e.to_string()))?;
        info!("CTF redeemPositions submitted: {tx_hash}");
        Ok(tx_hash)
    }

    pub fn address(&self) -> &str {
        &self.wallet.address
    }

    /// Estimate the USDC.e balance of a given address on Polygon.
    pub async fn usdc_balance(&self, usdc_address: &str, executor_address: &str) -> Result<u64> {
        let padded_addr = format!(
            "000000000000000000000000{}",
            executor_address.trim_start_matches("0x")
        );
        let calldata = format!("0x70a08231{padded_addr}");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": usdc_address, "data": calldata}, "latest"],
            "id": 1
        });

        let resp: serde_json::Value = self
            .http
            .post(&self.polygon_rpc)
            .json(&body)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| RouterError::CtfRedeem(e.to_string()))?;

        let hex = resp["result"]
            .as_str()
            .ok_or_else(|| RouterError::CtfRedeem("missing eth_call result".to_string()))?;

        let bytes = hex::decode(hex.trim_start_matches("0x"))
            .map_err(|e| RouterError::CtfRedeem(e.to_string()))?;

        if bytes.len() < 32 {
            return Ok(0);
        }

        let balance = u64::from_be_bytes(bytes[24..32].try_into().unwrap_or([0u8; 8]));
        Ok(balance)
    }
}

// ── ABI encoding ──────────────────────────────────────────────────────────────

fn encode_redeem_positions(
    collateral_token: &str,
    condition_id: &str,
    index_sets: &[u64],
) -> Result<String> {
    let addr_bytes = pad32_address(collateral_token)?;
    let cid_bytes = bytes32_from_hex(condition_id)?;
    let parent = [0u8; 32];
    let array_offset: u64 = 4 * 32;

    let mut data: Vec<u8> = Vec::new();
    data.extend_from_slice(&addr_bytes);
    data.extend_from_slice(&parent);
    data.extend_from_slice(&cid_bytes);
    data.extend_from_slice(&pad32_u64(array_offset));
    data.extend_from_slice(&pad32_u64(index_sets.len() as u64));
    for &idx in index_sets {
        data.extend_from_slice(&pad32_u64(idx));
    }

    Ok(format!("0x{}{}", REDEEM_POSITIONS_SELECTOR, hex::encode(&data)))
}

fn pad32_address(addr: &str) -> Result<[u8; 32]> {
    let hex = addr.trim_start_matches("0x");
    if hex.len() != 40 {
        return Err(RouterError::CtfRedeem(format!("bad address: {addr}")));
    }
    let bytes = hex::decode(hex).map_err(|e| RouterError::CtfRedeem(e.to_string()))?;
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&bytes);
    Ok(out)
}

fn bytes32_from_hex(s: &str) -> Result<[u8; 32]> {
    let hex = s.trim_start_matches("0x");
    let padded = format!("{:0>64}", hex);
    let bytes = hex::decode(&padded).map_err(|e| RouterError::CtfRedeem(e.to_string()))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn pad32_u64(val: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&val.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_redeem_smoke() {
        let calldata = encode_redeem_positions(
            "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
            "0xabc123",
            &[1],
        );
        assert!(calldata.is_ok());
        let s = calldata.unwrap();
        assert!(s.starts_with("0x8c5d7d37"));
    }
}
