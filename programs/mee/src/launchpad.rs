//! Launchpad: token launches on a bonding curve that graduate into a
//! locked-liquidity AMM, whose time-weighted price feeds the MEE chain's
//! perp oracle.
//!
//! 1. `CreateLaunch` mints a fixed supply to a vault and opens a virtual
//!    constant-product curve (`virtual_quote · virtual_tokens = k`).
//! 2. `LaunchBuy` / `LaunchSell` trade against the curve; the protocol and
//!    the creator take a fee on every trade.
//! 3. When the curve's token allotment sells out, `Graduate` turns the
//!    raised quote plus the liquidity reserve into a constant-product pool at
//!    the curve's final price. There are no LP tokens: the liquidity is
//!    locked for good, and pool fees deepen it.
//! 4. `PoolSwap` trades against the pool and updates a cumulative-price
//!    accumulator; validators read two observations and attest the TWAP to
//!    the chain, which lists a perp market for the token (creator earns the
//!    perp fee share).

use bytemuck::{Pod, Zeroable};
use mee_core::Key;
use solana_program::account_info::AccountInfo;
use solana_program::clock::Clock;
use solana_program::entrypoint::ProgramResult;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::program::{invoke, invoke_signed};
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;
use solana_program::rent::Rent;
use solana_program::sysvar::SysvarSerialize;

use crate::error::MeeError;
use crate::instruction::MeeInstruction;
use crate::token;

pub const CONFIG_DISCRIMINATOR: [u8; 8] = *b"MEE:LCFG";
pub const LAUNCH_DISCRIMINATOR: [u8; 8] = *b"MEE:LNCH";
pub const CONFIG_SEED: &[u8] = b"launch_config";
pub const AUTHORITY_SEED: &[u8] = b"launch_authority";
pub const LAUNCH_SEED: &[u8] = b"launch";
pub const MINT_SEED: &[u8] = b"launch_mint";
pub const TOKEN_VAULT_SEED: &[u8] = b"launch_tokens";
pub const QUOTE_VAULT_SEED: &[u8] = b"launch_quote";

