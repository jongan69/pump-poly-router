//! End-to-end example: route a Pump.fun SPL token trade to a Polymarket position.
//!
//! Run with a populated `.env`:
//!
//!   cp .env.example .env
//!   # fill in your keys
//!   cargo run --example route_trade
//!
//! The example creates an intent with configurable parameters (read from env or
//! hard-coded defaults), submits it to the router, then polls `advance()` until
//! the order reaches a terminal state, printing the proof trail at each step.

use pump_poly_router::{
    config::RouterConfig,
    executor::TradeRouter,
    store::OrderStore,
    types::{OrderIntent, OrderStatus, Outcome},
    RouterError,
};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("pump_poly_router=debug".parse()?))
        .init();

    // ── Config ────────────────────────────────────────────────────────────────
    let config = RouterConfig::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;

    // ── Order parameters (override with env vars for convenience) ─────────────

    // The Pump.fun SPL token mint you want to sell.
    let input_mint = std::env::var("INPUT_MINT")
        .unwrap_or_else(|_| "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string());

    // Amount in native token units (e.g. 1_000_000 = 1 USDC-equivalent with 6 decimals).
    let input_amount: u64 = std::env::var("INPUT_AMOUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);

    // Polymarket condition ID (hex, 0x-prefixed).
    let market_id = std::env::var("MARKET_ID")
        .unwrap_or_else(|_| "0x0000000000000000000000000000000000000000000000000000000000000001".to_string());

    // ERC-1155 outcome token ID for the YES side.
    let outcome_token_id = std::env::var("OUTCOME_TOKEN_ID")
        .unwrap_or_else(|_| "0x0000000000000000000000000000000000000000000000000000000000000001".to_string());

    let outcome = match std::env::var("OUTCOME").as_deref() {
        Ok("NO") | Ok("no") => Outcome::No,
        _ => Outcome::Yes,
    };

    let min_shares: f64 = std::env::var("MIN_SHARES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);

    // Deadline: 1 hour from now.
    let deadline_unix = (chrono::Utc::now().timestamp() + 3600) as u64;

    // ── User pubkey (read from executor keypair or env) ────────────────────────
    let user_pubkey = std::env::var("USER_PUBKEY")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok())
        .unwrap_or_else(Pubkey::new_unique);

    let input_mint_pk = Pubkey::from_str(&input_mint)?;

    // ── Build intent ──────────────────────────────────────────────────────────
    let intent = OrderIntent::new(
        user_pubkey,
        input_mint_pk,
        input_amount,
        market_id,
        outcome_token_id,
        outcome,
        min_shares,
        deadline_unix,
        config.protocol_fee_bps,
    );

    info!(
        "Submitting intent: {} {} {} shares on {}",
        input_amount, outcome, min_shares, intent.market_id
    );

    // ── Router ────────────────────────────────────────────────────────────────
    let store = match config.order_store_path.clone() {
        Some(path) => OrderStore::with_persistence(&path)?,
        None => OrderStore::new(),
    };

    let mut router = TradeRouter::new(config, store).map_err(|e| anyhow::anyhow!("{e}"))?;

    let id = router.submit_intent(intent).map_err(|e| anyhow::anyhow!("{e}"))?;
    info!("Order submitted: {id}");

    // ── Drive the state machine ────────────────────────────────────────────────
    loop {
        match router.get_status(id) {
            Some(s) if s.is_terminal() => {
                print_final_status(id, s);
                break;
            }
            Some(_) => {}
            None => {
                error!("Order {id} disappeared from store");
                break;
            }
        }

        match router.advance(id).await {
            Ok(status) => {
                print_status(&status);
                if status.is_terminal() {
                    print_final_status(id, &status);
                    break;
                }
            }
            Err(RouterError::AlreadyTerminal(_)) => break,
            Err(e) => {
                error!("advance error: {e}");
                break;
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    Ok(())
}

fn print_status(status: &OrderStatus) {
    match status {
        OrderStatus::Pending => info!("  ⏳ Pending"),
        OrderStatus::SolanaSwapInProgress { tx } => info!("  🔄 Solana swap in progress: {tx}"),
        OrderStatus::SolanaSwapComplete { usdc_amount } => {
            info!("  ✅ Solana swap complete: {} µUSDC", usdc_amount)
        }
        OrderStatus::BridgePending { message_hash, .. } => {
            info!("  🌉 CCTP bridge pending, attestation hash: {message_hash}")
        }
        OrderStatus::BridgeRelaying { polygon_tx, .. } => {
            info!("  🌉 CCTP relaying to Polygon: {polygon_tx}")
        }
        OrderStatus::BridgeComplete { polygon_usdc } => {
            info!("  ✅ Bridge complete: {} µUSDC on Polygon", polygon_usdc)
        }
        OrderStatus::PolymarketOrderPosted { order_id } => {
            info!("  📋 Polymarket order posted: {order_id}")
        }
        OrderStatus::PolymarketFilled { shares, avg_price } => {
            info!("  ✅ Polymarket filled: {shares:.4} shares @ {avg_price:.4}")
        }
        OrderStatus::AwaitingResolution => info!("  ⏳ Awaiting market resolution"),
        OrderStatus::Redeeming { redeem_tx } => info!("  💰 Redeeming positions: {redeem_tx}"),
        OrderStatus::SettlementBridging { message_hash, .. } => {
            info!("  🌉 Return bridge pending: {message_hash}")
        }
        OrderStatus::SettlementSwapping => info!("  🔄 Swapping USDC → SOL"),
        OrderStatus::Complete { sol_paid, payout_tx } => {
            info!("  🎉 Complete! Paid {} lamports SOL. Tx: {payout_tx}", sol_paid)
        }
        OrderStatus::Failed { reason, stage } => {
            error!("  ❌ Failed at {stage}: {reason}")
        }
        OrderStatus::Cancelled => info!("  🚫 Cancelled"),
    }
}

fn print_final_status(id: uuid::Uuid, status: &OrderStatus) {
    println!("\n══════════════════════════════════════════");
    println!("  Order {id} final state:");
    print_status(status);
    println!("══════════════════════════════════════════\n");
}
