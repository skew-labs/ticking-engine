//! State commitments: an incremental Merkle tree over accounts (only
//! accounts touched since the last commit are rehashed), multiset hashes
//! for order books, and a state root that light clients can prove account
//! inclusion against.

use sha2::{Digest, Sha256};

use crate::state::*;

pub type Hash = [u8; 32];

pub fn sha256(parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn node(l: &Hash, r: &Hash) -> Hash {
    sha256(&[b"MEE/node", l, r])
}

/// 256-bit additive multiset hash: order-independent, updatable in O(1).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MultisetHash(pub [u64; 4]);

impl MultisetHash {
    fn lanes(h: &Hash) -> [u64; 4] {
        core::array::from_fn(|i| u64::from_le_bytes(h[i * 8..i * 8 + 8].try_into().unwrap()))
    }

    pub fn add(&mut self, h: &Hash) {
        let x = Self::lanes(h);
        let mut carry = 0u128;
        for (lane, xi) in self.0.iter_mut().zip(x) {
            let s = *lane as u128 + xi as u128 + carry;
            *lane = s as u64;
            carry = s >> 64;
        }
    }

    pub fn sub(&mut self, h: &Hash) {
        let x = Self::lanes(h);
        let mut borrow = 0i128;
        for (lane, xi) in self.0.iter_mut().zip(x) {
            let d = *lane as i128 - xi as i128 - borrow;
            *lane = d as u64;
            borrow = (d < 0) as i128;
        }
    }

    pub fn bytes(&self) -> Hash {
        let mut out = [0u8; 32];
        for (i, l) in self.0.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&l.to_le_bytes());
        }
        out
    }
}

/// Binary Merkle tree over a growing list of leaves, padded to a power of
/// two with zero hashes. Updates cost O(log n) per changed leaf.
#[derive(Clone, Debug, Default)]
pub struct MerkleTree {
    /// levels[0] = leaves, last level = root.
    levels: Vec<Vec<Hash>>,
    len: usize,
}

impl MerkleTree {
    pub fn root(&self) -> Hash {
        self.levels.last().map_or([0; 32], |l| l[0])
    }

    fn rebuild(&mut self, leaves: Vec<Hash>) {
        let len = leaves.len();
        let mut level = leaves;
        level.resize(len.next_power_of_two().max(1), [0; 32]);
        let mut levels = vec![level];
        while levels.last().unwrap().len() > 1 {
            let prev = levels.last().unwrap();
            levels.push(prev.chunks(2).map(|c| node(&c[0], &c[1])).collect());
        }
        self.levels = levels;
        self.len = len;
    }

    /// Sets leaves `(index, hash)`; indices may extend the tree.
    pub fn update(&mut self, changes: &[(usize, Hash)], new_len: usize) {
        if self.levels.is_empty() || new_len > self.levels[0].len() {
            let mut leaves = self.levels.first().cloned().unwrap_or_default();
            leaves.truncate(self.len);
            leaves.resize(new_len, [0; 32]);
            for &(i, h) in changes {
                leaves[i] = h;
            }
            self.rebuild(leaves);
            return;
        }
        self.len = self.len.max(new_len);
        let mut dirty: Vec<usize> = Vec::with_capacity(changes.len());
        for &(i, h) in changes {
            self.levels[0][i] = h;
            dirty.push(i);
        }
        for lvl in 1..self.levels.len() {
            let mut parents: Vec<usize> = dirty.iter().map(|i| i / 2).collect();
            parents.sort_unstable();
            parents.dedup();
            for &p in &parents {
                let l = self.levels[lvl - 1][2 * p];
                let r = self.levels[lvl - 1][2 * p + 1];
                self.levels[lvl][p] = node(&l, &r);
            }
            dirty = parents;
        }
    }

    /// Sibling hashes from leaf `i` up to the root.
    pub fn proof(&self, mut i: usize) -> Vec<Hash> {
        let mut out = Vec::new();
        for level in &self.levels[..self.levels.len().saturating_sub(1)] {
            out.push(level[i ^ 1]);
            i /= 2;
        }
        out
    }
}