pub const STATUS_CURVE: u8 = 1;
pub const STATUS_COMPLETE: u8 = 2;
pub const STATUS_POOL: u8 = 3;
pub const BPS: u128 = 10_000;
/// Pool prices are quote atoms per token atom scaled by 1e12.
pub const PRICE_SCALE: u128 = 1_000_000_000_000;
pub const TOKEN_DECIMALS: u8 = 6;

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct LaunchConfig {
    pub discriminator: [u8; 8],
    pub admin: Key,
    pub quote_mint: Key,
    /// Quote token account receiving protocol fees.
    pub treasury: Key,
    pub total_supply: u64,
    /// Sold on the curve; the rest seeds the pool at graduation.
    pub curve_supply: u64,
    pub virtual_quote: u64,
    pub virtual_tokens: u64,
    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
    /// Extra pool fee that stays in the pool (locked LP fee).
    pub pool_fee_bps: u64,
    pub next_launch_id: u64,
    pub bump: u8,
    pub authority_bump: u8,
    pub _pad: [u8; 6],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct Launch {
    pub discriminator: [u8; 8],
    pub id: u64,
    pub creator: Key,
    pub mint: Key,
    pub name: [u8; 32],
    pub symbol: [u8; 16],
    pub virtual_quote: u64,
    pub virtual_tokens: u64,
    /// Quote actually raised on the curve.
    pub real_quote: u64,
    /// Curve allotment still unsold.
    pub real_tokens: u64,
    /// Liquidity reserve set aside for the pool.
    pub reserve_tokens: u64,
    pub pool_quote: u64,
    pub pool_tokens: u64,
    /// Reserve tokens not needed for price continuity; locked forever.
    pub locked_tokens: u64,
    /// Creator fees waiting in the quote vault.
    pub creator_fees: u64,
    /// Σ price × seconds (wrapping u128, little endian).
    pub price_cumulative: [u8; 16],
    pub last_ts: i64,
    pub created_ts: i64,
    pub graduated_ts: i64,
    pub status: u8,
    pub bump: u8,
    pub _pad: [u8; 6],
}

impl Launch {
    pub fn cumulative(&self) -> u128 {
        u128::from_le_bytes(self.price_cumulative)
    }

    /// Spot price × `PRICE_SCALE` (curve price before graduation).
    pub fn spot_price(&self) -> u128 {
        let (q, t) = if self.status == STATUS_POOL {
            (self.pool_quote, self.pool_tokens)
        } else {
            (self.virtual_quote, self.virtual_tokens)
        };
        (q as u128 * PRICE_SCALE).checked_div(t as u128).unwrap_or(0)
    }

    /// Advances the TWAP accumulator to `now` at the current pool price.
    fn accrue(&mut self, now: i64) {
        if self.status == STATUS_POOL && now > self.last_ts {
            let add = self.spot_price().wrapping_mul((now - self.last_ts) as u128);
            self.price_cumulative = self.cumulative().wrapping_add(add).to_le_bytes();
        }
        self.last_ts = self.last_ts.max(now);
    }
}

/// TWAP between two observations `(cumulative, ts)`, × `PRICE_SCALE`.
pub fn twap(older: (u128, i64), newer: (u128, i64)) -> Option<u128> {
    let dt = newer.1.checked_sub(older.1).filter(|d| *d > 0)?;
    Some(newer.0.wrapping_sub(older.0) / dt as u128)
}

// --- curve math (u128, rounding always favours the pool) -------------------------

fn ceil_div(a: u128, b: u128) -> u128 {
    a.div_ceil(b)
}

fn fee(amount: u128, bps: u64) -> u128 {
    ceil_div(amount * bps as u128, BPS)
}

pub struct BuyQuote {
    pub gross_in: u64,
    pub net_in: u64,
    pub tokens_out: u64,
    pub protocol_fee: u64,
    pub creator_fee: u64,
}

/// Buying with `quote_in` on reserves (q, t), never taking more than `cap`
/// tokens. Fees are charged on the input.
pub fn quote_buy(q: u64, t: u64, cap: u64, quote_in: u64, fee_bps: (u64, u64)) -> Option<BuyQuote> {
    let (q, t, cap) = (q as u128, t as u128, cap as u128);
    let total_bps = (fee_bps.0 + fee_bps.1) as u128;
    let k = q.checked_mul(t)?;
    let fees_of = |gross: u128| (fee(gross, fee_bps.0), fee(gross, fee_bps.1));
    let (pf, cf) = fees_of(quote_in as u128);
    let mut net = (quote_in as u128).checked_sub(pf + cf)?;
    let mut out = t - ceil_div(k, q + net);
    let mut gross = quote_in as u128;
    if out > cap {
        out = cap;
        net = ceil_div(k, t - out) - q;
        // Smallest gross whose fees leave `net`.
        gross = ceil_div(net * BPS, BPS - total_bps);
        while {
            let (pf, cf) = fees_of(gross);
            gross - pf - cf < net
        } {
            gross += 1;
        }
    }
    if out == 0 {
        return None;
    }
    let (pf, cf) = fees_of(gross);
    Some(BuyQuote {
        gross_in: gross as u64,
        net_in: (gross - pf - cf) as u64,
        tokens_out: out as u64,
        protocol_fee: pf as u64,
        creator_fee: cf as u64,
    })
}

pub struct SellQuote {
    pub gross_out: u64,
    pub net_out: u64,
    pub protocol_fee: u64,
    pub creator_fee: u64,
}

/// Selling `tokens_in` on reserves (q, t); fees come out of the output.
pub fn quote_sell(q: u64, t: u64, tokens_in: u64, fee_bps: (u64, u64)) -> Option<SellQuote> {
    let (q, t) = (q as u128, t as u128);
    let k = q.checked_mul(t)?;
    let gross = q - ceil_div(k, t + tokens_in as u128);
    let (pf, cf) = (fee(gross, fee_bps.0), fee(gross, fee_bps.1));
    let net = gross.checked_sub(pf + cf)?;
    if net == 0 {
        return None;
    }
    Some(SellQuote { gross_out: gross as u64, net_out: net as u64, protocol_fee: pf as u64, creator_fee: cf as u64 })
}

// --- PDAs ----------------------------------------------------------------------

pub fn config_pda(pid: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CONFIG_SEED], pid)
}
pub fn authority_pda(pid: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[AUTHORITY_SEED], pid)
}
pub fn launch_pda(pid: &Pubkey, id: u64) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[LAUNCH_SEED, &id.to_le_bytes()], pid)
}
pub fn mint_pda(pid: &Pubkey, launch: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[MINT_SEED, launch.as_ref()], pid)
}
pub fn token_vault_pda(pid: &Pubkey, launch: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[TOKEN_VAULT_SEED, launch.as_ref()], pid)
}
pub fn quote_vault_pda(pid: &Pubkey, launch: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[QUOTE_VAULT_SEED, launch.as_ref()], pid)
}

