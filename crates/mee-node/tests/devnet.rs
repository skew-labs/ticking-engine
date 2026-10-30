//! Validators (and observers), each with its own chain state, agree on
//! blocks through BFT consensus over a simulated 10 ms network. Bridged
//! deposits, a HIP-3 deployment, order flow, crashes, restarts from disk and
//! validator-set changes go through it; every node must reach the same app
//! hash at every height.

use std::collections::VecDeque;
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use mee_chain::crypto;
use mee_chain::*;
use mee_consensus::Config;
use mee_core::EngineParams;
use mee_engine::Event;
use mee_node::{Envelope, NetOut, Node};

const CHAIN: u64 = 77;

fn params() -> EngineParams {
    EngineParams {
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
        ..EngineParams::default()
    }
}

struct Devnet {
    /// Physical nodes; the first `genesis validators` are validators at
    /// genesis, the rest start as observers.
    nodes: Vec<Node>,
    keys: Vec<SigningKey>,
    crashed: Vec<usize>,
    inbox: VecDeque<(u64, usize, Envelope)>,
    now: u64,
    nonces: std::collections::HashMap<[u8; 32], u64>,
    deposits: u64,
    genesis: Genesis,
    cfg: Config,
    dirs: Option<Vec<PathBuf>>,
}

impl Devnet {
    fn new(crashed: Vec<usize>) -> Devnet {
        Devnet::build(4, 4, crashed, None, Staking::default())
    }

    /// `n` nodes, the first `validators` of them in the genesis set.
    fn build(n: usize, validators: usize, crashed: Vec<usize>, dirs: Option<Vec<PathBuf>>, staking: Staking) -> Devnet {
        let keys: Vec<SigningKey> = (0..n as u8).map(|i| SigningKey::from_bytes(&[30 + i; 32])).collect();
        let genesis = Genesis {
            chain_id: CHAIN,
            params: params(),
            validators: keys[..validators].iter().map(crypto::pubkey).collect(),
            timestamp_ms: 0,
            launch: Default::default(),
            staking,
            bridge_program: [0; 32],
        };
        let cfg = Config { base_timeout_ms: 200, max_timeout_ms: 2_000 };
        let mut starts = Vec::new();
        let nodes = keys
            .iter()
            .enumerate()
            .map(|(i, k)| match &dirs {
                Some(d) => {
                    let (node, outs) = Node::open(&d[i], k.clone(), genesis.clone(), cfg, 0).unwrap();
                    starts.push(outs);
                    node
                }
                None => {
                    let mut node = Node::new(k.clone(), genesis.clone(), cfg);
                    starts.push(node.start(0));
                    node
                }
            })
            .collect();
        let mut net = Devnet {
            nodes,
            keys,
            crashed,
            inbox: VecDeque::new(),
            now: 0,
            nonces: Default::default(),
            deposits: 0,
            genesis,
            cfg,
            dirs,
        };
        for (i, outs) in starts.into_iter().enumerate() {
            if !net.crashed.contains(&i) {
                net.route(outs);
            }
        }
        net
    }

    fn crash(&mut self, i: usize) {
        self.crashed.push(i);
    }

    /// Brings node `i` back from its data directory.
    fn restart(&mut self, i: usize) {
        let dir = &self.dirs.as_ref().expect("persistent devnet")[i];
        let (node, outs) = Node::open(dir, self.keys[i].clone(), self.genesis.clone(), self.cfg, self.now).unwrap();
        self.nodes[i] = node;
        self.crashed.retain(|&c| c != i);
        self.route(outs);
    }

    fn route(&mut self, outs: Vec<NetOut>) {
        // Delivered next tick: 10 ms links.
        for o in outs {
            match o {
                NetOut::Send(to, env) => {
                    if let Some(i) = self.keys.iter().position(|k| crypto::pubkey(k) == to) {
                        self.inbox.push_back((self.now + 10, i, env));
                    }
                }
                NetOut::Broadcast(env) => {
                    for i in 0..self.nodes.len() {
                        if self.nodes[i].me != env.from {
                            self.inbox.push_back((self.now + 10, i, env.clone()));
                        }
                    }
                }
            }
        }
    }