pub fn verify_proof(root: &Hash, mut index: usize, leaf: &Hash, proof: &[Hash]) -> bool {
    let mut h = *leaf;
    for sib in proof {
        h = if index % 2 == 0 { node(&h, sib) } else { node(sib, &h) };
        index /= 2;
    }
    &h == root
}

/// One SHA-256 compression over a single fixed-layout 64-byte block (domain
/// tag + order fields). For fixed-length input this is as collision
/// resistant as full SHA-256 and about 3x cheaper, which matters because it
/// runs on every book change.
pub fn order_hash(o: &crate::book::Order) -> Hash {
    let mut block = [0u8; 64];
    block[..8].copy_from_slice(b"MEE/ordr");
    block[8..16].copy_from_slice(&o.id.to_le_bytes());
    block[16..20].copy_from_slice(&o.account.to_le_bytes());
    block[20] = o.side as u8;
    block[24..32].copy_from_slice(&o.price.to_le_bytes());
    block[32..40].copy_from_slice(&o.remaining.to_le_bytes());
    block[40..48].copy_from_slice(&o.client_id.to_le_bytes());
    let mut state: [u32; 8] =
        [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    sha2::compress256(&mut state, &[block.into()]);
    let mut out = [0u8; 32];
    for (i, w) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
    }
    out
}

pub fn account_leaf(id: AccountId, a: &Account) -> Hash {
    let mut buf = Vec::with_capacity(96 + a.margins.len() * 64);
    buf.extend_from_slice(b"MEE/account");
    buf.extend_from_slice(&id.to_le_bytes());
    buf.extend_from_slice(&a.owner);
    buf.extend_from_slice(&a.balance.to_le_bytes());
    buf.extend_from_slice(&a.stake_balance.to_le_bytes());
    buf.extend_from_slice(&a.nonce.to_le_bytes());
    for (dex, mg) in &a.margins {
        buf.extend_from_slice(&dex.to_le_bytes());
        buf.extend_from_slice(&mg.collateral.to_le_bytes());
        for (m, p) in &mg.positions {
            buf.extend_from_slice(&m.to_le_bytes());
            buf.extend_from_slice(&p.base.to_le_bytes());
            buf.extend_from_slice(&p.quote.to_le_bytes());
            buf.extend_from_slice(&p.last_funding.to_le_bytes());
            buf.extend_from_slice(&p.open_bids.to_le_bytes());
            buf.extend_from_slice(&p.open_asks.to_le_bytes());
        }
    }
    sha256(&[&buf])
}

fn market_leaf(m: &Market) -> Hash {
    let c = &m.cfg;
    sha256(&[
        b"MEE/market",
        &m.id.to_le_bytes(),
        &m.dex.to_le_bytes(),
        &c.symbol,
        &[
            c.tick_size,
            c.min_base_lots,
            c.initial_margin_bps,
            c.maintenance_margin_bps,
            c.liquidation_fee_bps,
            c.oi_cap,
            c.initial_price,
            c.max_funding_bps,
        ]
        .map(u64::to_le_bytes)
        .concat(),
        &c.funding_interval.to_le_bytes(),
        &[m.status],
        &m.oracle_price.to_le_bytes(),
        &m.oracle_ts.to_le_bytes(),
        &m.cum_funding.to_le_bytes(),
        &m.last_funding_ts.to_le_bytes(),
        &m.open_interest.to_le_bytes(),
        &m.last_trade_price.to_le_bytes(),
        &m.settlement_price.to_le_bytes(),
        &m.creator.map_or(u64::MAX, |c| c as u64).to_le_bytes(),
        &m.creator_fee_share_bps.to_le_bytes(),
        &m.premium_acc.to_le_bytes(),
        &m.premium_ts.to_le_bytes(),
        &m.book.digest().bytes(),
    ])
}

fn dex_leaf(d: &Dex) -> Hash {
    sha256(&[
        b"MEE/dex",
        &d.id.to_le_bytes(),
        &d.name,
        &d.deployer.to_le_bytes(),
        &d.oracle_authority,
        &[d.status, d.attested_oracle as u8],
        &[d.fee_share_bps, d.stake].map(u64::to_le_bytes).concat(),
        &[
            d.deployed_ts,
            d.unstake_requested_ts,
            d.protocol_fees,
            d.deployer_fees,
            d.insurance,
            d.socialized_loss,
            d.unrecovered_loss,
        ]
        .map(i64::to_le_bytes)
        .concat(),
        &d.markets.iter().flat_map(|m| m.to_le_bytes()).collect::<Vec<_>>(),
        &d.members_hash,
    ])
}

