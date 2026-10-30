use borsh::BorshDeserialize;
use mee_core::engine::{self, OrderRequest};
use mee_core::{DEX_ACTIVE, DEX_FROZEN, DexState, MAX_GUARDIANS, OrderKind, Side, governance};
use solana_program::account_info::AccountInfo;
use solana_program::clock::Clock;
use solana_program::entrypoint::ProgramResult;
use solana_program::program::{invoke, set_return_data};
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;
use solana_program::rent::Rent;
use solana_program::sysvar::SysvarSerialize;

use crate::error::{MeeError, engine as eng};
use crate::instruction::{MeeInstruction, OrderArgs, ParamsArgs};
use crate::pda;
use crate::state::*;
use crate::token;

type Accounts<'a, 'b> = core::slice::Iter<'b, AccountInfo<'a>>;

fn next<'a, 'b>(it: &mut Accounts<'a, 'b>) -> Result<&'b AccountInfo<'a>, ProgramError> {
    it.next().ok_or(ProgramError::NotEnoughAccountKeys)
}

fn require_signer(ai: &AccountInfo) -> ProgramResult {
    if !ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    Ok(())
}

fn require_key(ai: &AccountInfo, expected: &[u8; 32]) -> ProgramResult {
    if ai.key.to_bytes() != *expected {
        return Err(MeeError::Unauthorized.into());
    }
    Ok(())
}

fn exchange_account(ai: &AccountInfo, program_id: &Pubkey) -> Result<Exchange, ProgramError> {
    let ex = load_exchange(ai, program_id)?;
    let expected = Pubkey::create_program_address(&[pda::EXCHANGE_SEED, &[ex.bump]], program_id)
        .map_err(|_| MeeError::InvalidPda)?;
    if ai.key != &expected {
        return Err(MeeError::InvalidPda.into());
    }
    Ok(ex)
}

/// Runs `f` on the mapped DEX state. The borrow ends before any CPI.
fn with_dex<R>(
    dex_ai: &AccountInfo,
    program_id: &Pubkey,
    exchange: Option<&Pubkey>,
    f: impl FnOnce(&mut DexState) -> Result<R, ProgramError>,
) -> Result<R, ProgramError> {
    check_dex_account(dex_ai, program_id)?;
    let mut data = dex_ai.try_borrow_mut_data()?;
    f(dex_mut(&mut data, exchange)?)
}

/// Runs `f` on the DEX and the book of `market`.
fn with_dex_book<R>(
    dex_ai: &AccountInfo,
    book_ai: &AccountInfo,
    program_id: &Pubkey,
    exchange: Option<&Pubkey>,
    market: usize,
    f: impl FnOnce(&mut DexState, &mut mee_core::Book) -> Result<R, ProgramError>,
) -> Result<R, ProgramError> {
    check_dex_account(dex_ai, program_id)?;
    check_book_account(book_ai, program_id)?;
    let mut dex_data = dex_ai.try_borrow_mut_data()?;
    let mut book_data = book_ai.try_borrow_mut_data()?;
    let dex = dex_mut(&mut dex_data, exchange)?;
    let book = book_mut(&mut book_data, book_ai.key, dex_ai.key, dex, market)?;
    f(dex, book)
}

fn trader_of(dex: &DexState, owner: &Pubkey) -> Result<usize, ProgramError> {
    engine::find_trader(dex, &owner.to_bytes()).ok_or_else(|| eng(mee_core::EngineError::TraderNotFound))
}

/// Checks the vault-authority PDA of `dex` and returns its signer seeds bump.
fn vault_authority_bump(
    authority: &AccountInfo,
    dex_key: &Pubkey,
    bump: u8,
    program_id: &Pubkey,
) -> Result<u8, ProgramError> {
    let expected = Pubkey::create_program_address(&[pda::VAULT_AUTHORITY_SEED, dex_key.as_ref(), &[bump]], program_id)
        .map_err(|_| MeeError::InvalidPda)?;
    if authority.key != &expected {
        return Err(MeeError::InvalidPda.into());
    }
    Ok(bump)
}

