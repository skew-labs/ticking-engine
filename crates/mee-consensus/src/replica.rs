//! One validator's consensus state machine (sans-IO).
//!
//! Protocol (HotStuff family, 2-chain commit as in Jolteon / DiemBFT v4):
//! - Round `r` has leader `r mod n`. The leader proposes a block carrying the
//!   highest QC it knows (and a TC if the previous round timed out).
//! - Vote rule: vote once per round, only for the current round, and only if
//!   the block extends a QC from round `r-1`, or a TC for `r-1` with a QC at
//!   least as high as the TC's highest QC. Votes go to the next leader.
//! - 2f+1 votes form a QC; seeing a QC for round `r` moves everyone to `r+1`.
//! - Commit rule: a block is final once its child in the very next round is
//!   certified.
//! - Pacemaker: on timeout a replica stops voting in the round and broadcasts
//!   its highest QC; 2f+1 timeouts form a TC that moves everyone on. f+1
//!   timeouts for a round make a replica join (so slow timers can't stall).
//!
//! - Leader reputation: once enough blocks are committed, leaders rotate only
//!   among validators that authored or voted in the last `REPUTATION_WINDOW`
//!   committed blocks (looking only at blocks at least `REPUTATION_LAG`
//!   rounds old, so honest replicas agree). A crashed validator stops being
//!   chosen, so its rounds no longer time out. If too few are active, it
//!   falls back to plain round robin.
//!
//! All collections that are iterated are vectors; hash maps are lookup only.

use std::collections::VecDeque;

use ed25519_dalek::SigningKey;
use rustc_hash::FxHashMap;

use crate::types::*;

/// QC validity under a validator set: the genesis QC (round 0, no votes),
/// or 2f+1 distinct valid votes.
pub fn qc_is_valid(validators: &[Pubkey], genesis: &Hash, qc: &Qc) -> bool {
    if qc.round == 0 {
        return qc.block == *genesis && qc.votes.is_empty();
    }
    let msg = vote_message(&qc.block, qc.round);
    let mut seen = vec![false; validators.len()];
    let mut n = 0;
    for (v, sig) in &qc.votes {
        let i = *v as usize;
        if i < seen.len() && !seen[i] && verify(&validators[i], &msg, sig) {
            seen[i] = true;
            n += 1;
        }
    }
    n >= quorum(validators.len())
}

/// How a synced chain segment proves its last block is committed.
#[derive(Clone, Debug, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum CommitProof {
    /// The next two committed blocks: the first is from the very next round
    /// and the second certifies it (the 2-chain rule).
    Children(Block, Block),
    /// Void blocks after the segment (an epoch's tail past its boundary),
    /// then a block certified by `qc` from the round right after the last of
    /// them.
    Witness { tail: Vec<Block>, certified: Block, qc: Qc },
}

/// Checks that `blocks` extend `prev` as a chain of certified blocks and
/// that `proof` shows the last one committed, all under `validators`.
pub fn verify_committed_segment(
    validators: &[Pubkey],
    genesis: &Hash,
    prev: &Hash,
    blocks: &[Block],
    proof: &CommitProof,
) -> bool {
    let mut prev = *prev;
    let mut last_round = None;
    let check = |b: &Block, prev: &mut Hash| -> bool {
        let ok = b.qc.block == *prev && qc_is_valid(validators, genesis, &b.qc);
        *prev = b.hash();
        ok
    };
    for b in blocks {
        if !check(b, &mut prev) {
            return false;
        }
        last_round = Some(b.round);
    }
    match proof {
        CommitProof::Children(c1, c2) => {
            let Some(r) = last_round else { return false };
            c1.round == r + 1 && check(c1, &mut prev) && check(c2, &mut prev)
        }
        CommitProof::Witness { tail, certified, qc } => {
            let mut r = last_round;
            for b in tail {
                if !check(b, &mut prev) {
                    return false;
                }
                r = Some(b.round);
            }
            let Some(r) = r else { return false };
            certified.round == r + 1
                && check(certified, &mut prev)
                && qc.block == certified.hash()
                && qc_is_valid(validators, genesis, qc)
        }
    }
}

pub const REPUTATION_WINDOW: usize = 10;
pub const REPUTATION_LAG: u64 = 3;

