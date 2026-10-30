//! Instruction set and client-side builders. Account order in each builder is
//! the order the processor reads them in.

use borsh::{BorshDeserialize, BorshSerialize};
use mee_core::EngineParams;
use mee_core::engine::MarketArgs;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use crate::pda;
use crate::token::{SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID};

const CLOCK: Pubkey = solana_program::sysvar::clock::ID;
const RENT: Pubkey = solana_program::sysvar::rent::ID;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ParamsArgs {
    pub taker_fee_bps: u64,
    pub maker_fee_bps: u64,
    pub max_fee_share_bps: u64,
    pub max_oracle_move_bps: u64,
    pub min_oracle_interval: i64,
    pub max_oracle_staleness: i64,
    pub mark_band_bps: u64,
    pub order_band_bps: u64,
    pub max_fills_per_order: u64,
    pub liquidation_insurance_share_bps: u64,
    pub min_deployer_stake: u64,
    pub min_stake_hold_secs: i64,
    pub unstake_delay_secs: i64,
    pub min_initial_margin_bps: u64,
}

impl From<ParamsArgs> for EngineParams {
    fn from(a: ParamsArgs) -> Self {
        EngineParams {
            taker_fee_bps: a.taker_fee_bps,
            maker_fee_bps: a.maker_fee_bps,
            max_fee_share_bps: a.max_fee_share_bps,
            max_oracle_move_bps: a.max_oracle_move_bps,
            min_oracle_interval: a.min_oracle_interval,
            max_oracle_staleness: a.max_oracle_staleness,
            mark_band_bps: a.mark_band_bps,
            order_band_bps: a.order_band_bps,
            max_fills_per_order: a.max_fills_per_order,
            liquidation_insurance_share_bps: a.liquidation_insurance_share_bps,
            min_deployer_stake: a.min_deployer_stake,
            min_stake_hold_secs: a.min_stake_hold_secs,
            unstake_delay_secs: a.unstake_delay_secs,
            min_initial_margin_bps: a.min_initial_margin_bps,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AddMarketArgs {
    pub symbol: [u8; 16],
    pub tick_size: u64,
    pub min_base_lots: u64,
    pub initial_margin_bps: u64,
    pub maintenance_margin_bps: u64,
    pub liquidation_fee_bps: u64,
    pub oi_cap: u64,
    pub initial_price: u64,
    pub funding_interval: i64,
    pub max_funding_bps: u64,
}

impl From<AddMarketArgs> for MarketArgs {
    fn from(a: AddMarketArgs) -> Self {
        MarketArgs {
            symbol: a.symbol,
            tick_size: a.tick_size,
            min_base_lots: a.min_base_lots,
            initial_margin_bps: a.initial_margin_bps,
            maintenance_margin_bps: a.maintenance_margin_bps,
            liquidation_fee_bps: a.liquidation_fee_bps,
            oi_cap: a.oi_cap,
            initial_price: a.initial_price,
            funding_interval: a.funding_interval,
            max_funding_bps: a.max_funding_bps,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct OrderArgs {
    pub market: u8,
    /// 0 = bid, 1 = ask.
    pub side: u8,
    /// 0 = limit, 1 = post-only, 2 = immediate-or-cancel.
    pub kind: u8,
    pub reduce_only: bool,
    pub price: u64,
    pub base_lots: u64,
    pub client_order_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum MeeInstruction {
    // --- governance -----------------------------------------------------------
    /// [admin (s,w), exchange (w), quote_mint, stake_mint, treasury_quote, treasury_stake, system_program, rent]
    InitExchange { params: ParamsArgs, guardians: Vec<[u8; 32]>, guardian_threshold: u8 },
    /// [admin (s), exchange (w)]
    UpdateExchange { params: ParamsArgs, guardians: Vec<[u8; 32]>, guardian_threshold: u8, new_admin: Option<[u8; 32]> },
    /// [guardian (s), exchange, dex (w)]
    VoteSlash { slash_bps: u64, freeze: bool },
    /// [exchange, dex (w), stake_vault (w), treasury_stake (w), vault_authority, token_program]
    ExecuteSlash,
    /// [admin (s), exchange, dex (w)]
    ClearSlash,

    // --- deployer --------------------------------------------------------------
    /// [deployer (s,w), exchange, dex (s,w), quote_vault (w), stake_vault (w), vault_authority,
    ///  quote_mint, stake_mint, deployer_stake_token (w), token_program, system_program, rent, clock]
    DeployDex { name: [u8; 16], fee_share_bps: u64, oracle_authority: [u8; 32], stake: u64 },
    /// [deployer (s), exchange, dex (w)]
    UpdateDex { fee_share_bps: Option<u64>, oracle_authority: Option<[u8; 32]> },
    /// [deployer (s), exchange, dex (w), book (s,w), clock]
    AddMarket { args: AddMarketArgs },
    /// [oracle_authority (s), exchange, dex (w), clock]
    SetOracle { prices: Vec<(u8, u64)> },
    /// [deployer, or admin once frozen (s), exchange, dex (w), book (w)]
    HaltMarket { market: u8, settlement_price: u64 },
    /// [deployer (s), exchange, dex (w), clock]
    RequestUnstake,
    /// [deployer (s), exchange, dex (w), stake_vault (w), deployer_stake_token (w), vault_authority, token_program, clock]
    CompleteUnstake,
    /// [deployer (s), dex (w), deployer_quote_token (w), quote_vault (w), vault_authority, token_program]
    ClaimDeployerFees,

    // --- permissionless cranks -------------------------------------------------
    /// [exchange, dex (w)]
    SettleMarket { market: u8, max_traders: u16 },
    /// [exchange, dex (w), clock]
    UpdateFunding { market: u8 },
    /// [exchange, dex (w), treasury_quote (w), quote_vault (w), vault_authority, token_program]
    ClaimProtocolFees,

    // --- traders ---------------------------------------------------------------
    /// [owner (s), dex (w)]
    RegisterTrader,
    /// [owner (s), dex (w)]
    CloseTrader,
    /// [owner (s), dex (w), owner_token (w), quote_vault (w), token_program]
    Deposit { amount: u64 },
    /// [owner (s), exchange, dex (w), owner_token (w), quote_vault (w), vault_authority, token_program, clock]
    Withdraw { amount: u64 },
    /// [owner (s), exchange, dex (w), book (w), clock]
    PlaceOrder { order: OrderArgs },
    /// [owner (s), dex (w), book (w)]
    CancelOrder { market: u8, order_id: u64 },
    /// [owner (s), dex (w), book (w)]
    CancelAll { market: u8 },
    /// [liquidator (s), exchange, dex (w), book (w), clock]
    Liquidate { market: u8, liquidatee: [u8; 32], max_base: u64 },
    /// [funder (s), dex (w), funder_token (w), quote_vault (w), token_program]
    FundInsurance { amount: u64 },
}

fn ix(program_id: &Pubkey, data: MeeInstruction, accounts: Vec<AccountMeta>) -> Instruction {
    Instruction { program_id: *program_id, accounts, data: borsh::to_vec(&data).expect("borsh") }
}

fn w(k: &Pubkey) -> AccountMeta {
    AccountMeta::new(*k, false)
}

fn r(k: &Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(*k, false)
}

fn s(k: &Pubkey) -> AccountMeta {
    AccountMeta::new_readonly(*k, true)
}

fn sw(k: &Pubkey) -> AccountMeta {
    AccountMeta::new(*k, true)
}

#[allow(clippy::too_many_arguments)]
pub fn init_exchange(
    program_id: &Pubkey,
    admin: &Pubkey,
    quote_mint: &Pubkey,
    stake_mint: &Pubkey,
    treasury_quote: &Pubkey,
    treasury_stake: &Pubkey,
    params: ParamsArgs,
    guardians: Vec<[u8; 32]>,
    guardian_threshold: u8,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(
        program_id,
        MeeInstruction::InitExchange { params, guardians, guardian_threshold },
        vec![
            sw(admin),
            w(&exchange),
            r(quote_mint),
            r(stake_mint),
            r(treasury_quote),
            r(treasury_stake),
            r(&SYSTEM_PROGRAM_ID),
            r(&RENT),
        ],
    )
}

pub fn update_exchange(
    program_id: &Pubkey,
    admin: &Pubkey,
    params: ParamsArgs,
    guardians: Vec<[u8; 32]>,
    guardian_threshold: u8,
    new_admin: Option<[u8; 32]>,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(
        program_id,
        MeeInstruction::UpdateExchange { params, guardians, guardian_threshold, new_admin },
        vec![s(admin), w(&exchange)],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn deploy_dex(
    program_id: &Pubkey,
    deployer: &Pubkey,
    dex: &Pubkey,
    quote_mint: &Pubkey,
    stake_mint: &Pubkey,
    deployer_stake_token: &Pubkey,
    name: [u8; 16],
    fee_share_bps: u64,
    oracle_authority: &Pubkey,
    stake: u64,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    let (quote_vault, _) = pda::quote_vault(program_id, dex);
    let (stake_vault, _) = pda::stake_vault(program_id, dex);
    let (authority, _) = pda::vault_authority(program_id, dex);
    ix(
        program_id,
        MeeInstruction::DeployDex { name, fee_share_bps, oracle_authority: oracle_authority.to_bytes(), stake },
        vec![
            sw(deployer),
            r(&exchange),
            sw(dex),
            w(&quote_vault),
            w(&stake_vault),
            r(&authority),
            r(quote_mint),
            r(stake_mint),
            w(deployer_stake_token),
            r(&TOKEN_PROGRAM_ID),
            r(&SYSTEM_PROGRAM_ID),
            r(&RENT),
            r(&CLOCK),
        ],
    )
}

pub fn update_dex(
    program_id: &Pubkey,
    deployer: &Pubkey,
    dex: &Pubkey,
    fee_share_bps: Option<u64>,
    oracle_authority: Option<Pubkey>,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(
        program_id,
        MeeInstruction::UpdateDex { fee_share_bps, oracle_authority: oracle_authority.map(|k| k.to_bytes()) },
        vec![s(deployer), r(&exchange), w(dex)],
    )
}

pub fn add_market(
    program_id: &Pubkey,
    deployer: &Pubkey,
    dex: &Pubkey,
    book: &Pubkey,
    args: AddMarketArgs,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::AddMarket { args }, vec![s(deployer), r(&exchange), w(dex), sw(book), r(&CLOCK)])
}

pub fn set_oracle(program_id: &Pubkey, oracle_authority: &Pubkey, dex: &Pubkey, prices: Vec<(u8, u64)>) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::SetOracle { prices }, vec![s(oracle_authority), r(&exchange), w(dex), r(&CLOCK)])
}

pub fn halt_market(
    program_id: &Pubkey,
    authority: &Pubkey,
    dex: &Pubkey,
    book: &Pubkey,
    market: u8,
    settlement_price: u64,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(
        program_id,
        MeeInstruction::HaltMarket { market, settlement_price },
        vec![s(authority), r(&exchange), w(dex), w(book)],
    )
}

pub fn settle_market(program_id: &Pubkey, dex: &Pubkey, market: u8, max_traders: u16) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::SettleMarket { market, max_traders }, vec![r(&exchange), w(dex)])
}

pub fn update_funding(program_id: &Pubkey, dex: &Pubkey, market: u8) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::UpdateFunding { market }, vec![r(&exchange), w(dex), r(&CLOCK)])
}

pub fn request_unstake(program_id: &Pubkey, deployer: &Pubkey, dex: &Pubkey) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::RequestUnstake, vec![s(deployer), r(&exchange), w(dex), r(&CLOCK)])
}

