//! MEE chain state machine.
//!
//! Validators order blocks with BFT consensus (`mee-consensus`) and apply
//! them here. A block's transactions run cancels first, then post-only
//! orders, then the rest. Bridge deposits and governance (HIP-3 slashing)
//! need a validator quorum; withdrawals produce receipts that the Solana
//! bridge pays out against quorum signatures.

pub mod chain;
pub mod crypto;
pub mod tx;

pub use chain::{BlockError, BlockOutcome, Chain, Genesis, TxError};
pub use tx::*;
