//! CEX-grade in-memory engine for the MEE chain.
//!
//! Same accounting model as `mee-core` (cash-flow positions, cross margin per
//! DEX, backstop liquidation, insurance then socialized bad debt, lazy
//! funding), without the on-chain size limits: unbounded accounts and
//! orders, a price-level book with O(1) cancel, and any number of HIP-3
//! DEXes and markets. Deterministic: hash maps are only used for lookups.

pub mod book;
pub mod engine;
pub mod state;

pub use engine::{CancelReason, Event, Events, OrderRequest, PlaceOutcome};
pub use state::*;
