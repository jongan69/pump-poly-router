/// Circle CCTP bridge — Solana side (depositForBurn + receiveMessage) and
/// Polygon side (receiveMessage + depositForBurn for the return leg).
///
/// Forward path (inbound):
///   Solana depositForBurn → Circle attestation → Polygon receiveMessage
///
/// Return path (outbound):
///   Polygon approve + depositForBurn → Circle attestation → Solana receiveMessage
use crate::{
    config::{CCTP_POLYGON_DOMAIN, CCTP_SOLANA_DOMAIN},
    error::{Result, RouterError},
    evm::{extract_nonce_from_cctp_message, EvmWallet},
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use borsh::{BorshDeserialize, BorshSerialize};
use sha2::{Digest as Sha2Digest, Sha256 as Sha2_256};
use sha3::Keccak256;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    system_program,
    transaction::Transaction,
};
use solana_transaction_status::UiTransactionEncoding;
use std::str::FromStr;
use tracing::{info, warn};

// ── Solana CCTP program IDs (mainnet) ─────────────────────────────────────────

#[allow(dead_code)]
const CCTP_TOKEN_MESSENGER_MINTER_PROGRAM: &str =
    "CCTPiPYEnTQLuNaWZkhe7mWx5bkGEuHiVLmRKs7VHqpW";

const CCTP_MESSAGE_TRANSMITTER_PROGRAM: &str = "CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd";

// Anchor instruction discriminators (sha256("global:{name}")[0..8])
const DEPOSIT_FOR_BURN_DISCRIMINATOR: [u8; 8] = [0x9c, 0x91, 0x72, 0x37, 0xf8, 0xa4, 0x24, 0x73];

/// ABI function selector for Polygon CCTP `receiveMessage(bytes,bytes)` = 0x57ecfd28
const RECEIVE_MESSAGE_SELECTOR: &str = "57ecfd28";

/// ABI function selector for Polygon CCTP `depositForBurn(uint256,uint32,bytes32,address)`.
/// keccak256("depositForBurn(uint256,uint32,bytes32,address)")[0..4]
fn deposit_for_burn_polygon_selector() -> [u8; 4] {
    let hash = Keccak256::digest("depositForBurn(uint256,uint32,bytes32,address)");
    hash[..4].try_into().unwrap()
}

/// ABI function selector for ERC-20 `approve(address,uint256)` = 0x095ea7b3
const APPROVE_SELECTOR: &str = "095ea7b3";

/// Topic0 for the EVM CCTP `MessageSent(bytes)` event.
/// keccak256("MessageSent(bytes)")
fn message_sent_topic0() -> [u8; 32] {
    let hash = Keccak256::digest("MessageSent(bytes)");
    hash.into()
}

/// Anchor event discriminator for `MessageSent { message: Vec<u8> }`.
/// sha256("event:MessageSent")[0..8]
fn anchor_message_sent_discriminator() -> [u8; 8] {
    let hash = Sha2_256::digest("event:MessageSent");
    hash[..8].try_into().unwrap()
}

/// Parameters for the Solana `depositForBurn` instruction.
#[derive(BorshSerialize, BorshDeserialize, Debug)]
struct DepositForBurnParams {
    amount: u64,
    destination_domain: u32,
    mint_recipient: [u8; 32],
}

/// Parses a hex `0x`-prefixed EVM address into a 32-byte array (left-padded).
fn evm_addr_to_bytes32(addr: &str) -> Result<[u8; 32]> {
    let hex = addr.strip_prefix("0x").unwrap_or(addr);
    if hex.len() != 40 {
        return Err(RouterError::Config(format!("EVM address wrong length: {addr}")));
    }
    let bytes = hex::decode(hex)
        .map_err(|_| RouterError::Config(format!("invalid hex in EVM address: {addr}")))?;
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&bytes);
    Ok(out)
}


// ── Solana-side CCTP client ───────────────────────────────────────────────────

pub struct CctpSolanaClient {
    token_messenger_program: Pubkey,
    message_transmitter_program: Pubkey,
}

impl CctpSolanaClient {
    pub fn new(token_messenger_program_id: &str) -> Result<Self> {
        Ok(CctpSolanaClient {
            token_messenger_program: Pubkey::from_str(token_messenger_program_id)
                .map_err(|_| RouterError::Config("invalid CCTP program id".to_string()))?,
            message_transmitter_program: Pubkey::from_str(CCTP_MESSAGE_TRANSMITTER_PROGRAM)
                .expect("hardcoded address is valid"),
        })
    }

