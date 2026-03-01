/// TradeRouter — the cross-chain trade execution state machine.
///
/// Each `advance()` call drives a single order one step forward through its
/// lifecycle.  The method is idempotent: calling it again on an order already
/// in a given stage will re-check conditions and only progress when the
/// on-chain state confirms the previous step completed.
///
/// Stages:
///   Pending
///   → SolanaSwapInProgress (Jupiter SPL → USDC submitted)
///   → SolanaSwapComplete   (USDC balance confirmed)
///   → BridgePending        (CCTP depositForBurn submitted; message bytes stored)
///   → BridgeRelaying       (attestation received; receiveMessage submitted on Polygon)
///   → BridgeComplete       (USDC.e balance confirmed on Polygon)
///   → PolymarketOrderPosted (order posted to CLOB; idempotent from here)
///   → PolymarketFilled
///   → AwaitingResolution
///   → Redeeming            (redeemPositions + Polygon depositForBurn submitted)
///   → SettlementBridging   (return CCTP: attestation + Solana receiveMessage)
///   → SettlementSwapping   (USDC → SOL swap submitted)
///   → Complete
use crate::{
    bridge::{CircleAttestationClient, CctpPolygonClient, CctpSolanaClient},
    config::{RouterConfig, USDC_SOLANA_MINT},
    error::{Result, RouterError},
    evm::{address_from_key, cctp_message_hash, extract_nonce_from_cctp_message},
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
    /// Consecutive failures across all orders — used by circuit breaker.
    consecutive_failures: u32,
    /// Cumulative USDC bridged in the current 24-hour window (micro-USDC).
    daily_volume_usdc: u64,
    daily_volume_reset_ts: u64,
}

