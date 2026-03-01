/// TradeRouter — the cross-chain trade execution state machine.
///
/// Each `advance()` call drives a single order one step forward through its
/// lifecycle.  The method is idempotent: calling it again on an order that is
/// already in a given stage will re-check conditions and only progress if the
/// on-chain state confirms the previous step completed.
///
/// Stages:
///   Pending
///   → SolanaSwapInProgress (Jupiter SPL → USDC submitted)
///   → SolanaSwapComplete   (USDC balance confirmed)
///   → BridgePending        (CCTP depositForBurn submitted)
///   → BridgeRelaying       (attestation received; receiveMessage submitted on Polygon)
///   → BridgeComplete       (USDC.e balance confirmed on Polygon)
///   → PolymarketOrderPosted
///   → PolymarketFilled
///   → AwaitingResolution
///   → Redeeming            (redeemPositions submitted)
///   → SettlementBridging   (return CCTP submitted)
///   → SettlementSwapping   (USDC → SOL swap submitted)
///   → Complete
use crate::{
    bridge::{CircleAttestationClient, CctpPolygonClient, CctpSolanaClient},
    config::{RouterConfig, USDC_SOLANA_MINT},
    error::{Result, RouterError},
    evm::address_from_key,
    polymarket::{ConditionResolver, PolymarketOrderClient, SettlementClient},
    solana::{jupiter::JupiterClient, spl},
    store::OrderStore,
    types::{OrderIntent, OrderStatus, Outcome},
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::read_keypair_file, signer::Signer};
use std::str::FromStr;
use tracing::{error, info, warn};
use uuid::Uuid;

pub struct TradeRouter {
    config: RouterConfig,
    jupiter: JupiterClient,
    cctp_solana: CctpSolanaClient,
    cctp_polygon: CctpPolygonClient,
    attestation: CircleAttestationClient,
    poly_client: PolymarketOrderClient,
    resolver: ConditionResolver,
    settlement: SettlementClient,
    store: OrderStore,
    rpc: RpcClient,
}

impl TradeRouter {
    /// Initialise the router from environment config.
    pub fn new(config: RouterConfig, store: OrderStore) -> Result<Self> {
        let rpc = RpcClient::new(config.solana_rpc_url.clone());

        let jupiter = JupiterClient::new(&config.jupiter_api_url);

        let cctp_solana = CctpSolanaClient::new(&config.cctp_solana_token_messenger)?;

        let cctp_polygon = CctpPolygonClient::new(
            &config.polygon_rpc_url,
            &config.cctp_polygon_message_transmitter,
            &config.polygon_executor_private_key,
        )?;

        let attestation = CircleAttestationClient::new(&config.cctp_attestation_url);

        // Build the Polymarket CLOB client (authenticates lazily per order).
        let poly_client = PolymarketOrderClient::new(
            &config.poly_clob_url,
            &config.poly_secret,
            config.poly_order_fill_timeout_secs,
        );

        let resolver = ConditionResolver::new(&config.polygon_rpc_url, &config.ctf_contract_address);

        let settlement = SettlementClient::new(
            &config.polygon_rpc_url,
            &config.ctf_contract_address,
            &config.polygon_executor_private_key,
        )?;

        Ok(TradeRouter {
            config,
            jupiter,
            cctp_solana,
            cctp_polygon,
            attestation,
            poly_client,
            resolver,
            settlement,
            store,
            rpc,
        })
    }

    /// Submit a new intent.  Returns the assigned order UUID.
    pub fn submit_intent(&mut self, intent: OrderIntent) -> Result<Uuid> {
        let id = intent.id;
        self.store.insert(intent)?;
        info!("Order {id} submitted");
        Ok(id)
    }

    /// Get the current status of an order.
    pub fn get_status(&self, id: Uuid) -> Option<&OrderStatus> {
        self.store.get(id).map(|o| &o.status)
    }

