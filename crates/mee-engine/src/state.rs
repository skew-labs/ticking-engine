use mee_core::engine::MarketArgs;
use mee_core::{EngineParams, Key};
use rustc_hash::FxHashMap;

use crate::book::Book;

pub type AccountId = u32;
pub type DexId = u32;
pub type MarketId = u32;
pub type OrderId = u64;

/// One market's position. Same cash-flow model as `mee-core`: unrealized PnL
/// is `base * mark + quote`, and `quote` moves to collateral when flat.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub base: i64,
    pub quote: i128,
    pub last_funding: i128,
    pub open_bids: u64,
    pub open_asks: u64,
}

impl Position {
    pub fn is_empty(&self) -> bool {
        self.base == 0 && self.quote == 0 && self.open_bids == 0 && self.open_asks == 0
    }
}

/// An account's cross-margin sub-account inside one perp DEX. DEXes are
/// isolated from each other, like HIP-3.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Margin {
    pub collateral: i64,
    /// Sorted by market id; small, so a vec beats a map.
    pub positions: Vec<(MarketId, Position)>,
}

impl Margin {
    pub fn position(&self, m: MarketId) -> Position {
        match self.positions.binary_search_by_key(&m, |(id, _)| *id) {
            Ok(i) => self.positions[i].1,
            Err(_) => Position::default(),
        }
    }

    pub fn position_mut(&mut self, m: MarketId) -> &mut Position {
        let i = match self.positions.binary_search_by_key(&m, |(id, _)| *id) {
            Ok(i) => i,
            Err(i) => {
                self.positions.insert(i, (m, Position::default()));
                i
            }
        };
        &mut self.positions[i].1
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub owner: Key,
    /// Free quote balance (bridged USDC), outside any DEX.
    pub balance: i64,
    /// Free stake-token balance, used to deploy DEXes.
    pub stake_balance: u64,
    /// Highest transaction nonce used (replay protection, kept here so it is
    /// part of the committed state).
    pub nonce: u64,
    /// Sorted by DEX id.
    pub margins: Vec<(DexId, Margin)>,
}

impl Account {
    pub fn margin(&self, dex: DexId) -> Option<&Margin> {
        self.margins.binary_search_by_key(&dex, |(id, _)| *id).ok().map(|i| &self.margins[i].1)
    }

    /// Returns the margin and whether it was just created.
    pub fn margin_mut(&mut self, dex: DexId) -> (&mut Margin, bool) {
        match self.margins.binary_search_by_key(&dex, |(id, _)| *id) {
            Ok(i) => (&mut self.margins[i].1, false),
            Err(i) => {
                self.margins.insert(i, (dex, Margin::default()));
                (&mut self.margins[i].1, true)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Market {
    pub id: MarketId,
    pub dex: DexId,
    pub cfg: MarketArgs,
    pub status: u8,
    pub oracle_price: u64,
    pub oracle_ts: i64,
    pub cum_funding: i128,
    pub last_funding_ts: i64,
    pub open_interest: u64,
    pub last_trade_price: u64,
    pub settlement_price: u64,
    /// Launchpad markets: the token's creator earns the fee share instead of
    /// the DEX deployer, credited straight to their balance.
    pub creator: Option<AccountId>,
    pub creator_fee_share_bps: u64,
    /// Σ premium_bps × seconds since the last funding payment (funding TWAP).
    pub premium_acc: i128,
    pub premium_ts: i64,
    pub book: Book,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dex {
    pub id: DexId,
    pub name: [u8; 16],
    pub deployer: AccountId,
    pub oracle_authority: Key,
    pub status: u8,
    pub fee_share_bps: u64,
    pub stake: u64,
    pub deployed_ts: i64,
    pub unstake_requested_ts: i64,
    pub protocol_fees: i64,
    pub deployer_fees: i64,
    pub insurance: i64,
    pub socialized_loss: i64,
    pub unrecovered_loss: i64,
    pub markets: Vec<MarketId>,
    /// Prices come from validator attestations (protocol-run DEX), not from
    /// a deployer key.
    pub attested_oracle: bool,
    /// Accounts holding a margin here, in join order (for deterministic sweeps).
    pub members: Vec<AccountId>,
    /// Hash chain over `members`, so the list is committed in O(1) per join.
    pub members_hash: [u8; 32],
}

/// Account storage that remembers which accounts were mutated since the last
/// state commit: every mutable access goes through `IndexMut` / `get_mut`.
#[derive(Clone, Debug, Default)]
pub struct Accounts {
    v: Vec<Account>,
    dirty: Vec<AccountId>,
    is_dirty: Vec<bool>,
}

impl Accounts {
    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    pub fn push(&mut self, a: Account) {
        self.v.push(a);
        self.is_dirty.push(false);
        self.mark(self.v.len() - 1);
    }

    pub fn get(&self, i: usize) -> Option<&Account> {
        self.v.get(i)
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut Account> {
        if i < self.v.len() {
            self.mark(i);
        }
        self.v.get_mut(i)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Account> {
        self.v.iter()
    }

    fn mark(&mut self, i: usize) {
        if !self.is_dirty[i] {
            self.is_dirty[i] = true;
            self.dirty.push(i as AccountId);
        }
    }

    pub fn take_dirty(&mut self) -> Vec<AccountId> {
        for &i in &self.dirty {
            self.is_dirty[i as usize] = false;
        }
        std::mem::take(&mut self.dirty)
    }
}

impl std::ops::Index<usize> for Accounts {
    type Output = Account;
    fn index(&self, i: usize) -> &Account {
        &self.v[i]
    }
}

impl std::ops::IndexMut<usize> for Accounts {
    fn index_mut(&mut self, i: usize) -> &mut Account {
        self.mark(i);
        &mut self.v[i]
    }
}

impl<'a> IntoIterator for &'a Accounts {
    type Item = &'a Account;
    type IntoIter = std::slice::Iter<'a, Account>;
    fn into_iter(self) -> Self::IntoIter {
        self.v.iter()
    }
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub params: EngineParams,
    pub now: i64,
    pub accounts: Accounts,
    pub account_tree: crate::commit::MerkleTree,
    pub last_roots: crate::commit::StateRoots,
    pub account_index: FxHashMap<Key, AccountId>,
    pub dexes: Vec<Dex>,
    pub markets: Vec<Market>,
    pub next_order_id: OrderId,
    /// Protocol fees swept out of DEXes, owed to the treasury.
    pub treasury: i64,
    /// Stake slashed from deployers.
    pub slashed_stake: u64,
}

impl State {
    pub fn new(params: EngineParams) -> State {
        State { params, next_order_id: 1, ..State::default() }
    }

    pub fn account_id(&self, owner: &Key) -> Option<AccountId> {
        self.account_index.get(owner).copied()
    }

    pub fn account_or_create(&mut self, owner: &Key) -> AccountId {
        if let Some(id) = self.account_id(owner) {
            return id;
        }
        let id = self.accounts.len() as AccountId;
        self.accounts.push(Account { owner: *owner, ..Account::default() });
        self.account_index.insert(*owner, id);
        id
    }
}
