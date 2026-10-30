//! Four validators, each with its own chain state, agree on blocks through
//! BFT consensus; bridged deposits, a HIP-3 deployment and order flow go
//! through it. Every node must reach the same app hash at every height,
//! including with one validator crashed.

use std::collections::VecDeque;

use ed25519_dalek::SigningKey;
use mee_chain::crypto;
use mee_chain::*;
use mee_consensus::{Config, Message, NodeId, Out};
use mee_core::EngineParams;
use mee_engine::Event;
use mee_node::Node;

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
    nodes: Vec<Node>,
    validators: Vec<SigningKey>,
    crashed: Vec<usize>,
    inbox: VecDeque<(u64, NodeId, NodeId, Message)>,
    now: u64,
    nonces: std::collections::HashMap<[u8; 32], u64>,
    deposits: u64,
}

impl Devnet {
    fn new(crashed: Vec<usize>) -> Devnet {
        let validators: Vec<SigningKey> = (0..4u8).map(|i| SigningKey::from_bytes(&[30 + i; 32])).collect();
        let genesis = Genesis {
            chain_id: CHAIN,
            params: params(),
            validators: validators.iter().map(crypto::pubkey).collect(),
            timestamp_ms: 0,
            launch: Default::default(),
        };
        let cfg = Config { base_timeout_ms: 200, max_timeout_ms: 2_000 };
        let nodes = validators
            .iter()
            .enumerate()
            .map(|(i, k)| Node::new(i as NodeId, k.clone(), genesis.clone(), cfg))
            .collect();
        let mut net = Devnet {
            nodes,
            validators,
            crashed,
            inbox: VecDeque::new(),
            now: 0,
            nonces: Default::default(),
            deposits: 0,
        };
        for i in 0..4 {
            if !net.crashed.contains(&i) {
                let outs = net.nodes[i].start(0);
                net.route(i as NodeId, outs);
            }
        }
        net
    }

    fn route(&mut self, from: NodeId, outs: Vec<Out>) {
        // Delivered next tick: 10 ms links.
        for o in outs {
            match o {
                Out::Send(to, m) => self.inbox.push_back((self.now + 10, from, to, m)),
                Out::Broadcast(m) => {
                    for to in 0..4 {
                        if to != from {
                            self.inbox.push_back((self.now + 10, from, to, m.clone()));
                        }
                    }
                }
            }
        }
    }

    fn step(&mut self) {
        self.now += 10;
        let due: Vec<_> = {
            let (due, later): (VecDeque<_>, VecDeque<_>) =
                std::mem::take(&mut self.inbox).into_iter().partition(|(at, ..)| *at <= self.now);
            self.inbox = later;
            due.into_iter().collect()
        };
        for (_, from, to, m) in due {
            if self.crashed.contains(&(to as usize)) {
                continue;
            }
            let outs = self.nodes[to as usize].on_message(from, m, self.now);
            self.route(to, outs);
        }
        for i in 0..4 {
            if !self.crashed.contains(&i) {
                let outs = self.nodes[i].on_tick(self.now);
                self.route(i as NodeId, outs);
            }
        }
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 10 {
            self.step();
        }
    }

    /// Clients gossip to every live validator.
    fn broadcast_tx(&mut self, tx: Tx) {
        for i in 0..4 {
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

    fn deposit(&mut self, owner: &SigningKey, asset: u8, amount: u64) {
        let id = self.deposits;
        self.deposits += 1;
        let owner = crypto::pubkey(owner);
        let msg = crypto::deposit_message(CHAIN, id, &owner, asset, amount);
        let attestations: Vec<Attestation> = (0..4)
            .filter(|i| !self.crashed.contains(i))
            .map(|i| Attestation { validator: i as u16, signature: crypto::sign(&self.validators[i], &msg) })
            .collect();
        self.broadcast_tx(Tx::System(SystemTx::BridgeDeposit { deposit_id: id, owner, asset, amount, attestations }));
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
        net.user_tx(
            &a,
            Action::PlaceOrder {
                market: 0,
                side: 1,
                kind: 1,
                price: 10_000 + i,
                size: 2,
                reduce_only: false,
                client_id: i,
            },
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
    // Round-robin leaders: rounds led by (or voting into) the dead node time
    // out, so give each phase longer. Leader reputation would remove this.
    scenario(vec![2], 4_000);
}