// --- processing ------------------------------------------------------------------

type Accounts<'a, 'b> = core::slice::Iter<'b, AccountInfo<'a>>;

fn next<'a, 'b>(it: &mut Accounts<'a, 'b>) -> Result<&'b AccountInfo<'a>, ProgramError> {
    it.next().ok_or(ProgramError::NotEnoughAccountKeys)
}

fn load<T: Pod>(ai: &AccountInfo, pid: &Pubkey, disc: [u8; 8]) -> Result<T, ProgramError> {
    if ai.owner != pid {
        return Err(MeeError::InvalidAccountOwner.into());
    }
    let data = ai.try_borrow_data()?;
    let v: &T = bytemuck::try_from_bytes(&data[..]).map_err(|_| MeeError::InvalidAccountData)?;
    if data[..8] != disc {
        return Err(MeeError::InvalidAccountData.into());
    }
    Ok(*v)
}

fn store<T: Pod>(ai: &AccountInfo, v: &T) -> ProgramResult {
    ai.try_borrow_mut_data()?.copy_from_slice(bytemuck::bytes_of(v));
    Ok(())
}

fn load_config(ai: &AccountInfo, pid: &Pubkey) -> Result<LaunchConfig, ProgramError> {
    let c: LaunchConfig = load(ai, pid, CONFIG_DISCRIMINATOR)?;
    if ai.key != &Pubkey::create_program_address(&[CONFIG_SEED, &[c.bump]], pid).map_err(|_| MeeError::InvalidPda)? {
        return Err(MeeError::InvalidPda.into());
    }
    Ok(c)
}

fn load_launch(ai: &AccountInfo, pid: &Pubkey) -> Result<Launch, ProgramError> {
    let l: Launch = load(ai, pid, LAUNCH_DISCRIMINATOR)?;
    let expected = Pubkey::create_program_address(&[LAUNCH_SEED, &l.id.to_le_bytes(), &[l.bump]], pid)
        .map_err(|_| MeeError::InvalidPda)?;
    if ai.key != &expected {
        return Err(MeeError::InvalidPda.into());
    }
    Ok(l)
}

