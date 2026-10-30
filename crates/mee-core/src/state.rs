//! Account layouts. Every struct is `Pod` with 8-byte alignment and explicit
//! padding, so the program can map raw account data onto it without copying.

use bytemuck::{Pod, Zeroable};

use crate::math::I128;

pub const MAX_MARKETS: usize = 8;
pub const MAX_TRADERS: usize = 256;
pub const BOOK_CAPACITY: usize = 128;
pub const MAX_GUARDIANS: usize = 8;

pub type Key = [u8; 32];

pub const MARKET_UNINIT: u8 = 0;
pub const MARKET_ACTIVE: u8 = 1;
/// Trading stopped, book cleared, positions waiting for `settle_market`.
pub const MARKET_HALTED: u8 = 2;
/// Every position closed at the settlement price.
pub const MARKET_SETTLED: u8 = 3;

pub const DEX_ACTIVE: u8 = 1;
/// Slashed with freeze: the deployer lost control, governance winds it down.
pub const DEX_FROZEN: u8 = 2;
/// Stake returned after unstaking.
pub const DEX_RETIRED: u8 = 3;

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }

    pub fn from_u8(v: u8) -> Option<Side> {
        match v {
            0 => Some(Side::Bid),
            1 => Some(Side::Ask),
            _ => None,
        }
    }
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OrderKind {
    /// Match what crosses, rest the remainder.
    Limit = 0,
    /// Rejected if it would cross; otherwise rests.
    PostOnly = 1,
    /// Match what crosses, drop the remainder.
    ImmediateOrCancel = 2,
}

impl OrderKind {
    pub fn from_u8(v: u8) -> Option<OrderKind> {
        match v {
            0 => Some(OrderKind::Limit),
            1 => Some(OrderKind::PostOnly),
            2 => Some(OrderKind::ImmediateOrCancel),
            _ => None,
        }
    }
}

/// Protocol-wide risk and fee parameters, stored in the exchange account and
/// passed to every engine call.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct EngineParams {
    pub taker_fee_bps: u64,
    pub maker_fee_bps: u64,
    /// Upper bound on a deployer's fee share (HIP-3 allows 0-300%).
    pub max_fee_share_bps: u64,
    /// Largest oracle (or settlement price) move allowed per update.
    pub max_oracle_move_bps: u64,
    pub min_oracle_interval: i64,
    /// Older oracle prices block risk-increasing orders, liquidations,
    /// funding and withdrawals against that market.
    pub max_oracle_staleness: i64,
    /// Mark price is clamped to oracle ± this band.
    pub mark_band_bps: u64,
    /// Limit prices must sit within oracle ± this band.
    pub order_band_bps: u64,
    pub max_fills_per_order: u64,
    /// Share of the liquidation penalty that goes to the insurance fund.
    pub liquidation_insurance_share_bps: u64,
    pub min_deployer_stake: u64,
    pub min_stake_hold_secs: i64,
    pub unstake_delay_secs: i64,
    /// Highest leverage any market may offer (`10_000 / max_leverage`).
    pub min_initial_margin_bps: u64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Market {
    pub symbol: [u8; 16],
    pub status: u8,
    pub _pad0: [u8; 7],
    /// Book account bound to this market at creation.
    pub book: Key,
    /// Prices are quote atoms per base lot and must be multiples of this.
    pub tick_size: u64,
    pub min_base_lots: u64,
    pub initial_margin_bps: u64,
    pub maintenance_margin_bps: u64,
    pub liquidation_fee_bps: u64,
    /// Cap on open interest (sum of long base lots).
    pub oi_cap: u64,
    pub open_interest: u64,
    pub oracle_price: u64,
    pub oracle_ts: i64,
    /// Book top cached so the mark price is available without the book account.
    pub best_bid: u64,
    pub best_ask: u64,
    pub last_trade_price: u64,
    pub settlement_price: u64,
    pub settle_cursor: u64,
    pub funding_interval: i64,
    pub last_funding_ts: i64,
    pub max_funding_bps: u64,
    pub _pad1: u64,
    pub cum_funding: I128,
}

