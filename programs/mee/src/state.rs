use bytemuck::{Pod, Zeroable};
use mee_core::{Book, DexState, EngineParams, Key, MAX_GUARDIANS};
use solana_program::account_info::AccountInfo;
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;

use crate::error::MeeError;

pub const EXCHANGE_DISCRIMINATOR: [u8; 8] = *b"MEE:EXCH";
pub const DEX_DISCRIMINATOR: [u8; 8] = *b"MEE:DEX\0";
pub const BOOK_DISCRIMINATOR: [u8; 8] = *b"MEE:BOOK";

pub const EXCHANGE_SIZE: usize = core::mem::size_of::<Exchange>();
pub const DEX_SIZE: usize = core::mem::size_of::<DexState>();
pub const BOOK_SIZE: usize = core::mem::size_of::<Book>();

/// Protocol singleton (PDA `["exchange"]`).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Exchange {
    pub discriminator: [u8; 8],
    pub admin: Key,
    pub quote_mint: Key,
    pub stake_mint: Key,
    /// Receives protocol fees (quote mint).
    pub treasury_quote: Key,
    /// Receives slashed stake (stake mint).
    pub treasury_stake: Key,
    pub params: EngineParams,
    pub guardians: [Key; MAX_GUARDIANS],
    pub guardian_count: u8,
    pub guardian_threshold: u8,
    pub bump: u8,
    pub _pad: [u8; 5],
}

impl Exchange {
    pub fn guardian_index(&self, key: &Pubkey) -> Option<usize> {
        let k = key.to_bytes();
        self.guardians[..self.guardian_count as usize].iter().position(|g| *g == k)
    }
}

fn check_owner(ai: &AccountInfo, program_id: &Pubkey) -> Result<(), ProgramError> {
    if ai.owner != program_id {
        return Err(MeeError::InvalidAccountOwner.into());
    }
    Ok(())
}

pub fn load_exchange(ai: &AccountInfo, program_id: &Pubkey) -> Result<Exchange, ProgramError> {
    check_owner(ai, program_id)?;
    let data = ai.try_borrow_data()?;
    let ex: &Exchange = bytemuck::try_from_bytes(&data[..]).map_err(|_| MeeError::InvalidAccountData)?;
    if ex.discriminator != EXCHANGE_DISCRIMINATOR {
        return Err(MeeError::InvalidAccountData.into());
    }
    Ok(*ex)
}

/// Maps an initialized DEX account. When `exchange` is given, the DEX must
/// belong to it.
pub fn dex_mut<'a>(data: &'a mut [u8], exchange: Option<&Pubkey>) -> Result<&'a mut DexState, ProgramError> {
    let dex: &mut DexState = bytemuck::try_from_bytes_mut(data).map_err(|_| MeeError::InvalidAccountData)?;
    if dex.header.discriminator != DEX_DISCRIMINATOR {
        return Err(MeeError::InvalidAccountData.into());
    }
    if let Some(ex) = exchange
        && dex.header.exchange != ex.to_bytes()
    {
        return Err(MeeError::InvalidAccountData.into());
    }
    Ok(dex)
}

pub fn check_dex_account(ai: &AccountInfo, program_id: &Pubkey) -> Result<(), ProgramError> {
    check_owner(ai, program_id)?;
    if ai.data_len() != DEX_SIZE {
        return Err(MeeError::InvalidAccountData.into());
    }
    Ok(())
}

/// Maps the book of `market`, checking it is the book bound to that market.
pub fn book_mut<'a>(
    data: &'a mut [u8],
    book_key: &Pubkey,
    dex_key: &Pubkey,
    dex: &DexState,
    market: usize,
) -> Result<&'a mut Book, ProgramError> {
    let book: &mut Book = bytemuck::try_from_bytes_mut(data).map_err(|_| MeeError::InvalidAccountData)?;
    let bound = market < dex.header.market_count as usize && dex.markets[market].book == book_key.to_bytes();
    if book.discriminator != BOOK_DISCRIMINATOR
        || book.dex != dex_key.to_bytes()
        || book.market_index as usize != market
        || !bound
    {
        return Err(MeeError::BookMismatch.into());
    }
    Ok(book)
}

pub fn check_book_account(ai: &AccountInfo, program_id: &Pubkey) -> Result<(), ProgramError> {
    check_owner(ai, program_id)?;
    if ai.data_len() != BOOK_SIZE {
        return Err(MeeError::InvalidAccountData.into());
    }
    Ok(())
}