/// Transfers `amount` out of one of the DEX vaults.
fn pay_out<'a>(
    token_program: &AccountInfo<'a>,
    vault: &AccountInfo<'a>,
    destination: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    dex_key: &Pubkey,
    bump: u8,
    amount: u64,
) -> ProgramResult {
    token::check_token_program(token_program)?;
    token::transfer(
        token_program,
        vault,
        destination,
        authority,
        amount,
        &[&[pda::VAULT_AUTHORITY_SEED, dex_key.as_ref(), &[bump]]],
    )
}

fn check_guardians(guardians: &[[u8; 32]], threshold: u8) -> ProgramResult {
    if guardians.is_empty() || guardians.len() > MAX_GUARDIANS {
        return Err(MeeError::TooManyGuardians.into());
    }
    if threshold == 0 || threshold as usize > guardians.len() {
        return Err(MeeError::InvalidInstruction.into());
    }
    Ok(())
}

fn store_guardians(ex: &mut Exchange, guardians: &[[u8; 32]], threshold: u8) {
    ex.guardians = [[0; 32]; MAX_GUARDIANS];
    ex.guardians[..guardians.len()].copy_from_slice(guardians);
    ex.guardian_count = guardians.len() as u8;
    ex.guardian_threshold = threshold;
}

/// Reads the Clock sysvar account (the id is checked by `from_account_info`).
fn clock(ai: &AccountInfo) -> Result<i64, ProgramError> {
    Ok(Clock::from_account_info(ai)?.unix_timestamp)
}

