//! Minimal SPL Token and System program instruction encoders, hand-rolled to
//! keep the dependency tree small.

use solana_program::account_info::AccountInfo;
use solana_program::entrypoint::ProgramResult;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::program::invoke_signed;
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;

use crate::error::MeeError;

pub const TOKEN_PROGRAM_ID: Pubkey = solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0; 32]);
pub const TOKEN_ACCOUNT_LEN: usize = 165;
pub const MINT_LEN: usize = 82;

// --- SPL Token ---------------------------------------------------------------

pub fn transfer_ix(source: &Pubkey, destination: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

pub fn initialize_account3_ix(account: &Pubkey, mint: &Pubkey, owner: &Pubkey) -> Instruction {
    let mut data = vec![18u8];
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*account, false), AccountMeta::new_readonly(*mint, false)],
        data,
    }
}

pub fn initialize_mint2_ix(mint: &Pubkey, authority: &Pubkey, decimals: u8) -> Instruction {
    let mut data = vec![20u8, decimals];
    data.extend_from_slice(authority.as_ref());
    data.push(0); // no freeze authority
    Instruction { program_id: TOKEN_PROGRAM_ID, accounts: vec![AccountMeta::new(*mint, false)], data }
}

pub fn mint_to_ix(mint: &Pubkey, destination: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = vec![7u8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// Token CPI transfer. `seeds` signs for a PDA authority; pass `&[]` when the
/// authority signed the transaction.
pub fn transfer<'a>(
    token_program: &AccountInfo<'a>,
    source: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    amount: u64,
    seeds: &[&[&[u8]]],
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let ix = transfer_ix(source.key, destination.key, authority.key, amount);
    invoke_signed(&ix, &[source.clone(), destination.clone(), authority.clone(), token_program.clone()], seeds)
}

pub struct TokenAccount {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

pub fn read_token_account(ai: &AccountInfo) -> Result<TokenAccount, ProgramError> {
    if ai.owner != &TOKEN_PROGRAM_ID || ai.data_len() != TOKEN_ACCOUNT_LEN {
        return Err(MeeError::InvalidTokenAccount.into());
    }
    let d = ai.try_borrow_data()?;
    let key = |r: core::ops::Range<usize>| Pubkey::new_from_array(d[r].try_into().unwrap());
    Ok(TokenAccount { mint: key(0..32), owner: key(32..64), amount: u64::from_le_bytes(d[64..72].try_into().unwrap()) })
}

pub fn check_token_program(ai: &AccountInfo) -> ProgramResult {
    if ai.key != &TOKEN_PROGRAM_ID {
        return Err(MeeError::InvalidTokenProgram.into());
    }
    Ok(())
}

// --- System program ------------------------------------------------------------

pub fn create_account_ix(from: &Pubkey, to: &Pubkey, lamports: u64, space: u64, owner: &Pubkey) -> Instruction {
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&space.to_le_bytes());
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, true)],
        data,
    }
}

fn system_transfer_ix(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

fn allocate_ix(account: &Pubkey, space: u64) -> Instruction {
    let mut data = 8u32.to_le_bytes().to_vec();
    data.extend_from_slice(&space.to_le_bytes());
    Instruction { program_id: SYSTEM_PROGRAM_ID, accounts: vec![AccountMeta::new(*account, true)], data }
}

fn assign_ix(account: &Pubkey, owner: &Pubkey) -> Instruction {
    let mut data = 1u32.to_le_bytes().to_vec();
    data.extend_from_slice(owner.as_ref());
    Instruction { program_id: SYSTEM_PROGRAM_ID, accounts: vec![AccountMeta::new(*account, true)], data }
}

/// Creates a PDA account owned by `owner`. Works even if someone pre-funded
/// the address (which would make a plain `create_account` fail).
pub fn create_pda_account<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    rent: &solana_program::rent::Rent,
    space: usize,
    owner: &Pubkey,
    seeds: &[&[u8]],
) -> ProgramResult {
    if system_program.key != &SYSTEM_PROGRAM_ID {
        return Err(MeeError::InvalidSystemProgram.into());
    }
    if account.owner != &SYSTEM_PROGRAM_ID || account.data_len() != 0 {
        return Err(MeeError::AlreadyInitialized.into());
    }
    let needed = rent.minimum_balance(space);
    let infos = [payer.clone(), account.clone(), system_program.clone()];
    if account.lamports() == 0 {
        let ix = create_account_ix(payer.key, account.key, needed, space as u64, owner);
        return invoke_signed(&ix, &infos, &[seeds]);
    }
    let missing = needed.saturating_sub(account.lamports());
    if missing > 0 {
        invoke_signed(&system_transfer_ix(payer.key, account.key, missing), &infos, &[])?;
    }
    invoke_signed(&allocate_ix(account.key, space as u64), &infos, &[seeds])?;
    invoke_signed(&assign_ix(account.key, owner), &infos, &[seeds])
}
