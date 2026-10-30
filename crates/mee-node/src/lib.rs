//! A validator (or observer) node: consensus orders opaque payloads, the
//! node turns each committed payload into a chain block and applies it.
//!
//! - Payload = borsh `(timestamp_ms, Vec<Tx>)` chosen by the proposer. The
//!   block timestamp is clamped to be non-decreasing and the height is the
//!   commit index, so every honest node derives the same chain blocks.
//! - Epochs: when a block ends an epoch with a new validator set, every node
//!   stops the old consensus instance right there (later old-epoch commits
//!   are ignored by everyone) and starts a fresh one for the new set, with a
//!   genesis derived from the epoch and the app hash. Nodes outside the set
//!   run it as observers and keep following the chain.
//! - Network messages are `Envelope`s carrying the epoch and the sender's
//!   key; node indices only exist inside one epoch's consensus instance.

pub mod storage;

use std::path::Path;

use borsh::{BorshDeserialize, BorshSerialize};
use ed25519_dalek::SigningKey;
use mee_chain::{Block, BlockOutcome, Chain, Genesis, Tx, crypto};
use mee_consensus::{CommitProof, Config, Hash, Message, NodeId, Out, Pubkey, Qc, Replica, verify_committed_segment};

use crate::storage::Storage;

#[derive(BorshSerialize, BorshDeserialize)]
struct Payload {
    timestamp_ms: i64,
    txs: Vec<Tx>,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Envelope {
    pub epoch: u64,
    pub from: Pubkey,
    pub msg: Message,
}

/// Ask a peer for committed blocks starting at `from_height`.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SyncRequest {
    pub from_height: u64,
}

/// A segment of one epoch's committed blocks with proof that the last one is
/// committed. The receiver checks it against validator signatures alone, so
/// any peer can serve it.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SyncResponse {
    pub epoch: u64,
    pub from_height: u64,
    pub blocks: Vec<mee_consensus::Block>,
    pub proof: CommitProof,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    WrongEpoch,
    WrongHeight,
    BadProof,
}

/// Blocks per sync response (soft limit).
pub const SYNC_BATCH: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetOut {
    /// To every peer (validators and observers).
    Broadcast(Envelope),
    Send(Pubkey, Envelope),
}

pub struct Node {
    key: SigningKey,
    pub me: Pubkey,
    cfg: Config,
    pub epoch: u64,
    pub replica: Replica,
    pub chain: Chain,
    mempool: Vec<Tx>,
    /// Messages for a future epoch, replayed once we switch.
    future: Vec<Envelope>,
    /// App hash after each applied height (index = height - 1).
    pub app_hashes: Vec<Hash>,
    pub outcomes: Vec<BlockOutcome>,
    max_block_txs: usize,
    storage: Option<Storage>,
    /// Every applied consensus block with its epoch (index = height - 1).
    committed: Vec<(u64, mee_consensus::Block)>,
    /// Ended epochs: blocks committed past the boundary (void) and the
    /// certified block + QC that proved the last commit.
    epoch_tails: std::collections::BTreeMap<u64, (Vec<mee_consensus::Block>, mee_consensus::Block, Qc)>,
    /// Consensus genesis of the current epoch.
    epoch_genesis: Hash,
}

fn consensus_genesis(chain: &Chain) -> Hash {
    if chain.epoch == 0 {
        crypto::sha256(&[b"MEE/consensus-genesis", &chain.chain_id.to_le_bytes()])
    } else {
        crypto::sha256(&[
            b"MEE/epoch-genesis",
            &chain.chain_id.to_le_bytes(),
            &chain.epoch.to_le_bytes(),
            &chain.app_hash,
        ])
    }
}

fn make_replica(key: &SigningKey, chain: &Chain, cfg: Config) -> Replica {
    let me = crypto::pubkey(key);
    let genesis = consensus_genesis(chain);
    match chain.validators.iter().position(|v| *v == me) {
        Some(i) => Replica::new(i as NodeId, key.clone(), chain.validators.clone(), genesis, cfg),
        None => Replica::observer(chain.validators.clone(), genesis, cfg),
    }
}