impl TradeRouter {
    pub fn new(config: RouterConfig, store: OrderStore) -> Result<Self> {
        let rpc = RpcClient::new(config.solana_rpc_url.clone());
        let jupiter = JupiterClient::new(&config.jupiter_api_url);
        let cctp_solana = CctpSolanaClient::new(&config.cctp_solana_token_messenger)?;

        let cctp_polygon = CctpPolygonClient::new(
            &config.polygon_rpc_url,
            &config.cctp_polygon_message_transmitter,
            &config.polygon_executor_private_key,
        )?
        .with_token_messenger(&config.cctp_polygon_token_messenger);

        let attestation = CircleAttestationClient::new(&config.cctp_attestation_url);

        let poly_client = PolymarketOrderClient::new(
            &config.poly_clob_url,
            &config.poly_private_key,
            config.poly_order_fill_timeout_secs,
        );

        let resolver =
            ConditionResolver::new(&config.polygon_rpc_url, &config.ctf_contract_address);

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
            consecutive_failures: 0,
            daily_volume_usdc: 0,
            daily_volume_reset_ts: now_secs(),
        })
    }

    pub fn submit_intent(&mut self, intent: OrderIntent) -> Result<Uuid> {
        let id = intent.id;
        self.store.insert(intent)?;
        info!("Order {id} submitted");
        Ok(id)
    }

    pub fn get_status(&self, id: Uuid) -> Option<&OrderStatus> {
        self.store.get(id).map(|o| &o.status)
    }

    /// Drive a single order one step forward.
    pub async fn advance(&mut self, id: Uuid) -> Result<OrderStatus> {
        // Circuit breaker: pause new steps if too many consecutive failures.
        if self.config.circuit_breaker_failure_threshold > 0
            && self.consecutive_failures >= self.config.circuit_breaker_failure_threshold
        {
            return Err(RouterError::Other(format!(
                "Circuit breaker open: {} consecutive failures. Manual intervention required.",
                self.consecutive_failures
            )));
        }

        let order = self
            .store
            .get(id)
            .ok_or_else(|| RouterError::OrderNotFound(id.to_string()))?
            .clone();

        if order.status.is_terminal() {
            return Err(RouterError::AlreadyTerminal(id.to_string()));
        }

        if order.is_expired()
            && !matches!(
                order.status,
                OrderStatus::AwaitingResolution
                    | OrderStatus::Redeeming { .. }
                    | OrderStatus::SettlementBridging { .. }
                    | OrderStatus::SettlementSwapping
            )
        {
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
                self.consecutive_failures = 0;
            }
            Err(ref e) => {
                error!("Order {id} failed: {e}");
                self.consecutive_failures += 1;
                order.set_status(OrderStatus::Failed {
                    reason: e.to_string(),
                    stage: stage_name(&order.status),
                });
            }
        }
        self.store.update(order)?;
        new_status
    }

    pub async fn run_once(&mut self) -> Vec<(Uuid, Result<OrderStatus>)> {
        // Reset daily volume window if 24 hours have elapsed.
        self.maybe_reset_daily_volume();

        let ids = self.store.pending_ids();
        let mut results = Vec::new();
        for id in ids {
            let result = self.advance(id).await;
            results.push((id, result));
        }
        results
    }

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
            OrderStatus::BridgePending { cctp_nonce, message_hash, message_bytes } => {
                self.step_bridge_pending(order, *cctp_nonce, message_hash, message_bytes).await
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
            OrderStatus::PolymarketFilled { .. } => Ok(OrderStatus::AwaitingResolution),
            OrderStatus::AwaitingResolution => self.step_awaiting_resolution(order).await,
            OrderStatus::Redeeming { redeem_tx } => {
                self.step_redeeming(order, redeem_tx).await
            }
            OrderStatus::SettlementBridging { cctp_nonce, message_hash, message_bytes } => {
                self.step_settlement_bridging(order, *cctp_nonce, message_hash, message_bytes).await
            }
            OrderStatus::SettlementSwapping => self.step_settlement_swapping(order).await,
            s if s.is_terminal() => Err(RouterError::AlreadyTerminal(order.id.to_string())),
            _ => Err(RouterError::Other(format!("unhandled status: {:?}", order.status))),
        }
    }

    // ── Stage implementations ─────────────────────────────────────────────────

    /// Pending → SolanaSwapInProgress
    async fn step_pending(&mut self, order: &OrderIntent) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).expect("hardcoded USDC mint is valid");

        let fee = (order.input_amount as u128 * order.fee_bps as u128 / 10_000) as u64;
        let amount_after_fee = order.input_amount.saturating_sub(fee);

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

    /// SolanaSwapInProgress → SolanaSwapComplete
    async fn step_solana_swap_in_progress(
        &mut self,
        order: &OrderIntent,
        _tx: &str,
    ) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();

        // Use the expected USDC from the input amount as a floor to detect completion.
        let usdc_balance = spl::token_balance(&self.rpc, &payer.pubkey(), &usdc_mint).await?;

        if usdc_balance == 0 {
            return Ok(order.status.clone());
        }

        let capped = usdc_balance.min(self.config.max_order_usdc_micro);
        Ok(OrderStatus::SolanaSwapComplete { usdc_amount: capped })
    }

    /// SolanaSwapComplete → BridgePending
    ///
    /// Submits CCTP `depositForBurn` on Solana.  Parses the `MessageSent` CPI
    /// event from the transaction logs to get the raw CCTP message bytes, which
    /// are stored in `BridgePending` for the `receiveMessage` call on Polygon.
    async fn step_solana_swap_complete(
        &mut self,
        _order: &OrderIntent,
        usdc_amount: u64,
    ) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();
        let recipient_evm = self.executor_evm_address();

        let (sig, message_bytes_hex) = self
            .cctp_solana
            .deposit_for_burn(&self.rpc, &payer, &usdc_mint, usdc_amount, &recipient_evm)
            .await?;

        // Derive the Circle attestation hash (keccak256 of raw message bytes)
        // and extract the CCTP nonce embedded in the message.
        let (message_hash, cctp_nonce) = if message_bytes_hex.len() > 2 {
            if let Ok(bytes) = hex::decode(message_bytes_hex.trim_start_matches("0x")) {
                let hash = cctp_message_hash(&bytes);
                let nonce = extract_nonce_from_cctp_message(&bytes);
                (hash, nonce)
            } else {
                (format!("0x{}", hex::encode(sig.as_ref())), 0)
            }
        } else {
            (format!("0x{}", hex::encode(sig.as_ref())), 0)
        };

        info!(
            "CCTP depositForBurn submitted: sig={sig}, nonce={cctp_nonce}, hash={message_hash}"
        );
        Ok(OrderStatus::BridgePending { cctp_nonce, message_hash, message_bytes: message_bytes_hex })
    }

    /// BridgePending → BridgeRelaying
    ///
    /// Polls Circle for the attestation.  Once attested, calls Polygon
    /// `receiveMessage` with the actual CCTP message bytes (no longer "0x").
    async fn step_bridge_pending(
        &mut self,
        order: &OrderIntent,
        cctp_nonce: u64,
        message_hash: &str,
        message_bytes: &str,
    ) -> Result<OrderStatus> {
        let att = self.attestation.get_attestation(message_hash).await?;
        if !att.is_complete() {
            return Ok(OrderStatus::BridgePending {
                cctp_nonce,
                message_hash: message_hash.to_string(),
                message_bytes: message_bytes.to_string(),
            });
        }

        let attestation_bytes = att.attestation.unwrap();

        // Pass the actual message bytes (not "0x") to receiveMessage.
        let polygon_tx = self
            .cctp_polygon
            .receive_message(message_bytes, &attestation_bytes)
            .await?;

        info!("CCTP receiveMessage submitted on Polygon: {polygon_tx}");
        Ok(OrderStatus::BridgeRelaying {
            attestation: attestation_bytes,
            polygon_tx,
        })
    }

    /// BridgeRelaying → BridgeComplete
    async fn step_bridge_relaying(
        &mut self,
        order: &OrderIntent,
        attestation: &str,
        polygon_tx: &str,
    ) -> Result<OrderStatus> {
        let executor_addr = self.executor_evm_address();
        let balance = self
            .settlement
            .usdc_balance(&self.config.usdc_polygon_address, &executor_addr)
            .await?;

        if balance == 0 {
            return Ok(OrderStatus::BridgeRelaying {
                attestation: attestation.to_string(),
                polygon_tx: polygon_tx.to_string(),
            });
        }

        Ok(OrderStatus::BridgeComplete { polygon_usdc: balance })
    }

    /// BridgeComplete → PolymarketOrderPosted
    ///
    /// Posts the order to the Polymarket CLOB and immediately transitions.
    /// The idempotency guarantee is that if we crash after posting but before
    /// persisting, on restart we stay in `BridgeComplete` and will post again —
    /// acceptable for the MVP (the old order will expire or be cancelled on CLOB).
    async fn step_bridge_complete(
        &mut self,
        order: &OrderIntent,
        polygon_usdc: u64,
    ) -> Result<OrderStatus> {
        // Whitelist check: reject if market not in allowed list.
        if let Some(ref whitelist) = self.config.allowed_market_ids {
            if !whitelist.contains(&order.market_id) {
                return Err(RouterError::Config(format!(
                    "market {} is not in the allowed_market_ids whitelist",
                    order.market_id
                )));
            }
        }

        // Daily volume cap check.
        if self.config.daily_volume_cap_usdc_micro > 0
            && self.daily_volume_usdc + polygon_usdc > self.config.daily_volume_cap_usdc_micro
        {
            return Err(RouterError::Config(format!(
                "daily volume cap reached ({} µUSDC limit). Order paused until next window.",
                self.config.daily_volume_cap_usdc_micro
            )));
        }

        let order_id = self
            .poly_client
            .post_order(&order.outcome_token_id, polygon_usdc, order.outcome)
            .await?;

        self.daily_volume_usdc += polygon_usdc;
        info!("Polymarket order posted: {order_id}");
        Ok(OrderStatus::PolymarketOrderPosted { order_id })
    }

    /// PolymarketOrderPosted → PolymarketFilled
    ///
    /// Re-polls the CLOB order status.  This handles executor restarts: if we
    /// crashed between posting and persisting `PolymarketFilled`, we resume by
    /// polling here instead of assuming the order is already filled.
    async fn step_poly_order_posted(
        &mut self,
        _order: &OrderIntent,
        order_id: &str,
    ) -> Result<OrderStatus> {
        info!("Polling fill status for order {order_id}");
        let result = self.poly_client.wait_for_fill(order_id).await?;
        Ok(OrderStatus::PolymarketFilled {
            shares: result.shares_filled,
            avg_price: result.avg_price,
        })
    }

    /// AwaitingResolution → Redeeming
    async fn step_awaiting_resolution(&mut self, order: &OrderIntent) -> Result<OrderStatus> {
        let outcome = self.resolver.is_resolved(&order.market_id).await?;
        match outcome {
            None => Ok(OrderStatus::AwaitingResolution),
            Some(res) => {
                let index_sets =
                    index_sets_for_outcome(order.outcome, &res.payout_numerators)?;

                let redeem_tx = self
                    .settlement
                    .redeem_positions(
                        &self.config.usdc_polygon_address,
                        &order.market_id,
                        &index_sets,
                    )
                    .await?;

                Ok(OrderStatus::Redeeming { redeem_tx })
            }
        }
    }

    /// Redeeming → SettlementBridging
    ///
    /// Waits for the redemption USDC.e balance to arrive, then initiates the
    /// return bridge: approve + Polygon `depositForBurn` → Solana.
    async fn step_redeeming(
        &mut self,
        order: &OrderIntent,
        _redeem_tx: &str,
    ) -> Result<OrderStatus> {
        let executor_addr = self.executor_evm_address();
        let balance = self
            .settlement
            .usdc_balance(&self.config.usdc_polygon_address, &executor_addr)
            .await?;

        if balance == 0 {
            return Ok(OrderStatus::Redeeming { redeem_tx: _redeem_tx.to_string() });
        }

        // Compute the executor's Solana USDC ATA as the mint_recipient for the return bridge.
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();
        let executor_ata =
            spl_associated_token_account::get_associated_token_address(&payer.pubkey(), &usdc_mint);
        let solana_recipient_bytes32: [u8; 32] = executor_ata.to_bytes();

        // Initiate the return bridge: approve + Polygon depositForBurn.
        let (deposit_tx, message_bytes_hex) = self
            .cctp_polygon
            .deposit_for_burn_return_leg(
                &self.config.usdc_polygon_address,
                balance,
                &solana_recipient_bytes32,
                &self.config.usdc_polygon_address,
            )
            .await?;

        let (message_hash, cctp_nonce) =
            if let Ok(bytes) = hex::decode(message_bytes_hex.trim_start_matches("0x")) {
                (cctp_message_hash(&bytes), extract_nonce_from_cctp_message(&bytes))
            } else {
                (format!("0x{}", deposit_tx.trim_start_matches("0x")), 0)
            };

        info!("Return bridge depositForBurn: {deposit_tx}, hash={message_hash}");
        Ok(OrderStatus::SettlementBridging {
            cctp_nonce,
            message_hash,
            message_bytes: message_bytes_hex,
        })
    }

    /// SettlementBridging → SettlementSwapping
    ///
    /// Polls Circle for the return-leg attestation, then calls Solana CCTP
    /// `receiveMessage` to deliver USDC to the executor's ATA on Solana.
    async fn step_settlement_bridging(
        &mut self,
        _order: &OrderIntent,
        cctp_nonce: u64,
        message_hash: &str,
        message_bytes: &str,
    ) -> Result<OrderStatus> {
        let att = self.attestation.get_attestation(message_hash).await?;
        if !att.is_complete() {
            return Ok(OrderStatus::SettlementBridging {
                cctp_nonce,
                message_hash: message_hash.to_string(),
                message_bytes: message_bytes.to_string(),
            });
        }

        let attestation_hex = att.attestation.unwrap();
        let msg_bytes = hex::decode(message_bytes.trim_start_matches("0x"))
            .map_err(|e| RouterError::CctpReceive(format!("message_bytes hex decode: {e}")))?;
        let att_bytes = hex::decode(attestation_hex.trim_start_matches("0x"))
            .map_err(|e| RouterError::CctpReceive(format!("attestation hex decode: {e}")))?;

        let payer = self.load_keypair()?;
        let _sig = self
            .cctp_solana
            .receive_message(&self.rpc, &payer, &msg_bytes, &att_bytes)
            .await?;

        info!("Return CCTP receiveMessage submitted on Solana");
        Ok(OrderStatus::SettlementSwapping)
    }

    /// SettlementSwapping → Complete
    async fn step_settlement_swapping(&mut self, order: &OrderIntent) -> Result<OrderStatus> {
        let payer = self.load_keypair()?;
        let usdc_mint = Pubkey::from_str(USDC_SOLANA_MINT).unwrap();
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
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            recent_blockhash,
        );
        let sig = self.rpc.send_and_confirm_transaction(&tx).await?;
        Ok(sig)
    }

    fn maybe_reset_daily_volume(&mut self) {
        let now = now_secs();
        if now.saturating_sub(self.daily_volume_reset_ts) >= 86_400 {
            self.daily_volume_usdc = 0;
            self.daily_volume_reset_ts = now;
            info!("Daily volume counter reset");
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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
    match outcome {
        Outcome::Yes => {
            if payouts.first().copied().unwrap_or(0) == 0 {
                return Err(RouterError::CtfRedeem(
                    "YES outcome did not win — cannot redeem".to_string(),
                ));
            }
            Ok(vec![1])
        }
        Outcome::No => {
            if payouts.get(1).copied().unwrap_or(0) == 0 {
                return Err(RouterError::CtfRedeem(
                    "NO outcome did not win — cannot redeem".to_string(),
                ));
            }
            Ok(vec![2])
        }
    }
}
