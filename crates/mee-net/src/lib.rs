//! Runs a `mee_node::Node` on a real network.
//!
//! - P2P: every node dials every configured peer and accepts inbound
//!   connections; each connection starts with a mutual key-proving
//!   handshake (`wire::handshake`), so a consensus envelope is only accepted
//!   from the key it claims to come from. Dialers reconnect with backoff;
//!   duplicate connections to one peer are harmless (sends use the newest).
//! - Gossip: a newly admitted transaction is forwarded to every peer except
//!   its source; the mempool dedups by hash, so floods die out.
//! - Sync: nodes announce their committed tip every second; a node that is
//!   more than `SYNC_LAG` blocks (or an epoch) behind a peer asks it for a
//!   verified segment (`Node::serve_sync` / `Node::apply_sync`).
//! - API: HTTP/JSON queries and a WebSocket stream of committed blocks
//!   (`api`).
//!
//! One task owns the node behind a mutex; network readers feed it through a
//! bounded channel and per-connection writer tasks drain bounded queues
//! (consensus tolerates the drops a full queue causes).

pub mod api;
pub mod observer;
pub mod wire;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use borsh::BorshDeserialize;
use ed25519_dalek::SigningKey;
use mee_chain::{Genesis, Tx, crypto};
use mee_consensus::{Config, Pubkey};
use mee_node::{NetOut, Node};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::wire::Wire;

/// Blocks behind a peer before asking it for a sync segment (consensus
/// itself fetches the last few missing blocks).
pub const SYNC_LAG: u64 = 8;
const TICK: Duration = Duration::from_millis(10);
const STATUS_EVERY: Duration = Duration::from_secs(1);
const SYNC_RETRY: Duration = Duration::from_millis(500);
const PEER_QUEUE: usize = 4096;
/// A sync target that leaves a request unanswered is skipped this long (a
/// peer can claim any tip; it can't make us wait on it forever).
const SYNC_PENALTY: Duration = Duration::from_secs(30);

pub struct NetConfig {
    pub key: SigningKey,
    pub genesis: Genesis,
    pub consensus: Config,
    /// Persist the safety state and block log here (recommended for
    /// validators; without it a restarted validator could double-vote).
    pub data_dir: Option<PathBuf>,
    /// Addresses to dial (and keep dialing).
    pub peers: Vec<SocketAddr>,
    /// Watch the Solana bridge and attest deposits (validators).
    pub solana: Option<observer::SolanaConfig>,
}

type Frame = Arc<Vec<u8>>;

#[derive(Default)]
struct Peers {
    next_id: u64,
    /// key -> live connections (conn id, writer queue); sends use the last.
    conns: HashMap<Pubkey, Vec<(u64, mpsc::Sender<Frame>)>>,
}

impl Peers {
    fn send(&self, to: &Pubkey, f: &Frame) {
        if let Some((_, q)) = self.conns.get(to).and_then(|v| v.last()) {
            let _ = q.try_send(f.clone());
        }
    }

    fn broadcast(&self, f: &Frame, except: Option<&Pubkey>) {
        for (k, v) in &self.conns {
            if Some(k) != except
                && let Some((_, q)) = v.last()
            {
                let _ = q.try_send(f.clone());
            }
        }
    }
}

enum Input {
    Peer(Pubkey, Wire),
    Submit(Tx, oneshot::Sender<bool>),
    /// An attestation this node signed.
    Attest(observer::SignedFact),
}

/// State shared between the node task, the network and the API.
pub struct Shared {
    pub node: Mutex<Node>,
    pub chain_id: u64,
    key: SigningKey,
    peers: Mutex<Peers>,
    inbox: mpsc::Sender<Input>,
    /// JSON of each committed block, for WebSocket subscribers.
    pub blocks: broadcast::Sender<Arc<str>>,
    /// Highest height published on `blocks`.
    pub published: AtomicU64,
    /// Attestations of Solana facts collected from validators.
    pub pool: Mutex<observer::Pool>,
}

impl Shared {
    pub fn me(&self) -> Pubkey {
        crypto::pubkey(&self.key)
    }

    pub fn sign(&self, msg: &[u8]) -> crypto::Sig {
        crypto::sign(&self.key, msg)
    }

    /// Keys of currently connected peers.
    pub fn peer_keys(&self) -> Vec<Pubkey> {
        let mut v: Vec<Pubkey> = self.peers.lock().unwrap().conns.keys().copied().collect();
        v.sort();
        v
    }

