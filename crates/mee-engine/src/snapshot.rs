//! Full engine state as bytes, for node snapshots. Derived data (account
//! index, Merkle tree, book digests and links) is rebuilt on load, and the
//! rebuilt state root must equal the one recorded in the snapshot, so a
//! corrupt or tampered snapshot is rejected. The engine parameters are not
//! part of the root (they are genesis constants): callers compare them with
//! their genesis.

use borsh::{BorshDeserialize, BorshSerialize};
use mee_core::Side;
use mee_core::engine::MarketArgs;

use crate::book::Order;
use crate::commit::StateRoots;
use crate::state::*;

#[derive(BorshSerialize, BorshDeserialize)]
struct MarketArgsSnap {
    symbol: [u8; 16],
    tick_size: u64,
    min_base_lots: u64,
    initial_margin_bps: u64,
    maintenance_margin_bps: u64,
    liquidation_fee_bps: u64,
    oi_cap: u64,
    initial_price: u64,
    funding_interval: i64,
    max_funding_bps: u64,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct OrderSnap {
    id: OrderId,
    account: AccountId,
    side: u8,
    price: u64,
    remaining: u64,
    client_id: u64,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct MarketSnap {
    id: MarketId,
    dex: DexId,
    cfg: MarketArgsSnap,
    status: u8,
    oracle_price: u64,
    oracle_ts: i64,
    cum_funding: i128,
    last_funding_ts: i64,
    open_interest: u64,
    last_trade_price: u64,
    settlement_price: u64,
    creator: Option<AccountId>,
    creator_fee_share_bps: u64,
    premium_acc: i128,
    premium_ts: i64,
    /// Bids then asks, each in priority order (so FIFO order survives).
    orders: Vec<OrderSnap>,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct StateSnap {
    params: Vec<u8>,
    now: i64,
    accounts: Vec<Account>,
    dexes: Vec<Dex>,
    markets: Vec<MarketSnap>,
    next_order_id: OrderId,
    treasury: i64,
    slashed_stake: u64,
    /// `last_roots.root`, checked after rebuilding.
    root: [u8; 32],
}

#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotError {
    Decode,
    RootMismatch,
}

impl State {
    /// Serializes the state as of the last `commit()`.
    pub fn to_snapshot(&self) -> Vec<u8> {
        let markets = self
            .markets
            .iter()
            .map(|m| {
                let c = &m.cfg;
                MarketSnap {
                    id: m.id,
                    dex: m.dex,
                    cfg: MarketArgsSnap {
                        symbol: c.symbol,
                        tick_size: c.tick_size,
                        min_base_lots: c.min_base_lots,
                        initial_margin_bps: c.initial_margin_bps,
                        maintenance_margin_bps: c.maintenance_margin_bps,
                        liquidation_fee_bps: c.liquidation_fee_bps,
                        oi_cap: c.oi_cap,
                        initial_price: c.initial_price,
                        funding_interval: c.funding_interval,
                        max_funding_bps: c.max_funding_bps,
                    },
                    status: m.status,
                    oracle_price: m.oracle_price,
                    oracle_ts: m.oracle_ts,
                    cum_funding: m.cum_funding,
                    last_funding_ts: m.last_funding_ts,
                    open_interest: m.open_interest,
                    last_trade_price: m.last_trade_price,
                    settlement_price: m.settlement_price,
                    creator: m.creator,
                    creator_fee_share_bps: m.creator_fee_share_bps,
                    premium_acc: m.premium_acc,
                    premium_ts: m.premium_ts,
                    orders: m
                        .book
                        .iter(Side::Bid)
                        .chain(m.book.iter(Side::Ask))
                        .map(|o| OrderSnap {
                            id: o.id,
                            account: o.account,
                            side: o.side as u8,
                            price: o.price,
                            remaining: o.remaining,
                            client_id: o.client_id,
                        })
                        .collect(),
                }
            })
            .collect();
        let snap = StateSnap {
            params: bytemuck::bytes_of(&self.params).to_vec(),
            now: self.now,
            accounts: self.accounts.iter().cloned().collect(),
            dexes: self.dexes.clone(),
            markets,
            next_order_id: self.next_order_id,
            treasury: self.treasury,
            slashed_stake: self.slashed_stake,
            root: self.last_roots.root,
        };
        borsh::to_vec(&snap).expect("borsh")
    }

    /// Rebuilds a state from `to_snapshot` bytes and checks its root.
    pub fn from_snapshot(bytes: &[u8]) -> Result<State, SnapshotError> {
        let snap = StateSnap::try_from_slice(bytes).map_err(|_| SnapshotError::Decode)?;
        let params = *bytemuck::try_from_bytes(&snap.params).map_err(|_| SnapshotError::Decode)?;
        let mut s = State::new(params);
        s.now = snap.now;
        for a in snap.accounts {
            if s.account_index.insert(a.owner, s.accounts.len() as AccountId).is_some() {
                return Err(SnapshotError::Decode);
            }
            s.accounts.push(a);
        }
        // The root commits a dex's member list only through its hash chain:
        // check the list against it.
        for d in &snap.dexes {
            let chained = d.members.iter().fold([0u8; 32], |h, a| crate::commit::sha256(&[&h, &a.to_le_bytes()]));
            if chained != d.members_hash {
                return Err(SnapshotError::Decode);
            }
        }
        s.dexes = snap.dexes;
        for m in snap.markets {
            let c = m.cfg;
            let mut market = Market {
                id: m.id,
                dex: m.dex,
                cfg: MarketArgs {
                    symbol: c.symbol,
                    tick_size: c.tick_size,
                    min_base_lots: c.min_base_lots,
                    initial_margin_bps: c.initial_margin_bps,
                    maintenance_margin_bps: c.maintenance_margin_bps,
                    liquidation_fee_bps: c.liquidation_fee_bps,
                    oi_cap: c.oi_cap,
                    initial_price: c.initial_price,
                    funding_interval: c.funding_interval,
                    max_funding_bps: c.max_funding_bps,
                },
                status: m.status,
                oracle_price: m.oracle_price,
                oracle_ts: m.oracle_ts,
                cum_funding: m.cum_funding,
                last_funding_ts: m.last_funding_ts,
                open_interest: m.open_interest,
                last_trade_price: m.last_trade_price,
                settlement_price: m.settlement_price,
                creator: m.creator,
                creator_fee_share_bps: m.creator_fee_share_bps,
                premium_acc: m.premium_acc,
                premium_ts: m.premium_ts,
                book: Default::default(),
            };
            for o in m.orders {
                let side = Side::from_u8(o.side).ok_or(SnapshotError::Decode)?;
                if market.book.get(o.id).is_some() || o.remaining == 0 {
                    return Err(SnapshotError::Decode);
                }
                market.book.insert(Order {
                    id: o.id,
                    account: o.account,
                    side,
                    price: o.price,
                    remaining: o.remaining,
                    client_id: o.client_id,
                });
            }
            s.markets.push(market);
        }
        s.next_order_id = snap.next_order_id;
        s.treasury = snap.treasury;
        s.slashed_stake = snap.slashed_stake;
        let roots: StateRoots = s.commit();
        if roots.root != snap.root {
            return Err(SnapshotError::RootMismatch);
        }
        Ok(s)
    }
}