    fn step(&mut self) {
        self.now += 10;
        let (due, later): (VecDeque<_>, VecDeque<_>) =
            std::mem::take(&mut self.inbox).into_iter().partition(|(at, ..)| *at <= self.now);
        self.inbox = later;
        for (_, to, env) in due {
            if self.crashed.contains(&to) {
                continue;
            }
            let outs = self.nodes[to].on_message(env, self.now);
            self.route(outs);
        }
        for i in 0..self.nodes.len() {
            if !self.crashed.contains(&i) {
                let outs = self.nodes[i].on_tick(self.now);
                self.route(outs);
            }
        }
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 10 {
            self.step();
        }
    }

    /// Clients gossip to every live node.
    fn broadcast_tx(&mut self, tx: Tx) {
        for i in 0..self.nodes.len() {
            if !self.crashed.contains(&i) {
                self.nodes[i].submit_tx(tx.clone());
            }
        }
    }

    fn user_tx(&mut self, k: &SigningKey, action: Action) {
        let n = self.nonces.entry(crypto::pubkey(k)).or_insert(0);
        *n += 1;
        let tx = Tx::User(SignedTx::sign(k, CHAIN, *n, action));
        self.broadcast_tx(tx);
    }

    /// Deposit attested by the current validator set (as seen by node 0's
    /// first live peer).
    fn deposit(&mut self, owner: &SigningKey, asset: u8, amount: u64) {
        let id = self.deposits;
        self.deposits += 1;
        let owner = crypto::pubkey(owner);
        let msg = crypto::deposit_message(CHAIN, &[0; 32], id, &owner, asset, amount);
        let set = self.live().next().unwrap().chain.validators.clone();
        let attestations: Vec<Attestation> = set
            .iter()
            .enumerate()
            .filter_map(|(vi, v)| {
                let phys = self.keys.iter().position(|k| crypto::pubkey(k) == *v)?;
                (!self.crashed.contains(&phys))
                    .then(|| Attestation { validator: vi as u16, signature: crypto::sign(&self.keys[phys], &msg) })
            })
            .collect();
        self.broadcast_tx(Tx::System(SystemTx::BridgeDeposit { deposit_id: id, owner, asset, amount, attestations }));
    }

    /// Node `i`'s key opens an account and registers as a validator
    /// candidate.
    fn register(&mut self, i: usize) {
        let k = self.keys[i].clone();
        self.deposit(&k, ASSET_QUOTE, 1);
        self.run(300);
        self.user_tx(&k, Action::RegisterValidator);
    }

    fn live(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().enumerate().filter(|(i, _)| !self.crashed.contains(i)).map(|(_, n)| n)
    }

    fn assert_agreement(&self) -> usize {
        let min = self.live().map(|n| n.app_hashes.len()).min().unwrap();
        for h in 0..min {
            let first = self.live().next().unwrap().app_hashes[h];
            assert!(self.live().all(|n| n.app_hashes[h] == first), "app hash diverged at height {}", h + 1);
        }
        min
    }
}

fn market() -> MarketConfig {
    MarketConfig {
        symbol: *b"NVDA\0\0\0\0\0\0\0\0\0\0\0\0",
        tick_size: 1,
        min_base_lots: 1,
        initial_margin_bps: 1_000,
        maintenance_margin_bps: 500,
        liquidation_fee_bps: 250,
        oi_cap: 1_000_000,
        initial_price: 10_000,
        funding_interval: 3_600,
        max_funding_bps: 50,
    }
}

