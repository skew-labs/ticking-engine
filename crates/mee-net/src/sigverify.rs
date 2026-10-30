//! Signature stage of the validator pipeline.
//!
//! Every transaction from the API or a peer passes through here before the
//! node task sees it: a dispatcher thread drains what has arrived (up to
//! `MAX_BATCH`), skips ids whose signature it already checked (gossip
//! delivers each transaction once per peer), and splits the remaining user
//! signatures across worker threads that batch-verify them (optionally
//! pinned one per core). Duplicates still reach the node, whose mempool
//! drops them; this stage only remembers verdicts, so a transaction the
//! node turned away for now (nonce too far ahead, full pool) can come back
//! later. The node
//! task then only runs the cheap stateful checks (`Node::submit_verified`),
//! so ingress scales with cores instead of being bound to the one thread
//! that owns the node.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use mee_chain::{SignedTx, Tx};
use mee_consensus::{Hash, Pubkey};
use tokio::sync::{mpsc, oneshot};

/// Transactions taken off the queue per round.
const MAX_BATCH: usize = 4_096;
/// Fewer user signatures than this are verified on the dispatcher itself.
const MIN_SPLIT: usize = 128;
/// Recently seen ids remembered per generation (two generations).
const SEEN_GENERATION: usize = 1 << 19;

pub enum Origin {
    /// From the API; the caller wants to know whether it was admitted.
    Api(oneshot::Sender<bool>),
    /// Gossip from this peer (not echoed back to it).
    Peer(Pubkey),
}

/// A transaction that passed this stage.
pub struct Checked {
    pub tx: Tx,
    pub id: Hash,
    /// The user signature is valid (always `false` for system transactions,
    /// which the node checks against the current validator set).
    pub sig_verified: bool,
    pub origin: Origin,
}

impl Origin {
    fn reject(self) {
        if let Origin::Api(reply) = self {
            let _ = reply.send(false);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Worker threads (0 = one per core, minus one for the node).
    pub threads: usize,
    /// Pin worker `i` to core `i + 1` (core 0 is left to the node and the
    /// network). Linux only; elsewhere this is ignored.
    pub pin_cores: bool,
}

/// Starts the stage. Checked batches go to `out`; the returned sender is
/// where transactions enter.
pub fn spawn(chain_id: u64, opts: Options, out: mpsc::Sender<Vec<Checked>>) -> std_mpsc::SyncSender<(Tx, Origin)> {
    let (tx_in, rx_in) = std_mpsc::sync_channel::<(Tx, Origin)>(1 << 16);
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let threads = if opts.threads > 0 { opts.threads } else { cores.saturating_sub(1).max(1) };
    let workers: Vec<Worker> = (0..threads).map(|i| Worker::spawn(chain_id, opts.pin_cores.then_some(i + 1))).collect();
    std::thread::Builder::new()
        .name("sigverify".into())
        .spawn(move || dispatch(chain_id, rx_in, &workers, &out))
        .expect("spawn sigverify dispatcher");
    tx_in
}

fn dispatch(chain_id: u64, rx: std_mpsc::Receiver<(Tx, Origin)>, workers: &[Worker], out: &mpsc::Sender<Vec<Checked>>) {
    let mut seen = Seen::default();
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }
        let (txs, origins): (Vec<Tx>, Vec<Origin>) = batch.into_iter().unzip();
        let ids = mee_node::tx_hashes(&txs);
        let mut items: Vec<(Tx, Hash, Origin)> =
            txs.into_iter().zip(ids).zip(origins).map(|((t, id), o)| (t, id, o)).collect();
        // Verdicts already known (repeats within the batch are verified once
        // and share the result).
        let mut ok: Vec<Option<bool>> = items.iter().map(|(_, id, _)| seen.get(id)).collect();
        let mut first: HashMap<Hash, usize> = HashMap::new();
        let mut users: Job = Vec::new();
        for (i, (tx, id, _)) in items.iter_mut().enumerate() {
            if let (None, Tx::User(t)) = (ok[i], tx)
                && *first.entry(*id).or_insert(i) == i
            {
                // Moved out for the workers and back afterwards: no copies.
                users.push((i, std::mem::replace(t, placeholder())));
            }
        }
        for (i, t, good) in verify(chain_id, users, workers) {
            items[i].0 = Tx::User(t);
            ok[i] = Some(good);
            seen.insert(items[i].1, good);
        }
        let mut checked = Vec::with_capacity(items.len());
        for (i, (tx, id, origin)) in items.into_iter().enumerate() {
            let verdict = ok[i].or_else(|| first.get(&id).and_then(|&j| ok[j]));
            match tx {
                Tx::User(_) if verdict != Some(true) => origin.reject(),
                tx => {
                    let sig_verified = matches!(tx, Tx::User(_));
                    checked.push(Checked { tx, id, sig_verified, origin });
                }
            }
        }
        if !checked.is_empty() && out.blocking_send(checked).is_err() {
            return;
        }
    }
}

fn placeholder() -> SignedTx {
    SignedTx {
        signer: [0; 32],
        nonce: 0,
        action: mee_chain::Action::Withdraw { asset: 0, amount: 0, destination: [0; 32] },
        signature: [0; 64],
    }
}

type Job = Vec<(usize, SignedTx)>;
type Done = Vec<(usize, SignedTx, bool)>;