pub fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let ix = MeeInstruction::try_from_slice(data).map_err(|_| MeeError::InvalidInstruction)?;
    let it = &mut accounts.iter();
    match ix {
        MeeInstruction::InitExchange { params, guardians, guardian_threshold } => {
            init_exchange(program_id, it, params, &guardians, guardian_threshold)
        }
        MeeInstruction::UpdateExchange { params, guardians, guardian_threshold, new_admin } => {
            let admin = next(it)?;
            let exchange_ai = next(it)?;
            require_signer(admin)?;
            let mut ex = exchange_account(exchange_ai, program_id)?;
            require_key(admin, &ex.admin)?;
            check_guardians(&guardians, guardian_threshold)?;
            ex.params = params.into();
            store_guardians(&mut ex, &guardians, guardian_threshold);
            if let Some(a) = new_admin {
                ex.admin = a;
            }
            exchange_ai.try_borrow_mut_data()?.copy_from_slice(bytemuck::bytes_of(&ex));
            Ok(())
        }
        MeeInstruction::VoteSlash { slash_bps, freeze } => {
            let guardian = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            require_signer(guardian)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            let g = ex.guardian_index(guardian.key).ok_or(MeeError::Unauthorized)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                governance::vote_slash(dex, g, ex.guardian_count as usize, slash_bps, freeze).map_err(eng)?;
                Ok(())
            })
        }
        MeeInstruction::ExecuteSlash => {
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let stake_vault = next(it)?;
            let treasury = next(it)?;
            let authority = next(it)?;
            let token_program = next(it)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            require_key(treasury, &ex.treasury_stake)?;
            let (amount, bump) = with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(stake_vault, &dex.header.stake_vault)?;
                let amount = governance::execute_slash(dex, ex.guardian_threshold as u32).map_err(eng)?;
                Ok((amount, dex.header.vault_authority_bump))
            })?;
            let bump = vault_authority_bump(authority, dex_ai.key, bump, program_id)?;
            pay_out(token_program, stake_vault, treasury, authority, dex_ai.key, bump, amount)
        }
        MeeInstruction::ClearSlash => {
            let admin = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            require_signer(admin)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            require_key(admin, &ex.admin)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                governance::clear_slash(dex);
                Ok(())
            })
        }
        MeeInstruction::DeployDex { name, fee_share_bps, oracle_authority, stake } => {
            deploy_dex(program_id, it, name, fee_share_bps, oracle_authority, stake)
        }
        MeeInstruction::UpdateDex { fee_share_bps, oracle_authority } => {
            let deployer = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            require_signer(deployer)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(deployer, &dex.header.deployer)?;
                governance::require_active(dex).map_err(eng)?;
                if let Some(bps) = fee_share_bps {
                    governance::set_fee_share(dex, &ex.params, bps).map_err(eng)?;
                }
                if let Some(k) = oracle_authority {
                    dex.header.oracle_authority = k;
                }
                Ok(())
            })
        }
        MeeInstruction::AddMarket { args } => {
            let deployer = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let book_ai = next(it)?;
            let now = clock(next(it)?)?;
            require_signer(deployer)?;
            require_signer(book_ai)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            check_book_account(book_ai, program_id)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(deployer, &dex.header.deployer)?;
                governance::require_active(dex).map_err(eng)?;
                let mut book_data = book_ai.try_borrow_mut_data()?;
                let book: &mut mee_core::Book =
                    bytemuck::try_from_bytes_mut(&mut book_data[..]).map_err(|_| MeeError::InvalidAccountData)?;
                if book.discriminator != [0; 8] {
                    return Err(MeeError::AlreadyInitialized.into());
                }
                let m = engine::add_market(dex, &ex.params, &book_ai.key.to_bytes(), &args.into(), now).map_err(eng)?;
                book.discriminator = BOOK_DISCRIMINATOR;
                book.dex = dex_ai.key.to_bytes();
                book.market_index = m as u64;
                book.next_order_id = 1;
                Ok(())
            })
        }
        MeeInstruction::SetOracle { prices } => {
            let authority = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let now = clock(next(it)?)?;
            require_signer(authority)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(authority, &dex.header.oracle_authority)?;
                governance::require_active(dex).map_err(eng)?;
                for (m, price) in prices {
                    engine::set_oracle(dex, &ex.params, m as usize, price, now).map_err(eng)?;
                }
                Ok(())
            })
        }
        MeeInstruction::HaltMarket { market, settlement_price } => {
            let authority = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let book_ai = next(it)?;
            require_signer(authority)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            with_dex_book(dex_ai, book_ai, program_id, Some(exchange_ai.key), market as usize, |dex, book| {
                match dex.header.status {
                    DEX_ACTIVE => require_key(authority, &dex.header.deployer)?,
                    DEX_FROZEN => require_key(authority, &ex.admin)?,
                    _ => return Err(eng(mee_core::EngineError::DexNotActive)),
                }
                engine::halt_market(dex, book, &ex.params, market as usize, settlement_price).map_err(eng)
            })
        }
        MeeInstruction::RequestUnstake => {
            let deployer = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let now = clock(next(it)?)?;
            require_signer(deployer)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(deployer, &dex.header.deployer)?;
                governance::request_unstake(dex, &ex.params, now).map_err(eng)
            })
        }
        MeeInstruction::CompleteUnstake => {
            let deployer = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let stake_vault = next(it)?;
            let destination = next(it)?;
            let authority = next(it)?;
            let token_program = next(it)?;
            let now = clock(next(it)?)?;
            require_signer(deployer)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            let (amount, bump) = with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(deployer, &dex.header.deployer)?;
                require_key(stake_vault, &dex.header.stake_vault)?;
                let amount = governance::complete_unstake(dex, &ex.params, now).map_err(eng)?;
                Ok((amount, dex.header.vault_authority_bump))
            })?;
            let bump = vault_authority_bump(authority, dex_ai.key, bump, program_id)?;
            pay_out(token_program, stake_vault, destination, authority, dex_ai.key, bump, amount)
        }
        MeeInstruction::ClaimDeployerFees => {
            let deployer = next(it)?;
            let dex_ai = next(it)?;
            let destination = next(it)?;
            let quote_vault = next(it)?;
            let authority = next(it)?;
            let token_program = next(it)?;
            require_signer(deployer)?;
            let (amount, bump) = with_dex(dex_ai, program_id, None, |dex| {
                require_key(deployer, &dex.header.deployer)?;
                require_key(quote_vault, &dex.header.quote_vault)?;
                // A frozen (slashed) deployer forfeits unclaimed fees.
                if dex.header.status == DEX_FROZEN {
                    return Err(eng(mee_core::EngineError::DexNotActive));
                }
                Ok((engine::take_deployer_fees(dex), dex.header.vault_authority_bump))
            })?;
            let bump = vault_authority_bump(authority, dex_ai.key, bump, program_id)?;
            pay_out(token_program, quote_vault, destination, authority, dex_ai.key, bump, amount)
        }
        MeeInstruction::SettleMarket { market, max_traders } => {
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            exchange_account(exchange_ai, program_id)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                engine::settle_market(dex, market as usize, max_traders as usize).map_err(eng)?;
                Ok(())
            })
        }
        MeeInstruction::UpdateFunding { market } => {
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let now = clock(next(it)?)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                engine::update_funding(dex, &ex.params, market as usize, now).map_err(eng)?;
                Ok(())
            })
        }
        MeeInstruction::ClaimProtocolFees => {
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let treasury = next(it)?;
            let quote_vault = next(it)?;
            let authority = next(it)?;
            let token_program = next(it)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            require_key(treasury, &ex.treasury_quote)?;
            let (amount, bump) = with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(quote_vault, &dex.header.quote_vault)?;
                Ok((engine::take_protocol_fees(dex), dex.header.vault_authority_bump))
            })?;
            let bump = vault_authority_bump(authority, dex_ai.key, bump, program_id)?;
            pay_out(token_program, quote_vault, treasury, authority, dex_ai.key, bump, amount)
        }
        MeeInstruction::RegisterTrader => {
            let owner = next(it)?;
            let dex_ai = next(it)?;
            require_signer(owner)?;
            with_dex(dex_ai, program_id, None, |dex| {
                engine::register_trader(dex, &owner.key.to_bytes()).map_err(eng)?;
                Ok(())
            })
        }
        MeeInstruction::CloseTrader => {
            let owner = next(it)?;
            let dex_ai = next(it)?;
            require_signer(owner)?;
            with_dex(dex_ai, program_id, None, |dex| {
                let t = trader_of(dex, owner.key)?;
                engine::close_trader(dex, t).map_err(eng)
            })
        }
        MeeInstruction::Deposit { amount } => {
            let owner = next(it)?;
            let dex_ai = next(it)?;
            let source = next(it)?;
            let quote_vault = next(it)?;
            let token_program = next(it)?;
            require_signer(owner)?;
            with_dex(dex_ai, program_id, None, |dex| {
                require_key(quote_vault, &dex.header.quote_vault)?;
                let t = trader_of(dex, owner.key)?;
                engine::deposit(dex, t, amount).map_err(eng)
            })?;
            token::check_token_program(token_program)?;
            token::transfer(token_program, source, quote_vault, owner, amount, &[])
        }
        MeeInstruction::Withdraw { amount } => {
            let owner = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let destination = next(it)?;
            let quote_vault = next(it)?;
            let authority = next(it)?;
            let token_program = next(it)?;
            let now = clock(next(it)?)?;
            require_signer(owner)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            let bump = with_dex(dex_ai, program_id, Some(exchange_ai.key), |dex| {
                require_key(quote_vault, &dex.header.quote_vault)?;
                let t = trader_of(dex, owner.key)?;
                engine::withdraw(dex, &ex.params, t, amount, now).map_err(eng)?;
                Ok(dex.header.vault_authority_bump)
            })?;
            let bump = vault_authority_bump(authority, dex_ai.key, bump, program_id)?;
            pay_out(token_program, quote_vault, destination, authority, dex_ai.key, bump, amount)
        }
        MeeInstruction::PlaceOrder { order } => {
            let owner = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let book_ai = next(it)?;
            let now = clock(next(it)?)?;
            require_signer(owner)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            let req = order_request(&order)?;
            let out = with_dex_book(dex_ai, book_ai, program_id, Some(exchange_ai.key), req.market, |dex, book| {
                let t = trader_of(dex, owner.key)?;
                engine::place_order(dex, book, &ex.params, t, &req, now).map_err(eng)
            })?;
            // Return data: order id (0 if nothing rested) and filled lots.
            let mut ret = [0u8; 16];
            ret[..8].copy_from_slice(&out.order_id.unwrap_or(0).to_le_bytes());
            ret[8..].copy_from_slice(&out.filled_lots.to_le_bytes());
            set_return_data(&ret);
            Ok(())
        }
        MeeInstruction::CancelOrder { market, order_id } => {
            let owner = next(it)?;
            let dex_ai = next(it)?;
            let book_ai = next(it)?;
            require_signer(owner)?;
            with_dex_book(dex_ai, book_ai, program_id, None, market as usize, |dex, book| {
                let t = trader_of(dex, owner.key)?;
                engine::cancel_order(dex, book, t, order_id).map_err(eng)?;
                Ok(())
            })
        }
        MeeInstruction::CancelAll { market } => {
            let owner = next(it)?;
            let dex_ai = next(it)?;
            let book_ai = next(it)?;
            require_signer(owner)?;
            with_dex_book(dex_ai, book_ai, program_id, None, market as usize, |dex, book| {
                let t = trader_of(dex, owner.key)?;
                engine::cancel_all(dex, book, t).map_err(eng)?;
                Ok(())
            })
        }
        MeeInstruction::Liquidate { market, liquidatee, max_base } => {
            let liquidator = next(it)?;
            let exchange_ai = next(it)?;
            let dex_ai = next(it)?;
            let book_ai = next(it)?;
            let now = clock(next(it)?)?;
            require_signer(liquidator)?;
            let ex = exchange_account(exchange_ai, program_id)?;
            with_dex_book(dex_ai, book_ai, program_id, Some(exchange_ai.key), market as usize, |dex, book| {
                let l = trader_of(dex, liquidator.key)?;
                let t = trader_of(dex, &Pubkey::new_from_array(liquidatee))?;
                engine::liquidate(dex, book, &ex.params, l, t, max_base, now).map_err(eng)?;
                Ok(())
            })
        }
        MeeInstruction::FundInsurance { amount } => {
            let funder = next(it)?;
            let dex_ai = next(it)?;
            let source = next(it)?;
            let quote_vault = next(it)?;
            let token_program = next(it)?;
            require_signer(funder)?;
            with_dex(dex_ai, program_id, None, |dex| {
                require_key(quote_vault, &dex.header.quote_vault)?;
                engine::fund_insurance(dex, amount).map_err(eng)
            })?;
            token::check_token_program(token_program)?;
            token::transfer(token_program, source, quote_vault, funder, amount, &[])
        }
    }
}

