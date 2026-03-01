use crate::error::{Result, RouterError};

/// Full configuration for the trade router, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    // ── Solana ────────────────────────────────────────────────────────────────
    pub solana_rpc_url: String,
    pub solana_keypair_path: String,
    pub jupiter_api_url: String,
    /// Slippage tolerance for Jupiter swaps, in basis points.
    pub jupiter_slippage_bps: u16,

    // ── CCTP Bridge ───────────────────────────────────────────────────────────
    pub cctp_attestation_url: String,
    pub cctp_solana_token_messenger: String,
    pub cctp_polygon_message_transmitter: String,
    pub cctp_polygon_token_messenger: String,
    /// How long to poll for CCTP attestation before erroring.
    pub cctp_attestation_timeout_secs: u64,

    // ── Polygon / EVM ─────────────────────────────────────────────────────────
    pub polygon_rpc_url: String,
    /// Hex-encoded private key (0x-prefixed) for the EVM executor wallet.
    pub polygon_executor_private_key: String,
    pub ctf_contract_address: String,
    pub ctf_exchange_contract_address: String,
    pub usdc_polygon_address: String,

    // ── Polymarket CLOB API ───────────────────────────────────────────────────
    pub poly_clob_url: String,
    pub poly_api_key: String,
    pub poly_secret: String,
    pub poly_passphrase: String,

    // ── Protocol ──────────────────────────────────────────────────────────────
    /// Protocol fee in basis points, deducted from the USDC amount before bridging.
    pub protocol_fee_bps: u16,
    /// Maximum single-order value in USDC (in micro-USDC: 1 USDC = 1_000_000).
    pub max_order_usdc_micro: u64,
    /// How long to wait for a Polymarket order to be filled before cancelling.
    pub poly_order_fill_timeout_secs: u64,
    /// Path for JSON order persistence (None → in-memory only).
    pub order_store_path: Option<String>,
}

impl RouterConfig {
    /// Load config from environment variables (uses `dotenvy` to read `.env`).
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok(); // ignore missing .env

        let get = |key: &str| -> Result<String> {
            std::env::var(key).map_err(|_| RouterError::Config(format!("missing env var: {key}")))
        };

        let get_or = |key: &str, default: &str| -> String {
            std::env::var(key).unwrap_or_else(|_| default.to_owned())
        };

        let parse_u16 = |key: &str, default: u16| -> Result<u16> {
            let v = get_or(key, &default.to_string());
            v.parse::<u16>().map_err(|_| RouterError::Config(format!("{key} must be a u16")))
        };

        let parse_u64 = |key: &str, default: u64| -> Result<u64> {
            let v = get_or(key, &default.to_string());
            v.parse::<u64>().map_err(|_| RouterError::Config(format!("{key} must be a u64")))
        };

        Ok(RouterConfig {
            solana_rpc_url: get_or("SOLANA_RPC_URL", "https://api.mainnet-beta.solana.com"),
            solana_keypair_path: get_or("SOLANA_EXECUTOR_KEYPAIR_PATH", "~/.config/solana/id.json"),
            jupiter_api_url: get_or("JUPITER_API_URL", "https://quote-api.jup.ag"),
            jupiter_slippage_bps: parse_u16("JUPITER_SLIPPAGE_BPS", 50)?,

            cctp_attestation_url: get_or("CCTP_ATTESTATION_URL", "https://iris-api.circle.com"),
            cctp_solana_token_messenger: get_or(
                "CCTP_SOLANA_TOKEN_MESSENGER",
                "CCTPiPYEnTQLuNaWZkhe7mWx5bkGEuHiVLmRKs7VHqpW",
            ),
            cctp_polygon_message_transmitter: get_or(
                "CCTP_POLYGON_MESSAGE_TRANSMITTER",
                "0xBd3fa81B58Ba92a82136038B25aDec7066af3155",
            ),
            cctp_polygon_token_messenger: get_or(
                "CCTP_POLYGON_TOKEN_MESSENGER",
                "0x9daF8c91AEFAE50b9c0E69629D3f6Ca40cA3B3FE",
            ),
            cctp_attestation_timeout_secs: parse_u64("CCTP_ATTESTATION_TIMEOUT_SECS", 600)?,

            polygon_rpc_url: get_or("POLYGON_RPC_URL", "https://polygon-rpc.com"),
            polygon_executor_private_key: get("POLYGON_EXECUTOR_PRIVATE_KEY")?,
            ctf_contract_address: get_or(
                "CTF_CONTRACT_ADDRESS",
                "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045",
            ),
            ctf_exchange_contract_address: get_or(
                "CTF_EXCHANGE_CONTRACT_ADDRESS",
                "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E",
            ),
            usdc_polygon_address: get_or(
                "USDC_POLYGON_ADDRESS",
                "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
            ),

            poly_clob_url: get_or("POLY_CLOB_URL", "https://clob.polymarket.com"),
            poly_api_key: get_or("POLY_API_KEY", ""),
            poly_secret: get_or("POLY_SECRET", ""),
            poly_passphrase: get_or("POLY_PASSPHRASE", ""),

            protocol_fee_bps: parse_u16("PROTOCOL_FEE_BPS", 30)?,
            max_order_usdc_micro: parse_u64("MAX_ORDER_USDC", 1000)? * 1_000_000,
            poly_order_fill_timeout_secs: parse_u64("POLY_ORDER_FILL_TIMEOUT_SECS", 120)?,
            order_store_path: std::env::var("ORDER_STORE_PATH").ok().filter(|s| !s.is_empty()),
        })
    }
}

/// Solana USDC mint on mainnet.
pub const USDC_SOLANA_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// CCTP destination domain ID for Polygon PoS.
pub const CCTP_POLYGON_DOMAIN: u32 = 7;

/// USDC decimals (both Solana and Polygon).
pub const USDC_DECIMALS: u8 = 6;