/// `phase_ms`: how long each step gets to commit before the next depends on it.
fn scenario(crashed: Vec<usize>, phase_ms: u64) {
    let mut net = Devnet::new(crashed);
    let (dep, a, b) =
        (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]), SigningKey::from_bytes(&[3; 32]));
    net.deposit(&dep, ASSET_STAKE, 5_000);
    net.deposit(&a, ASSET_QUOTE, 1_000_000);
    net.deposit(&b, ASSET_QUOTE, 1_000_000);
    net.run(phase_ms);
    net.user_tx(
        &dep,
        Action::DeployDex { name: [0; 16], fee_share_bps: 5_000, stake: 1_000, oracle_authority: crypto::pubkey(&dep) },
    );
    net.run(phase_ms);
    net.user_tx(&dep, Action::AddMarket { dex: 0, config: market() });
    net.user_tx(&a, Action::TransferToDex { dex: 0, amount: 1_000_000 });
    net.user_tx(&b, Action::TransferToDex { dex: 0, amount: 1_000_000 });
    net.run(phase_ms);
    for i in 0..20u64 {
        let price = 10_000 + i;
        net.user_tx(
            &a,
            Action::PlaceOrder { market: 0, side: 1, kind: 1, price, size: 2, reduce_only: false, client_id: i },
        );
    }
    net.run(phase_ms);
    net.user_tx(
        &b,
        Action::PlaceOrder { market: 0, side: 0, kind: 2, price: 10_010, size: 15, reduce_only: false, client_id: 0 },
    );
    net.user_tx(&b, Action::Withdraw { asset: ASSET_QUOTE, amount: 1, destination: [5; 32] });
    net.run(2 * phase_ms);

    let heights = net.assert_agreement();
    assert!(heights >= 10, "only {heights} blocks");
    let n = net.live().next().unwrap();
    let fills: u64 = n
        .outcomes
        .iter()
        .flat_map(|o| &o.events)
        .filter_map(|e| if let Event::Fill { size, .. } = e { Some(*size) } else { None })
        .sum();
    // 11 levels of 2 lots up to 10_010: the 15-lot IOC fills completely.
    assert_eq!(fills, 15);
    assert_eq!(n.chain.withdrawals.len(), 0, "free balance was all moved into the DEX, so the withdrawal fails");
    for node in net.live() {
        assert_eq!(node.chain.state.markets[0].open_interest, 15);
        assert_eq!(node.chain.next_deposit_id, 3);
    }
}

#[test]
fn four_validators_agree() {
    scenario(vec![], 500);
}

#[test]
fn agree_with_one_validator_down() {
    // Leader reputation skips the dead node once 10 blocks are committed, so
    // after a short warm-up each phase needs little more than the honest case.
    scenario(vec![2], 1_500);
}

