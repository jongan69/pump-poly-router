//! # pump-poly-router
//!
//! Cross-chain prediction trade router: converts Pump.fun SPL tokens on Solana
//! into Polymarket outcome position tokens on Polygon, and settles winnings
//! back to the user in SOL after market resolution.
//!
//! ## Pipeline
//!
//! ```text
//! User (Solana)
//!   → Jupiter DEX: SPL token → USDC
//!   → Circle CCTP: USDC (Solana) → USDC.e (Polygon)
//!   → Polymarket CLOB: buy YES / NO position
//!   → (await resolution)
//!   → CTF redeemPositions → CCTP back → Jupiter USDC → SOL → user
//! ```
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use pump_poly_router::{
//!     config::RouterConfig,
//!     executor::TradeRouter,
//!     store::OrderStore,
//!     types::{OrderIntent, Outcome},
//! };
//! use solana_sdk::pubkey::Pubkey;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = RouterConfig::from_env()?;
//!     let store = OrderStore::new();
//!     let mut router = TradeRouter::new(config, store)?;
//!
//!     let intent = OrderIntent::new(
//!         Pubkey::new_unique(),                  // user pubkey
//!         Pubkey::new_unique(),                  // input SPL mint
//!         1_000_000,                             // 1 token (6 decimals)
//!         "0xabc...condition_id",
//!         "0xdef...token_id",
//!         Outcome::Yes,
//!         10.0,                                  // min 10 shares
//!         9_999_999_999,                         // deadline
//!         30,                                    // 0.3% fee
//!     );
//!
//!     let id = router.submit_intent(intent)?;
//!     router.run_loop(std::time::Duration::from_secs(5)).await;
//!     Ok(())
//! }
//! ```

pub mod bridge;
pub mod config;
pub mod error;
pub mod evm;
pub mod executor;
pub mod polymarket;
pub mod solana;
pub mod store;
pub mod types;

pub use config::RouterConfig;
pub use error::{Result, RouterError};
pub use executor::TradeRouter;
pub use store::OrderStore;
pub use types::{OrderIntent, OrderStatus, Outcome};
