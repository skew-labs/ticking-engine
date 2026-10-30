//! MEE program: HIP-3 style builder-deployed perp DEXes on Solana.
//!
//! A deployer stakes the protocol's stake token to open a perp DEX, lists
//! markets, and runs their oracle. The program itself holds each DEX's order
//! books, matching, margin, liquidation, funding and settlement (all in
//! `mee-core`), so fills settle in the same instruction with no crank.
//! Guardians can slash a misbehaving deployer's stake and freeze the DEX.

pub mod bridge;
pub mod error;
pub mod instruction;
pub mod pda;
pub mod processor;
pub mod state;
pub mod token;

solana_program::declare_id!("2WB9JqZNgVQdtoQhQPNxZ8GiURJqcBwwKayGq7tttLii");

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint {
    use solana_program::account_info::AccountInfo;
    use solana_program::entrypoint::ProgramResult;
    use solana_program::pubkey::Pubkey;

    solana_program::entrypoint!(process_instruction);

    fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
        crate::processor::process_instruction(program_id, accounts, data)
    }
}
