use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use uuid::Uuid;

/// Which side of a prediction market to buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Yes,
    No,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Yes => write!(f, "YES"),
            Outcome::No => write!(f, "NO"),
        }
    }
}

/// All transaction / proof hashes collected during a trade's lifecycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrderProofs {
    /// Solana tx for Jupiter SPL → USDC swap.
    pub solana_swap_tx: Option<String>,
    /// Solana tx for CCTP deposit_for_burn.
    pub cctp_deposit_tx: Option<String>,
    /// Circle message hash used to poll for attestation.
    pub cctp_message_hash: Option<String>,
    /// Polygon tx for CCTP receiveMessage.
    pub cctp_receive_tx: Option<String>,
    /// Polymarket CLOB order ID.
    pub poly_order_id: Option<String>,
    /// Polygon tx for the on-chain order fill (Exchange contract).
    pub poly_fill_tx: Option<String>,
    /// Polygon tx for CTF redeemPositions.
    pub redeem_tx: Option<String>,
    /// Polygon tx for CCTP deposit_for_burn (return leg).
    pub return_cctp_deposit_tx: Option<String>,
    /// Circle message hash for return leg.
    pub return_cctp_message_hash: Option<String>,
    /// Solana tx for CCTP receiveMessage (return leg).
    pub return_cctp_receive_tx: Option<String>,
    /// Solana tx for Jupiter USDC → SOL swap.
    pub return_swap_tx: Option<String>,
    /// Solana tx for final SOL transfer to user.
    pub payout_tx: Option<String>,
}

/// Lifecycle state of a single cross-chain trade.
///
/// Each variant carries the minimal data needed to resume from that stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage", content = "data")]
pub enum OrderStatus {
    /// Accepted but not yet started.
    Pending,
    /// Jupiter swap submitted; waiting for confirmation.
    SolanaSwapInProgress { tx: String },
    /// SPL tokens swapped to USDC on Solana. Amount in micro-USDC.
    SolanaSwapComplete { usdc_amount: u64 },
    /// CCTP deposit_for_burn submitted; waiting for Circle attestation.
    BridgePending { cctp_nonce: u64, message_hash: String },
    /// CCTP attestation received; receiveMessage submitted on Polygon.
    BridgeRelaying { attestation: String, polygon_tx: String },
    /// USDC.e available on Polygon. Amount in micro-USDC.
    BridgeComplete { polygon_usdc: u64 },
    /// Polymarket limit/market order posted to CLOB.
    PolymarketOrderPosted { order_id: String },
    /// Order fully or partially filled; position tokens held by executor.
    PolymarketFilled { shares: f64, avg_price: f64 },
    /// Waiting for the prediction market to resolve.
    AwaitingResolution,
    /// redeemPositions tx submitted on Polygon.
    Redeeming { redeem_tx: String },
    /// Winnings in USDC.e; CCTP deposit submitted for return leg.
    SettlementBridging { cctp_nonce: u64, message_hash: String },
    /// CCTP return leg delivered to Solana; swapping USDC → SOL.
    SettlementSwapping,
    /// All steps complete. `sol_paid` is in lamports.
    Complete { sol_paid: u64, payout_tx: String },
    /// Unrecoverable error. `stage` identifies which step failed.
    Failed { reason: String, stage: String },
    /// User requested cancellation (only valid while still in Pending or early stages).
    Cancelled,
}

impl OrderStatus {
    /// True if the order is in a terminal state and should not be advanced.
    pub fn is_terminal(&self) -> bool {
        matches!(self, OrderStatus::Complete { .. } | OrderStatus::Failed { .. } | OrderStatus::Cancelled)
    }
}

/// An order intent submitted by a user or bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderIntent {
    /// Unique ID for this trade.
    pub id: Uuid,

    // ── Input (Solana side) ───────────────────────────────────────────────────
    /// User's Solana wallet that owns the input SPL tokens.
    #[serde(with = "pubkey_serde")]
    pub user_pubkey: Pubkey,
    /// Mint address of the Pump.fun SPL token to sell.
    #[serde(with = "pubkey_serde")]
    pub input_mint: Pubkey,
    /// Amount in the token's native decimals.
    pub input_amount: u64,

    // ── Output (Polymarket side) ──────────────────────────────────────────────
    /// Polymarket condition ID (bytes32, hex-encoded with or without 0x prefix).
    pub market_id: String,
    /// ERC-1155 position token ID for the desired outcome.
    pub outcome_token_id: String,
    /// Which outcome (YES / NO).
    pub outcome: Outcome,
    /// Minimum number of position shares to accept (slippage guard).
    pub min_position_shares: f64,

    // ── Execution constraints ─────────────────────────────────────────────────
    /// Unix timestamp after which the order should be cancelled if not yet filled.
    pub deadline_unix: u64,
    /// Protocol fee in basis points (overrides global config if set).
    pub fee_bps: u16,

    // ── Mutable state ─────────────────────────────────────────────────────────
    pub status: OrderStatus,
    pub proofs: OrderProofs,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl OrderIntent {
    pub fn new(
        user_pubkey: Pubkey,
        input_mint: Pubkey,
        input_amount: u64,
        market_id: impl Into<String>,
        outcome_token_id: impl Into<String>,
        outcome: Outcome,
        min_position_shares: f64,
        deadline_unix: u64,
        fee_bps: u16,
    ) -> Self {
        let now = Utc::now();
        OrderIntent {
            id: Uuid::new_v4(),
            user_pubkey,
            input_mint,
            input_amount,
            market_id: market_id.into(),
            outcome_token_id: outcome_token_id.into(),
            outcome,
            min_position_shares,
            deadline_unix,
            fee_bps,
            status: OrderStatus::Pending,
            proofs: OrderProofs::default(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Update status and bump `updated_at`.
    pub fn set_status(&mut self, status: OrderStatus) {
        self.status = status;
        self.updated_at = Utc::now();
    }

    /// True if the order deadline has passed.
    pub fn is_expired(&self) -> bool {
        let now = Utc::now().timestamp() as u64;
        now > self.deadline_unix
    }
}

// ── Serde helper for Pubkey ───────────────────────────────────────────────────

mod pubkey_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    pub fn serialize<S: Serializer>(pk: &Pubkey, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&pk.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Pubkey, D::Error> {
        let s = String::deserialize(d)?;
        Pubkey::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn order_intent_creation() {
        let pk = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let intent = OrderIntent::new(pk, mint, 1_000_000, "cid", "tid", Outcome::Yes, 10.0, 9999999999, 30);
        assert!(matches!(intent.status, OrderStatus::Pending));
        assert_eq!(intent.fee_bps, 30);
    }

    #[test]
    fn terminal_states() {
        assert!(OrderStatus::Complete { sol_paid: 0, payout_tx: "x".into() }.is_terminal());
        assert!(OrderStatus::Failed { reason: "x".into(), stage: "y".into() }.is_terminal());
        assert!(OrderStatus::Cancelled.is_terminal());
        assert!(!OrderStatus::Pending.is_terminal());
    }
}
