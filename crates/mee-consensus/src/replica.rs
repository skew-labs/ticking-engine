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
//! All collections that are iterated are vectors; hash maps are lookup only.

use std::collections::VecDeque;

use ed25519_dalek::SigningKey;
use rustc_hash::FxHashMap;

use crate::types::*;

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
    /// An ancestor we need before we can commit; re-requested every tick.
    missing: Option<Hash>,
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
            missing: None,
            mempool: VecDeque::new(),
            now: 0,
        }
    }

    pub fn leader(&self, r: Round) -> NodeId {
        (r % self.validators.len() as u64) as NodeId
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
        if qc.round == 0 {
            return qc.block == self.genesis && qc.votes.is_empty();
        }
        let msg = vote_message(&qc.block, qc.round);
        let mut seen = vec![false; self.validators.len()];
        let mut n = 0;
        for (v, sig) in &qc.votes {
            let i = *v as usize;
            if i < seen.len() && !seen[i] && verify(&self.validators[i], &msg, sig) {
                seen[i] = true;
                n += 1;
            }
        }
        n >= self.quorum()
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
        if round == self.round && round > self.last_voted_round && (extends_qc || extends_tc) {
            self.last_voted_round = round;
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
        if self.proposed_round >= self.round {
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
        let r = self.round;
        // Stop voting in this round, then (re)broadcast the timeout until the
        // round moves on, so dropped timeouts are retried.
        self.last_voted_round = self.last_voted_round.max(r);
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
        self.committed_round = parent_round;
        self.last_committed = parent;
        self.commits.extend(chain);
    }
}