/// The pieces of the engine state root. An account proof needs the other
/// three roots to recompute `root`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct StateRoots {
    pub accounts: Hash,
    pub markets: Hash,
    pub dexes: Hash,
    pub globals: Hash,
    pub root: Hash,
}

impl StateRoots {
    pub fn combine(accounts: &Hash, markets: &Hash, dexes: &Hash, globals: &Hash) -> Hash {
        sha256(&[b"MEE/state", accounts, markets, dexes, globals])
    }
}

impl State {
    /// Rehashes accounts touched since the last commit and every market and
    /// DEX (few, and each is a fixed-size digest), then returns the roots.
    pub fn commit(&mut self) -> StateRoots {
        let dirty = self.accounts.take_dirty();
        let changes: Vec<(usize, Hash)> =
            dirty.into_iter().map(|i| (i as usize, account_leaf(i, &self.accounts[i as usize]))).collect();
        self.account_tree.update(&changes, self.accounts.len());
        let mut markets = MerkleTree::default();
        markets.update(
            &self.markets.iter().enumerate().map(|(i, m)| (i, market_leaf(m))).collect::<Vec<_>>(),
            self.markets.len(),
        );
        let mut dexes = MerkleTree::default();
        dexes.update(
            &self.dexes.iter().enumerate().map(|(i, d)| (i, dex_leaf(d))).collect::<Vec<_>>(),
            self.dexes.len(),
        );
        let globals = sha256(&[
            b"MEE/globals",
            &self.now.to_le_bytes(),
            &self.next_order_id.to_le_bytes(),
            &self.treasury.to_le_bytes(),
            &self.slashed_stake.to_le_bytes(),
        ]);
        let (a, m, d) = (self.account_tree.root(), markets.root(), dexes.root());
        self.last_roots =
            StateRoots { accounts: a, markets: m, dexes: d, globals, root: StateRoots::combine(&a, &m, &d, &globals) };
        self.last_roots
    }

    /// Merkle proof of an account against the last commit's accounts root.
    pub fn prove_account(&self, id: AccountId) -> (Hash, Vec<Hash>) {
        (account_leaf(id, &self.accounts[id as usize]), self.account_tree.proof(id as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiset_is_order_independent_and_invertible() {
        let (a, b, c) = (sha256(&[b"a"]), sha256(&[b"b"]), sha256(&[b"c"]));
        let mut x = MultisetHash::default();
        x.add(&a);
        x.add(&b);
        let mut y = MultisetHash::default();
        y.add(&b);
        y.add(&c);
        y.add(&a);
        y.sub(&c);
        assert_eq!(x, y);
        y.sub(&a);
        y.sub(&b);
        assert_eq!(y, MultisetHash::default());
    }

    #[test]
    fn incremental_tree_matches_rebuild_and_proves() {
        let leaves: Vec<Hash> = (0..13u8).map(|i| sha256(&[&[i]])).collect();
        let mut t = MerkleTree::default();
        t.update(&leaves.iter().copied().enumerate().collect::<Vec<_>>(), 13);
        let mut fresh = MerkleTree::default();
        let mut changed = leaves.clone();
        changed[5] = sha256(&[b"x"]);
        changed[12] = sha256(&[b"y"]);
        fresh.update(&changed.iter().copied().enumerate().collect::<Vec<_>>(), 13);
        t.update(&[(5, changed[5]), (12, changed[12])], 13);
        assert_eq!(t.root(), fresh.root());
        for i in 0..13 {
            assert!(verify_proof(&t.root(), i, &changed[i], &t.proof(i)));
        }
        assert!(!verify_proof(&t.root(), 5, &leaves[5], &t.proof(5)));
        // Growth past the padded size rebuilds correctly.
        t.update(&[(20, sha256(&[b"z"]))], 21);
        assert!(verify_proof(&t.root(), 20, &sha256(&[b"z"]), &t.proof(20)));
        assert!(verify_proof(&t.root(), 12, &changed[12], &t.proof(12)));
    }
}