impl Node {
    pub fn new(key: SigningKey, genesis: Genesis, cfg: Config) -> Node {
        let chain = Chain::new(genesis);
        let replica = make_replica(&key, &chain, cfg);
        let epoch_genesis = consensus_genesis(&chain);
        Node {
            me: crypto::pubkey(&key),
            key,
            cfg,
            epoch: 0,
            replica,
            chain,
            mempool: Vec::new(),
            future: Vec::new(),
            app_hashes: Vec::new(),
            outcomes: Vec::new(),
            max_block_txs: 20_000,
            storage: None,
            committed: Vec::new(),
            epoch_tails: Default::default(),
            epoch_genesis,
        }
    }

    /// A node backed by `dir`. On a fresh directory this is a new node; on
    /// an existing one it replays the committed block log into the chain
    /// (crossing epochs exactly as it did live) and restores consensus for
    /// the current epoch from the persisted safety state.
    pub fn open(
        dir: &Path,
        key: SigningKey,
        genesis: Genesis,
        cfg: Config,
        now_ms: u64,
    ) -> std::io::Result<(Node, Vec<NetOut>)> {
        let (storage, rec) = Storage::open(dir)?;
        let mut node = Node::new(key, genesis, cfg);
        for (e, b) in &rec.blocks {
            node.committed.push((*e, b.clone()));
            node.apply_payload(&b.payload);
            if node.chain.epoch != node.epoch {
                node.epoch = node.chain.epoch;
                node.epoch_genesis = consensus_genesis(&node.chain);
            }
        }
        node.replica = make_replica(&node.key, &node.chain, cfg);
        let current: Vec<_> = rec.blocks.iter().filter(|(e, _)| *e == node.epoch).map(|(_, b)| b.clone()).collect();
        let outs = match rec.safety {
            Some((e, safety)) if e == node.epoch => node.replica.restore(safety, &current, now_ms),
            _ if !current.is_empty() => node.replica.restore(node.replica.safety_state(), &current, now_ms),
            _ => {
                node.refill(now_ms);
                node.replica.start(now_ms)
            }
        };
        node.storage = Some(storage);
        let outs = node.finish(outs, None);
        Ok((node, outs))
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

    /// Transactions waiting locally (for gossip).
    pub fn mempool(&self) -> &[Tx] {
        &self.mempool
    }

    fn refill(&mut self, now_ms: u64) {
        // Drop what can no longer execute, then hand the replica a fresh
        // batch if it has none.
        let chain = &self.chain;
        self.mempool.retain(|tx| match tx {
            Tx::User(_) => chain.check_tx(tx).is_ok(),
            Tx::System(mee_chain::SystemTx::BridgeDeposit { deposit_id, .. }) => *deposit_id >= chain.next_deposit_id,
            Tx::System(mee_chain::SystemTx::Governance { nonce, .. }) => *nonce >= chain.governance_nonce,
            Tx::System(mee_chain::SystemTx::ListLaunch { launch_id, .. }) => !chain.listings.contains_key(launch_id),
            Tx::System(mee_chain::SystemTx::LaunchOracle { solana_slot, .. }) => *solana_slot > chain.last_oracle_slot,
        });
        if self.replica.is_validator() && self.replica.queued_payloads() == 0 {
            let txs: Vec<Tx> = self.mempool.iter().take(self.max_block_txs).cloned().collect();
            let p = Payload { timestamp_ms: now_ms as i64, txs };
            self.replica.submit(borsh::to_vec(&p).expect("borsh"));
        }
    }

    /// Applies newly committed blocks; on an epoch change, switches
    /// consensus instances and returns the new instance's start output.
    fn apply_commits(&mut self, now_ms: u64) -> Vec<Out> {
        let mut void = Vec::new();
        for b in self.replica.drain_commits() {
            if self.chain.epoch != self.epoch {
                void.push(b); // old-epoch commits past the boundary are void for everyone
                continue;
            }
            self.commit_block(b);
        }
        if self.chain.epoch == self.epoch {
            return Vec::new();
        }
        if let Some((certified, qc)) = self.replica.commit_witness().cloned() {
            self.epoch_tails.insert(self.epoch, (void, certified, qc));
        }
        self.enter_epoch(now_ms)
    }

    fn commit_block(&mut self, b: mee_consensus::Block) {
        if let Some(st) = self.storage.as_mut() {
            st.append_block(self.epoch, &b).expect("append committed block");
        }
        self.apply_payload(&b.payload);
        self.committed.push((self.epoch, b));
    }

    /// Starts the consensus instance of the chain's (new) epoch and replays
    /// messages that arrived early for it.
    fn enter_epoch(&mut self, now_ms: u64) -> Vec<Out> {
        self.epoch = self.chain.epoch;
        self.epoch_genesis = consensus_genesis(&self.chain);
        self.replica = make_replica(&self.key, &self.chain, self.cfg);
        self.refill(now_ms);
        let mut out = self.replica.start(now_ms);
        let queued: Vec<Envelope> = std::mem::take(&mut self.future);
        for env in queued {
            if env.epoch == self.epoch {
                let from = self.index_of(&env.from);
                out.extend(self.replica.on_message(from, env.msg, now_ms));
            } else if env.epoch > self.epoch {
                self.future.push(env);
            }
        }
        out
    }

    fn apply_payload(&mut self, payload: &[u8]) {
        let (ts, txs) = match Payload::try_from_slice(payload) {
            Ok(p) => (p.timestamp_ms.max(self.chain.timestamp_ms), p.txs),
            // An unparsable payload is an empty block for everyone.
            Err(_) => (self.chain.timestamp_ms, Vec::new()),
        };
        let block = Block { height: self.chain.height + 1, timestamp_ms: ts, txs };
        let out = self.chain.apply_block(&block).expect("height and time are derived locally");
        self.app_hashes.push(out.app_hash);
        self.outcomes.push(out);
    }

    pub fn height(&self) -> u64 {
        self.chain.height
    }

    pub fn sync_request(&self) -> SyncRequest {
        SyncRequest { from_height: self.chain.height + 1 }
    }

    /// Serves committed blocks from `from_height` (within one epoch) with a
    /// commit proof, or `None` if this node can't prove them yet.
    pub fn serve_sync(&self, req: &SyncRequest) -> Option<SyncResponse> {
        let start = (req.from_height as usize).checked_sub(1)?;
        let (epoch, _) = self.committed.get(start)?;
        let epoch = *epoch;
        let end =
            self.committed[start..].iter().position(|(e, _)| *e != epoch).map_or(self.committed.len(), |p| start + p);
        let block = |i: usize| self.committed[i].1.clone();
        let witness = if epoch < self.epoch {
            self.epoch_tails.get(&epoch).map(|(tail, c, qc)| (tail.clone(), c.clone(), qc.clone()))
        } else {
            self.replica
                .commit_witness()
                .filter(|(c, _)| Some(c.parent()) == self.committed.last().map(|(_, b)| b.hash()))
                .map(|(c, qc)| (Vec::new(), c.clone(), qc.clone()))
        };
        // Prefer a cut near SYNC_BATCH proven by its next two blocks.
        let cut = (start..end.saturating_sub(2))
            .rev()
            .filter(|&k| k < start + SYNC_BATCH)
            .chain(start + SYNC_BATCH..end.saturating_sub(2))
            .find(|&k| self.committed[k + 1].1.round == self.committed[k].1.round + 1);
        let (last, proof) = match (cut, witness) {
            (_, Some((tail, certified, qc))) if end - start <= SYNC_BATCH => {
                (end - 1, CommitProof::Witness { tail, certified, qc })
            }
            (Some(k), _) => (k, CommitProof::Children(block(k + 1), block(k + 2))),
            (None, Some((tail, certified, qc))) => (end - 1, CommitProof::Witness { tail, certified, qc }),
            (None, None) => return None,
        };
        Some(SyncResponse { epoch, from_height: req.from_height, blocks: (start..=last).map(block).collect(), proof })
    }

    /// Verifies a sync segment against the current epoch's validators and
    /// applies it; consensus resumes from the new committed tip (or the next
    /// epoch, if the segment crossed into it).
    pub fn apply_sync(&mut self, resp: SyncResponse, now_ms: u64) -> Result<Vec<NetOut>, SyncError> {
        if resp.epoch != self.epoch {
            return Err(SyncError::WrongEpoch);
        }
        if resp.from_height != self.chain.height + 1 {
            return Err(SyncError::WrongHeight);
        }
        let prev = match self.committed.last() {
            Some((e, b)) if *e == self.epoch => b.hash(),
            _ => self.epoch_genesis,
        };
        if !verify_committed_segment(&self.chain.validators, &self.epoch_genesis, &prev, &resp.blocks, &resp.proof) {
            return Err(SyncError::BadProof);
        }
        for b in resp.blocks {
            if self.chain.epoch != self.epoch {
                break;
            }
            self.commit_block(b);
        }
        let outs = if self.chain.epoch != self.epoch {
            self.enter_epoch(now_ms)
        } else {
            let safety = self.replica.safety_state();
            let current: Vec<_> =
                self.committed.iter().filter(|(e, _)| *e == self.epoch).map(|(_, b)| b.clone()).collect();
            self.replica = make_replica(&self.key, &self.chain, self.cfg);
            self.replica.restore(safety, &current, now_ms)
        };
        Ok(self.finish(outs, None))
    }

    fn index_of(&self, key: &Pubkey) -> NodeId {
        self.chain.validators.iter().position(|v| v == key).map_or(NodeId::MAX, |i| i as NodeId)
    }

    /// Persists safety state (in output order, before anything after it is
    /// sent) and turns node indices into keys. `sender` answers replies to
    /// non-validators (index `NodeId::MAX`).
    fn finish(&mut self, outs: Vec<Out>, sender: Option<Pubkey>) -> Vec<NetOut> {
        let (epoch, validators) = (self.epoch, self.chain.validators.clone());
        self.finish_as(outs, sender, epoch, &validators)
    }

    /// `epoch`/`validators` are those of the consensus instance that
    /// produced `outs`: a leader that commits the epoch's last block switches
    /// right after, but its final old-epoch proposal must still go out as an
    /// old-epoch message so the others can commit that block too.
    fn finish_as(&mut self, outs: Vec<Out>, sender: Option<Pubkey>, epoch: u64, validators: &[Pubkey]) -> Vec<NetOut> {
        let mut net = Vec::with_capacity(outs.len());
        for o in outs {
            match o {
                Out::Persist(s) => {
                    if let Some(st) = self.storage.as_mut() {
                        st.save_safety(epoch, &s).expect("persist safety state before voting");
                    }
                }
                Out::Broadcast(msg) => net.push(NetOut::Broadcast(Envelope { epoch, from: self.me, msg })),
                Out::Send(to, msg) => {
                    let dest = validators.get(to as usize).copied().or(sender);
                    if let Some(d) = dest {
                        net.push(NetOut::Send(d, Envelope { epoch, from: self.me, msg }));
                    }
                }
            }
        }
        net
    }

    /// Runs `f` on the current consensus instance, applies commits (maybe
    /// switching epochs) and tags each output with the right epoch.
    fn drive(&mut self, now_ms: u64, sender: Option<Pubkey>, f: impl FnOnce(&mut Replica) -> Vec<Out>) -> Vec<NetOut> {
        let (epoch, validators) = (self.epoch, self.chain.validators.clone());
        let old = f(&mut self.replica);
        // Persist before applying commits: an epoch switch replaces the
        // safety file's epoch.
        let mut net = self.finish_as(old, sender, epoch, &validators);
        let new = self.apply_commits(now_ms);
        net.extend(self.finish(new, None));
        net
    }

    pub fn start(&mut self, now_ms: u64) -> Vec<NetOut> {
        self.refill(now_ms);
        let out = self.replica.start(now_ms);
        self.finish(out, None)
    }

    pub fn on_tick(&mut self, now_ms: u64) -> Vec<NetOut> {
        self.refill(now_ms);
        self.drive(now_ms, None, |r| r.on_tick(now_ms))
    }

    pub fn on_message(&mut self, env: Envelope, now_ms: u64) -> Vec<NetOut> {
        if env.epoch > self.epoch {
            if self.future.len() < 10_000 {
                self.future.push(env);
            }
            return Vec::new();
        }
        if env.epoch < self.epoch {
            return Vec::new();
        }
        let from = self.index_of(&env.from);
        self.drive(now_ms, Some(env.from), |r| r.on_message(from, env.msg, now_ms))
    }
}