pub fn complete_unstake(
    program_id: &Pubkey,
    deployer: &Pubkey,
    dex: &Pubkey,
    deployer_stake_token: &Pubkey,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    let (stake_vault, _) = pda::stake_vault(program_id, dex);
    let (authority, _) = pda::vault_authority(program_id, dex);
    ix(
        program_id,
        MeeInstruction::CompleteUnstake,
        vec![
            s(deployer),
            r(&exchange),
            w(dex),
            w(&stake_vault),
            w(deployer_stake_token),
            r(&authority),
            r(&TOKEN_PROGRAM_ID),
            r(&CLOCK),
        ],
    )
}

pub fn claim_deployer_fees(
    program_id: &Pubkey,
    deployer: &Pubkey,
    dex: &Pubkey,
    deployer_quote_token: &Pubkey,
) -> Instruction {
    let (quote_vault, _) = pda::quote_vault(program_id, dex);
    let (authority, _) = pda::vault_authority(program_id, dex);
    ix(
        program_id,
        MeeInstruction::ClaimDeployerFees,
        vec![s(deployer), w(dex), w(deployer_quote_token), w(&quote_vault), r(&authority), r(&TOKEN_PROGRAM_ID)],
    )
}

pub fn claim_protocol_fees(program_id: &Pubkey, dex: &Pubkey, treasury_quote: &Pubkey) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    let (quote_vault, _) = pda::quote_vault(program_id, dex);
    let (authority, _) = pda::vault_authority(program_id, dex);
    ix(
        program_id,
        MeeInstruction::ClaimProtocolFees,
        vec![r(&exchange), w(dex), w(treasury_quote), w(&quote_vault), r(&authority), r(&TOKEN_PROGRAM_ID)],
    )
}