fn order_request(o: &OrderArgs) -> Result<OrderRequest, ProgramError> {
    Ok(OrderRequest {
        market: o.market as usize,
        side: Side::from_u8(o.side).ok_or(MeeError::InvalidInstruction)?,
        kind: OrderKind::from_u8(o.kind).ok_or(MeeError::InvalidInstruction)?,
        price: o.price,
        base_lots: o.base_lots,
        reduce_only: o.reduce_only,
        client_order_id: o.client_order_id,
    })
}

fn init_exchange(
    program_id: &Pubkey,
    it: &mut Accounts,
    params: ParamsArgs,
    guardians: &[[u8; 32]],
    threshold: u8,
) -> ProgramResult {
    let admin = next(it)?;
    let exchange_ai = next(it)?;
    let quote_mint = next(it)?;
    let stake_mint = next(it)?;
    let treasury_quote = next(it)?;
    let treasury_stake = next(it)?;
    let system_program = next(it)?;
    let rent = Rent::from_account_info(next(it)?)?;
    require_signer(admin)?;
    check_guardians(guardians, threshold)?;
    let (expected, bump) = pda::exchange(program_id);
    if exchange_ai.key != &expected {
        return Err(MeeError::InvalidPda.into());
    }
    for (treasury, mint) in [(treasury_quote, quote_mint), (treasury_stake, stake_mint)] {
        if token::read_token_account(treasury)?.mint != *mint.key {
            return Err(MeeError::InvalidTokenAccount.into());
        }
    }
    token::create_pda_account(
        admin,
        exchange_ai,
        system_program,
        &rent,
        EXCHANGE_SIZE,
        program_id,
        &[pda::EXCHANGE_SEED, &[bump]],
    )?;

    let mut ex = Exchange {
        discriminator: EXCHANGE_DISCRIMINATOR,
        admin: admin.key.to_bytes(),
        quote_mint: quote_mint.key.to_bytes(),
        stake_mint: stake_mint.key.to_bytes(),
        treasury_quote: treasury_quote.key.to_bytes(),
        treasury_stake: treasury_stake.key.to_bytes(),
        params: params.into(),
        guardians: [[0; 32]; MAX_GUARDIANS],
        guardian_count: 0,
        guardian_threshold: 0,
        bump,
        _pad: [0; 5],
    };
    store_guardians(&mut ex, guardians, threshold);
    exchange_ai.try_borrow_mut_data()?.copy_from_slice(bytemuck::bytes_of(&ex));
    Ok(())
}

