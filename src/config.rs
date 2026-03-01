use crate::error::{Result, RouterError};

/// Full configuration for the trade router, loaded from environment variables.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    // ── Solana ────────────────────────────────────────────────────────────────
    pub solana_rpc_url: String,
    pub solana_keypair_path: String,
    pub jupiter_api_url: String,
    pub jupiter_slippage_bps: u16,

    // ── CCTP Bridge ───────────────────────────────────────────────────────────
    pub cctp_attestation_url: String,
    pub cctp_solana_token_messenger: String,
    pub cctp_polygon_message_transmitter: String,
    pub cctp_polygon_token_messenger: String,
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
    /// EVM private key (0x-prefixed) used to sign Polymarket CLOB orders.
    /// This is the L1 signing key submitted via the CLOB auth flow.
    /// (Previously called POLY_SECRET in old SDK flows; POLY_API_KEY and
    ///  POLY_PASSPHRASE are no longer used with the official SDK.)
    pub poly_private_key: String,

    // ── Protocol ──────────────────────────────────────────────────────────────
    pub protocol_fee_bps: u16,
    pub max_order_usdc_micro: u64,
    pub poly_order_fill_timeout_secs: u64,
    pub order_store_path: Option<String>,

    // ── Production safety ─────────────────────────────────────────────────────
    /// Allowed Polymarket condition IDs.  None = all markets allowed.
    pub allowed_market_ids: Option<Vec<String>>,
    /// Maximum USDC bridged per rolling 24-hour window (micro-USDC, 0 = no cap).
    pub daily_volume_cap_usdc_micro: u64,
    /// Pause new order processing after this many consecutive executor failures (0 = never pause).
    pub circuit_breaker_failure_threshold: u32,
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

        let parse_u32 = |key: &str, default: u32| -> Result<u32> {
            let v = get_or(key, &default.to_string());
            v.parse::<u32>().map_err(|_| RouterError::Config(format!("{key} must be a u32")))
        };

        // POLY_EVM_PRIVATE_KEY is the canonical name; fall back to the old POLY_SECRET for migration.
        let poly_private_key = std::env::var("POLY_EVM_PRIVATE_KEY")
            .or_else(|_| std::env::var("POLY_SECRET"))
            .unwrap_or_default();

        // Market ID whitelist: comma-separated list of condition IDs, or empty for all.
        let allowed_market_ids = std::env::var("ALLOWED_MARKET_IDS")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|id| id.trim().to_string()).collect());

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
            poly_private_key,

            protocol_fee_bps: parse_u16("PROTOCOL_FEE_BPS", 30)?,
            max_order_usdc_micro: parse_u64("MAX_ORDER_USDC", 1000)? * 1_000_000,
            poly_order_fill_timeout_secs: parse_u64("POLY_ORDER_FILL_TIMEOUT_SECS", 120)?,
            order_store_path: std::env::var("ORDER_STORE_PATH").ok().filter(|s| !s.is_empty()),

            allowed_market_ids,
            daily_volume_cap_usdc_micro: parse_u64("DAILY_VOLUME_CAP_USDC", 0)? * 1_000_000,
            circuit_breaker_failure_threshold: parse_u32("CIRCUIT_BREAKER_FAILURES", 5)?,
        })
    }
}

/// Solana USDC mint on mainnet.
pub const USDC_SOLANA_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// CCTP destination domain ID for Polygon PoS.
pub const CCTP_POLYGON_DOMAIN: u32 = 7;

/// CCTP source domain ID for Solana.
pub const CCTP_SOLANA_DOMAIN: u32 = 5;

/// USDC decimals (both Solana and Polygon).
pub const USDC_DECIMALS: u8 = 6;