/// A trader's state in one market. Cash flows go to `quote`, so the engine
/// never needs an average entry price: unrealized PnL is
/// `base * mark + quote`, and `quote` moves into collateral when `base`
/// returns to zero. Σbase and Σquote over all traders stay zero (funding
/// rounding only ever leaves a surplus with the venue).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Position {
    pub base: i64,
    pub open_bids: u64,
    pub open_asks: u64,
    pub _pad: u64,
    pub quote: I128,
    pub last_funding: I128,
}

impl Position {
    pub fn is_empty(&self) -> bool {
        self.base == 0 && self.open_bids == 0 && self.open_asks == 0 && self.quote.get() == 0
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Trader {
    pub owner: Key,
    /// Quote atoms. Can go negative only transiently (fees, losses); bad debt
    /// is resolved when the trader is flat.
    pub collateral: i64,
    pub active: u8,
    pub _pad: [u8; 7],
    pub positions: [Position; MAX_MARKETS],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct DexHeader {
    pub discriminator: [u8; 8],
    pub exchange: Key,
    pub deployer: Key,
    pub oracle_authority: Key,
    pub quote_vault: Key,
    pub stake_vault: Key,
    pub name: [u8; 16],
    pub status: u8,
    pub vault_authority_bump: u8,
    pub slash_freeze: u8,
    pub _pad0: [u8; 5],
    pub stake_amount: u64,
    pub deployed_ts: i64,
    pub unstake_requested_ts: i64,
    /// Deployer fee as a share of the base protocol fee, charged on top.
    pub fee_share_bps: u64,
    pub protocol_fees: i64,
    pub deployer_fees: i64,
    pub insurance: i64,
    /// Bad debt taken from other traders' collateral after the insurance fund.
    pub socialized_loss: i64,
    /// Bad debt nobody could absorb. Non-zero means the dex is insolvent.
    pub unrecovered_loss: i64,
    pub market_count: u64,
    pub slash_nonce: u64,
    pub slash_bps: u64,
    /// Bitmask of guardian indices that voted for the open proposal.
    pub slash_votes: u64,
}

/// One builder-deployed perp DEX: config, markets and every trader's margin
/// account. Liquidity and risk are isolated per DEX.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct DexState {
    pub header: DexHeader,
    pub markets: [Market; MAX_MARKETS],
    pub traders: [Trader; MAX_TRADERS],
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct Order {
    pub order_id: u64,
    pub trader: u32,
    pub _pad: u32,
    pub price: u64,
    pub base_lots: u64,
    pub client_order_id: u64,
}

/// Orders sorted worst → best, so the best order is last and matching pops it
/// without shifting.
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct BookSide {
    pub count: u64,
    pub orders: [Order; BOOK_CAPACITY],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct Book {
    pub discriminator: [u8; 8],
    pub dex: Key,
    pub market_index: u64,
    pub next_order_id: u64,
    pub bids: BookSide,
    pub asks: BookSide,
}

impl Book {
    pub fn side(&self, side: Side) -> &BookSide {
        match side {
            Side::Bid => &self.bids,
            Side::Ask => &self.asks,
        }
    }

    pub fn side_mut(&mut self, side: Side) -> &mut BookSide {
        match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    #[test]
    fn layouts_are_8_aligned() {
        assert_eq!(align_of::<DexState>(), 8);
        assert_eq!(align_of::<Book>(), 8);
        assert_eq!(size_of::<Position>(), 64);
        assert_eq!(size_of::<Order>(), 40);
        assert_eq!(size_of::<DexState>() % 8, 0);
        assert_eq!(size_of::<Book>() % 8, 0);
        // Must fit a single Solana account (10 MiB).
        assert!(size_of::<DexState>() < 10 * 1024 * 1024);
    }
}