#[derive(Copy, Clone, Debug)]
pub struct Config {
    pub base_timeout_ms: u64,
    pub max_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config { base_timeout_ms: 1_000, max_timeout_ms: 30_000 }
    }
}

pub struct Replica {
    pub id: NodeId,
    key: SigningKey,
    validators: Vec<Pubkey>,
    cfg: Config,
    genesis: Hash,
    blocks: FxHashMap<Hash, Block>,
    pub round: Round,
    pub last_voted_round: Round,
    pub high_qc: Qc,
    proposed_round: Round,
    timeout_sent_at: Option<(Round, u64)>,
    deadline: u64,
    consecutive_timeouts: u32,
    votes: FxHashMap<(Hash, Round), Vec<(NodeId, Sig)>>,
    timeouts: FxHashMap<Round, Vec<Timeout>>,
    pub committed_round: Round,
    last_committed: Hash,
    commits: Vec<Block>,
    /// (round, author | QC voters) of committed blocks, in commit order.
    history: VecDeque<(Round, u64)>,
    /// An ancestor we need before we can commit; re-requested every tick.
    missing: Option<Hash>,
    witness: Option<(Block, Qc)>,
    mempool: VecDeque<Vec<u8>>,
    now: u64,
}

impl Replica {
    pub fn new(id: NodeId, key: SigningKey, validators: Vec<Pubkey>, genesis: Hash, cfg: Config) -> Replica {
        Replica {
            id,
            key,
            validators,
            cfg,
            genesis,
            blocks: FxHashMap::default(),
            round: 0,
            last_voted_round: 0,
            high_qc: Qc { block: genesis, round: 0, votes: Vec::new() },
            proposed_round: 0,
            timeout_sent_at: None,
            deadline: 0,
            consecutive_timeouts: 0,
            votes: FxHashMap::default(),
            timeouts: FxHashMap::default(),
            committed_round: 0,
            last_committed: genesis,
            commits: Vec::new(),
            history: VecDeque::new(),
            missing: None,
            witness: None,
            mempool: VecDeque::new(),
            now: 0,
        }
    }

    pub fn safety_state(&self) -> SafetyState {
        SafetyState { round: self.round, last_voted_round: self.last_voted_round, high_qc: self.high_qc.clone() }
    }

    /// Rebuilds a replica after a restart from its persisted safety state
    /// and the committed blocks it had logged (in order). It will not vote in
    /// any round up to `last_voted_round` again.
    pub fn restore(&mut self, safety: SafetyState, committed: &[Block], now: u64) -> Vec<Out> {
        self.now = now;
        for b in committed {
            let h = b.hash();
            self.history.push_back((b.round, b.voter_mask() | (1u64 << (b.author as u32 % 64))));
            self.committed_round = b.round;
            self.last_committed = h;
            self.blocks.insert(h, b.clone());
        }
        while self.history.len() > 4 * REPUTATION_WINDOW {
            self.history.pop_front();
        }
        self.last_voted_round = safety.last_voted_round;
        self.proposed_round = safety.last_voted_round;
        if safety.high_qc.round > self.high_qc.round {
            self.high_qc = safety.high_qc;
        }
        let mut out = Vec::new();
        let r = safety.round.max(self.high_qc.round + 1).max(1);
        self.round = r - 1;
        self.enter_round(r, &mut out);
        out
    }

    /// A replica that follows commits (from proposals and their QCs) without
    /// voting, proposing or timing out: full nodes and validators-in-waiting.
    pub fn observer(validators: Vec<Pubkey>, genesis: Hash, cfg: Config) -> Replica {
        // Any key works: an observer never signs.
        Replica::new(NodeId::MAX, SigningKey::from_bytes(&[0; 32]), validators, genesis, cfg)
    }

    pub fn is_validator(&self) -> bool {
        (self.id as usize) < self.validators.len()
    }