    /// Build and send a `depositForBurn` instruction.
    ///
    /// Returns `(signature, message_bytes_hex)`.  The raw message bytes are
    /// parsed from the `MessageSent` CPI event in the transaction logs and are
    /// needed for the `receiveMessage` call on Polygon and for Circle's
    /// attestation API (`keccak256(message_bytes)` is the lookup hash).
    pub async fn deposit_for_burn(
        &self,
        rpc: &RpcClient,
        payer: &Keypair,
        usdc_mint: &Pubkey,
        amount: u64,
        recipient_evm_address: &str,
    ) -> Result<(Signature, String)> {
        let mint_recipient = evm_addr_to_bytes32(recipient_evm_address)?;

        let params = DepositForBurnParams {
            amount,
            destination_domain: CCTP_POLYGON_DOMAIN,
            mint_recipient,
        };

        let mut data = DEPOSIT_FOR_BURN_DISCRIMINATOR.to_vec();
        data.extend(borsh::to_vec(&params).map_err(|e| RouterError::CctpDeposit(e.to_string()))?);

        let (token_messenger_pda, _) = Pubkey::find_program_address(
            &[b"token_messenger"],
            &self.token_messenger_program,
        );
        let (message_transmitter_pda, _) = Pubkey::find_program_address(
            &[b"message_transmitter"],
            &self.message_transmitter_program,
        );
        let (token_minter_pda, _) =
            Pubkey::find_program_address(&[b"token_minter"], &self.token_messenger_program);

        let sender_ata = spl_associated_token_account::get_associated_token_address(
            &payer.pubkey(),
            usdc_mint,
        );

        let accounts = vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(token_messenger_pda, false),
            AccountMeta::new(message_transmitter_pda, false),
            AccountMeta::new_readonly(token_minter_pda, false),
            AccountMeta::new(sender_ata, false),
            AccountMeta::new(*usdc_mint, false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(system_program::id(), false),
        ];

        let ix = Instruction {
            program_id: self.token_messenger_program,
            accounts,
            data,
        };

        let recent_blockhash = rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        let sig = rpc
            .send_and_confirm_transaction(&tx)
            .await
            .map_err(|e| RouterError::CctpDeposit(e.to_string()))?;

        info!("CCTP depositForBurn submitted: {sig}");

        // Parse the MessageSent CPI event from the transaction logs to get the
        // raw message bytes.  These are required for Polygon receiveMessage and
        // for the Circle attestation API lookup (keccak256 of these bytes).
        let message_bytes = self.parse_message_sent_from_tx(rpc, &sig).await
            .unwrap_or_else(|e| {
                warn!("Could not parse CCTP MessageSent event from logs: {e}. \
                       Using tx signature as fallback (attestation polling may fail).");
                hex::encode(sig.as_ref())
            });

        let message_bytes_hex = format!("0x{message_bytes}");
        Ok((sig, message_bytes_hex))
    }

    /// Parse the raw CCTP `MessageSent` event bytes from a Solana transaction.
    ///
    /// Circle's `message_transmitter` program (Anchor) emits the event as a
    /// CPI log: `"Program data: <base64(8-byte-discriminator + borsh Vec<u8>)>"`.
    async fn parse_message_sent_from_tx(
        &self,
        rpc: &RpcClient,
        sig: &Signature,
    ) -> Result<String> {
        let tx = rpc
            .get_transaction(sig, UiTransactionEncoding::Json)
            .await
            .map_err(|e| RouterError::CctpDeposit(format!("get_transaction: {e}")))?;

        let meta = tx
            .transaction
            .meta
            .ok_or_else(|| RouterError::CctpDeposit("no transaction meta".to_string()))?;

        let logs = meta
            .log_messages
            .ok_or_else(|| RouterError::CctpDeposit("no log messages in tx meta".to_string()))?;

        let disc = anchor_message_sent_discriminator();

        for log in &logs {
            if let Some(b64) = log.strip_prefix("Program data: ") {
                if let Ok(bytes) = BASE64.decode(b64.trim()) {
                    if bytes.len() >= 12 && bytes[..8] == disc {
                        // Borsh Vec<u8>: 4-byte LE length + data
                        let msg_len =
                            u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
                        if bytes.len() >= 12 + msg_len {
                            return Ok(hex::encode(&bytes[12..12 + msg_len]));
                        }
                    }
                }
            }
        }

        Err(RouterError::CctpDeposit(
            "MessageSent event not found in transaction logs".to_string(),
        ))
    }