fn deploy_dex(
    program_id: &Pubkey,
    it: &mut Accounts,
    name: [u8; 16],
    fee_share_bps: u64,
    oracle_authority: [u8; 32],
    stake: u64,
) -> ProgramResult {
    let deployer = next(it)?;
    let exchange_ai = next(it)?;
    let dex_ai = next(it)?;
    let quote_vault = next(it)?;
    let stake_vault = next(it)?;
    let authority = next(it)?;
    let quote_mint = next(it)?;
    let stake_mint = next(it)?;
    let deployer_stake = next(it)?;
    let token_program = next(it)?;
    let system_program = next(it)?;
    let rent = Rent::from_account_info(next(it)?)?;
    let now = clock(next(it)?)?;
    require_signer(deployer)?;
    // The DEX account signs so nobody can initialize someone else's
    // freshly created account.
    require_signer(dex_ai)?;
    token::check_token_program(token_program)?;
    let ex = exchange_account(exchange_ai, program_id)?;
    require_key(quote_mint, &ex.quote_mint)?;
    require_key(stake_mint, &ex.stake_mint)?;
    check_dex_account(dex_ai, program_id)?;
    if dex_ai.try_borrow_data()?[..8] != [0; 8] {
        return Err(MeeError::AlreadyInitialized.into());
    }

    let (qv, qv_bump) = pda::quote_vault(program_id, dex_ai.key);
    let (sv, sv_bump) = pda::stake_vault(program_id, dex_ai.key);
    let (auth, auth_bump) = pda::vault_authority(program_id, dex_ai.key);
    if quote_vault.key != &qv || stake_vault.key != &sv || authority.key != &auth {
        return Err(MeeError::InvalidPda.into());
    }
    let vaults = [
        (quote_vault, quote_mint, pda::QUOTE_VAULT_SEED, qv_bump),
        (stake_vault, stake_mint, pda::STAKE_VAULT_SEED, sv_bump),
    ];
    for (vault, mint, seed, bump) in vaults {
        token::create_pda_account(
            deployer,
            vault,
            system_program,
            &rent,
            token::TOKEN_ACCOUNT_LEN,
            &token::TOKEN_PROGRAM_ID,
            &[seed, dex_ai.key.as_ref(), &[bump]],
        )?;
        invoke(
            &token::initialize_account3_ix(vault.key, mint.key, &auth),
            &[vault.clone(), mint.clone(), token_program.clone()],
        )?;
    }
    token::transfer(token_program, deployer_stake, stake_vault, deployer, stake, &[])?;

    let mut data = dex_ai.try_borrow_mut_data()?;
    let dex: &mut DexState = bytemuck::try_from_bytes_mut(&mut data[..]).map_err(|_| MeeError::InvalidAccountData)?;
    let h = &mut dex.header;
    h.discriminator = DEX_DISCRIMINATOR;
    h.exchange = exchange_ai.key.to_bytes();
    h.deployer = deployer.key.to_bytes();
    h.oracle_authority = oracle_authority;
    h.quote_vault = qv.to_bytes();
    h.stake_vault = sv.to_bytes();
    h.name = name;
    h.vault_authority_bump = auth_bump;
    governance::deploy(dex, &ex.params, stake, fee_share_bps, now).map_err(eng)
}