    /// Admits a transaction locally and gossips it on success.
    pub async fn submit(&self, tx: Tx) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.inbox.send(Input::Submit(tx, reply)).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

pub struct Handle {
    pub shared: Arc<Shared>,
    pub p2p_addr: SocketAddr,
    pub api_addr: SocketAddr,
    tasks: Vec<JoinHandle<()>>,
}

impl Handle {
    /// Stops every task of this node (connections close with them).
    pub fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        self.shared.peers.lock().unwrap().conns.clear();
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after 1970").as_millis() as u64
}

/// Starts a node on already-bound listeners (bind first so peers can be
/// told the addresses, e.g. port 0 in tests).
pub async fn start(cfg: NetConfig, p2p: TcpListener, api: TcpListener) -> std::io::Result<Handle> {
    let chain_id = cfg.genesis.chain_id;
    let (node, start_outs) = match &cfg.data_dir {
        Some(dir) => Node::open(dir, cfg.key.clone(), cfg.genesis.clone(), cfg.consensus, now_ms())?,
        None => {
            let mut n = Node::new(cfg.key.clone(), cfg.genesis.clone(), cfg.consensus);
            let outs = n.start(now_ms());
            (n, outs)
        }
    };
    let (inbox, rx) = mpsc::channel(1 << 16);
    let published = AtomicU64::new(node.height());
    let shared = Arc::new(Shared {
        node: Mutex::new(node),
        chain_id,
        key: cfg.key,
        peers: Mutex::new(Peers::default()),
        inbox,
        blocks: broadcast::channel(1024).0,
        published,
        pool: Mutex::new(observer::Pool::default()),
    });
    let (p2p_addr, api_addr) = (p2p.local_addr()?, api.local_addr()?);
    let mut tasks = vec![
        tokio::spawn(run_node(shared.clone(), rx, start_outs)),
        tokio::spawn(accept_loop(shared.clone(), p2p)),
        tokio::spawn(api::serve(shared.clone(), api)),
    ];
    for addr in cfg.peers {
        tasks.push(tokio::spawn(dial_loop(shared.clone(), addr)));
    }
    if let Some(sol) = cfg.solana {
        tasks.push(tokio::spawn(observe(shared.clone(), sol)));
    }
    Ok(Handle { shared, p2p_addr, api_addr, tasks })
}

async fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let s = shared.clone();
        tokio::spawn(async move {
            let _ = run_conn(s, stream).await;
        });
    }
}

