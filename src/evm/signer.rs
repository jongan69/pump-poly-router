/// EVM transaction signing using libsecp256k1 (already in the Solana dep tree).
///
/// Builds, signs, and broadcasts legacy EVM transactions (EIP-155) without
/// adding any new dependency that conflicts with Solana 1.18's zeroize version.
///
/// Signing algorithm:
///   1. RLP-encode (nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0)
///   2. keccak256 that payload → signing_hash
///   3. secp256k1 sign with recovery_id
///   4. RLP-encode (nonce, gasPrice, gasLimit, to, value, data, v, r, s)
///   5. eth_sendRawTransaction(hex(rlp_bytes))
use crate::error::{Result, RouterError};
use libsecp256k1::{Message, PublicKey, SecretKey};
use reqwest::Client as HttpClient;
use serde_json::json;
use sha3::{Digest, Keccak256};
use tracing::debug;

// ── Key helpers ───────────────────────────────────────────────────────────────

/// Parse a hex private key (0x-prefixed or bare) into a libsecp256k1 SecretKey.
fn parse_secret_key(hex_key: &str) -> Result<SecretKey> {
    let stripped = hex_key.trim_start_matches("0x");
    let bytes = hex::decode(stripped)
        .map_err(|_| RouterError::Config("EVM private key is not valid hex".to_string()))?;
    if bytes.len() != 32 {
        return Err(RouterError::Config("EVM private key must be 32 bytes".to_string()));
    }
    SecretKey::parse_slice(&bytes)
        .map_err(|_| RouterError::Config("EVM private key is not a valid secp256k1 scalar".to_string()))
}

/// Derive the Ethereum address from a private key.
/// Returns lowercase hex without 0x prefix.
pub fn address_from_key(hex_key: &str) -> Result<String> {
    let sk = parse_secret_key(hex_key)?;
    let pk = PublicKey::from_secret_key(&sk);
    // Uncompressed public key = 65 bytes: 04 || X || Y
    let pk_bytes = pk.serialize();
    // Ethereum address = keccak256(pk_bytes[1..])[12..]
    let hash = Keccak256::digest(&pk_bytes[1..]);
    Ok(hex::encode(&hash[12..]))
}

// ── RLP encoding ──────────────────────────────────────────────────────────────
// Minimal RLP encoder for the 9-field legacy transaction structure.

fn rlp_encode(items: &[RlpItem]) -> Vec<u8> {
    let mut payload = Vec::new();
    for item in items {
        payload.extend(item.encode());
    }
    wrap_list(&payload)
}

enum RlpItem<'a> {
    Bytes(&'a [u8]),
    Uint(u64),
    BigUint256(&'a [u8; 32]),
}

impl RlpItem<'_> {
    fn encode(&self) -> Vec<u8> {
        match self {
            RlpItem::Uint(v) => encode_uint(*v),
            RlpItem::Bytes(b) => encode_bytes(b),
            RlpItem::BigUint256(b) => encode_bytes(strip_leading_zeros(*b)),
        }
    }
}

fn strip_leading_zeros(b: &[u8]) -> &[u8] {
    let first_nonzero = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    &b[first_nonzero..]
}

fn encode_uint(v: u64) -> Vec<u8> {
    if v == 0 {
        return vec![0x80]; // RLP empty byte string = 0
    }
    let bytes = v.to_be_bytes();
    let stripped = strip_leading_zeros(&bytes);
    encode_bytes(stripped)
}

fn encode_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return vec![b[0]];
    }
    let mut out = encode_length(b.len(), 0x80);
    out.extend_from_slice(b);
    out
}

fn wrap_list(payload: &[u8]) -> Vec<u8> {
    let mut out = encode_length(payload.len(), 0xc0);
    out.extend_from_slice(payload);
    out
}

fn encode_length(len: usize, offset: u8) -> Vec<u8> {
    if len < 56 {
        vec![offset + len as u8]
    } else {
        let len_bytes = (len as u64).to_be_bytes();
        let stripped = strip_leading_zeros(&len_bytes);
        let mut out = vec![offset + 55 + stripped.len() as u8];
        out.extend_from_slice(stripped);
        out
    }
}