    /// Drive a single order one step forward.
    ///
    /// Returns the new `OrderStatus` after the step.
    pub async fn advance(&mut self, id: Uuid) -> Result<OrderStatus> {
        let order = self
            .store
            .get(id)
            .ok_or_else(|| RouterError::OrderNotFound(id.to_string()))?
            .clone();

        if order.status.is_terminal() {
            return Err(RouterError::AlreadyTerminal(id.to_string()));
        }

        if order.is_expired() && !matches!(order.status, OrderStatus::AwaitingResolution | OrderStatus::Redeeming { .. } | OrderStatus::SettlementBridging { .. } | OrderStatus::SettlementSwapping) {
            warn!("Order {id} expired");
            let mut o = order;
            o.set_status(OrderStatus::Cancelled);
            self.store.update(o)?;
            return Ok(OrderStatus::Cancelled);
        }

        let new_status = self.step(&order).await;

        let mut order = self.store.get(id).unwrap().clone();
        match new_status {
            Ok(ref s) => {
                info!("Order {id} advanced to {:?}", s);
                order.set_status(s.clone());
            }
            Err(ref e) => {
                error!("Order {id} failed: {e}");
                order.set_status(OrderStatus::Failed {
                    reason: e.to_string(),
                    stage: stage_name(&order.status),
                });
            }
        }
        self.store.update(order)?;
        new_status
    }

    /// Poll all non-terminal orders.  Returns after one pass.
    pub async fn run_once(&mut self) -> Vec<(Uuid, Result<OrderStatus>)> {
        let ids = self.store.pending_ids();
        let mut results = Vec::new();
        for id in ids {
            let result = self.advance(id).await;
            results.push((id, result));
        }
        results
    }

    /// Continuously poll in a loop until all orders are terminal.
    pub async fn run_loop(&mut self, poll_interval: std::time::Duration) {
        loop {
            let pending = self.store.pending_ids();
            if pending.is_empty() {
                info!("No pending orders; run_loop exiting");
                break;
            }
            for id in pending {
                let _ = self.advance(id).await;
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    // ── Internal step dispatch ────────────────────────────────────────────────

    async fn step(&mut self, order: &OrderIntent) -> Result<OrderStatus> {
        match &order.status {
            OrderStatus::Pending => self.step_pending(order).await,
            OrderStatus::SolanaSwapInProgress { tx } => {
                self.step_solana_swap_in_progress(order, tx).await
            }
            OrderStatus::SolanaSwapComplete { usdc_amount } => {
                self.step_solana_swap_complete(order, *usdc_amount).await
            }
            OrderStatus::BridgePending { cctp_nonce, message_hash } => {
                self.step_bridge_pending(order, *cctp_nonce, message_hash).await
            }
            OrderStatus::BridgeRelaying { attestation, polygon_tx } => {
                self.step_bridge_relaying(order, attestation, polygon_tx).await
            }
            OrderStatus::BridgeComplete { polygon_usdc } => {
                self.step_bridge_complete(order, *polygon_usdc).await
            }
            OrderStatus::PolymarketOrderPosted { order_id } => {
                self.step_poly_order_posted(order, order_id).await
            }
            OrderStatus::PolymarketFilled { .. } => {
                Ok(OrderStatus::AwaitingResolution)
            }
            OrderStatus::AwaitingResolution => self.step_awaiting_resolution(order).await,
            OrderStatus::Redeeming { redeem_tx } => {
                self.step_redeeming(order, redeem_tx).await
            }
            OrderStatus::SettlementBridging { cctp_nonce, message_hash } => {
                self.step_settlement_bridging(order, *cctp_nonce, message_hash).await
            }
            OrderStatus::SettlementSwapping => self.step_settlement_swapping(order).await,
            s if s.is_terminal() => Err(RouterError::AlreadyTerminal(order.id.to_string())),
            _ => Err(RouterError::Other(format!("unhandled status: {:?}", order.status))),
        }
    }

    // ── Stage implementations ─────────────────────────────────────────────────

    /// Stage: Pending → SolanaSwapInProgress
    async fn step_pending(&mut self, order: &OrderIntent) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT)
            .expect("hardcoded USDC mint is valid");

        // Apply protocol fee to input amount
        let fee = (order.input_amount as u128 * order.fee_bps as u128 / 10_000) as u64;
        let amount_after_fee = order.input_amount.saturating_sub(fee);

        // Submit Jupiter swap: SPL token → USDC
        let quote = self
            .jupiter
            .get_quote(
                &order.input_mint.to_string(),
                &usdc_mint.to_string(),
                amount_after_fee,
                self.config.jupiter_slippage_bps,
            )
            .await?;

        let tx_b64 = self
            .jupiter
            .get_swap_transaction(&quote, &payer.pubkey(), None)
            .await?;

        let sig = self.jupiter.execute_swap(&tx_b64, &payer, &self.rpc).await?;
        let tx = sig.to_string();

        info!("Jupiter swap submitted: {tx}");
        Ok(OrderStatus::SolanaSwapInProgress { tx })
    }

    /// Stage: SolanaSwapInProgress → SolanaSwapComplete (confirm USDC balance)
    async fn step_solana_swap_in_progress(
        &mut self,
        order: &OrderIntent,
        _tx: &str,
    ) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();
        let usdc_balance = spl::token_balance(&self.rpc, &payer.pubkey(), &usdc_mint).await?;

        if usdc_balance == 0 {
            // Still pending — return same status
            return Ok(order.status.clone());
        }

        // Cap at configured maximum
        let capped = usdc_balance.min(self.config.max_order_usdc_micro);
        Ok(OrderStatus::SolanaSwapComplete { usdc_amount: capped })
    }