fn require(cond: bool, e: MeeError) -> ProgramResult {
    if cond { Ok(()) } else { Err(e.into()) }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct LaunchParams {
    pub total_supply: u64,
    pub curve_supply: u64,
    pub virtual_quote: u64,
    pub virtual_tokens: u64,
    pub protocol_fee_bps: u64,
    pub creator_fee_bps: u64,
    pub pool_fee_bps: u64,
}

pub fn init(pid: &Pubkey, it: &mut Accounts, p: LaunchParams) -> ProgramResult {
    let admin = next(it)?;
    let config = next(it)?;
    let quote_mint = next(it)?;
    let treasury = next(it)?;
    let system_program = next(it)?;
    let rent = Rent::from_account_info(next(it)?)?;
    require(admin.is_signer, MeeError::Unauthorized)?;
    require(
        p.curve_supply > 0
            && p.curve_supply < p.total_supply
            && p.virtual_tokens > p.curve_supply
            && p.virtual_quote > 0
            && p.protocol_fee_bps + p.creator_fee_bps + p.pool_fee_bps < 1_000,
        MeeError::InvalidInstruction,
    )?;
    require(token::read_token_account(treasury)?.mint == *quote_mint.key, MeeError::InvalidTokenAccount)?;
    let (cp, bump) = config_pda(pid);
    require(config.key == &cp, MeeError::InvalidPda)?;
    token::create_pda_account(
        admin,
        config,
        system_program,
        &rent,
        size_of::<LaunchConfig>(),
        pid,
        &[CONFIG_SEED, &[bump]],
    )?;
    store(
        config,
        &LaunchConfig {
            discriminator: CONFIG_DISCRIMINATOR,
            admin: admin.key.to_bytes(),
            quote_mint: quote_mint.key.to_bytes(),
            treasury: treasury.key.to_bytes(),
            total_supply: p.total_supply,
            curve_supply: p.curve_supply,
            virtual_quote: p.virtual_quote,
            virtual_tokens: p.virtual_tokens,
            protocol_fee_bps: p.protocol_fee_bps,
            creator_fee_bps: p.creator_fee_bps,
            pool_fee_bps: p.pool_fee_bps,
            next_launch_id: 0,
            bump,
            authority_bump: authority_pda(pid).1,
            _pad: [0; 6],
        },
    )
}

pub fn create(pid: &Pubkey, it: &mut Accounts, name: [u8; 32], symbol: [u8; 16]) -> ProgramResult {
    let creator = next(it)?;
    let config_ai = next(it)?;
    let launch_ai = next(it)?;
    let mint = next(it)?;
    let token_vault = next(it)?;
    let quote_vault = next(it)?;
    let quote_mint = next(it)?;
    let authority = next(it)?;
    let token_program = next(it)?;
    let system_program = next(it)?;
    let rent = Rent::from_account_info(next(it)?)?;
    let clock = Clock::from_account_info(next(it)?)?;
    require(creator.is_signer, MeeError::Unauthorized)?;
    token::check_token_program(token_program)?;
    let mut cfg = load_config(config_ai, pid)?;
    require(quote_mint.key.to_bytes() == cfg.quote_mint, MeeError::InvalidTokenAccount)?;
    let id = cfg.next_launch_id;
    let (lp, lbump) = launch_pda(pid, id);
    let (mp, mbump) = mint_pda(pid, &lp);
    let (tv, tbump) = token_vault_pda(pid, &lp);
    let (qv, qbump) = quote_vault_pda(pid, &lp);
    let (auth, _) = authority_pda(pid);
    require(
        launch_ai.key == &lp
            && mint.key == &mp
            && token_vault.key == &tv
            && quote_vault.key == &qv
            && authority.key == &auth,
        MeeError::InvalidPda,
    )?;

    let id_bytes = id.to_le_bytes();
    token::create_pda_account(
        creator,
        launch_ai,
        system_program,
        &rent,
        size_of::<Launch>(),
        pid,
        &[LAUNCH_SEED, &id_bytes, &[lbump]],
    )?;
    token::create_pda_account(
        creator,
        mint,
        system_program,
        &rent,
        token::MINT_LEN,
        &token::TOKEN_PROGRAM_ID,
        &[MINT_SEED, lp.as_ref(), &[mbump]],
    )?;
    invoke(&token::initialize_mint2_ix(&mp, &auth, TOKEN_DECIMALS), &[mint.clone(), token_program.clone()])?;
    for (vault, m, seed, bump) in
        [(token_vault, mint, TOKEN_VAULT_SEED, tbump), (quote_vault, quote_mint, QUOTE_VAULT_SEED, qbump)]
    {
        token::create_pda_account(
            creator,
            vault,
            system_program,
            &rent,
            token::TOKEN_ACCOUNT_LEN,
            &token::TOKEN_PROGRAM_ID,
            &[seed, lp.as_ref(), &[bump]],
        )?;
        invoke(
            &token::initialize_account3_ix(vault.key, m.key, &auth),
            &[vault.clone(), m.clone(), token_program.clone()],
        )?;
    }
    // The whole supply is minted once, to the vault; the mint authority is a
    // PDA that never mints again.
    invoke_signed(
        &token::mint_to_ix(&mp, &tv, &auth, cfg.total_supply),
        &[mint.clone(), token_vault.clone(), authority.clone(), token_program.clone()],
        &[&[AUTHORITY_SEED, &[cfg.authority_bump]]],
    )?;

    store(
        launch_ai,
        &Launch {
            discriminator: LAUNCH_DISCRIMINATOR,
            id,
            creator: creator.key.to_bytes(),
            mint: mp.to_bytes(),
            name,
            symbol,
            virtual_quote: cfg.virtual_quote,
            virtual_tokens: cfg.virtual_tokens,
            real_quote: 0,
            real_tokens: cfg.curve_supply,
            reserve_tokens: cfg.total_supply - cfg.curve_supply,
            pool_quote: 0,
            pool_tokens: 0,
            locked_tokens: 0,
            creator_fees: 0,
            price_cumulative: [0; 16],
            last_ts: clock.unix_timestamp,
            created_ts: clock.unix_timestamp,
            graduated_ts: 0,
            status: STATUS_CURVE,
            bump: lbump,
            _pad: [0; 6],
        },
    )?;
    cfg.next_launch_id += 1;
    store(config_ai, &cfg)
}

/// Shared trade accounts: [user (s), config, launch (w), token_vault (w),
/// quote_vault (w), user_token (w), user_quote (w), treasury (w), authority,
/// token_program, clock].
struct Trade<'a, 'b> {
    user: &'b AccountInfo<'a>,
    launch_ai: &'b AccountInfo<'a>,
    token_vault: &'b AccountInfo<'a>,
    quote_vault: &'b AccountInfo<'a>,
    user_token: &'b AccountInfo<'a>,
    user_quote: &'b AccountInfo<'a>,
    treasury: &'b AccountInfo<'a>,
    authority: &'b AccountInfo<'a>,
    token_program: &'b AccountInfo<'a>,
    cfg: LaunchConfig,
    launch: Launch,
    now: i64,
}

fn trade_accounts<'a, 'b>(pid: &Pubkey, it: &mut Accounts<'a, 'b>) -> Result<Trade<'a, 'b>, ProgramError> {
    let user = next(it)?;
    let config_ai = next(it)?;
    let launch_ai = next(it)?;
    let token_vault = next(it)?;
    let quote_vault = next(it)?;
    let user_token = next(it)?;
    let user_quote = next(it)?;
    let treasury = next(it)?;
    let authority = next(it)?;
    let token_program = next(it)?;
    let clock = Clock::from_account_info(next(it)?)?;
    require(user.is_signer, MeeError::Unauthorized)?;
    token::check_token_program(token_program)?;
    let cfg = load_config(config_ai, pid)?;
    let launch = load_launch(launch_ai, pid)?;
    require(
        token_vault.key == &token_vault_pda(pid, launch_ai.key).0
            && quote_vault.key == &quote_vault_pda(pid, launch_ai.key).0
            && authority.key == &authority_pda(pid).0
            && treasury.key.to_bytes() == cfg.treasury,
        MeeError::InvalidPda,
    )?;
    Ok(Trade {
        user,
        launch_ai,
        token_vault,
        quote_vault,
        user_token,
        user_quote,
        treasury,
        authority,
        token_program,
        cfg,
        launch,
        now: clock.unix_timestamp,
    })
}

impl<'a> Trade<'a, '_> {
    fn pay_in(&self, quote: u64, protocol_fee: u64) -> ProgramResult {
        token::transfer(self.token_program, self.user_quote, self.quote_vault, self.user, quote - protocol_fee, &[])?;
        token::transfer(self.token_program, self.user_quote, self.treasury, self.user, protocol_fee, &[])
    }

    fn vault_pay(&self, from: &AccountInfo<'a>, to: &AccountInfo<'a>, amount: u64) -> ProgramResult {
        let seeds: &[&[u8]] = &[AUTHORITY_SEED, &[self.cfg.authority_bump]];
        invoke_signed(
            &token::transfer_ix(from.key, to.key, self.authority.key, amount),
            &[from.clone(), to.clone(), self.authority.clone(), self.token_program.clone()],
            &[seeds],
        )
    }

    fn fees(&self) -> (u64, u64) {
        (self.cfg.protocol_fee_bps, self.cfg.creator_fee_bps)
    }
}

