/// Watches for Polymarket condition resolution on Polygon.
///
/// Uses raw JSON-RPC `eth_getLogs` to query the ConditionalTokens (CTF)
/// contract for `ConditionResolution` events.
///
/// Event signature:
///   ConditionResolution(bytes32 indexed conditionId, address indexed oracle,
///                       bytes32 indexed questionId, uint outcomeSlotCount,
///                       uint[] payoutNumerators)
/// Topic0: keccak256("ConditionResolution(bytes32,address,bytes32,uint256,uint256[])")
///       = 0xb8da7...  (computed from ABI)
use crate::error::{Result, RouterError};
use reqwest::Client as HttpClient;
use serde_json::json;
use tracing::debug;

/// Topic0 for the CTF `ConditionResolution` event.
const CONDITION_RESOLUTION_TOPIC: &str =
    "0xb8da7db0a8a9d1046bd78f1a7ab0a9c4537a8900bfeeac52d8e7c5c81ef76b37";

/// The payout denominators for a resolved condition.  Each element corresponds
/// to one outcome slot (index 0 = YES, index 1 = NO on binary markets).
/// Values sum to 1_000_000_000 (10^9 fixed-point scale used by the CTF).
#[derive(Debug, Clone)]
pub struct ResolutionOutcome {
    pub condition_id: String,
    pub payout_numerators: Vec<u64>,
}

impl ResolutionOutcome {
    /// True if the YES outcome won (first slot has the non-zero payout).
    pub fn yes_wins(&self) -> bool {
        self.payout_numerators.first().copied().unwrap_or(0) > 0
    }
}

pub struct ConditionResolver {
    polygon_rpc: String,
    ctf_address: String,
    http: HttpClient,
}

impl ConditionResolver {
    pub fn new(polygon_rpc: impl Into<String>, ctf_address: impl Into<String>) -> Self {
        ConditionResolver {
            polygon_rpc: polygon_rpc.into(),
            ctf_address: ctf_address.into(),
            http: HttpClient::new(),
        }
    }

    /// Check whether a given condition has been resolved.
    ///
    /// Returns `Some(ResolutionOutcome)` if resolved, `None` if still open.
    pub async fn is_resolved(&self, condition_id: &str) -> Result<Option<ResolutionOutcome>> {
        // Normalise condition_id to 0x-prefixed, left-padded 32 bytes for the topic filter.
        let topic1 = normalise_bytes32(condition_id)?;

        let params = json!([{
            "address": self.ctf_address,
            "topics": [CONDITION_RESOLUTION_TOPIC, topic1],
            "fromBlock": "earliest",
            "toBlock": "latest"
        }]);

        let body = json!({
            "jsonrpc": "2.0",
            "method": "eth_getLogs",
            "params": params,
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
            .map_err(|e| RouterError::PolyResolver(e.to_string()))?;

        if let Some(err) = resp.get("error") {
            return Err(RouterError::PolyResolver(err.to_string()));
        }

        let logs = resp["result"].as_array().ok_or_else(|| {
            RouterError::PolyResolver("eth_getLogs missing result array".to_string())
        })?;

        debug!("ConditionResolution logs for {condition_id}: {} entries", logs.len());

        if logs.is_empty() {
            return Ok(None);
        }

        // Decode the first matching log.
        let log = &logs[0];
        let data_hex = log["data"]
            .as_str()
            .ok_or_else(|| RouterError::PolyResolver("missing log data".to_string()))?;

        let payout_numerators = decode_payout_numerators(data_hex)?;

        Ok(Some(ResolutionOutcome {
            condition_id: condition_id.to_string(),
            payout_numerators,
        }))
    }
}

// ── ABI decoding helpers ──────────────────────────────────────────────────────

/// Ensure the condition ID is hex-encoded with 0x prefix and padded to 32 bytes.
fn normalise_bytes32(hex_str: &str) -> Result<String> {
    let stripped = hex_str.trim_start_matches("0x");
    if stripped.len() > 64 {
        return Err(RouterError::PolyResolver(format!("bytes32 too long: {hex_str}")));
    }
    // left-pad to 64 hex chars
    Ok(format!("0x{:0>64}", stripped))
}

/// Decode the `payoutNumerators` dynamic array from CTF event data.
///
/// The event `data` field contains ABI-encoded non-indexed params:
///   uint256 outcomeSlotCount, uint256[] payoutNumerators
fn decode_payout_numerators(data_hex: &str) -> Result<Vec<u64>> {
    let bytes = hex::decode(data_hex.trim_start_matches("0x"))
        .map_err(|e| RouterError::PolyResolver(format!("hex decode: {e}")))?;

    if bytes.len() < 64 {
        return Err(RouterError::PolyResolver("log data too short".to_string()));
    }

    // Word 0: outcomeSlotCount (u256 — we read lower 8 bytes)
    // Word 1: offset of the dynamic array (should be 0x40 = 64)
    // Word 2 (at offset from word 1): array length
    // Words 3+: array elements

    let array_offset = u256_to_usize(&bytes[32..64])?;
    if bytes.len() < array_offset + 32 {
        return Err(RouterError::PolyResolver("log data truncated (array length)".to_string()));
    }

    let array_len = u256_to_usize(&bytes[array_offset..array_offset + 32])?;
    let data_start = array_offset + 32;
    let data_end = data_start + array_len * 32;

    if bytes.len() < data_end {
        return Err(RouterError::PolyResolver("log data truncated (array elements)".to_string()));
    }

    let mut result = Vec::with_capacity(array_len);
    for i in 0..array_len {
        let word_start = data_start + i * 32;
        let val = u64::from_be_bytes(bytes[word_start + 24..word_start + 32].try_into().unwrap());
        result.push(val);
    }

    Ok(result)
}

fn u256_to_usize(word: &[u8]) -> Result<usize> {
    if word.len() < 32 {
        return Err(RouterError::PolyResolver("word too short".to_string()));
    }
    // Only use the lower 8 bytes (u64) — values should be small.
    let val = u64::from_be_bytes(word[24..32].try_into().unwrap()) as usize;
    Ok(val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_bytes32_pads() {
        let s = normalise_bytes32("abc123").unwrap();
        assert!(s.starts_with("0x"));
        assert_eq!(s.len(), 66); // 0x + 64 hex chars
    }

    #[test]
    fn yes_wins() {
        let r = ResolutionOutcome {
            condition_id: "test".into(),
            payout_numerators: vec![1_000_000_000, 0],
        };
        assert!(r.yes_wins());
    }

    #[test]
    fn no_wins() {
        let r = ResolutionOutcome {
            condition_id: "test".into(),
            payout_numerators: vec![0, 1_000_000_000],
        };
        assert!(!r.yes_wins());
    }
}
