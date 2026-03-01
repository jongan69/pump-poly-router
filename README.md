# pump-poly-router

A custodial cross-chain **prediction trade router** that accepts a Pump.fun SPL token on Solana and routes it into a Polymarket outcome position on Polygon, then settles winnings back to the user in SOL after market resolution.

```
User (Solana)
  └─ SPL token input
       │
       ▼ Jupiter DEX (SPL → USDC)
       │
       ▼ Circle CCTP (USDC Solana → USDC.e Polygon)
       │
       ▼ Polymarket CLOB (buy YES / NO position)
       │
       ▼ (await market resolution)
       │
       ▼ CTF redeemPositions (USDC.e → executor)
       │
       ▼ Circle CCTP (USDC.e Polygon → USDC Solana)  [TODO: return leg]
       │
       ▼ Jupiter DEX (USDC → SOL)
       │
       ▼ SOL payout → user wallet
```

---

## Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Configuration](#configuration)
- [Usage](#usage)
  - [Running the example](#running-the-example)
  - [Library usage](#library-usage)
- [Pipeline stages](#pipeline-stages)
- [Module reference](#module-reference)
- [Known limitations](#known-limitations)
- [Security notes](#security-notes)
- [License](#license)

---

## Overview

`pump-poly-router` implements the **intent model**: a user submits an `OrderIntent` specifying their Solana wallet, input SPL token and amount, and the Polymarket market/outcome they want to bet on. The router then drives a state machine through every cross-chain step autonomously, retrying from the last confirmed on-chain state after any restart.

Design choices:

| Concern | Choice | Rationale |
|---|---|---|
| DEX | Jupiter v6 aggregator | Best-in-class Solana liquidity routing; no Pump.fun bonding curve dependency |
| Bridge | Circle CCTP | Native USDC burn-and-mint; no wrapped/synthetic risk; Circle-attested |
| Collateral | USDC / USDC.e | Polymarket's native collateral; Circle CCTP standardises both sides |
| Custody | Custodial MVP | Simplest path to end-to-end correctness; upgrade path to non-custodial via ZK proofs |
| EVM signing | libsecp256k1 (via Solana dep tree) | Avoids `zeroize` version conflict between Solana 1.18 and alloy/k256 ≥ 0.11 |

---

## Architecture

### State machine

Each `OrderIntent` progresses through a linear state machine. The `advance()` call is **idempotent**: it re-checks on-chain state before progressing, so the executor can be safely restarted at any point.

```
Pending
  ↓ Jupiter SPL → USDC swap submitted
SolanaSwapInProgress { tx }
  ↓ USDC balance confirmed
SolanaSwapComplete { usdc_amount }
  ↓ CCTP depositForBurn submitted on Solana
BridgePending { cctp_nonce, message_hash }
  ↓ Circle attestation received; receiveMessage submitted on Polygon
BridgeRelaying { attestation, polygon_tx }
  ↓ USDC.e balance confirmed on Polygon
BridgeComplete { polygon_usdc }
  ↓ Polymarket market order posted
PolymarketOrderPosted { order_id }
  ↓ CLOB confirms fill
PolymarketFilled { shares, avg_price }
  ↓ (wait for CTF market resolution)
AwaitingResolution
  ↓ CTF redeemPositions called
Redeeming { redeem_tx }
  ↓ Return CCTP burn on Polygon [TODO]
SettlementBridging { cctp_nonce, message_hash }
  ↓ CCTP relay on Solana + USDC → SOL swap
SettlementSwapping
  ↓ SOL transferred to user
Complete { sol_paid, payout_tx }

(any stage can fail → Failed { reason, stage } or expire → Cancelled)
```

### Module layout

```
src/
├── lib.rs                      # Crate root, re-exports
├── config.rs                   # RouterConfig, env loading, constants
├── error.rs                    # RouterError enum
├── types.rs                    # OrderIntent, OrderStatus, OrderProofs, Outcome
├── store.rs                    # OrderStore (in-memory + JSON persistence)
├── executor.rs                 # TradeRouter — state machine, advance()
├── evm/
│   ├── mod.rs
│   └── signer.rs               # EvmWallet: EIP-155 signing via libsecp256k1
├── bridge/
│   ├── mod.rs
│   ├── attestation.rs          # CircleAttestationClient (Iris API)
│   └── cctp.rs                 # CctpSolanaClient + CctpPolygonClient
├── polymarket/
│   ├── mod.rs
│   ├── order.rs                # PolymarketOrderClient (CLOB market buy)
│   ├── resolver.rs             # ConditionResolver (eth_getLogs)
│   └── settlement.rs          # SettlementClient (CTF redeemPositions)
└── solana/
    ├── mod.rs
    ├── jupiter.rs              # JupiterClient (quote + swap via v6 API)
    └── spl.rs                  # SPL token helpers (balance, decimals, ATA)

examples/
└── route_trade.rs              # End-to-end demo
```

---

## Prerequisites

| Requirement | Version | Notes |
|---|---|---|
| Rust | ≥ 1.75 | `rustup update stable` |
| Solana CLI | ≥ 1.18 | For keypair generation |
| A Solana keypair | — | `solana-keygen new` |
| Solana RPC | — | Private endpoint recommended (Helius, Triton, etc.) |
| Polygon RPC | — | Alchemy, Infura, or Ankr |
| Polymarket L2 credentials | — | API key + secret + passphrase from Polymarket |
| USDC on Solana | — | Executor wallet must hold USDC for bridging |
| MATIC on Polygon | — | Executor EVM wallet must hold MATIC for gas |

---

## Installation

```bash
git clone https://github.com/jongan69/pump-poly-router
cd pump-poly-router

# Build
cargo build --release

# Run tests
cargo test
```

> **Dependency note**: The crate uses `libsecp256k1 = "0.6"` (already a transitive dependency of `solana-client 1.18`) for EVM transaction signing, instead of `alloy` or `k256 ≥ 0.11`. This avoids an irreconcilable `zeroize` version conflict where Solana 1.18 requires `zeroize < 1.4` while modern EVM crates require `zeroize ≥ 1.5`.

---

## Configuration

Copy `.env.example` to `.env` and fill in all values:

```bash
cp .env.example .env
$EDITOR .env
```

### Full variable reference

#### Solana

| Variable | Default | Description |
|---|---|---|
| `SOLANA_RPC_URL` | `https://api.mainnet-beta.solana.com` | Solana JSON-RPC endpoint |
| `SOLANA_EXECUTOR_KEYPAIR_PATH` | `~/.config/solana/id.json` | Path to the executor Solana keypair JSON file |
| `JUPITER_API_URL` | `https://quote-api.jup.ag` | Jupiter v6 aggregator API base URL |
| `JUPITER_SLIPPAGE_BPS` | `50` | Swap slippage tolerance in basis points (100 = 1%) |

#### Circle CCTP

| Variable | Default | Description |
|---|---|---|
| `CCTP_ATTESTATION_URL` | `https://iris-api.circle.com` | Circle Iris attestation API base URL |
| `CCTP_SOLANA_TOKEN_MESSENGER` | (mainnet program ID) | Solana CCTP TokenMessengerMinter program |
| `CCTP_POLYGON_MESSAGE_TRANSMITTER` | (mainnet contract) | Polygon CCTP MessageTransmitter contract |
| `CCTP_POLYGON_TOKEN_MESSENGER` | (mainnet contract) | Polygon CCTP TokenMessenger contract |
| `CCTP_ATTESTATION_TIMEOUT_SECS` | `600` | Maximum seconds to wait for Circle attestation |

#### Polygon / EVM

| Variable | Required | Description |
|---|---|---|
| `POLYGON_RPC_URL` | Yes | Polygon PoS JSON-RPC endpoint |
| `POLYGON_EXECUTOR_PRIVATE_KEY` | Yes | Hex private key for the EVM executor wallet (holds USDC.e + MATIC) |
| `CTF_CONTRACT_ADDRESS` | Yes | Polymarket Conditional Token Framework contract |
| `CTF_EXCHANGE_CONTRACT_ADDRESS` | Yes | Polymarket Exchange contract |
| `USDC_POLYGON_ADDRESS` | Yes | USDC.e contract address on Polygon |

#### Polymarket CLOB API

| Variable | Required | Description |
|---|---|---|
| `POLY_CLOB_URL` | Yes | Polymarket CLOB REST API base URL |
| `POLY_API_KEY` | Yes | L2 API key (from EIP-712 auth flow) |
| `POLY_SECRET` | Yes | L2 API secret |
| `POLY_PASSPHRASE` | Yes | L2 API passphrase |

#### Protocol parameters

| Variable | Default | Description |
|---|---|---|
| `PROTOCOL_FEE_BPS` | `30` | Fee taken from escrowed input in basis points (30 = 0.3%) |
| `MAX_ORDER_USDC` | `1000` | Maximum single order size in USDC (post-swap) |
| `POLY_ORDER_FILL_TIMEOUT_SECS` | `120` | Time to wait for Polymarket order fill before failing |
| `ORDER_STORE_PATH` | _(empty)_ | JSON file path for order state persistence across restarts; leave blank for in-memory only |

---

## Usage

### Running the example

```bash
# Set env vars
cp .env.example .env && $EDITOR .env

# Run end-to-end demo
cargo run --example route_trade
```

The example reads all config from `.env`, constructs a test `OrderIntent`, and polls `advance()` every 5 seconds until the order reaches a terminal state, printing the current status at each step.

### Library usage

```rust
use pump_poly_router::{
    config::RouterConfig,
    executor::TradeRouter,
    store::OrderStore,
    types::{OrderIntent, Outcome},
};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config from environment / .env file
    let config = RouterConfig::from_env()?;

    // Persistent order store (survives restarts)
    let store = OrderStore::from_file("orders.json")?;

    // Initialise the router
    let mut router = TradeRouter::new(config, store)?;

    // Describe the user's intent
    let intent = OrderIntent::new(
        Pubkey::from_str("UserWalletPubkeyBase58")?,
        Pubkey::from_str("PumpFunTokenMintBase58")?,
        5_000_000,                            // 5 tokens (6-decimal SPL)
        "0xabc...condition_id_bytes32",
        "0xdef...outcome_token_id",
        Outcome::Yes,
        50.0,                                 // want at least 50 YES shares
        chrono::Utc::now().timestamp() as u64 + 3600, // 1h deadline
        30,                                   // 0.3% protocol fee
    );

    let id = router.submit_intent(intent)?;
    println!("Order submitted: {id}");

    // Drive the state machine continuously
    router.run_loop(std::time::Duration::from_secs(5)).await;

    Ok(())
}
```

#### Driving a single order manually

```rust
// Advance one step at a time (useful for testing / custom loops)
let status = router.advance(id).await?;
println!("Status: {:?}", status);
```

#### Checking order status

```rust
if let Some(status) = router.get_status(id) {
    match status {
        OrderStatus::Complete { sol_paid, payout_tx } => {
            println!("Done! Paid {sol_paid} lamports, tx: {payout_tx}");
        }
        OrderStatus::Failed { reason, stage } => {
            println!("Failed at {stage}: {reason}");
        }
        other => println!("In progress: {:?}", other),
    }
}
```

---

## Pipeline stages

### 1. Pending → SolanaSwapInProgress

The executor calls the Jupiter v6 aggregator to swap the user's input SPL token into USDC on Solana. The protocol fee (default 0.3%) is deducted from the input amount before quoting.

**Key parameters**: `input_mint`, `input_amount`, `JUPITER_SLIPPAGE_BPS`

**On-chain**: Jupiter swap transaction submitted to Solana.

### 2. SolanaSwapInProgress → SolanaSwapComplete

The executor checks the USDC balance of its associated token account. Once non-zero, it caps the amount at `MAX_ORDER_USDC` and proceeds. This step is re-entrant: if the executor restarts while the swap is in-flight, the balance check will confirm it on the next poll.

### 3. SolanaSwapComplete → BridgePending

The executor calls the Circle CCTP `depositForBurn` instruction on Solana, burning the USDC from its token account and targeting the executor's EVM address as the recipient on Polygon (domain 7).

**Key accounts**: TokenMessengerMinter program, MessageTransmitter program, USDC ATA.

**Returns**: `(Signature, cctp_nonce)`. The `cctp_nonce` and `message_hash` (keccak256 of the emitted `MessageSent` bytes) are stored for the next stage.

### 4. BridgePending → BridgeRelaying

The executor polls the Circle Iris attestation API (`GET /v1/attestations/{message_hash}`) until `status: "complete"`. This typically takes 5–20 minutes on mainnet (Circle's finality window).

Once attested, the executor calls `receiveMessage(bytes message, bytes attestation)` on the Polygon CCTP MessageTransmitter, which mints USDC.e to the executor's Polygon address.

**Bridging latency**: Plan for 10–25 minutes of end-to-end bridge time.

### 5. BridgeRelaying → BridgeComplete

The executor checks the USDC.e balance on Polygon via `eth_call balanceOf`. Once non-zero, it proceeds.

### 6. BridgeComplete → PolymarketOrderPosted

The executor places a **market buy** order on the Polymarket CLOB via the REST API, using the bridged USDC.e as collateral. The order is signed with the L2 HMAC credentials and submitted to the CLOB.

**Key fields**: `outcome_token_id`, USDC amount, `Outcome::Yes` or `Outcome::No`.

### 7. PolymarketOrderPosted → PolymarketFilled

The executor polls the CLOB order status until `MATCHED` or `FILLED`. If the order is not filled within `POLY_ORDER_FILL_TIMEOUT_SECS`, the router transitions to `Failed`.

**Output**: `shares` (number of outcome tokens received), `avg_price` (average fill price).

### 8. PolymarketFilled → AwaitingResolution

The order is filled. The executor waits passively for the market to resolve. `advance()` can be called repeatedly; it will continue to return `AwaitingResolution` until the CTF contract emits a `ConditionResolution` event.

**Typical wait**: hours to weeks depending on the market.

### 9. AwaitingResolution → Redeeming

The executor monitors Polygon for `ConditionResolution` events via `eth_getLogs` on the CTF contract. Once resolved, it computes the winning `indexSets` bitmask (bit 0 for YES, bit 1 for NO) and calls `redeemPositions` on the CTF contract to collect USDC.e.

**Resolution decoding**: Payout numerators are ABI-decoded from the event log data. A value > 0 at slot 0 means YES won; slot 1 means NO won.

### 10. Redeeming → SettlementBridging _(TODO)_

After confirming the USDC.e redemption balance, the executor initiates the return CCTP bridge from Polygon back to Solana by calling `depositForBurn` on the Polygon TokenMessenger contract.

> **Current status**: This stage returns an error (`Return bridge not yet implemented`). The `EvmWallet` infrastructure is in place; the EVM calldata encoding for Polygon `depositForBurn` needs to be added.

### 11. SettlementBridging → SettlementSwapping _(TODO)_

Mirror of stage 4: poll Circle attestation for the return leg, then relay to Solana via the Solana CCTP MessageTransmitter.

### 12. SettlementSwapping → Complete

The executor swaps the USDC back to SOL via Jupiter, then transfers the SOL to the user's Solana wallet with a `SystemProgram::Transfer` instruction. The order is marked `Complete` with the lamports paid and payout transaction signature.

---

## Module reference

### `config` — `RouterConfig`

Loads all settings from environment variables (with `.env` file support via `dotenvy`).

```rust
let config = RouterConfig::from_env()?;
```

Key constants exported from this module:

| Constant | Value | Description |
|---|---|---|
| `USDC_SOLANA_MINT` | `EPjFWdd5...` | Native USDC mint on Solana mainnet |
| `CCTP_POLYGON_DOMAIN` | `7` | Circle CCTP domain ID for Polygon PoS |
| `USDC_DECIMALS` | `6` | USDC decimal precision |

### `types` — Order data model

```rust
pub struct OrderIntent {
    pub id: Uuid,
    pub user_pubkey: Pubkey,     // user's Solana wallet
    pub input_mint: Pubkey,      // SPL token to swap
    pub input_amount: u64,       // in raw token units (token decimals)
    pub market_id: String,       // CTF condition ID (hex bytes32)
    pub outcome_token_id: String,// Polymarket CLOB token ID
    pub outcome: Outcome,        // Yes or No
    pub min_position_shares: f64,
    pub deadline_unix: u64,
    pub fee_bps: u16,
    pub status: OrderStatus,
    pub proofs: OrderProofs,     // tx hashes for each stage
    // ...
}
```

`OrderIntent::new(...)` creates a new intent with `status: Pending` and a fresh `Uuid`.

### `store` — `OrderStore`

Thread-unsafe in-memory store with optional JSON file persistence.

```rust
let store = OrderStore::new();                    // in-memory only
let store = OrderStore::from_file("orders.json")?;// load from disk

store.insert(intent)?;
store.update(intent)?;
let order = store.get(id);
let pending = store.pending_ids();               // all non-terminal order IDs
```

### `executor` — `TradeRouter`

```rust
// Construct
let mut router = TradeRouter::new(config, store)?;

// Submit a new intent
let id = router.submit_intent(intent)?;

// Advance one step
let new_status = router.advance(id).await?;

// Check status (sync)
let status = router.get_status(id);

// Run one pass over all pending orders
let results = router.run_once().await;

// Continuously drive all pending orders
router.run_loop(Duration::from_secs(5)).await;
```

### `evm::signer` — `EvmWallet`

Minimal EVM transaction signer using `libsecp256k1` and keccak256. Implements EIP-155 legacy transactions with automatic nonce, gas price, and gas estimation.

```rust
use pump_poly_router::evm::{EvmWallet, address_from_key, cctp_message_hash};

// Derive Ethereum address from hex private key
let addr = address_from_key("0xdeadbeef...")?;  // returns lowercase hex, no 0x

// Create a wallet for Polygon mainnet (chain ID 137)
let wallet = EvmWallet::new("0xdeadbeef...", 137)?;
println!("EVM address: 0x{}", wallet.address);

// Sign and broadcast a transaction
let tx_hash = wallet.send_transaction(
    "https://polygon-rpc.com",       // RPC URL
    "0xContractAddress",             // to
    "0xcalldata...",                 // ABI-encoded calldata
    0,                               // value in wei
).await?;

// Compute CCTP message hash for Circle Iris API
let hash = cctp_message_hash(&message_bytes);  // keccak256, 0x-prefixed
```

### `bridge::cctp` — CCTP clients

```rust
// Solana side: burn USDC
let client = CctpSolanaClient::new(token_messenger_program_id)?;
let (sig, nonce) = client.deposit_for_burn(
    &rpc, &payer, &usdc_mint, amount, "0xRecipientEvmAddress"
).await?;

// Polygon side: relay attested message
let client = CctpPolygonClient::new(rpc_url, message_transmitter, private_key)?;
let tx_hash = client.receive_message("0xmessage...", "0xattestation...").await?;

// Get executor EVM address
println!("{}", client.address());
```

### `bridge::attestation` — `CircleAttestationClient`

```rust
let client = CircleAttestationClient::new("https://iris-api.circle.com");

// Single poll (non-blocking, suitable for advance() loops)
let resp = client.get_attestation("0xmessage_hash").await?;
if resp.is_complete() {
    let attestation_hex = resp.attestation.unwrap();
}

// Blocking poll with timeout (for testing / one-shot scripts)
let attestation = client.poll_until_complete("0xhash", 600, 10).await?;
```

### `polymarket::order` — `PolymarketOrderClient`

```rust
let result = client.buy_position(token_id, usdc_amount, Outcome::Yes).await?;
// result.order_id, result.shares_filled, result.avg_price
```

### `polymarket::resolver` — `ConditionResolver`

```rust
let resolver = ConditionResolver::new(polygon_rpc, ctf_contract_address);
match resolver.is_resolved(condition_id).await? {
    None => { /* not yet resolved */ }
    Some(outcome) => {
        println!("Payout numerators: {:?}", outcome.payout_numerators);
    }
}
```

### `polymarket::settlement` — `SettlementClient`

```rust
let client = SettlementClient::new(polygon_rpc, ctf_contract, private_key)?;

// Redeem winning positions
let tx_hash = client.redeem_positions(usdc_address, condition_id, &[1]).await?;

// Check USDC.e balance
let balance = client.usdc_balance(usdc_address, wallet_address).await?;

// Get executor address
println!("{}", client.address());
```

### `solana::jupiter` — `JupiterClient`

```rust
let client = JupiterClient::new("https://quote-api.jup.ag");

// Get quote
let quote = client.get_quote(input_mint, output_mint, amount, slippage_bps).await?;

// Get swap transaction (base64)
let tx_b64 = client.get_swap_transaction(&quote, &payer_pubkey, None).await?;

// Execute swap, returns (Signature, out_amount)
let (sig, usdc_out) = client.execute_swap(&tx_b64, &payer, &rpc).await?;

// All in one
let (sig, usdc_out) = client.swap(input_mint, output_mint, amount, slippage, &payer, &rpc).await?;
```

---

## Known limitations

| Area | Status | Notes |
|---|---|---|
| Return bridge (Polygon → Solana) | **TODO** | `step_redeeming` and `step_settlement_bridging` return errors. `EvmWallet` is ready; needs `depositForBurn` calldata encoding for the EVM side |
| CCTP message hash | Partial | Production should parse the `MessageSent` CPI event from the Solana tx logs to get the raw message bytes; current logic uses signature bytes as a placeholder in `step_solana_swap_complete` |
| Concurrent orders | Not safe | `OrderStore` is not thread-safe; the router processes orders serially in `run_loop` |
| Polymarket order resume | Simplified | `step_poly_order_posted` assumes the order is filled if the executor restarts; a production implementation should re-poll the CLOB |
| Priority fees | Optional | Jupiter swap includes priority fee support (pass `Some(lamports)` to `get_swap_transaction`) |
| MEV protection | None | Jupiter's default routing; consider Jito bundles for latency-sensitive fills |
| Circuit breaker | None | No automatic halt on unexpected losses; add volume and loss caps for production |
| Nonce management | Per-tx | `EvmWallet` fetches a fresh nonce for each transaction; replace with a nonce manager for concurrent EVM sends |

---

## Security notes

This is a **custodial MVP** — the executor wallet holds user funds between stages. For production deployment:

- **Key management**: Store `POLYGON_EXECUTOR_PRIVATE_KEY` in a secrets manager (AWS Secrets Manager, HashiCorp Vault, etc.). Never commit it to source control.
- **Hot wallet exposure**: The executor wallets (Solana keypair + EVM private key) are hot wallets. Limit their balances to the operational float needed for pending orders.
- **Max order size**: `MAX_ORDER_USDC` limits the blast radius of any single bad trade.
- **Whitelist markets**: Before production, add a market whitelist to reject `OrderIntent`s targeting unvetted condition IDs.
- **Bridge delays**: Circle CCTP attestation takes 5–20 minutes. During this window the USDC is in transit and cannot be recovered if the bridge is paused.
- **Resolution trust**: The router trusts the CTF contract's `ConditionResolution` event. Verify the contract addresses against Polymarket's official deployment docs.
- **Slippage**: Low-liquidity SPL tokens may experience significant slippage on Jupiter. The `JUPITER_SLIPPAGE_BPS` guard limits this but does not eliminate it.

---

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.