    pub fn leader(&self, r: Round) -> NodeId {
        let n = self.validators.len();
        let cutoff = r.saturating_sub(REPUTATION_LAG);
        let mut active = 0u64;
        let mut seen = 0;
        for &(round, mask) in self.history.iter().rev() {
            if round > cutoff {
                continue;
            }
            active |= mask;
            seen += 1;
            if seen == REPUTATION_WINDOW {
                break;
            }
        }
        if seen == REPUTATION_WINDOW {
            let ids: Vec<NodeId> = (0..n as NodeId).filter(|&v| active & (1u64 << (v as u32 % 64)) != 0).collect();
            if ids.len() >= quorum(n) {
                return ids[(r % ids.len() as u64) as usize];
            }
        }
        (r % n as u64) as NodeId
    }

    fn quorum(&self) -> usize {
        quorum(self.validators.len())
    }

    /// Queues a payload for the next block this replica proposes.
    pub fn submit(&mut self, payload: Vec<u8>) {
        self.mempool.push_back(payload);
    }

    /// Payloads waiting to be proposed.
    pub fn queued_payloads(&self) -> usize {
        self.mempool.len()
    }

    /// Finalized blocks since the last call, in chain order.
    pub fn drain_commits(&mut self) -> Vec<Block> {
        std::mem::take(&mut self.commits)
    }

    pub fn start(&mut self, now: u64) -> Vec<Out> {
        self.now = now;
        let mut out = Vec::new();
        self.enter_round(1, &mut out);
        out
    }

    pub fn on_tick(&mut self, now: u64) -> Vec<Out> {
        self.now = now;
        let mut out = Vec::new();
        if let Some(h) = self.missing {
            out.push(Out::Broadcast(Message::Fetch(h)));
        }
        if now >= self.deadline {
            self.local_timeout(&mut out);
        }
        out
    }

    pub fn on_message(&mut self, from: NodeId, msg: Message, now: u64) -> Vec<Out> {
        self.now = now;
        let mut out = Vec::new();
        self.handle(from, msg, &mut out);
        out
    }

    fn handle(&mut self, from: NodeId, msg: Message, out: &mut Vec<Out>) {
        match msg {
            Message::Propose { block, tc } => self.on_proposal(from, block, tc, out),
            Message::Vote(v) => self.on_vote(v, out),
            Message::Timeout(t) => self.on_timeout(t, out),
            Message::Fetch(h) => {
                if let Some(b) = self.blocks.get(&h) {
                    out.push(Out::Send(from, Message::Deliver(b.clone())));
                }
            }
            Message::Deliver(block) => {
                if self.valid_block(&block) && !self.blocks.contains_key(&block.hash()) {
                    self.insert_block(block, out);
                }
            }
        }
    }

    // --- verification --------------------------------------------------------

    fn valid_qc(&self, qc: &Qc) -> bool {
        qc_is_valid(&self.validators, &self.genesis, qc)
    }

    /// The block certified by the QC that made the latest commit: proof that
    /// everything up to `last_committed` is final (used to serve state sync).
    pub fn commit_witness(&self) -> Option<&(Block, Qc)> {
        self.witness.as_ref()
    }

    fn valid_tc(&self, tc: &Tc) -> bool {
        let mut seen = vec![false; self.validators.len()];
        let mut n = 0;
        let mut max_hqc = 0;
        for (v, hqr, sig) in &tc.timeouts {
            let i = *v as usize;
            if i < seen.len() && !seen[i] && verify(&self.validators[i], &timeout_message(tc.round, *hqr), sig) {
                seen[i] = true;
                n += 1;
                max_hqc = max_hqc.max(*hqr);
            }
        }
        n >= self.quorum() && tc.high_qc.round == max_hqc && self.valid_qc(&tc.high_qc)
    }

    fn valid_block(&self, b: &Block) -> bool {
        b.author == self.leader(b.round)
            && b.qc.round < b.round
            && verify(&self.validators[b.author as usize], &proposal_message(&b.hash()), &b.signature)
            && self.valid_qc(&b.qc)
    }

    fn known(&self, h: &Hash) -> bool {
        *h == self.genesis || self.blocks.contains_key(h)
    }

    fn block_round(&self, h: &Hash) -> Option<Round> {
        if *h == self.genesis { Some(0) } else { self.blocks.get(h).map(|b| b.round) }
    }

    // --- core ----------------------------------------------------------------

