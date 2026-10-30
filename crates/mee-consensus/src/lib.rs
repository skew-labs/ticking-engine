//! BFT consensus for the MEE chain: HotStuff family with a 2-chain commit
//! (Jolteon / DiemBFT v4 style), written sans-IO. A node feeds messages and
//! clock ticks in and sends what comes out; `sim` drives replicas over a
//! simulated network for tests.

pub mod replica;
pub mod sim;
pub mod types;

pub use replica::{CommitProof, Config, Replica, qc_is_valid, verify_committed_segment};
pub use types::*;