pub fn buy(pid: &Pubkey, it: &mut Accounts, quote_in: u64, min_tokens_out: u64) -> ProgramResult {
    let mut t = trade_accounts(pid, it)?;
    require(t.launch.status == STATUS_CURVE, MeeError::InvalidInstruction)?;
    let l = &mut t.launch;
    let qb = quote_buy(
        l.virtual_quote,
        l.virtual_tokens,
        l.real_tokens,
        quote_in,
        (t.cfg.protocol_fee_bps, t.cfg.creator_fee_bps),
    )
    .ok_or(MeeError::InvalidInstruction)?;
    require(qb.tokens_out >= min_tokens_out, MeeError::SlippageExceeded)?;
    l.virtual_quote += qb.net_in;
    l.virtual_tokens -= qb.tokens_out;
    l.real_quote += qb.net_in;
    l.real_tokens -= qb.tokens_out;
    l.creator_fees += qb.creator_fee;
    if l.real_tokens == 0 {
        l.status = STATUS_COMPLETE;
    }
    let (launch, gross, pf, out) = (t.launch, qb.gross_in, qb.protocol_fee, qb.tokens_out);
    store(t.launch_ai, &launch)?;
    t.pay_in(gross, pf)?;
    t.vault_pay(t.token_vault, t.user_token, out)
}