pub fn vote_slash(program_id: &Pubkey, guardian: &Pubkey, dex: &Pubkey, slash_bps: u64, freeze: bool) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::VoteSlash { slash_bps, freeze }, vec![s(guardian), r(&exchange), w(dex)])
}

pub fn execute_slash(program_id: &Pubkey, dex: &Pubkey, treasury_stake: &Pubkey) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    let (stake_vault, _) = pda::stake_vault(program_id, dex);
    let (authority, _) = pda::vault_authority(program_id, dex);
    ix(
        program_id,
        MeeInstruction::ExecuteSlash,
        vec![r(&exchange), w(dex), w(&stake_vault), w(treasury_stake), r(&authority), r(&TOKEN_PROGRAM_ID)],
    )
}

pub fn clear_slash(program_id: &Pubkey, admin: &Pubkey, dex: &Pubkey) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::ClearSlash, vec![s(admin), r(&exchange), w(dex)])
}

pub fn register_trader(program_id: &Pubkey, owner: &Pubkey, dex: &Pubkey) -> Instruction {
    ix(program_id, MeeInstruction::RegisterTrader, vec![s(owner), w(dex)])
}

pub fn close_trader(program_id: &Pubkey, owner: &Pubkey, dex: &Pubkey) -> Instruction {
    ix(program_id, MeeInstruction::CloseTrader, vec![s(owner), w(dex)])
}

