//! Deterministic network simulator: random delays, drops, crashes, a
//! temporary partition and an equivocating leader. Same seed, same run.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use ed25519_dalek::SigningKey;

use crate::replica::{Config, Replica};
use crate::types::*;

#[derive(Clone, Debug)]
pub struct SimConfig {
    pub n: usize,
    pub seed: u64,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
    /// Chance (per mille) that any message is lost.
    pub drop_per_mille: u64,
    pub duration_ms: u64,
    pub tick_ms: u64,
    /// Nodes that never start.
    pub crashed: Vec<NodeId>,
    /// A node that, when leading, sends conflicting proposals to the two
    /// halves of the network.
    pub equivocator: Option<NodeId>,
    /// (from_ms, to_ms, group): messages between the group and the rest are
    /// held back until `to_ms`.
    pub partition: Option<(u64, u64, Vec<NodeId>)>,
    pub consensus: Config,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            n: 4,
            seed: 1,
            min_delay_ms: 5,
            max_delay_ms: 50,
            drop_per_mille: 0,
            duration_ms: 20_000,
            tick_ms: 10,
            crashed: Vec::new(),
            equivocator: None,
            partition: None,
            consensus: Config { base_timeout_ms: 300, max_timeout_ms: 5_000 },
        }
    }
}

#[derive(Debug)]
pub struct SimResult {
    /// Committed block hashes per node, in order.
    pub commits: Vec<Vec<Hash>>,
    /// Committed payloads per node, in order.
    pub payloads: Vec<Vec<Vec<u8>>>,
    pub rounds: Vec<Round>,
    pub messages: u64,
}

impl SimResult {
    /// Every pair of nodes agrees on the common prefix of their commits.
    pub fn safe(&self) -> bool {
        for a in &self.commits {
            for b in &self.commits {
                let n = a.len().min(b.len());
                if a[..n] != b[..n] {
                    return false;
                }
            }
        }
        true
    }

    pub fn min_commits(&self, nodes: impl Iterator<Item = usize>) -> usize {
        nodes.map(|i| self.commits[i].len()).min().unwrap_or(0)
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

enum Ev {
    Deliver { to: NodeId, from: NodeId, msg: Box<Message> },
    Tick(NodeId),
}

type Queue = BinaryHeap<Reverse<(u64, u64)>>;
type Slab = Vec<Option<Ev>>;

pub fn key_for(seed: u64, i: usize) -> SigningKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&seed.to_le_bytes());
    b[8..16].copy_from_slice(&(i as u64).to_le_bytes());
    b[31] = 0x4d;
    SigningKey::from_bytes(&b)
}

pub fn run(cfg: &SimConfig) -> SimResult {
    let keys: Vec<SigningKey> = (0..cfg.n).map(|i| key_for(cfg.seed, i)).collect();
    let validators: Vec<Pubkey> = keys.iter().map(|k| k.verifying_key().to_bytes()).collect();
    let genesis = sha256(&[b"MEE/genesis-sim", &cfg.seed.to_le_bytes()]);
    let mut nodes: Vec<Replica> = (0..cfg.n)
        .map(|i| Replica::new(i as NodeId, keys[i].clone(), validators.clone(), genesis, cfg.consensus))
        .collect();
    let alive = |i: usize| !cfg.crashed.contains(&(i as NodeId));
    let mut rng = Rng(cfg.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);

    let mut slab: Slab = Vec::new();
    let mut heap: Queue = BinaryHeap::new();
    let mut seq = 0u64;
    let mut push = |heap: &mut Queue, slab: &mut Slab, at: u64, ev: Ev| {
        slab.push(Some(ev));
        heap.push(Reverse((at, seq)));
        seq += 1;
        debug_assert_eq!(slab.len() as u64, seq);
    };
    let mut messages = 0u64;
    let mut payload_counter = vec![0u64; cfg.n];

    let send = |now: u64,
                from: NodeId,
                outs: Vec<Out>,
                rng: &mut Rng,
                heap: &mut Queue,
                slab: &mut Slab,
                messages: &mut u64,
                push: &mut dyn FnMut(&mut Queue, &mut Slab, u64, Ev)| {
        for o in outs {
            let targets: Vec<(NodeId, Message)> = match o {
                Out::Persist(_) => continue,
                Out::Send(to, m) => vec![(to, m)],
                Out::Broadcast(m) => {
                    let alt = match (&m, cfg.equivocator) {
                        (Message::Propose { block, tc }, Some(e)) if e == from => {
                            let mut b2 = block.clone();
                            b2.payload = [block.payload.clone(), b"-equivocated".to_vec()].concat();
                            b2.signature = sign(&keys[from as usize], &proposal_message(&b2.hash()));
                            Some(Message::Propose { block: b2, tc: tc.clone() })
                        }
                        _ => None,
                    };
                    (0..cfg.n as NodeId)
                        .filter(|&to| to != from)
                        .map(|to| match &alt {
                            Some(a) if (to as usize) >= cfg.n / 2 => (to, a.clone()),
                            _ => (to, m.clone()),
                        })
                        .collect()
                }
            };
            for (to, m) in targets {
                *messages += 1;
                if rng.next() % 1_000 < cfg.drop_per_mille {
                    continue;
                }
                let mut at = now + rng.range(cfg.min_delay_ms, cfg.max_delay_ms);
                if let Some((p_from, p_to, group)) = &cfg.partition {
                    let split = group.contains(&from) != group.contains(&to);
                    if split && at >= *p_from && now < *p_to {
                        at = at.max(*p_to + rng.range(cfg.min_delay_ms, cfg.max_delay_ms));
                    }
                }
                push(heap, slab, at, Ev::Deliver { to, from, msg: Box::new(m) });
            }
        }
    };

    for (i, node) in nodes.iter_mut().enumerate() {
        if alive(i) {
            node.submit(format!("n{i}-0").into_bytes());
            let outs = node.start(0);
            send(0, i as NodeId, outs, &mut rng, &mut heap, &mut slab, &mut messages, &mut push);
            push(&mut heap, &mut slab, cfg.tick_ms, Ev::Tick(i as NodeId));
        }
    }

    while let Some(Reverse((at, id))) = heap.pop() {
        if at > cfg.duration_ms {
            break;
        }
        let ev = slab[id as usize].take().expect("event once");
        match ev {
            Ev::Tick(i) => {
                let n = &mut nodes[i as usize];
                // Keep the mempool topped up so blocks carry distinct payloads.
                payload_counter[i as usize] += 1;
                n.submit(format!("n{i}-{}", payload_counter[i as usize]).into_bytes());
                let outs = n.on_tick(at);
                send(at, i, outs, &mut rng, &mut heap, &mut slab, &mut messages, &mut push);
                push(&mut heap, &mut slab, at + cfg.tick_ms, Ev::Tick(i));
            }
            Ev::Deliver { to, from, msg } => {
                if !alive(to as usize) {
                    continue;
                }
                let outs = nodes[to as usize].on_message(from, *msg, at);
                send(at, to, outs, &mut rng, &mut heap, &mut slab, &mut messages, &mut push);
            }
        }
    }

    let mut commits = Vec::new();
    let mut payloads = Vec::new();
    for n in nodes.iter_mut() {
        let c = n.drain_commits();
        commits.push(c.iter().map(|b| b.hash()).collect());
        payloads.push(c.into_iter().map(|b| b.payload).collect());
    }
    SimResult { commits, payloads, rounds: nodes.iter().map(|n| n.round).collect(), messages }
}