pub fn sell(pid: &Pubkey, it: &mut Accounts, tokens_in: u64, min_quote_out: u64) -> ProgramResult {
    let mut t = trade_accounts(pid, it)?;
    require(t.launch.status == STATUS_CURVE, MeeError::InvalidInstruction)?;
    let fees = t.fees();
    let l = &mut t.launch;
    let qs = quote_sell(l.virtual_quote, l.virtual_tokens, tokens_in, fees).ok_or(MeeError::InvalidInstruction)?;
    require(qs.gross_out <= l.real_quote, MeeError::InvalidInstruction)?;
    require(qs.net_out >= min_quote_out, MeeError::SlippageExceeded)?;
    l.virtual_quote -= qs.gross_out;
    l.virtual_tokens += tokens_in;
    l.real_quote -= qs.gross_out;
    l.real_tokens += tokens_in;
    l.creator_fees += qs.creator_fee;
    let launch = t.launch;
    store(t.launch_ai, &launch)?;
    token::transfer(t.token_program, t.user_token, t.token_vault, t.user, tokens_in, &[])?;
    t.vault_pay(t.quote_vault, t.user_quote, qs.net_out)?;
    t.vault_pay(t.quote_vault, t.treasury, qs.protocol_fee)
}

/// Permissionless once the curve sold out: opens the pool at the curve's
/// final price. Tokens stay in the same vault; only accounting moves.
pub fn graduate(pid: &Pubkey, it: &mut Accounts) -> ProgramResult {
    let launch_ai = next(it)?;
    let clock = Clock::from_account_info(next(it)?)?;
    let mut l = load_launch(launch_ai, pid)?;
    require(l.status == STATUS_COMPLETE, MeeError::InvalidInstruction)?;
    // pool_quote / pool_tokens == virtual_quote / virtual_tokens.
    let pool_tokens = (l.real_quote as u128 * l.virtual_tokens as u128 / l.virtual_quote as u128)
        .min(l.reserve_tokens as u128) as u64;
    require(pool_tokens > 0 && l.real_quote > 0, MeeError::InvalidInstruction)?;
    l.pool_quote = l.real_quote;
    l.pool_tokens = pool_tokens;
    l.locked_tokens = l.reserve_tokens - pool_tokens;
    l.status = STATUS_POOL;
    l.graduated_ts = clock.unix_timestamp;
    l.last_ts = clock.unix_timestamp;
    store(launch_ai, &l)
}

pub fn swap(pid: &Pubkey, it: &mut Accounts, quote_to_token: bool, amount_in: u64, min_out: u64) -> ProgramResult {
    let mut t = trade_accounts(pid, it)?;
    require(t.launch.status == STATUS_POOL, MeeError::InvalidInstruction)?;
    let (pf_bps, cf_bps, lp_bps) = (t.cfg.protocol_fee_bps, t.cfg.creator_fee_bps, t.cfg.pool_fee_bps);
    let now = t.now;
    let l = &mut t.launch;
    l.accrue(now);
    let (q, tk) = (l.pool_quote as u128, l.pool_tokens as u128);
    let k = q * tk;
    if quote_to_token {
        let (pf, cf, lf) =
            (fee(amount_in as u128, pf_bps), fee(amount_in as u128, cf_bps), fee(amount_in as u128, lp_bps));
        let net = (amount_in as u128).checked_sub(pf + cf + lf).ok_or(MeeError::InvalidInstruction)?;
        let out = tk - ceil_div(k, q + net);
        require(out > 0 && out as u64 >= min_out, MeeError::SlippageExceeded)?;
        // The LP fee joins the reserves, so k grows with every trade.
        l.pool_quote = (q + net + lf) as u64;
        l.pool_tokens = (tk - out) as u64;
        l.creator_fees += cf as u64;
        let launch = t.launch;
        store(t.launch_ai, &launch)?;
        t.pay_in(amount_in, pf as u64)?;
        t.vault_pay(t.token_vault, t.user_token, out as u64)
    } else {
        let gross = q - ceil_div(k, tk + amount_in as u128);
        let (pf, cf, lf) = (fee(gross, pf_bps), fee(gross, cf_bps), fee(gross, lp_bps));
        let net = gross.checked_sub(pf + cf + lf).ok_or(MeeError::InvalidInstruction)?;
        require(net > 0 && net as u64 >= min_out, MeeError::SlippageExceeded)?;
        l.pool_quote = (q - gross + lf) as u64;
        l.pool_tokens = (tk + amount_in as u128) as u64;
        l.creator_fees += cf as u64;
        let launch = t.launch;
        store(t.launch_ai, &launch)?;
        token::transfer(t.token_program, t.user_token, t.token_vault, t.user, amount_in, &[])?;
        t.vault_pay(t.quote_vault, t.user_quote, net as u64)?;
        t.vault_pay(t.quote_vault, t.treasury, pf as u64)
    }
}