    /// Build and send a Solana CCTP `receiveMessage` instruction.
    ///
    /// Called during the return path (settlement bridge) after Circle attests
    /// the Polygon `depositForBurn` transaction.
    ///
    /// Returns the Solana transaction signature.
    pub async fn receive_message(
        &self,
        rpc: &RpcClient,
        payer: &Keypair,
        message: &[u8],
        attestation: &[u8],
    ) -> Result<Signature> {
        // Anchor instruction discriminator for `receive_message`:
        // sha256("global:receive_message")[0..8]
        let disc: [u8; 8] = {
            let hash = Sha2_256::digest("global:receive_message");
            hash[..8].try_into().unwrap()
        };

        // Borsh-encode the params: message (Vec<u8>) + attestation (Vec<u8>)
        let mut data = disc.to_vec();
        // Borsh Vec<u8>: 4-byte LE len + bytes
        data.extend_from_slice(&(message.len() as u32).to_le_bytes());
        data.extend_from_slice(message);
        data.extend_from_slice(&(attestation.len() as u32).to_le_bytes());
        data.extend_from_slice(attestation);

        let (message_transmitter_pda, _) = Pubkey::find_program_address(
            &[b"message_transmitter"],
            &self.message_transmitter_program,
        );

        // Extract nonce and source domain from message to build the used-nonces PDA.
        let nonce = extract_nonce_from_cctp_message(message);
        let source_domain = if message.len() >= 8 {
            u32::from_be_bytes(message[4..8].try_into().unwrap_or([0u8; 4]))
        } else {
            CCTP_POLYGON_DOMAIN
        };

        let (used_nonces_pda, _) = Pubkey::find_program_address(
            &[
                b"used_nonces",
                &source_domain.to_be_bytes(),
                &nonce.to_be_bytes(),
            ],
            &self.message_transmitter_program,
        );

        let accounts = vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(message_transmitter_pda, false),
            AccountMeta::new(used_nonces_pda, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ];

        let ix = Instruction {
            program_id: self.message_transmitter_program,
            accounts,
            data,
        };

        let recent_blockhash = rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[payer],
            recent_blockhash,
        );

        let sig = rpc
            .send_and_confirm_transaction(&tx)
            .await
            .map_err(|e| RouterError::CctpReceive(e.to_string()))?;

        info!("CCTP receiveMessage submitted on Solana: {sig}");
        Ok(sig)
    }
}

// ── Polygon-side CCTP client (raw JSON-RPC) ───────────────────────────────────

pub struct CctpPolygonClient {
    rpc_url: String,
    message_transmitter: String,
    token_messenger: String,
    wallet: EvmWallet,
    http: reqwest::Client,
}

impl CctpPolygonClient {
    pub fn new(
        rpc_url: impl Into<String>,
        message_transmitter: impl Into<String>,
        executor_private_key: &str,
    ) -> Result<Self> {
        let rpc_url = rpc_url.into();
        let wallet = EvmWallet::new(executor_private_key, 137)?;
        Ok(CctpPolygonClient {
            rpc_url,
            message_transmitter: message_transmitter.into(),
            // Default to mainnet; overridden by config if needed.
            token_messenger: "0x9daF8c91AEFAE50b9c0E69629D3f6Ca40cA3B3FE".to_string(),
            wallet,
            http: reqwest::Client::new(),
        })
    }

    /// Set the Polygon TokenMessenger address (for the return-leg depositForBurn).
    pub fn with_token_messenger(mut self, addr: impl Into<String>) -> Self {
        self.token_messenger = addr.into();
        self
    }

