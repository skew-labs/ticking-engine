//! A validator node: consensus orders opaque payloads, the node turns each
//! committed payload into a chain block and applies it.
//!
//! Payload = borsh `(timestamp_ms, Vec<Tx>)` chosen by the proposer. The
//! block timestamp is clamped to be non-decreasing, and the height is the
//! commit index, so every honest node derives the same chain blocks.

pub mod storage;

use std::path::Path;

use borsh::{BorshDeserialize, BorshSerialize};
use ed25519_dalek::SigningKey;
use mee_chain::{Block, BlockOutcome, Chain, Genesis, Tx};
use mee_consensus::{Config, Hash, Message, NodeId, Out, Replica};

use crate::storage::Storage;

#[derive(BorshSerialize, BorshDeserialize)]
struct Payload {
    timestamp_ms: i64,
    txs: Vec<Tx>,
}

pub struct Node {
    pub replica: Replica,
    pub chain: Chain,
    mempool: Vec<Tx>,
    /// App hash after each applied height (index = height - 1).
    pub app_hashes: Vec<Hash>,
    pub outcomes: Vec<BlockOutcome>,
    max_block_txs: usize,
    storage: Option<Storage>,
}

impl Node {
    pub fn new(id: NodeId, key: SigningKey, genesis: Genesis, cfg: Config) -> Node {
        let validators = genesis.validators.clone();
        let consensus_genesis = mee_chain::crypto::sha256(&[b"MEE/consensus-genesis", &genesis.chain_id.to_le_bytes()]);
        Node {
            replica: Replica::new(id, key, validators, consensus_genesis, cfg),
            chain: Chain::new(genesis),
            mempool: Vec::new(),
            app_hashes: Vec::new(),
            outcomes: Vec::new(),
            max_block_txs: 20_000,
            storage: None,
        }
    }

    /// A node backed by `dir`. On a fresh directory this is a new node; on
    /// an existing one it replays the committed block log into the chain and
    /// restores consensus from the persisted safety state.
    pub fn open(
        dir: &Path,
        id: NodeId,
        key: SigningKey,
        genesis: Genesis,
        cfg: Config,
        now_ms: u64,
    ) -> std::io::Result<(Node, Vec<Out>)> {
        let (storage, rec) = Storage::open(dir)?;
        let mut node = Node::new(id, key, genesis, cfg);
        for b in &rec.blocks {
            node.apply_payload(&b.payload);
        }
        let outs = match rec.safety {
            Some(safety) => node.replica.restore(safety, &rec.blocks, now_ms),
            None if rec.blocks.is_empty() => {
                node.refill(now_ms);
                node.replica.start(now_ms)
            }
            None => node.replica.restore(node.replica.safety_state(), &rec.blocks, now_ms),
        };
        node.storage = Some(storage);
        let outs = node.handle_persist(outs);
        Ok((node, outs))
    }

    /// Persists safety state in output order and strips it from what goes on
    /// the wire.
    fn handle_persist(&mut self, outs: Vec<Out>) -> Vec<Out> {
        let mut net = Vec::with_capacity(outs.len());
        for o in outs {
            match o {
                Out::Persist(s) => {
                    if let Some(st) = self.storage.as_mut() {
                        st.save_safety(&s).expect("persist safety state before voting");
                    }
                }
                other => net.push(other),
            }
        }
        net
    }

    /// Admits a transaction to the local mempool if it passes stateless and
    /// nonce checks against the current state.
    pub fn submit_tx(&mut self, tx: Tx) -> bool {
        if self.chain.check_tx(&tx).is_err() {
            return false;
        }
        self.mempool.push(tx);
        true
    }

    fn refill(&mut self, now_ms: u64) {
        // Drop what can no longer execute (nonce used or deposit already
        // credited), then hand the replica a fresh batch if it has none.
        let chain = &self.chain;
        self.mempool.retain(|tx| match tx {
            Tx::User(_) => chain.check_tx(tx).is_ok(),
            Tx::System(mee_chain::SystemTx::BridgeDeposit { deposit_id, .. }) => *deposit_id >= chain.next_deposit_id,
            Tx::System(mee_chain::SystemTx::Governance { nonce, .. }) => *nonce >= chain.governance_nonce,
            Tx::System(mee_chain::SystemTx::ListLaunch { launch_id, .. }) => !chain.listings.contains_key(launch_id),
            Tx::System(mee_chain::SystemTx::LaunchOracle { solana_slot, .. }) => *solana_slot > chain.last_oracle_slot,
        });
        if self.replica.queued_payloads() == 0 {
            let txs: Vec<Tx> = self.mempool.iter().take(self.max_block_txs).cloned().collect();
            let p = Payload { timestamp_ms: now_ms as i64, txs };
            self.replica.submit(borsh::to_vec(&p).expect("borsh"));
        }
    }

    fn apply_commits(&mut self) {
        for b in self.replica.drain_commits() {
            if let Some(st) = self.storage.as_mut() {
                st.append_block(&b).expect("append committed block");
            }
            self.apply_payload(&b.payload);
        }
    }

    fn apply_payload(&mut self, payload: &[u8]) {
        match Payload::try_from_slice(payload) {
            Ok(p) => self.apply(p.timestamp_ms.max(self.chain.timestamp_ms), p.txs),
            // An unparsable payload is an empty block for everyone.
            Err(_) => self.apply(self.chain.timestamp_ms, Vec::new()),
        }
    }

    fn apply(&mut self, timestamp_ms: i64, txs: Vec<Tx>) {
        let block = Block { height: self.chain.height + 1, timestamp_ms, txs };
        let out = self.chain.apply_block(&block).expect("height and time are derived locally");
        self.app_hashes.push(out.app_hash);
        self.outcomes.push(out);
    }

    pub fn start(&mut self, now_ms: u64) -> Vec<Out> {
        self.refill(now_ms);
        let out = self.replica.start(now_ms);
        self.handle_persist(out)
    }

    pub fn on_tick(&mut self, now_ms: u64) -> Vec<Out> {
        self.refill(now_ms);
        let out = self.replica.on_tick(now_ms);
        self.apply_commits();
        self.handle_persist(out)
    }

    pub fn on_message(&mut self, from: NodeId, msg: Message, now_ms: u64) -> Vec<Out> {
        let out = self.replica.on_message(from, msg, now_ms);
        self.apply_commits();
        self.handle_persist(out)
    }
}