/// [creator (s), launch (w), quote_vault (w), creator_quote (w), authority, token_program]
pub fn claim_creator_fees(pid: &Pubkey, it: &mut Accounts) -> ProgramResult {
    let creator = next(it)?;
    let launch_ai = next(it)?;
    let quote_vault = next(it)?;
    let dest = next(it)?;
    let authority = next(it)?;
    let token_program = next(it)?;
    require(creator.is_signer, MeeError::Unauthorized)?;
    token::check_token_program(token_program)?;
    let mut l = load_launch(launch_ai, pid)?;
    require(creator.key.to_bytes() == l.creator, MeeError::Unauthorized)?;
    let (auth, bump) = authority_pda(pid);
    require(quote_vault.key == &quote_vault_pda(pid, launch_ai.key).0 && authority.key == &auth, MeeError::InvalidPda)?;
    let amount = l.creator_fees;
    l.creator_fees = 0;
    store(launch_ai, &l)?;
    token::transfer(token_program, quote_vault, dest, authority, amount, &[&[AUTHORITY_SEED, &[bump]]])
}

// --- client builders --------------------------------------------------------------

fn ix(pid: &Pubkey, data: MeeInstruction, accounts: Vec<AccountMeta>) -> Instruction {
    Instruction { program_id: *pid, accounts, data: borsh::to_vec(&data).expect("borsh") }
}

pub fn init_ix(
    pid: &Pubkey,
    admin: &Pubkey,
    quote_mint: &Pubkey,
    treasury: &Pubkey,
    params: LaunchParams,
) -> Instruction {
    ix(
        pid,
        MeeInstruction::InitLaunchpad { params },
        vec![
            AccountMeta::new(*admin, true),
            AccountMeta::new(config_pda(pid).0, false),
            AccountMeta::new_readonly(*quote_mint, false),
            AccountMeta::new_readonly(*treasury, false),
            AccountMeta::new_readonly(token::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::rent::ID, false),
        ],
    )
}

