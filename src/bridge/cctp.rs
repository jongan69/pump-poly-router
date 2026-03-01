/// Circle CCTP bridge — Solana side (depositForBurn) and Polygon side (receiveMessage).
///
/// Circle Cross-Chain Transfer Protocol (CCTP) burns native USDC on the source
/// chain and mints it on the destination chain after Circle attests the burn.
///
/// Solana CCTP docs: https://developers.circle.com/stablecoins/docs/cctp-on-solana
/// Polygon domain ID: 7
use crate::{
    config::CCTP_POLYGON_DOMAIN,
    error::{Result, RouterError},
    evm::EvmWallet,
};
use borsh::{BorshDeserialize, BorshSerialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    system_program,
    transaction::Transaction,
};
use std::str::FromStr;
use tracing::info;

// ── Solana CCTP program IDs (mainnet) ─────────────────────────────────────────

/// Circle's TokenMessengerMinter program on Solana (default; overridable via config).
#[allow(dead_code)]
const CCTP_TOKEN_MESSENGER_MINTER_PROGRAM: &str =
    "CCTPiPYEnTQLuNaWZkhe7mWx5bkGEuHiVLmRKs7VHqpW";

/// Circle's MessageTransmitter program on Solana.
const CCTP_MESSAGE_TRANSMITTER_PROGRAM: &str = "CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd";

// ── Borsh-encoded instruction discriminator for depositForBurn ────────────────
// Derived from the Circle Solana CCTP IDL: sha256("global:deposit_for_burn")[..8]
const DEPOSIT_FOR_BURN_DISCRIMINATOR: [u8; 8] = [0x9c, 0x91, 0x72, 0x37, 0xf8, 0xa4, 0x24, 0x73];

/// Parameters for the `depositForBurn` instruction.
#[derive(BorshSerialize, BorshDeserialize, Debug)]
struct DepositForBurnParams {
    /// Amount in micro-USDC (6 decimals).
    amount: u64,
    /// Destination chain domain (7 = Polygon PoS).
    destination_domain: u32,
    /// Recipient on the destination chain — must be 32 bytes (left-padded EVM addr).
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
    out[12..].copy_from_slice(&bytes); // left-pad with 12 zero bytes
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
    /// Burns `amount` micro-USDC from `payer`'s ATA and initiates a cross-chain
    /// transfer to `recipient_evm_address` on Polygon.
    ///
    /// Returns `(signature, cctp_nonce)`.  The nonce can be used to derive the
    /// message hash for polling the Circle attestation API.
    pub async fn deposit_for_burn(
        &self,
        rpc: &RpcClient,
        payer: &Keypair,
        usdc_mint: &Pubkey,
        amount: u64,
        recipient_evm_address: &str,
    ) -> Result<(Signature, u64)> {
        let mint_recipient = evm_addr_to_bytes32(recipient_evm_address)?;

        let params = DepositForBurnParams {
            amount,
            destination_domain: CCTP_POLYGON_DOMAIN,
            mint_recipient,
        };

        let mut data = DEPOSIT_FOR_BURN_DISCRIMINATOR.to_vec();
        data.extend(borsh::to_vec(&params).map_err(|e| RouterError::CctpDeposit(e.to_string()))?);

        // Derive the PDAs required by the CCTP program (simplified — a real
        // integration should match the on-chain IDL exactly).
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
            AccountMeta::new(payer.pubkey(), true),         // owner / payer
            AccountMeta::new_readonly(token_messenger_pda, false),
            AccountMeta::new(message_transmitter_pda, false),
            AccountMeta::new_readonly(token_minter_pda, false),
            AccountMeta::new(sender_ata, false),            // source USDC ATA
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

        // The nonce is returned in the transaction logs; for now we return 0 as
        // a placeholder — a production integration should parse the emitted event.
        Ok((sig, 0))
    }
}

// ── Polygon-side CCTP client (raw JSON-RPC) ───────────────────────────────────

/// ABI function selector for `receiveMessage(bytes,bytes)`.
/// keccak256("receiveMessage(bytes,bytes)")[..4] = 0x57ecfd28
const RECEIVE_MESSAGE_SELECTOR: &str = "57ecfd28";

pub struct CctpPolygonClient {
    rpc_url: String,
    message_transmitter: String,
    wallet: EvmWallet,
}

impl CctpPolygonClient {
    pub fn new(
        rpc_url: impl Into<String>,
        message_transmitter: impl Into<String>,
        executor_private_key: &str,
    ) -> Result<Self> {
        let rpc_url = rpc_url.into();
        let wallet = EvmWallet::new(executor_private_key, 137)?; // Polygon mainnet
        Ok(CctpPolygonClient {
            rpc_url,
            message_transmitter: message_transmitter.into(),
            wallet,
        })
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

    /// The executor wallet's Ethereum address (derived from the private key).
    pub fn address(&self) -> &str {
        &self.wallet.address
    }
}

// ── ABI encoding helpers ──────────────────────────────────────────────────────

/// ABI-encode `receiveMessage(bytes message, bytes attestation)` calldata.
fn encode_receive_message_calldata(message: &str, attestation: &str) -> Result<String> {
    let msg_bytes =
        hex::decode(message.trim_start_matches("0x")).map_err(|e| RouterError::CctpReceive(e.to_string()))?;
    let att_bytes = hex::decode(attestation.trim_start_matches("0x"))
        .map_err(|e| RouterError::CctpReceive(e.to_string()))?;

    // ABI encoding for (bytes, bytes): head (offsets) + body (length + data)
    // offset of first param  = 0x40 (64 bytes = two 32-byte head words)
    // offset of second param = 0x40 + 0x20 + padded(msg_bytes)
    let msg_padded_len = ((msg_bytes.len() + 31) / 32) * 32;
    let att_offset = 64 + 32 + msg_padded_len;

    let mut data = Vec::new();
    // head
    data.extend(pad32(64u64));                // offset to message bytes
    data.extend(pad32(att_offset as u64));    // offset to attestation bytes
    // message length + data
    data.extend(pad32(msg_bytes.len() as u64));
    data.extend(&msg_bytes);
    data.extend(vec![0u8; msg_padded_len - msg_bytes.len()]);
    // attestation length + data
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
}
