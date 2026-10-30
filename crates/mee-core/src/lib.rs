//! Pure engine for MEE perp DEXes.
//!
//! Everything a perp venue needs lives here: the order book, matching, margin,
//! liquidation, funding and settlement, plus the deployer stake / slashing
//! state machine. The crate has no Solana dependency. The on-chain program maps
//! account data onto these `Pod` structs and calls into it, so the same code
//! runs in unit tests, fuzzing and on chain.
//!
//! Error contract: an engine call that returns `Err` may have mutated state.
//! On Solana the runtime discards the whole transaction, so that is safe;
//! off-chain callers must drop the state too.

pub mod book;
pub mod engine;
pub mod error;
pub mod governance;
pub mod margin;
pub mod math;
pub mod state;

pub use error::{EngineError, Result};
pub use state::*;