// ── EVM Wallet ────────────────────────────────────────────────────────────────

pub struct EvmWallet {
    private_key: String,
    pub address: String,
    chain_id: u64,
    http: HttpClient,
}

impl EvmWallet {
    pub fn new(private_key: &str, chain_id: u64) -> Result<Self> {
        let address = address_from_key(private_key)?;
        Ok(EvmWallet {
            private_key: private_key.to_string(),
            address: format!("0x{address}"),
            chain_id,
            http: HttpClient::new(),
        })
    }

    /// Get the current nonce for the executor address.
    pub async fn get_nonce(&self, rpc_url: &str) -> Result<u64> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "eth_getTransactionCount",
            "params": [self.address, "latest"],
            "id": 1
        });
        let resp: serde_json::Value = self.http.post(rpc_url).json(&body).send().await?.json().await?;
        let hex = resp["result"].as_str()
            .ok_or_else(|| RouterError::Other("eth_getTransactionCount missing result".to_string()))?;
        let nonce = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|_| RouterError::Other("nonce parse error".to_string()))?;
        Ok(nonce)
    }

    /// Get the current gas price from the network.
    pub async fn get_gas_price(&self, rpc_url: &str) -> Result<u64> {
        let body = json!({ "jsonrpc": "2.0", "method": "eth_gasPrice", "params": [], "id": 1 });
        let resp: serde_json::Value = self.http.post(rpc_url).json(&body).send().await?.json().await?;
        let hex = resp["result"].as_str()
            .ok_or_else(|| RouterError::Other("eth_gasPrice missing result".to_string()))?;
        let price = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|_| RouterError::Other("gas price parse error".to_string()))?;
        // Add 20% tip to avoid underpricing
        Ok(price + price / 5)
    }

    /// Estimate gas for a call.
    pub async fn estimate_gas(&self, rpc_url: &str, to: &str, calldata: &str) -> Result<u64> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "eth_estimateGas",
            "params": [{ "from": self.address, "to": to, "data": calldata }],
            "id": 1
        });
        let resp: serde_json::Value = self.http.post(rpc_url).json(&body).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(RouterError::Other(format!("eth_estimateGas error: {err}")));
        }
        let hex = resp["result"].as_str()
            .ok_or_else(|| RouterError::Other("eth_estimateGas missing result".to_string()))?;
        let gas = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|_| RouterError::Other("gas estimate parse error".to_string()))?;
        // Add 30% buffer
        Ok(gas + gas / 3)
    }

    /// Build, sign, and send an EVM transaction.
    ///
    /// Returns the transaction hash.
    pub async fn send_transaction(
        &self,
        rpc_url: &str,
        to: &str,
        calldata: &str,
        value: u64,
    ) -> Result<String> {
        let nonce = self.get_nonce(rpc_url).await?;
        let gas_price = self.get_gas_price(rpc_url).await?;
        let gas_limit = self.estimate_gas(rpc_url, to, calldata).await?;

        debug!(
            "EVM tx: nonce={nonce} gas_price={gas_price} gas_limit={gas_limit} to={to}",
        );

        let raw_tx = self.sign_transaction(to, calldata, value, nonce, gas_price, gas_limit)?;
        let tx_hash = self.eth_send_raw_transaction(rpc_url, &raw_tx).await?;
        Ok(tx_hash)
    }

    /// Sign an EVM legacy transaction (EIP-155).
    ///
    /// Returns the hex-encoded raw transaction (0x-prefixed).
    fn sign_transaction(
        &self,
        to: &str,
        calldata: &str,
        value: u64,
        nonce: u64,
        gas_price: u64,
        gas_limit: u64,
    ) -> Result<String> {
        let sk = parse_secret_key(&self.private_key)?;

        // Decode the destination address and calldata
        let to_bytes = hex::decode(to.trim_start_matches("0x"))
            .map_err(|_| RouterError::Other(format!("invalid to address: {to}")))?;
        let data_bytes = hex::decode(calldata.trim_start_matches("0x"))
            .map_err(|_| RouterError::Other("invalid calldata hex".to_string()))?;

        // 1. RLP-encode unsigned transaction for signing (EIP-155)
        let signing_rlp = rlp_encode(&[
            RlpItem::Uint(nonce),
            RlpItem::Uint(gas_price),
            RlpItem::Uint(gas_limit),
            RlpItem::Bytes(&to_bytes),
            RlpItem::Uint(value),
            RlpItem::Bytes(&data_bytes),
            RlpItem::Uint(self.chain_id), // chainId
            RlpItem::Uint(0),             // 0
            RlpItem::Uint(0),             // 0
        ]);

        // 2. Hash and sign
        let signing_hash = Keccak256::digest(&signing_rlp);
        let msg = Message::parse_slice(&signing_hash)
            .map_err(|_| RouterError::Other("signing hash parse failed".to_string()))?;
        let (sig, rec_id) = libsecp256k1::sign(&msg, &sk);

        // 3. Extract r, s, compute v (EIP-155)
        let sig_bytes = sig.serialize();
        let r: [u8; 32] = sig_bytes[..32].try_into().unwrap();
        let s: [u8; 32] = sig_bytes[32..].try_into().unwrap();
        let v = self.chain_id * 2 + 35 + rec_id.serialize() as u64;

        // 4. RLP-encode signed transaction
        let signed_rlp = rlp_encode(&[
            RlpItem::Uint(nonce),
            RlpItem::Uint(gas_price),
            RlpItem::Uint(gas_limit),
            RlpItem::Bytes(&to_bytes),
            RlpItem::Uint(value),
            RlpItem::Bytes(&data_bytes),
            RlpItem::Uint(v),
            RlpItem::BigUint256(&r),
            RlpItem::BigUint256(&s),
        ]);

        Ok(format!("0x{}", hex::encode(signed_rlp)))
    }

    async fn eth_send_raw_transaction(&self, rpc_url: &str, raw_tx: &str) -> Result<String> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": "eth_sendRawTransaction",
            "params": [raw_tx],
            "id": 1
        });
        let resp: serde_json::Value = self.http.post(rpc_url).json(&body).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(RouterError::Other(format!("eth_sendRawTransaction error: {err}")));
        }
        let hash = resp["result"].as_str()
            .ok_or_else(|| RouterError::Other("missing tx hash in sendRawTransaction response".to_string()))?
            .to_string();
        Ok(hash)
    }
}

