use thiserror::Error;

pub type Result<T> = std::result::Result<T, RouterError>;

#[derive(Debug, Error)]
pub enum RouterError {
    // ── Solana side ───────────────────────────────────────────────────────────
    #[error("Solana RPC error: {0}")]
    SolanaRpc(#[from] solana_client::client_error::ClientError),

    #[error("Jupiter quote failed: {0}")]
    JupiterQuote(String),

    #[error("Jupiter swap failed: {0}")]
    JupiterSwap(String),

    #[error("SPL token error: {0}")]
    SplToken(String),

    // ── Bridge ────────────────────────────────────────────────────────────────
    #[error("CCTP deposit_for_burn failed: {0}")]
    CctpDeposit(String),

    #[error("CCTP attestation polling timed out after {secs}s for message hash {hash}")]
    CctpAttestationTimeout { secs: u64, hash: String },

    #[error("CCTP attestation API error: {0}")]
    CctpAttestation(String),

    #[error("CCTP receiveMessage on Polygon failed: {0}")]
    CctpReceive(String),

    // ── Polymarket side ───────────────────────────────────────────────────────
    #[error("Polymarket order posting failed: {0}")]
    PolyOrderPost(String),

    #[error("Polymarket order fill timed out after {secs}s for order {order_id}")]
    PolyFillTimeout { secs: u64, order_id: String },

    #[error("Polymarket resolver error: {0}")]
    PolyResolver(String),

    #[error("CTF redeem failed: {0}")]
    CtfRedeem(String),

    // ── Common ────────────────────────────────────────────────────────────────
    #[error("Order not found: {0}")]
    OrderNotFound(String),

    #[error("Order deadline exceeded for order {0}")]
    DeadlineExceeded(String),

    #[error("Order already in terminal state: {0}")]
    AlreadyTerminal(String),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}