    /// Relay the attested CCTP message to Polygon by calling `receiveMessage`.
    ///
    /// `message_bytes` and `attestation` are hex-encoded (0x-prefixed).
    ///
    /// Returns the Polygon transaction hash.
    pub async fn receive_message(
        &self,
        message_bytes: &str,
        attestation: &str,
    ) -> Result<String> {
        let calldata = encode_receive_message_calldata(message_bytes, attestation)?;
        let tx_hash = self
            .wallet
            .send_transaction(&self.rpc_url, &self.message_transmitter, &calldata, 0)
            .await
            .map_err(|e| RouterError::CctpReceive(e.to_string()))?;
        info!("CCTP receiveMessage submitted on Polygon: {tx_hash}");
        Ok(tx_hash)
    }

    /// Approve the CCTP TokenMessenger to spend USDC.e on behalf of the executor.
    pub async fn approve_usdc(
        &self,
        usdc_address: &str,
        spender: &str,
        amount: u64,
    ) -> Result<String> {
        let padded_spender = format!(
            "000000000000000000000000{}",
            spender.trim_start_matches("0x")
        );
        let amount_hex = format!("{:0>64x}", amount);
        let calldata = format!("0x{APPROVE_SELECTOR}{padded_spender}{amount_hex}");

        let tx_hash = self
            .wallet
            .send_transaction(&self.rpc_url, usdc_address, &calldata, 0)
            .await
            .map_err(|e| RouterError::CctpDeposit(format!("USDC approve failed: {e}")))?;

        info!("USDC approve submitted: {tx_hash}");
        Ok(tx_hash)
    }

    /// Call Polygon CCTP `depositForBurn` to initiate the return bridge.
    ///
    /// Burns USDC.e on Polygon and initiates a CCTP transfer to Solana.
    /// `solana_recipient` is the Solana pubkey (as bytes32) that will receive
    /// the minted USDC on Solana.
    ///
    /// Returns `(tx_hash, message_bytes_hex)` where message_bytes_hex is the
    /// raw CCTP message extracted from the receipt (0x-prefixed hex).
    pub async fn deposit_for_burn_return_leg(
        &self,
        usdc_address: &str,
        amount: u64,
        solana_recipient_bytes32: &[u8; 32],
        usdc_token_address: &str,
    ) -> Result<(String, String)> {
        // 1. Approve USDC.e for the TokenMessenger
        let _approval = self
            .approve_usdc(usdc_address, &self.token_messenger, amount)
            .await?;

        // 2. Build depositForBurn calldata
        // ABI: (uint256 amount, uint32 destinationDomain, bytes32 mintRecipient, address burnToken)
        let sel = deposit_for_burn_polygon_selector();
        let mut abi_data = Vec::new();
        // uint256 amount (32 bytes)
        let mut amount_word = [0u8; 32];
        amount_word[24..].copy_from_slice(&amount.to_be_bytes());
        abi_data.extend_from_slice(&amount_word);
        // uint32 destinationDomain = 5 (Solana) padded to 32 bytes
        let mut domain_word = [0u8; 32];
        domain_word[28..].copy_from_slice(&CCTP_SOLANA_DOMAIN.to_be_bytes());
        abi_data.extend_from_slice(&domain_word);
        // bytes32 mintRecipient
        abi_data.extend_from_slice(solana_recipient_bytes32);
        // address burnToken (USDC.e on Polygon)
        let mut token_word = [0u8; 32];
        let token_bytes = hex::decode(usdc_token_address.trim_start_matches("0x"))
            .map_err(|_| RouterError::CctpDeposit("invalid USDC token address".to_string()))?;
        if token_bytes.len() == 20 {
            token_word[12..].copy_from_slice(&token_bytes);
        }
        abi_data.extend_from_slice(&token_word);

        let calldata = format!("0x{}{}", hex::encode(sel), hex::encode(&abi_data));

        let tx_hash = self
            .wallet
            .send_transaction(&self.rpc_url, &self.token_messenger, &calldata, 0)
            .await
            .map_err(|e| RouterError::CctpDeposit(format!("Polygon depositForBurn failed: {e}")))?;

        info!("Polygon CCTP depositForBurn submitted: {tx_hash}");

        // 3. Fetch receipt and extract MessageSent event bytes
        let receipt = self
            .wallet
            .wait_for_receipt(&self.rpc_url, &tx_hash, 30)
            .await
            .map_err(|e| RouterError::CctpDeposit(format!("receipt polling failed: {e}")))?;

        let message_bytes = self
            .parse_message_sent_from_receipt(&receipt)
            .unwrap_or_else(|e| {
                warn!("Could not parse Polygon MessageSent from receipt: {e}. \
                       Attestation polling may fail.");
                tx_hash.trim_start_matches("0x").to_string()
            });

        Ok((tx_hash, format!("0x{message_bytes}")))
    }