async fn dial_loop(shared: Arc<Shared>, addr: SocketAddr) {
    let mut backoff = Duration::from_millis(100);
    loop {
        if let Ok(stream) = TcpStream::connect(addr).await {
            backoff = Duration::from_millis(100);
            let _ = run_conn(shared.clone(), stream).await;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

/// Handshake, register, then pump frames both ways until either side fails.
async fn run_conn(shared: Arc<Shared>, mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let peer = wire::handshake(&mut stream, &shared.key, shared.chain_id).await?;
    let (mut rd, mut wr) = stream.into_split();
    let (q, mut out) = mpsc::channel::<Frame>(PEER_QUEUE);
    let id = {
        let mut p = shared.peers.lock().unwrap();
        p.next_id += 1;
        let id = p.next_id;
        p.conns.entry(peer).or_default().push((id, q));
        id
    };
    // Introduce ourselves so the peer can sync from us right away.
    let status = {
        let n = shared.node.lock().unwrap();
        Arc::new(wire::encode(&Wire::Status { epoch: n.epoch, height: n.height() }))
    };
    shared.peers.lock().unwrap().send(&peer, &status);
    let writer = tokio::spawn(async move {
        while let Some(f) = out.recv().await {
            if wr.write_all(&f).await.is_err() {
                break;
            }
        }
    });
    let result = async {
        loop {
            let body = wire::read_frame(&mut rd).await?;
            let msg =
                Wire::try_from_slice(&body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            if shared.inbox.send(Input::Peer(peer, msg)).await.is_err() {
                return Ok(());
            }
        }
    }
    .await;
    writer.abort();
    let mut p = shared.peers.lock().unwrap();
    if let Some(v) = p.conns.get_mut(&peer) {
        v.retain(|(c, _)| *c != id);
        if v.is_empty() {
            p.conns.remove(&peer);
        }
    }
    result
}

/// What the node task remembers about sync with peers.
#[derive(Default)]
struct SyncState {
    /// Best tip each peer announced: (epoch, height).
    tips: HashMap<Pubkey, (u64, u64)>,
    last_request: Option<Instant>,
    /// The peer asked last and our height when we asked.
    pending: Option<(Pubkey, u64)>,
    /// Peers skipped as sync targets until the given time.
    ignored: HashMap<Pubkey, Instant>,
}

impl SyncState {
    /// The peer to sync from, if we are far enough behind a connected,
    /// non-penalized one.
    fn target(&self, epoch: u64, height: u64, connected: &[Pubkey]) -> Option<Pubkey> {
        let now = Instant::now();
        self.tips
            .iter()
            .filter(|(k, (e, h))| {
                (*e > epoch || *h > height + SYNC_LAG)
                    && connected.contains(k)
                    && self.ignored.get(*k).is_none_or(|until| *until <= now)
            })
            .max_by_key(|(k, (e, h))| (*h, *e, **k))
            .map(|(k, _)| *k)
    }
}

async fn run_node(shared: Arc<Shared>, mut rx: mpsc::Receiver<Input>, start_outs: Vec<NetOut>) {
    dispatch(&shared, start_outs);
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_status = Instant::now() - STATUS_EVERY;
    let mut sync = SyncState::default();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let outs = shared.node.lock().unwrap().on_tick(now_ms());
                dispatch(&shared, outs);
                if last_status.elapsed() >= STATUS_EVERY {
                    last_status = Instant::now();
                    let (epoch, height) = {
                        let n = shared.node.lock().unwrap();
                        (n.epoch, n.height())
                    };
                    let f = Arc::new(wire::encode(&Wire::Status { epoch, height }));
                    shared.peers.lock().unwrap().broadcast(&f, None);
                    maybe_sync(&shared, &mut sync, false);
                    // Facts that reached a quorum (possibly after a set change).
                    let ready = {
                        let n = shared.node.lock().unwrap();
                        shared.pool.lock().unwrap().refresh(&n.chain)
                    };
                    for tx in ready {
                        submit_local(&shared, tx);
                    }
                }
            }
            input = rx.recv() => {
                let Some(input) = input else { return };
                handle(&shared, &mut sync, input);
            }
        }
        publish(&shared);
    }
}

fn handle(shared: &Shared, sync: &mut SyncState, input: Input) {
    match input {
        Input::Submit(tx, reply) => {
            let ok = shared.node.lock().unwrap().submit_tx(tx.clone());
            if ok {
                let f = Arc::new(wire::encode(&Wire::Tx(tx)));
                shared.peers.lock().unwrap().broadcast(&f, None);
            }
            let _ = reply.send(ok);
        }
        Input::Peer(from, Wire::Tx(tx)) => {
            if shared.node.lock().unwrap().submit_tx(tx.clone()) {
                let f = Arc::new(wire::encode(&Wire::Tx(tx)));
                shared.peers.lock().unwrap().broadcast(&f, Some(&from));
            }
        }
        Input::Peer(from, Wire::Consensus(env)) => {
            // Only the authenticated sender may speak for its key.
            if env.from == from {
                let outs = shared.node.lock().unwrap().on_message(env, now_ms());
                dispatch(shared, outs);
            }
        }
        Input::Peer(from, Wire::Status { epoch, height }) => {
            sync.tips.insert(from, (epoch, height));
            maybe_sync(shared, sync, false);
        }
        Input::Peer(from, Wire::SyncRequest(req)) => {
            let resp = shared.node.lock().unwrap().serve_sync(&req);
            if let Some(resp) = resp {
                let f = Arc::new(wire::encode(&Wire::SyncResponse(resp)));
                shared.peers.lock().unwrap().send(&from, &f);
            }
        }
        Input::Attest(s) => {
            let f = Arc::new(wire::encode(&Wire::Attest(s.clone())));
            shared.peers.lock().unwrap().broadcast(&f, None);
            add_attestation(shared, s);
        }
        Input::Peer(_, Wire::Attest(s)) => add_attestation(shared, s),
        Input::Peer(_, Wire::SyncResponse(resp)) => {
            let res = shared.node.lock().unwrap().apply_sync(resp, now_ms());
            if let Ok(outs) = res {
                dispatch(shared, outs);
                maybe_sync(shared, sync, true);
            }
        }
    }
}

fn add_attestation(shared: &Shared, s: observer::SignedFact) {
    let ready = {
        let n = shared.node.lock().unwrap();
        shared.pool.lock().unwrap().add(&n.chain, s)
    };
    if let Some(tx) = ready {
        submit_local(shared, tx);
    }
}

/// Admits a transaction this node built and gossips it if it was new.
fn submit_local(shared: &Shared, tx: Tx) {
    if shared.node.lock().unwrap().submit_tx(tx.clone()) {
        let f = Arc::new(wire::encode(&Wire::Tx(tx)));
        shared.peers.lock().unwrap().broadcast(&f, None);
    }
}

/// Polls the Solana bridge for deposit records past the chain's next
/// expected id and attests them while this node is a validator. Pending
/// attestations are re-sent every poll until the chain has them, so peers
/// that connect late still get them.
async fn observe(shared: Arc<Shared>, cfg: observer::SolanaConfig) {
    let rpc = observer::Rpc::new(&cfg.rpc_url);
    let me = shared.me();
    loop {
        let (next, bridge, is_validator) = {
            let n = shared.node.lock().unwrap();
            (n.chain.next_deposit_id, n.chain.bridge_program, n.chain.validators.contains(&me))
        };
        if is_validator {
            for id in next..next + observer::LOOKAHEAD {
                let fact = match observer::read_deposit(&rpc, &bridge, id).await {
                    Ok(Some(f)) => f,
                    Ok(None) => break, // ids are sequential
                    Err(e) => {
                        eprintln!("observer: {e}");
                        break;
                    }
                };
                let msg = {
                    let n = shared.node.lock().unwrap();
                    fact.message(&n.chain)
                };
                let s = observer::SignedFact { fact, validator: me, signature: shared.sign(&msg) };
                if shared.inbox.send(Input::Attest(s)).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(cfg.poll).await;
    }
}

fn maybe_sync(shared: &Shared, sync: &mut SyncState, progressed: bool) {
    if !progressed && sync.last_request.is_some_and(|t| t.elapsed() < SYNC_RETRY) {
        return;
    }
    let connected = shared.peer_keys();
    sync.tips.retain(|k, _| connected.contains(k));
    let (req, epoch, height) = {
        let n = shared.node.lock().unwrap();
        (n.sync_request(), n.epoch, n.height())
    };
    // The last target let a full retry interval pass without moving us.
    if let Some((peer, at)) = sync.pending.take()
        && !progressed
        && height == at
    {
        sync.ignored.insert(peer, Instant::now() + SYNC_PENALTY);
    }
    if let Some(peer) = sync.target(epoch, height, &connected) {
        sync.last_request = Some(Instant::now());
        sync.pending = Some((peer, height));
        let f = Arc::new(wire::encode(&Wire::SyncRequest(req)));
        shared.peers.lock().unwrap().send(&peer, &f);
    }
}

fn dispatch(shared: &Shared, outs: Vec<NetOut>) {
    let local = shared.node.lock().unwrap().take_local_txs();
    if !local.is_empty() {
        let peers = shared.peers.lock().unwrap();
        for tx in local {
            peers.broadcast(&Arc::new(wire::encode(&Wire::Tx(tx))), None);
        }
    }
    if outs.is_empty() {
        return;
    }
    let peers = shared.peers.lock().unwrap();
    for o in outs {
        match o {
            NetOut::Broadcast(env) => peers.broadcast(&Arc::new(wire::encode(&Wire::Consensus(env))), None),
            NetOut::Send(to, env) => peers.send(&to, &Arc::new(wire::encode(&Wire::Consensus(env)))),
        }
    }
}

/// Pushes newly committed blocks to WebSocket subscribers.
fn publish(shared: &Shared) {
    let from = shared.published.load(Ordering::Relaxed);
    let node = shared.node.lock().unwrap();
    let to = node.height();
    if to == from {
        return;
    }
    for h in from + 1..=to {
        if let Some(out) = node.outcomes.get(h as usize - 1) {
            let _ = shared.blocks.send(api::block_json(h, out).to_string().into());
        }
    }
    shared.published.store(to, Ordering::Relaxed);
}

/// Local devnet keys, derived from the index. Public knowledge: never use
/// them outside a local network.
pub fn devnet_key(i: u32) -> SigningKey {
    SigningKey::from_bytes(&crypto::sha256(&[b"MEE/devnet-key", &i.to_le_bytes()]))
}

/// Genesis for a local devnet whose first `validators` devnet keys validate.
pub fn devnet_genesis(chain_id: u64, validators: u32) -> Genesis {
    Genesis {
        chain_id,
        params: mee_core::EngineParams {
            taker_fee_bps: 5,
            maker_fee_bps: 1,
            max_fee_share_bps: 30_000,
            max_oracle_move_bps: 500,
            max_oracle_staleness: 3_600,
            mark_band_bps: 100,
            order_band_bps: 5_000,
            max_fills_per_order: 1_000,
            liquidation_insurance_share_bps: 5_000,
            min_deployer_stake: 1_000,
            min_initial_margin_bps: 500,
            ..mee_core::EngineParams::default()
        },
        validators: (0..validators).map(|i| crypto::pubkey(&devnet_key(i))).collect(),
        timestamp_ms: 0,
        launch: Default::default(),
        staking: Default::default(),
        bridge_program: [0; 32],
    }
}