    fn on_proposal(&mut self, _from: NodeId, block: Block, tc: Option<Tc>, out: &mut Vec<Out>) {
        if !self.valid_block(&block) {
            return;
        }
        if let Some(t) = &tc
            && (t.round + 1 != block.round || !self.valid_tc(t))
        {
            return;
        }
        // The block is stored even if its parent is unknown: voting only
        // needs the QC. Missing ancestors are fetched when committing.
        let qc = block.qc.clone();
        let hash = block.hash();
        let round = block.round;
        self.insert_block(block, out);
        self.process_qc(qc.clone(), out);
        if let Some(t) = &tc {
            self.process_tc(t.clone(), out);
        }

        let extends_qc = qc.round + 1 == round;
        let extends_tc = tc.as_ref().is_some_and(|t| t.round + 1 == round && qc.round >= t.high_qc.round);
        if self.is_validator() && round == self.round && round > self.last_voted_round && (extends_qc || extends_tc) {
            self.last_voted_round = round;
            out.push(Out::Persist(self.safety_state()));
            let vote =
                Vote { block: hash, round, voter: self.id, signature: sign(&self.key, &vote_message(&hash, round)) };
            let next = self.leader(round + 1);
            if next == self.id {
                self.on_vote(vote, out);
            } else {
                out.push(Out::Send(next, Message::Vote(vote)));
            }
        }
    }

    fn insert_block(&mut self, block: Block, out: &mut Vec<Out>) {
        let h = block.hash();
        self.blocks.insert(h, block);
        if self.missing == Some(h) {
            self.missing = None;
        }
        let hqc = self.high_qc.clone();
        self.try_commit(&hqc, out);
    }

    fn process_qc(&mut self, qc: Qc, out: &mut Vec<Out>) {
        if qc.round > self.high_qc.round {
            self.high_qc = qc.clone();
        }
        self.try_commit(&qc, out);
        if qc.round + 1 > self.round {
            self.consecutive_timeouts = 0;
            self.enter_round(qc.round + 1, out);
        }
    }

    fn process_tc(&mut self, tc: Tc, out: &mut Vec<Out>) {
        if tc.high_qc.round > self.high_qc.round {
            self.high_qc = tc.high_qc.clone();
        }
        if tc.round + 1 > self.round {
            self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
            self.enter_round(tc.round + 1, out);
            if self.leader(self.round) == self.id {
                self.propose(Some(tc), out);
            }
        }
    }

    fn enter_round(&mut self, r: Round, out: &mut Vec<Out>) {
        if r <= self.round {
            return;
        }
        self.round = r;
        let backoff = self.cfg.base_timeout_ms.saturating_mul(1u64 << self.consecutive_timeouts.min(16));
        self.deadline = self.now + backoff.min(self.cfg.max_timeout_ms);
        self.votes.retain(|(_, round), _| *round + 2 >= r);
        self.timeouts.retain(|round, _| *round + 2 >= r);
        if self.leader(r) == self.id && self.high_qc.round + 1 == r {
            self.propose(None, out);
        }
    }

    fn propose(&mut self, tc: Option<Tc>, out: &mut Vec<Out>) {
        if !self.is_validator() || self.proposed_round >= self.round {
            return;
        }
        self.proposed_round = self.round;
        let payload = self.mempool.pop_front().unwrap_or_default();
        let mut block =
            Block { round: self.round, author: self.id, qc: self.high_qc.clone(), payload, signature: [0; 64] };
        block.signature = sign(&self.key, &proposal_message(&block.hash()));
        out.push(Out::Broadcast(Message::Propose { block: block.clone(), tc: tc.clone() }));
        self.on_proposal(self.id, block, tc, out);
    }

    fn on_vote(&mut self, v: Vote, out: &mut Vec<Out>) {
        let i = v.voter as usize;
        if i >= self.validators.len()
            || !self.is_validator()
            || self.leader(v.round + 1) != self.id
            || v.round + 1 < self.round
            || !verify(&self.validators[i], &vote_message(&v.block, v.round), &v.signature)
        {
            return;
        }
        let q = self.quorum();
        let entry = self.votes.entry((v.block, v.round)).or_default();
        if entry.iter().any(|(who, _)| *who == v.voter) {
            return;
        }
        entry.push((v.voter, v.signature));
        if entry.len() == q {
            let qc = Qc { block: v.block, round: v.round, votes: entry.clone() };
            if !self.known(&v.block) {
                out.push(Out::Send(v.voter, Message::Fetch(v.block)));
            }
            self.process_qc(qc, out);
        }
    }