// ── CCTP message hash derivation ──────────────────────────────────────────────

/// Compute the Circle attestation message hash from the raw CCTP message bytes.
///
/// Circle's attestation API expects `keccak256(message_bytes)` as the lookup key.
/// The `message_bytes` are emitted by the CCTP MessageTransmitter program in a
/// `MessageSent` event.  On Solana, parse the transaction logs for the CPI event
/// that contains the message bytes, then call this function.
pub fn cctp_message_hash(message_bytes: &[u8]) -> String {
    let hash = Keccak256::digest(message_bytes);
    format!("0x{}", hex::encode(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic test key (NOT a real key — only used in tests)
    const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn address_derivation() {
        // Known address for the Hardhat default account #0
        let addr = address_from_key(TEST_KEY).unwrap();
        assert_eq!(addr.to_lowercase(), "f39fd6e51aad88f6f4ce6ab8827279cfffb92266");
    }

    #[test]
    fn rlp_uint_zero() {
        let encoded = encode_uint(0);
        assert_eq!(encoded, vec![0x80]);
    }

    #[test]
    fn rlp_uint_small() {
        // Single byte < 0x80 is encoded as itself
        assert_eq!(encode_uint(0x42), vec![0x42]);
    }

    #[test]
    fn sign_transaction_smoke() {
        let wallet = EvmWallet::new(TEST_KEY, 137).unwrap();
        assert!(wallet.address.starts_with("0x"));

        let raw = wallet.sign_transaction(
            "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
            "0x70a08231",
            0,
            0,      // nonce
            30_000_000_000, // 30 gwei
            50_000, // gas limit
        ).unwrap();
        assert!(raw.starts_with("0x"));
    }

    #[test]
    fn cctp_hash_smoke() {
        let h = cctp_message_hash(b"hello");
        assert!(h.starts_with("0x"));
        assert_eq!(h.len(), 66);
    }
}