    /// Stage: SolanaSwapComplete → BridgePending (CCTP depositForBurn)
    async fn step_solana_swap_complete(
        &mut self,
        _order: &OrderIntent,
        usdc_amount: u64,
    ) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();

        // The EVM executor wallet on Polygon receives the bridged USDC
        let recipient_evm = self.executor_evm_address();

        let (sig, nonce) = self
            .cctp_solana
            .deposit_for_burn(&self.rpc, &payer, &usdc_mint, usdc_amount, &recipient_evm)
            .await?;

        // Derive the Circle message hash from the tx (placeholder logic).
        // In production: parse the emitted MessageSent event to get the message bytes,
        // then compute keccak256(message_bytes) for the attestation API.
        let message_hash = format!("0x{}", hex::encode(sig.as_ref()));

        info!("CCTP depositForBurn: sig={sig}, nonce={nonce}");
        Ok(OrderStatus::BridgePending { cctp_nonce: nonce, message_hash })
    }

    /// Stage: BridgePending → BridgeRelaying (poll attestation + receiveMessage)
    async fn step_bridge_pending(
        &mut self,
        _order: &OrderIntent,
        _nonce: u64,
        message_hash: &str,
    ) -> Result<OrderStatus> {
        // Poll Circle attestation API (non-blocking: single attempt per advance() call)
        let att = self.attestation.get_attestation(message_hash).await?;
        if !att.is_complete() {
            // Not ready yet — stay in BridgePending
            return Ok(OrderStatus::BridgePending {
                cctp_nonce: _nonce,
                message_hash: message_hash.to_string(),
            });
        }

        let attestation_bytes = att.attestation.unwrap();

        // Relay to Polygon
        let polygon_tx = self
            .cctp_polygon
            .receive_message("0x", &attestation_bytes)
            .await?;

        Ok(OrderStatus::BridgeRelaying {
            attestation: attestation_bytes,
            polygon_tx,
        })
    }

    /// Stage: BridgeRelaying → BridgeComplete (confirm USDC.e on Polygon)
    async fn step_bridge_relaying(
        &mut self,
        _order: &OrderIntent,
        _attestation: &str,
        _polygon_tx: &str,
    ) -> Result<OrderStatus> {
        // Check USDC.e balance of executor on Polygon
        let executor_addr = self.executor_evm_address();
        let balance = self
            .settlement
            .usdc_balance(&self.config.usdc_polygon_address, &executor_addr)
            .await?;

        if balance == 0 {
            // Still waiting
            return Ok(OrderStatus::BridgeRelaying {
                attestation: _attestation.to_string(),
                polygon_tx: _polygon_tx.to_string(),
            });
        }

        Ok(OrderStatus::BridgeComplete { polygon_usdc: balance })
    }

    /// Stage: BridgeComplete → PolymarketOrderPosted
    async fn step_bridge_complete(
        &mut self,
        order: &OrderIntent,
        polygon_usdc: u64,
    ) -> Result<OrderStatus> {
        let result = self
            .poly_client
            .buy_position(&order.outcome_token_id, polygon_usdc, order.outcome)
            .await?;

        info!(
            "Polymarket order posted: id={}, filled={}, avg_price={}",
            result.order_id, result.shares_filled, result.avg_price
        );

        Ok(OrderStatus::PolymarketOrderPosted { order_id: result.order_id })
    }

    /// Stage: PolymarketOrderPosted → PolymarketFilled (poll fill status)
    async fn step_poly_order_posted(
        &mut self,
        order: &OrderIntent,
        order_id: &str,
    ) -> Result<OrderStatus> {
        // Re-use buy_position's fill-polling logic by polling order status directly.
        // In practice, the `post_order` call in step_bridge_complete already blocks
        // until filled.  This stage handles cases where the executor restarted mid-fill.
        //
        // Simplified: assume filled if we reach this stage (the order_id exists).
        // A production implementation would re-poll the CLOB order status here.
        info!("Order {order_id} assumed filled (resuming from PolymarketOrderPosted)");

        Ok(OrderStatus::PolymarketFilled {
            shares: order.min_position_shares, // conservative placeholder
            avg_price: 0.0,
        })
    }

    /// Stage: AwaitingResolution → Redeeming (check resolution + redeem)
    async fn step_awaiting_resolution(&mut self, order: &OrderIntent) -> Result<OrderStatus> {
        let outcome = self.resolver.is_resolved(&order.market_id).await?;
        match outcome {
            None => {
                // Not yet resolved
                Ok(OrderStatus::AwaitingResolution)
            }
            Some(res) => {
                // Determine which index set to redeem
                let index_sets = index_sets_for_outcome(order.outcome, &res.payout_numerators)?;

                let redeem_tx = self
                    .settlement
                    .redeem_positions(&self.config.usdc_polygon_address, &order.market_id, &index_sets)
                    .await?;

                Ok(OrderStatus::Redeeming { redeem_tx })
            }
        }
    }

    /// Stage: Redeeming → SettlementBridging (CCTP back to Solana)
    async fn step_redeeming(&mut self, order: &OrderIntent, _redeem_tx: &str) -> Result<OrderStatus> {
        // Check if USDC.e has arrived from the redemption
        let executor_addr = self.executor_evm_address();
        let balance = self
            .settlement
            .usdc_balance(&self.config.usdc_polygon_address, &executor_addr)
            .await?;

        if balance == 0 {
            return Ok(OrderStatus::Redeeming { redeem_tx: _redeem_tx.to_string() });
        }

        // Initiate CCTP transfer back to Solana
        // TODO: implement Polygon depositForBurn + return-leg relay.
        // Variables below will be used once the EVM signing layer is wired in.
        let _payer = self.load_keypair()?;
        let _user_solana_addr = order.user_pubkey.to_string();
        let _usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();

        // For the return leg, we bridge from Polygon → Solana.
        // Circle CCTP supports both directions; the Polygon side calls depositForBurn
        // on the Polygon TokenMessenger contract (same pattern as Solana side, but EVM).
        //
        // TODO: implement Polygon depositForBurn call via EvmWallet.send_transaction().
        Err(RouterError::Other(
            "Return bridge (Polygon → Solana) not yet implemented".to_string(),
        ))
    }

    /// Stage: SettlementBridging → SettlementSwapping
    async fn step_settlement_bridging(
        &mut self,
        _order: &OrderIntent,
        _nonce: u64,
        _message_hash: &str,
    ) -> Result<OrderStatus> {
        // Poll Circle attestation for return leg, relay to Solana.
        // Mirrors step_bridge_pending but in the reverse direction.
        Err(RouterError::Other("Return bridge relay not yet implemented".to_string()))
    }

    /// Stage: SettlementSwapping → Complete
    async fn step_settlement_swapping(&mut self, order: &OrderIntent) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();

        // Swap USDC → SOL via Jupiter
        let sol_mint = "So11111111111111111111111111111111111111112";
        let usdc_balance = spl::token_balance(&self.rpc, &payer.pubkey(), &usdc_mint).await?;

        let (_sig, sol_received) = self
            .jupiter
            .swap(
                USDC_SOLANA_MINT,
                sol_mint,
                usdc_balance,
                self.config.jupiter_slippage_bps,
                &payer,
                &self.rpc,
            )
            .await?;

        // Transfer SOL to the user
        let payout_sig = self.transfer_sol(&order.user_pubkey, sol_received).await?;

        Ok(OrderStatus::Complete { sol_paid: sol_received, payout_tx: payout_sig.to_string() })
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn load_keypair(&self) -> Result<solana_sdk::signature::Keypair> {
        let path = shellexpand::tilde(&self.config.solana_keypair_path).to_string();
        read_keypair_file(&path)
            .map_err(|e| RouterError::Config(format!("failed to read keypair: {e}")))
    }

    fn executor_evm_address(&self) -> String {
        match address_from_key(&self.config.polygon_executor_private_key) {
            Ok(addr) => format!("0x{addr}"),
            Err(e) => {
                error!("executor_evm_address: key derivation failed: {e}");
                "0x0000000000000000000000000000000000000000".to_string()
            }
        }
    }

    async fn transfer_sol(
        &self,
        recipient: &Pubkey,
        lamports: u64,
    ) -> Result<solana_sdk::signature::Signature> {
        use solana_sdk::{system_instruction, transaction::Transaction};
        let payer = self.load_keypair()?;
        let ix = system_instruction::transfer(&payer.pubkey(), recipient, lamports);
        let recent_blockhash = self.rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], recent_blockhash);
        let sig = self.rpc.send_and_confirm_transaction(&tx).await?;
        Ok(sig)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn stage_name(status: &OrderStatus) -> String {
    match status {
        OrderStatus::Pending => "Pending".into(),
        OrderStatus::SolanaSwapInProgress { .. } => "SolanaSwapInProgress".into(),
        OrderStatus::SolanaSwapComplete { .. } => "SolanaSwapComplete".into(),
        OrderStatus::BridgePending { .. } => "BridgePending".into(),
        OrderStatus::BridgeRelaying { .. } => "BridgeRelaying".into(),
        OrderStatus::BridgeComplete { .. } => "BridgeComplete".into(),
        OrderStatus::PolymarketOrderPosted { .. } => "PolymarketOrderPosted".into(),
        OrderStatus::PolymarketFilled { .. } => "PolymarketFilled".into(),
        OrderStatus::AwaitingResolution => "AwaitingResolution".into(),
        OrderStatus::Redeeming { .. } => "Redeeming".into(),
        OrderStatus::SettlementBridging { .. } => "SettlementBridging".into(),
        OrderStatus::SettlementSwapping => "SettlementSwapping".into(),
        OrderStatus::Complete { .. } => "Complete".into(),
        OrderStatus::Failed { .. } => "Failed".into(),
        OrderStatus::Cancelled => "Cancelled".into(),
    }
}

/// Compute the CTF `indexSets` argument based on the outcome side and payout.
fn index_sets_for_outcome(outcome: Outcome, payouts: &[u64]) -> Result<Vec<u64>> {
    // For a binary market, slot 0 = YES, slot 1 = NO.
    // indexSets is a bitmask: bit N set means "include slot N in the redemption".
    match outcome {
        Outcome::Yes => {
            if payouts.first().copied().unwrap_or(0) == 0 {
                return Err(RouterError::CtfRedeem(
                    "YES outcome did not win — cannot redeem".to_string(),
                ));
            }
            Ok(vec![1]) // bit 0 set
        }
        Outcome::No => {
            if payouts.get(1).copied().unwrap_or(0) == 0 {
                return Err(RouterError::CtfRedeem(
                    "NO outcome did not win — cannot redeem".to_string(),
                ));
            }
            Ok(vec![2]) // bit 1 set
        }
    }
}
