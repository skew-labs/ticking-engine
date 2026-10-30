//! A validator node: consensus orders opaque payloads, the node turns each
//! committed payload into a chain block and applies it.
//!
//! Payload = borsh `(timestamp_ms, Vec<Tx>)` chosen by the proposer. The
//! block timestamp is clamped to be non-decreasing, and the height is the
//! commit index, so every honest node derives the same chain blocks.

use borsh::{BorshDeserialize, BorshSerialize};
use ed25519_dalek::SigningKey;
use mee_chain::{Block, BlockOutcome, Chain, Genesis, Tx};
use mee_consensus::{Config, Hash, Message, NodeId, Out, Replica};

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
        }
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
            let Ok(p) = Payload::try_from_slice(&b.payload) else {
                // An unparsable payload is an empty block for everyone.
                self.apply(self.chain.timestamp_ms, Vec::new());
                continue;
            };
            self.apply(p.timestamp_ms.max(self.chain.timestamp_ms), p.txs);
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
        self.replica.start(now_ms)
    }

    pub fn on_tick(&mut self, now_ms: u64) -> Vec<Out> {
        self.refill(now_ms);
        let out = self.replica.on_tick(now_ms);
        self.apply_commits();
        out
    }

    pub fn on_message(&mut self, from: NodeId, msg: Message, now_ms: u64) -> Vec<Out> {
        let out = self.replica.on_message(from, msg, now_ms);
        self.apply_commits();
        out
    }
}