fn temp_dirs(tag: &str, n: usize) -> (PathBuf, Vec<PathBuf>) {
    let base = std::env::temp_dir().join(format!("mee-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dirs = (0..n).map(|i| base.join(format!("v{i}"))).collect();
    (base, dirs)
}

#[test]
fn restarted_validator_recovers_from_disk_and_catches_up() {
    let (base, dirs) = temp_dirs("restart", 4);
    let mut net = Devnet::build(4, 4, vec![], Some(dirs), Staking::default());
    let (dep, a) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
    net.deposit(&dep, ASSET_STAKE, 5_000);
    net.deposit(&a, ASSET_QUOTE, 1_000_000);
    net.run(1_000);
    let before = net.nodes[1].app_hashes.len();
    assert!(before > 5);

    net.crash(1);
    net.user_tx(
        &dep,
        Action::DeployDex { name: [0; 16], fee_share_bps: 0, stake: 1_000, oracle_authority: crypto::pubkey(&dep) },
    );
    net.run(2_000);
    let progressed = net.nodes[0].app_hashes.len();
    assert!(progressed > before + 5, "the other three keep going");

    net.restart(1);
    // Replay rebuilt everything it had committed before the crash.
    assert!(net.nodes[1].app_hashes.len() >= before);
    assert_eq!(net.nodes[1].app_hashes[..before], net.nodes[0].app_hashes[..before]);
    net.run(3_000);
    // It caught up (fetching missed blocks) and agrees at every height.
    let heights = net.assert_agreement();
    assert!(heights > progressed, "restarted node caught up: {heights} vs {progressed}");
    assert_eq!(net.nodes[1].chain.state.dexes.len(), 1, "it saw the DEX deployed while it was down");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn stake_rotates_validators_across_epochs() {
    // Five nodes; four validate at genesis, node 4 observes. Each genesis
    // validator carries 10 virtual stake; epochs end every 20 blocks.
    let staking = Staking { epoch_blocks: 20, max_validators: 4, min_validator_stake: 10, unbond_secs: 1 };
    let (base, dirs) = temp_dirs("epochs", 5);
    let mut net = Devnet::build(5, 4, vec![], Some(dirs), staking);
    assert!(!net.nodes[4].replica.is_validator());
    let whale = SigningKey::from_bytes(&[9; 32]);
    net.deposit(&whale, ASSET_STAKE, 1_000);
    net.register(4);
    net.run(500);
    let newcomer = net.nodes[4].me;
    // Weight 11 vs 10 for each genesis validator: it displaces one of them
    // without holding a third of the weight on its own.
    net.user_tx(&whale, Action::Delegate { validator: newcomer, amount: 11 });
    net.run(3_000);

    let heights = net.assert_agreement();
    assert!(heights > 25, "only {heights} blocks");
    let set = net.nodes[0].chain.validators.clone();
    assert!(set.contains(&newcomer), "the staked observer joined the set");
    assert_eq!(set.len(), 4);
    let epochs: Vec<(u64, usize, bool)> =
        net.nodes.iter().map(|n| (n.epoch, n.app_hashes.len(), n.replica.is_validator())).collect();
    assert!(net.nodes.iter().all(|n| n.epoch == 1), "{epochs:?}");
    assert!(net.nodes[4].replica.is_validator());
    let out = net.nodes[1].outcomes.iter().find_map(|o| o.validator_set.clone()).expect("rotation recorded");
    assert_eq!(out, (1, net.nodes[0].chain.committee()));
    assert!(out.1.contains(&(newcomer, 11)));
    let dropped = net.nodes.iter().position(|n| !set.contains(&n.me)).unwrap();
    assert!(!net.nodes[dropped].replica.is_validator(), "the displaced validator now observes");

    // The new set keeps producing blocks and attests deposits.
    let user = SigningKey::from_bytes(&[8; 32]);
    net.deposit(&user, ASSET_QUOTE, 777);
    net.run(1_500);
    let after = net.assert_agreement();
    assert!(after > heights + 5);
    let n = &net.nodes[0];
    let id = n.chain.state.account_id(&crypto::pubkey(&user)).expect("deposit attested by the new set");
    assert_eq!(n.chain.state.accounts[id as usize].balance, 777);

    // An observer restarted from disk crosses the epoch in replay.
    net.crash(dropped);
    net.run(300);
    net.restart(dropped);
    assert_eq!(net.nodes[dropped].epoch, 1);
    net.run(1_000);
    net.assert_agreement();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn late_joiner_catches_up_by_verified_sync() {
    // Node 4 is an observer that is offline from genesis; 4 validators run,
    // with an epoch change (node 4 gets staked in) along the way.
    let staking = Staking { epoch_blocks: 30, max_validators: 4, min_validator_stake: 10, unbond_secs: 1 };
    let mut net = Devnet::build(5, 4, vec![4], None, staking);
    let whale = SigningKey::from_bytes(&[9; 32]);
    net.deposit(&whale, ASSET_STAKE, 1_000);
    net.register(4);
    net.run(300);
    let newcomer = net.nodes[4].me;
    net.user_tx(&whale, Action::Delegate { validator: newcomer, amount: 11 });
    net.run(6_000);
    let target = net.nodes[0].height();
    // The offline node is in the new set, so after the boundary the network
    // runs 3-of-4 (slower) until it returns.
    assert!(target > 100, "a long chain to catch up on: {target}");
    assert_eq!(net.nodes[0].epoch, 1, "the set changed while node 4 was away");

    // Tampering with a served segment is detected.
    let req = net.nodes[4].sync_request();
    let mut bad = net.nodes[0].serve_sync(&req).unwrap();
    bad.blocks[3].payload.push(0);
    let mut fresh = Node::new(net.keys[4].clone(), net.genesis.clone(), net.cfg);
    assert_eq!(fresh.apply_sync(bad, net.now), Err(mee_node::SyncError::BadProof));

    // Sync in batches from any peer until caught up, across the epoch.
    let mut rounds = 0;
    while net.nodes[4].height() < net.nodes[0].height() {
        let req = net.nodes[4].sync_request();
        let peer = 1 + rounds % 3;
        let Some(resp) = net.nodes[peer].serve_sync(&req) else { break };
        assert!(resp.blocks.len() <= mee_node::SYNC_BATCH + 64);
        net.nodes[4].apply_sync(resp, net.now).unwrap();
        rounds += 1;
    }
    assert!(rounds > 1, "synced in several batches (one per epoch at least)");
    assert_eq!(net.nodes[4].epoch, 1);
    assert!(net.nodes[4].replica.is_validator(), "it is in the new set");
    assert!(net.nodes[4].height() + 5 >= net.nodes[0].height());

    // Back online: it joins consensus and the network agrees everywhere.
    net.crashed.clear();
    net.run(2_000);
    let h = net.assert_agreement();
    assert!(h as u64 > target);
}

#[test]
fn whole_network_restart_resumes_from_disk() {
    // Everyone goes down at once: the blocks certified but not yet committed
    // exist only in the voters' persisted safety state, and the chain must
    // still be completed from there.
    let (base, dirs) = temp_dirs("full-restart", 4);
    let mut net = Devnet::build(4, 4, vec![], Some(dirs), Staking::default());
    let a = SigningKey::from_bytes(&[2; 32]);
    net.deposit(&a, ASSET_QUOTE, 1_000_000);
    net.run(1_000);
    for i in 0..4 {
        net.crash(i);
    }
    net.inbox.clear();
    net.run(500);
    let before = net.nodes.iter().map(|n| n.height()).max().unwrap();
    for i in 0..4 {
        net.restart(i);
    }
    net.run(3_000);
    let heights = net.assert_agreement();
    assert!(heights as u64 > before + 20, "progress after a full restart: {heights} vs {before}");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn proposals_from_the_future_get_no_votes() {
    let net = Devnet::new(vec![]);
    let (g, cfg) = (net.genesis.clone(), net.cfg);
    // Node 1 leads round 1; its clock is 100 s ahead.
    let mut leader = Node::new(net.keys[1].clone(), g.clone(), cfg);
    let prop = leader
        .start(100_000)
        .into_iter()
        .find_map(|o| match o {
            NetOut::Broadcast(e) if matches!(e.msg, mee_consensus::Message::Propose { .. }) => Some(e),
            _ => None,
        })
        .expect("leader proposes");
    let votes = |outs: Vec<NetOut>| {
        outs.iter().any(|o| matches!(o, NetOut::Send(_, e) if matches!(e.msg, mee_consensus::Message::Vote(_))))
    };
    let mut honest = Node::new(net.keys[2].clone(), g.clone(), cfg);
    honest.start(0);
    assert!(!votes(honest.on_message(prop.clone(), 0)), "100 s in the future: no vote");
    let mut close = Node::new(net.keys[3].clone(), g, cfg);
    close.start(99_000);
    assert!(votes(close.on_message(prop, 99_000)), "within the drift bound: votes");
}

#[test]
fn mempool_bounds_pending_transactions_per_sender() {
    let mut net = Devnet::new(vec![]);
    let a = SigningKey::from_bytes(&[2; 32]);
    net.deposit(&a, ASSET_QUOTE, 1_000);
    net.run(500);
    let node = &mut net.nodes[0];
    let tx = |nonce| {
        Tx::User(SignedTx::sign(&a, CHAIN, nonce, Action::Withdraw { asset: 0, amount: 1, destination: [0; 32] }))
    };
    assert!(!node.submit_tx(tx(mee_node::MAX_NONCE_AHEAD + 1)), "too far ahead");
    for n in 1..=mee_node::MAX_PENDING_PER_SENDER as u64 {
        assert!(node.submit_tx(tx(n)));
    }
    assert!(!node.submit_tx(tx(1)), "duplicate");
    let other = Tx::User(SignedTx::sign(&a, CHAIN, 1, Action::Withdraw { asset: 0, amount: 2, destination: [0; 32] }));
    assert!(!node.submit_tx(other), "per-sender cap");
}

#[test]
fn double_voting_validator_is_caught_slashed_and_ejected() {
    let mut net = Devnet::new(vec![]);
    net.run(500);
    let domain = net.nodes[0].chain.consensus_genesis();
    assert_eq!(
        crypto::consensus_vote_message(&domain, &[5; 32], 3),
        mee_consensus::vote_message(&domain, &[5; 32], 3),
        "chain and consensus agree on the signed vote bytes"
    );
    // Validator 3 signs votes for two different blocks in an upcoming round;
    // whoever leads the next round sees both.
    let round = net.nodes.iter().map(|n| n.replica.round).max().unwrap() + 2;
    let bad = net.keys[3].clone();
    for block in [[0xAA; 32], [0xBB; 32]] {
        let signature = crypto::sign(&bad, &mee_consensus::vote_message(&domain, &block, round));
        let v = mee_consensus::Vote { block, round, voter: 3, signature };
        let env = Envelope { epoch: 0, from: crypto::pubkey(&bad), msg: mee_consensus::Message::Vote(v) };
        for i in 0..3 {
            net.inbox.push_back((net.now + 10, i, env.clone()));
        }
    }
    net.run(3_000);
    let bad_key = crypto::pubkey(&bad);
    for n in &net.nodes {
        assert!(n.chain.tombstoned.contains(&bad_key), "slashed everywhere");
        assert_eq!(n.epoch, 1, "ejected through an immediate election");
        assert!(!n.chain.validators.contains(&bad_key));
    }
    assert!(!net.nodes[3].replica.is_validator());
    let before = net.assert_agreement();
    net.run(1_000);
    assert!(net.assert_agreement() > before + 5, "the remaining three keep going");
}

#[test]
fn restart_resumes_from_snapshot_and_falls_back_if_it_is_corrupt() {
    let (base, dirs) = temp_dirs("snapshot", 4);
    let mut net = Devnet::build(4, 4, vec![], Some(dirs.clone()), Staking::default());
    for n in &mut net.nodes {
        n.snapshot_every = 25;
    }
    let (dep, a) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
    net.deposit(&dep, ASSET_STAKE, 5_000);
    net.deposit(&a, ASSET_QUOTE, 1_000_000);
    net.run(500);
    net.user_tx(
        &dep,
        Action::DeployDex { name: [0; 16], fee_share_bps: 0, stake: 1_000, oracle_authority: crypto::pubkey(&dep) },
    );
    net.run(1_000);

    net.crash(2);
    net.run(300);
    net.restart(2);
    let from = net.nodes[2].restored_from.expect("resumed from the snapshot");
    assert!(from >= 25 && from.is_multiple_of(25), "snapshot height {from}");
    assert_eq!(net.nodes[2].chain.state.dexes.len(), 1);
    net.run(1_500);
    let heights = net.assert_agreement();
    assert!(heights as u64 > from + 10);

    // A damaged snapshot is detected and ignored: full replay instead.
    net.crash(2);
    let path = dirs[2].join("snapshot.bin");
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x40;
    std::fs::write(&path, bytes).unwrap();
    net.restart(2);
    assert_eq!(net.nodes[2].restored_from, None, "corrupt snapshot refused");
    net.run(1_000);
    net.assert_agreement();
    let _ = std::fs::remove_dir_all(&base);
}