pub fn deposit(program_id: &Pubkey, owner: &Pubkey, dex: &Pubkey, owner_token: &Pubkey, amount: u64) -> Instruction {
    let (quote_vault, _) = pda::quote_vault(program_id, dex);
    ix(
        program_id,
        MeeInstruction::Deposit { amount },
        vec![s(owner), w(dex), w(owner_token), w(&quote_vault), r(&TOKEN_PROGRAM_ID)],
    )
}

pub fn withdraw(program_id: &Pubkey, owner: &Pubkey, dex: &Pubkey, owner_token: &Pubkey, amount: u64) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    let (quote_vault, _) = pda::quote_vault(program_id, dex);
    let (authority, _) = pda::vault_authority(program_id, dex);
    ix(
        program_id,
        MeeInstruction::Withdraw { amount },
        vec![
            s(owner),
            r(&exchange),
            w(dex),
            w(owner_token),
            w(&quote_vault),
            r(&authority),
            r(&TOKEN_PROGRAM_ID),
            r(&CLOCK),
        ],
    )
}

pub fn place_order(program_id: &Pubkey, owner: &Pubkey, dex: &Pubkey, book: &Pubkey, order: OrderArgs) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(program_id, MeeInstruction::PlaceOrder { order }, vec![s(owner), r(&exchange), w(dex), w(book), r(&CLOCK)])
}

pub fn cancel_order(
    program_id: &Pubkey,
    owner: &Pubkey,
    dex: &Pubkey,
    book: &Pubkey,
    market: u8,
    order_id: u64,
) -> Instruction {
    ix(program_id, MeeInstruction::CancelOrder { market, order_id }, vec![s(owner), w(dex), w(book)])
}

pub fn cancel_all(program_id: &Pubkey, owner: &Pubkey, dex: &Pubkey, book: &Pubkey, market: u8) -> Instruction {
    ix(program_id, MeeInstruction::CancelAll { market }, vec![s(owner), w(dex), w(book)])
}

pub fn liquidate(
    program_id: &Pubkey,
    liquidator: &Pubkey,
    dex: &Pubkey,
    book: &Pubkey,
    market: u8,
    liquidatee: &Pubkey,
    max_base: u64,
) -> Instruction {
    let (exchange, _) = pda::exchange(program_id);
    ix(
        program_id,
        MeeInstruction::Liquidate { market, liquidatee: liquidatee.to_bytes(), max_base },
        vec![s(liquidator), r(&exchange), w(dex), w(book), r(&CLOCK)],
    )
}

pub fn fund_insurance(
    program_id: &Pubkey,
    funder: &Pubkey,
    dex: &Pubkey,
    funder_token: &Pubkey,
    amount: u64,
) -> Instruction {
    let (quote_vault, _) = pda::quote_vault(program_id, dex);
    ix(
        program_id,
        MeeInstruction::FundInsurance { amount },
        vec![s(funder), w(dex), w(funder_token), w(&quote_vault), r(&TOKEN_PROGRAM_ID)],
    )
}