/// `id` is the config's `next_launch_id`.
pub fn create_ix(
    pid: &Pubkey,
    creator: &Pubkey,
    quote_mint: &Pubkey,
    id: u64,
    name: [u8; 32],
    symbol: [u8; 16],
) -> Instruction {
    let launch = launch_pda(pid, id).0;
    ix(
        pid,
        MeeInstruction::CreateLaunch { name, symbol },
        vec![
            AccountMeta::new(*creator, true),
            AccountMeta::new(config_pda(pid).0, false),
            AccountMeta::new(launch, false),
            AccountMeta::new(mint_pda(pid, &launch).0, false),
            AccountMeta::new(token_vault_pda(pid, &launch).0, false),
            AccountMeta::new(quote_vault_pda(pid, &launch).0, false),
            AccountMeta::new_readonly(*quote_mint, false),
            AccountMeta::new_readonly(authority_pda(pid).0, false),
            AccountMeta::new_readonly(token::TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(token::SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::rent::ID, false),
            AccountMeta::new_readonly(solana_program::sysvar::clock::ID, false),
        ],
    )
}

fn trade_metas(
    pid: &Pubkey,
    user: &Pubkey,
    id: u64,
    user_token: &Pubkey,
    user_quote: &Pubkey,
    treasury: &Pubkey,
) -> Vec<AccountMeta> {
    let launch = launch_pda(pid, id).0;
    vec![
        AccountMeta::new_readonly(*user, true),
        AccountMeta::new_readonly(config_pda(pid).0, false),
        AccountMeta::new(launch, false),
        AccountMeta::new(token_vault_pda(pid, &launch).0, false),
        AccountMeta::new(quote_vault_pda(pid, &launch).0, false),
        AccountMeta::new(*user_token, false),
        AccountMeta::new(*user_quote, false),
        AccountMeta::new(*treasury, false),
        AccountMeta::new_readonly(authority_pda(pid).0, false),
        AccountMeta::new_readonly(token::TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(solana_program::sysvar::clock::ID, false),
    ]
}

#[allow(clippy::too_many_arguments)]
pub fn buy_ix(
    pid: &Pubkey,
    user: &Pubkey,
    id: u64,
    user_token: &Pubkey,
    user_quote: &Pubkey,
    treasury: &Pubkey,
    quote_in: u64,
    min_out: u64,
) -> Instruction {
    ix(
        pid,
        MeeInstruction::LaunchBuy { quote_in, min_tokens_out: min_out },
        trade_metas(pid, user, id, user_token, user_quote, treasury),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn sell_ix(
    pid: &Pubkey,
    user: &Pubkey,
    id: u64,
    user_token: &Pubkey,
    user_quote: &Pubkey,
    treasury: &Pubkey,
    tokens_in: u64,
    min_out: u64,
) -> Instruction {
    ix(
        pid,
        MeeInstruction::LaunchSell { tokens_in, min_quote_out: min_out },
        trade_metas(pid, user, id, user_token, user_quote, treasury),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn swap_ix(
    pid: &Pubkey,
    user: &Pubkey,
    id: u64,
    user_token: &Pubkey,
    user_quote: &Pubkey,
    treasury: &Pubkey,
    quote_to_token: bool,
    amount_in: u64,
    min_out: u64,
) -> Instruction {
    ix(
        pid,
        MeeInstruction::PoolSwap { quote_to_token, amount_in, min_out },
        trade_metas(pid, user, id, user_token, user_quote, treasury),
    )
}

pub fn graduate_ix(pid: &Pubkey, id: u64) -> Instruction {
    ix(
        pid,
        MeeInstruction::Graduate,
        vec![
            AccountMeta::new(launch_pda(pid, id).0, false),
            AccountMeta::new_readonly(solana_program::sysvar::clock::ID, false),
        ],
    )
}

pub fn claim_creator_fees_ix(pid: &Pubkey, creator: &Pubkey, id: u64, dest: &Pubkey) -> Instruction {
    let launch = launch_pda(pid, id).0;
    ix(
        pid,
        MeeInstruction::ClaimCreatorFees,
        vec![
            AccountMeta::new_readonly(*creator, true),
            AccountMeta::new(launch, false),
            AccountMeta::new(quote_vault_pda(pid, &launch).0, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(authority_pda(pid).0, false),
            AccountMeta::new_readonly(token::TOKEN_PROGRAM_ID, false),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buy_caps_at_allotment_and_charges_exactly() {
        // Reserves 5_000 USDC / 1.073B tokens, 800M on the curve.
        let (q, t, cap) = (5_000_000_000, 1_073_000_000_000_000, 800_000_000_000_000);
        let small = quote_buy(q, t, cap, 1_000_000, (50, 50)).unwrap();
        assert_eq!(small.gross_in, 1_000_000);
        assert_eq!(small.net_in + small.protocol_fee + small.creator_fee, 1_000_000);
        let huge = quote_buy(q, t, cap, 1_000_000_000_000, (50, 50)).unwrap();
        assert_eq!(huge.tokens_out, cap);
        assert!(huge.gross_in < 1_000_000_000_000);
        // Paying one atom less than the charged gross would not have bought it all.
        let fees = |g: u128| fee(g, 50) + fee(g, 50);
        assert!((huge.gross_in as u128 - 1) - fees(huge.gross_in as u128 - 1) < huge.net_in as u128);
    }

    #[test]
    fn round_trip_never_profits() {
        let (q, t) = (5_000_000_000u64, 1_073_000_000_000_000u64);
        for amount in [1_000u64, 1_000_000, 123_456_789, 50_000_000_000] {
            let b = quote_buy(q, t, u64::MAX, amount, (50, 50)).unwrap();
            let s = quote_sell(q + b.net_in, t - b.tokens_out, b.tokens_out, (50, 50)).unwrap();
            assert!(s.net_out < amount, "{amount}");
            assert!(s.gross_out <= b.net_in, "the curve never pays out more than it took in");
        }
    }

    #[test]
    fn twap_wraps() {
        assert_eq!(twap((u128::MAX - 5, 10), (4, 20)), Some(1));
        assert_eq!(twap((0, 10), (0, 10)), None);
    }
}
