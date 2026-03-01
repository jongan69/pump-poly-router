use crate::error::{Result, RouterError};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;

/// Returns the balance of a given SPL mint for `owner`, in native token units.
pub async fn token_balance(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<u64> {
    let ata = get_associated_token_address(owner, mint);
    let account = rpc
        .get_token_account_balance(&ata)
        .await
        .map_err(|e| RouterError::SplToken(e.to_string()))?;
    let amount: u64 = account
        .amount
        .parse()
        .map_err(|_| RouterError::SplToken("invalid token amount".to_string()))?;
    Ok(amount)
}

/// Returns the decimals for an SPL mint.
pub async fn mint_decimals(rpc: &RpcClient, mint: &Pubkey) -> Result<u8> {
    let supply = rpc
        .get_token_supply(mint)
        .await
        .map_err(|e| RouterError::SplToken(e.to_string()))?;
    Ok(supply.decimals)
}

/// Derives the ATA for `owner` / `mint`, without checking on-chain existence.
pub fn ata_for(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address(owner, mint)
}

/// Parses a base58-encoded pubkey string, returning a `RouterError` on failure.
pub fn parse_pubkey(s: &str) -> Result<Pubkey> {
    Pubkey::from_str(s).map_err(|_| RouterError::Config(format!("invalid pubkey: {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_pubkey() {
        let pk = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        assert!(parse_pubkey(pk).is_ok());
    }

    #[test]
    fn parse_invalid_pubkey() {
        assert!(parse_pubkey("not-a-pubkey").is_err());
    }
}
