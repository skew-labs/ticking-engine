//! Chain snapshots: everything a node needs to resume at a height without
//! replaying history. Loading checks the snapshot against the genesis (chain
//! id, bridge program, engine params; the launch and staking configuration
//! come from the genesis itself), recomputes the withdrawal and listing hash
//! chains, and requires the rebuilt state root to equal the recorded one, so
//! a corrupt or tampered snapshot is refused.

use borsh::{BorshDeserialize, BorshSerialize};
use mee_engine::{AccountId, MarketId, State};

use crate::chain::{Chain, Genesis, Listing, Trigger};
use crate::crypto::{self, Hash, Pubkey};
use crate::tx::WithdrawalReceipt;

#[derive(BorshSerialize, BorshDeserialize)]
struct TriggerSnap {
    account: AccountId,
    market: MarketId,
    side: u8,
    above: bool,
    trigger_price: u64,
    limit_price: u64,
    size: u64,
    reduce_only: bool,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct ChainSnap {
    chain_id: u64,
    bridge_program: Pubkey,
    validators: Vec<Pubkey>,
    validator_weights: Vec<u64>,
    state: Vec<u8>,
    height: u64,
    timestamp_ms: i64,
    app_hash: Hash,
    state_root: Hash,
    withdrawals_hash: Hash,
    listings_hash: Hash,
    next_deposit_id: u64,
    next_withdrawal_id: u64,
    governance_nonce: u64,
    withdrawals: Vec<WithdrawalReceipt>,
    launch_dex: Option<u32>,
    /// (launch id, market, lot atoms) by market id = listing order.
    listings: Vec<(u64, MarketId, u64)>,
    last_oracle_slot: u64,
    epoch: u64,
    stakes: Vec<(Pubkey, u64)>,
    delegations: Vec<((AccountId, Pubkey), u64)>,
    unbonding: Vec<(i64, u64, AccountId, Pubkey, u64)>,
    epochs: Vec<(u64, (Hash, Vec<Pubkey>))>,
    tombstoned: Vec<Pubkey>,
    slashed_validator_stake: u64,
    triggers: Vec<(u64, TriggerSnap)>,
    next_trigger_id: u64,
    registered: Vec<Pubkey>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SnapshotError {
    Decode,
    WrongGenesis,
    Corrupt,
}

impl Chain {
    pub fn to_snapshot(&self) -> Vec<u8> {
        let mut listings: Vec<(u64, MarketId, u64)> =
            self.listings.iter().map(|(id, l)| (*id, l.market, l.lot_atoms)).collect();
        listings.sort_by_key(|l| l.1);
        let snap = ChainSnap {
            chain_id: self.chain_id,
            bridge_program: self.bridge_program,
            validators: self.validators.clone(),
            validator_weights: self.validator_weights.clone(),
            state: self.state.to_snapshot(),
            height: self.height,
            timestamp_ms: self.timestamp_ms,
            app_hash: self.app_hash,
            state_root: self.state_root,
            withdrawals_hash: self.withdrawals_hash,
            listings_hash: self.listings_hash,
            next_deposit_id: self.next_deposit_id,
            next_withdrawal_id: self.next_withdrawal_id,
            governance_nonce: self.governance_nonce,
            withdrawals: self.withdrawals.clone(),
            launch_dex: self.launch_dex,
            listings,
            last_oracle_slot: self.last_oracle_slot,
            epoch: self.epoch,
            stakes: self.stakes.iter().map(|(k, v)| (*k, *v)).collect(),
            delegations: self.delegations.iter().map(|(k, v)| (*k, *v)).collect(),
            unbonding: self.unbonding.clone(),
            epochs: self.epochs.iter().map(|(k, v)| (*k, v.clone())).collect(),
            tombstoned: self.tombstoned.iter().copied().collect(),
            slashed_validator_stake: self.slashed_validator_stake,
            triggers: self
                .triggers
                .iter()
                .map(|(id, t)| {
                    let t = TriggerSnap {
                        account: t.account,
                        market: t.market,
                        side: t.side as u8,
                        above: t.above,
                        trigger_price: t.trigger_price,
                        limit_price: t.limit_price,
                        size: t.size,
                        reduce_only: t.reduce_only,
                    };
                    (*id, t)
                })
                .collect(),
            next_trigger_id: self.next_trigger_id,
            registered: self.registered.iter().copied().collect(),
        };
        borsh::to_vec(&snap).expect("borsh")
    }

    pub fn from_snapshot(genesis: &Genesis, bytes: &[u8]) -> Result<Chain, SnapshotError> {
        let s = ChainSnap::try_from_slice(bytes).map_err(|_| SnapshotError::Decode)?;
        let state = State::from_snapshot(&s.state).map_err(|_| SnapshotError::Corrupt)?;
        if s.chain_id != genesis.chain_id
            || s.bridge_program != genesis.bridge_program
            || bytemuck::bytes_of(&state.params) != bytemuck::bytes_of(&genesis.params)
        {
            return Err(SnapshotError::WrongGenesis);
        }
        if s.validators.len() != s.validator_weights.len() {
            return Err(SnapshotError::Corrupt);
        }
        // Hash chains over append-only data: recompute them.
        let withdrawals =
            s.withdrawals.iter().fold([0u8; 32], |h, w| crypto::sha256(&[&h, &borsh::to_vec(w).expect("borsh")]));
        let listings = s.listings.iter().fold([0u8; 32], |h, (id, m, lot)| {
            crypto::sha256(&[&h, &id.to_le_bytes(), &m.to_le_bytes(), &lot.to_le_bytes()])
        });
        if withdrawals != s.withdrawals_hash || listings != s.listings_hash {
            return Err(SnapshotError::Corrupt);
        }
        let mut triggers = std::collections::BTreeMap::new();
        for (id, t) in s.triggers {
            let side = mee_core::Side::from_u8(t.side).ok_or(SnapshotError::Decode)?;
            let t = Trigger {
                account: t.account,
                market: t.market,
                side,
                above: t.above,
                trigger_price: t.trigger_price,
                limit_price: t.limit_price,
                size: t.size,
                reduce_only: t.reduce_only,
            };
            triggers.insert(id, t);
        }
        let mut c = Chain::new(genesis.clone());
        c.validators = s.validators;
        c.validator_weights = s.validator_weights;
        c.state = state;
        c.height = s.height;
        c.timestamp_ms = s.timestamp_ms;
        c.app_hash = s.app_hash;
        c.withdrawals_hash = s.withdrawals_hash;
        c.listings_hash = s.listings_hash;
        c.next_deposit_id = s.next_deposit_id;
        c.next_withdrawal_id = s.next_withdrawal_id;
        c.governance_nonce = s.governance_nonce;
        c.withdrawals = s.withdrawals;
        c.launch_dex = s.launch_dex;
        c.listings =
            s.listings.into_iter().map(|(id, market, lot_atoms)| (id, Listing { market, lot_atoms })).collect();
        c.last_oracle_slot = s.last_oracle_slot;
        c.epoch = s.epoch;
        c.stakes = s.stakes.into_iter().collect();
        c.delegations = s.delegations.into_iter().collect();
        c.unbonding = s.unbonding;
        c.epochs = s.epochs.into_iter().collect();
        c.epochs_hash = c.epochs.iter().fold([0u8; 32], |h, (e, (g, m))| crate::chain::epoch_link(&h, *e, g, m));
        c.tombstoned = s.tombstoned.into_iter().collect();
        c.slashed_validator_stake = s.slashed_validator_stake;
        c.slashed_in_block = false;
        for (id, t) in triggers {
            c.add_trigger(id, t);
        }
        c.next_trigger_id = s.next_trigger_id;
        c.registered = s.registered.into_iter().collect();
        if !c.epochs.contains_key(&c.epoch) {
            return Err(SnapshotError::Corrupt);
        }
        c.state_root = crypto::sha256(&[b"MEE/chain-state", &c.state.last_roots.root, &c.globals_hash()]);
        if c.state_root != s.state_root {
            return Err(SnapshotError::Corrupt);
        }
        Ok(c)
    }
}