    /// Parse the CCTP `MessageSent(bytes)` event from a Polygon tx receipt.
    ///
    /// The receipt `logs` array contains entries with `topics` and `data`.
    /// The MessageSent event has topic0 = keccak256("MessageSent(bytes)") and
    /// `data` is ABI-encoded `bytes` (offset + length + data).
    fn parse_message_sent_from_receipt(&self, receipt: &serde_json::Value) -> Result<String> {
        let topic0_bytes = message_sent_topic0();
        let topic0_hex = format!("0x{}", hex::encode(topic0_bytes));

        let logs = receipt["logs"]
            .as_array()
            .ok_or_else(|| RouterError::CctpDeposit("no logs in receipt".to_string()))?;

        let empty_topics: Vec<serde_json::Value> = vec![];
        for log in logs {
            let topics = log["topics"].as_array().unwrap_or(&empty_topics);
            if topics.first().and_then(|t| t.as_str()) == Some(&topic0_hex) {
                let data_hex = log["data"]
                    .as_str()
                    .ok_or_else(|| RouterError::CctpDeposit("missing log data".to_string()))?;

                let data = hex::decode(data_hex.trim_start_matches("0x"))
                    .map_err(|e| RouterError::CctpDeposit(format!("hex decode: {e}")))?;

                // ABI-encoded bytes: first word = offset (should be 0x20),
                // second word = length, rest = data
                if data.len() < 64 {
                    continue;
                }
                let msg_len =
                    u64::from_be_bytes(data[32..40].try_into().unwrap_or([0u8; 8])) as usize;
                if data.len() >= 64 + msg_len {
                    return Ok(hex::encode(&data[64..64 + msg_len]));
                }
            }
        }

        Err(RouterError::CctpDeposit(
            "MessageSent event not found in Polygon tx receipt".to_string(),
        ))
    }

    pub fn address(&self) -> &str {
        &self.wallet.address
    }
}

// ── ABI encoding helpers ──────────────────────────────────────────────────────

fn encode_receive_message_calldata(message: &str, attestation: &str) -> Result<String> {
    let msg_bytes =
        hex::decode(message.trim_start_matches("0x")).map_err(|e| RouterError::CctpReceive(e.to_string()))?;
    let att_bytes = hex::decode(attestation.trim_start_matches("0x"))
        .map_err(|e| RouterError::CctpReceive(e.to_string()))?;

    let msg_padded_len = ((msg_bytes.len() + 31) / 32) * 32;
    let att_offset = 64 + 32 + msg_padded_len;

    let mut data = Vec::new();
    data.extend(pad32(64u64));
    data.extend(pad32(att_offset as u64));
    data.extend(pad32(msg_bytes.len() as u64));
    data.extend(&msg_bytes);
    data.extend(vec![0u8; msg_padded_len - msg_bytes.len()]);
    let att_padded_len = ((att_bytes.len() + 31) / 32) * 32;
    data.extend(pad32(att_bytes.len() as u64));
    data.extend(&att_bytes);
    data.extend(vec![0u8; att_padded_len - att_bytes.len()]);

    Ok(format!("0x{}{}", RECEIVE_MESSAGE_SELECTOR, hex::encode(data)))
}

fn pad32(val: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&val.to_be_bytes());
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evm_addr_padding() {
        let addr = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
        let bytes = evm_addr_to_bytes32(addr).unwrap();
        assert_eq!(&bytes[..12], &[0u8; 12]);
        assert_eq!(bytes[12..], hex::decode("2791Bca1f2de4661ED88A30C99A7a9449Aa84174").unwrap()[..]);
    }

    #[test]
    fn pad32_roundtrip() {
        let v: u64 = 64;
        let bytes = pad32(v);
        assert_eq!(u64::from_be_bytes(bytes[24..].try_into().unwrap()), v);
    }

    #[test]
    fn deposit_for_burn_selector_len() {
        let sel = deposit_for_burn_polygon_selector();
        assert_eq!(sel.len(), 4);
        // Verify it's non-zero (proves keccak ran)
        assert_ne!(sel, [0u8; 4]);
    }
}