fn verify(chain_id: u64, users: Job, workers: &[Worker]) -> Done {
    let check = |job: Job| -> Done {
        let refs: Vec<&SignedTx> = job.iter().map(|(_, t)| t).collect();
        let ok = mee_chain::verify_batch(chain_id, &refs);
        job.into_iter().zip(ok).map(|((i, t), ok)| (i, t, ok)).collect()
    };
    if users.len() < MIN_SPLIT || workers.len() < 2 {
        return check(users);
    }
    let per = users.len().div_ceil(workers.len());
    let (done_tx, done_rx) = std_mpsc::channel();
    let mut sent = 0;
    let mut rest = users;
    for w in workers {
        if rest.is_empty() {
            break;
        }
        let tail = rest.split_off(per.min(rest.len()));
        w.jobs.send((std::mem::replace(&mut rest, tail), done_tx.clone())).expect("sigverify worker");
        sent += 1;
    }
    (0..sent).flat_map(|_| done_rx.recv().expect("sigverify worker")).collect()
}

struct Worker {
    jobs: std_mpsc::Sender<(Job, std_mpsc::Sender<Done>)>,
}

impl Worker {
    fn spawn(chain_id: u64, core: Option<usize>) -> Worker {
        let (jobs, rx) = std_mpsc::channel::<(Job, std_mpsc::Sender<Done>)>();
        std::thread::Builder::new()
            .name("sigverify-worker".into())
            .spawn(move || {
                if let Some(c) = core {
                    pin_to_core(c);
                }
                while let Ok((job, done)) = rx.recv() {
                    let refs: Vec<&SignedTx> = job.iter().map(|(_, t)| t).collect();
                    let ok = mee_chain::verify_batch(chain_id, &refs);
                    let _ = done.send(job.into_iter().zip(ok).map(|((i, t), ok)| (i, t, ok)).collect());
                }
            })
            .expect("spawn sigverify worker");
        Worker { jobs }
    }
}

/// Pins the calling thread to one core (best effort).
#[cfg(target_os = "linux")]
pub fn pin_to_core(core: usize) {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    // SAFETY: `set` is a plain bitmask owned by this frame; the call only
    // reads it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core % cores, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn pin_to_core(_core: usize) {}

/// Signature verdicts of recent ids: two generations, the older dropped
/// when the newer fills.
#[derive(Default)]
struct Seen {
    cur: HashMap<Hash, bool>,
    old: HashMap<Hash, bool>,
}

impl Seen {
    fn get(&self, id: &Hash) -> Option<bool> {
        self.cur.get(id).or_else(|| self.old.get(id)).copied()
    }

    fn insert(&mut self, id: Hash, ok: bool) {
        if self.cur.len() >= SEEN_GENERATION {
            self.old = std::mem::take(&mut self.cur);
        }
        self.cur.insert(id, ok);
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;
    use mee_chain::{Action, SystemTx};

    use super::*;

    #[test]
    fn verdicts_repeats_and_system_transactions() {
        let chain_id = 9;
        let (out, mut rx) = mpsc::channel(64);
        let input = spawn(chain_id, Options { threads: 3, pin_cores: true }, out);
        let key = SigningKey::from_bytes(&[3; 32]);
        let user = |nonce| {
            Tx::User(SignedTx::sign(
                &key,
                chain_id,
                nonce,
                Action::Withdraw { asset: 0, amount: 1, destination: [0; 32] },
            ))
        };
        let mut bad = user(2);
        if let Tx::User(t) = &mut bad {
            t.signature[0] ^= 1;
        }
        let system = Tx::System(SystemTx::Governance {
            nonce: 0,
            action: mee_chain::Governance::Slash { dex: 0, bps: 1, freeze: false },
            attestations: vec![],
        });
        let peer = [1; 32];
        // Enough to be split across the workers, with a repeat inside the
        // batch, a bad signature and a system transaction.
        let mut sent = vec![user(1), user(1), bad.clone(), system];
        sent.extend((10..400).map(user));
        let (api, answer) = oneshot::channel();
        input.send((bad, Origin::Api(api))).unwrap();
        for tx in &sent {
            input.send((tx.clone(), Origin::Peer(peer))).unwrap();
        }
        let recv = |rx: &mut mpsc::Receiver<Vec<Checked>>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                match rx.try_recv() {
                    Ok(b) => return b,
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(1))
                    }
                    Err(e) => panic!("nothing from the stage: {e:?}"),
                }
            }
        };
        let mut got = Vec::new();
        while got.len() < sent.len() - 1 {
            got.extend(recv(&mut rx));
        }
        assert_eq!(answer.blocking_recv(), Ok(false), "bad signature rejected back to the API caller");
        assert!(got.iter().all(|c| c.id == mee_node::tx_hash(&c.tx)));
        let users: Vec<&Checked> = got.iter().filter(|c| matches!(c.tx, Tx::User(_))).collect();
        assert_eq!(users.len(), 2 + 390, "both copies of the repeat pass; the node's mempool dedups");
        assert!(users.iter().all(|c| c.sig_verified));
        let sys: Vec<&Checked> = got.iter().filter(|c| matches!(c.tx, Tx::System(_))).collect();
        assert_eq!(sys.len(), 1);
        assert!(!sys[0].sig_verified, "system transactions are left to the node");
        // A later copy is served from the verdict cache.
        input.send((user(1), Origin::Peer(peer))).unwrap();
        let again = recv(&mut rx);
        assert!(again.len() == 1 && again[0].sig_verified);
    }
}