    fn local_timeout(&mut self, out: &mut Vec<Out>) {
        if !self.is_validator() {
            return;
        }
        let r = self.round;
        // Stop voting in this round, then (re)broadcast the timeout until the
        // round moves on, so dropped timeouts are retried.
        if r > self.last_voted_round {
            self.last_voted_round = r;
            out.push(Out::Persist(self.safety_state()));
        }
        let t = Timeout {
            round: r,
            high_qc: self.high_qc.clone(),
            voter: self.id,
            signature: sign(&self.key, &timeout_message(r, self.high_qc.round)),
        };
        self.timeout_sent_at = Some((r, self.now));
        let backoff = self.cfg.base_timeout_ms.saturating_mul(1u64 << self.consecutive_timeouts.min(16));
        self.deadline = self.now + backoff.min(self.cfg.max_timeout_ms);
        out.push(Out::Broadcast(Message::Timeout(t.clone())));
        self.on_timeout(t, out);
    }

    fn on_timeout(&mut self, t: Timeout, out: &mut Vec<Out>) {
        let i = t.voter as usize;
        if i >= self.validators.len()
            || !verify(&self.validators[i], &timeout_message(t.round, t.high_qc.round), &t.signature)
            || !self.valid_qc(&t.high_qc)
        {
            return;
        }
        // A timeout's QC can still help a lagging replica catch up.
        self.process_qc(t.high_qc.clone(), out);
        if t.round < self.round {
            return;
        }
        let round = t.round;
        let entry = self.timeouts.entry(round).or_default();
        if entry.iter().any(|x| x.voter == t.voter) {
            return;
        }
        entry.push(t);
        let count = entry.len();
        let f = (self.validators.len() - 1) / 3;
        // Join a timeout that f+1 replicas started.
        let joined = matches!(self.timeout_sent_at, Some((r, _)) if r >= round);
        if count == f + 1 && round == self.round && !joined {
            self.local_timeout(out);
            return;
        }
        if count == self.quorum() && round >= self.round {
            let entry = &self.timeouts[&round];
            let high_qc = entry.iter().map(|x| &x.high_qc).max_by_key(|q| q.round).expect("non-empty").clone();
            let tc = Tc {
                round,
                timeouts: entry.iter().map(|x| (x.voter, x.high_qc.round, x.signature)).collect(),
                high_qc,
            };
            self.process_tc(tc, out);
        }
    }

    /// 2-chain rule: if `qc` certifies B and B's parent P is from the round
    /// right before B, P (and everything before it) is final.
    fn try_commit(&mut self, qc: &Qc, out: &mut Vec<Out>) {
        let Some(b) = self.blocks.get(&qc.block) else { return };
        let parent = b.parent();
        let Some(parent_round) = self.block_round(&parent) else { return };
        if parent_round + 1 != b.round || parent_round <= self.committed_round {
            return;
        }
        // Walk back to the last committed block.
        let mut chain = Vec::new();
        let mut cur = parent;
        while cur != self.last_committed {
            let Some(blk) = self.blocks.get(&cur) else {
                if self.missing != Some(cur) {
                    self.missing = Some(cur);
                    out.push(Out::Broadcast(Message::Fetch(cur)));
                }
                return;
            };
            if blk.round <= self.committed_round {
                // Would fork below the committed prefix: never happens with
                // < n/3 faults; refuse rather than commit a conflict.
                return;
            }
            chain.push(blk.clone());
            cur = blk.parent();
        }
        chain.reverse();
        self.witness = Some((b.clone(), qc.clone()));
        self.committed_round = parent_round;
        self.last_committed = parent;
        for blk in &chain {
            self.history.push_back((blk.round, blk.voter_mask() | (1u64 << (blk.author as u32 % 64))));
        }
        while self.history.len() > 4 * REPUTATION_WINDOW {
            self.history.pop_front();
        }
        self.commits.extend(chain);
    }
}
