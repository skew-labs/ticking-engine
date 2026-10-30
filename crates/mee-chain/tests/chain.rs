use ed25519_dalek::SigningKey;
use mee_chain::crypto::{self, Pubkey};
use mee_chain::*;
use mee_core::{DEX_FROZEN, EngineError, EngineParams};
use mee_engine::Event;

const CHAIN: u64 = 7;
const PRICE: u64 = 10_000;

fn params() -> EngineParams {
    EngineParams {
        taker_fee_bps: 5,
        maker_fee_bps: 1,
        max_fee_share_bps: 30_000,
        max_oracle_move_bps: 500,
        min_oracle_interval: 0,
        max_oracle_staleness: 60,
        mark_band_bps: 100,
        order_band_bps: 5_000,
        max_fills_per_order: 1_000,
        liquidation_insurance_share_bps: 5_000,
        min_deployer_stake: 1_000,
        min_stake_hold_secs: 100,
        unstake_delay_secs: 10,
        min_initial_margin_bps: 500,
    }
}

fn key(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i; 32])
}

struct Net {
    chain: Chain,
    validators: Vec<SigningKey>,
    nonces: std::collections::HashMap<Pubkey, u64>,
    deposits: u64,
    ts: i64,
}

impl Net {
    fn new() -> Net {
        let validators: Vec<SigningKey> = (100..104).map(key).collect();
        let chain = Chain::new(Genesis {
            chain_id: CHAIN,
            params: params(),
            validators: validators.iter().map(crypto::pubkey).collect(),
            timestamp_ms: 0,
            launch: Default::default(),
            staking: Default::default(),
        });
        Net { chain, validators, nonces: Default::default(), deposits: 0, ts: 0 }
    }

    fn tx(&mut self, k: &SigningKey, action: Action) -> Tx {
        let n = self.nonces.entry(crypto::pubkey(k)).or_insert(0);
        *n += 1;
        Tx::User(SignedTx::sign(k, CHAIN, *n, action))
    }

    fn attest(&self, msg: &[u8; 32], who: &[usize]) -> Vec<Attestation> {
        who.iter()
            .map(|&i| Attestation { validator: i as u16, signature: crypto::sign(&self.validators[i], msg) })
            .collect()
    }

    fn deposit_tx(&mut self, owner: &SigningKey, asset: u8, amount: u64) -> Tx {
        let id = self.deposits;
        self.deposits += 1;
        let owner = crypto::pubkey(owner);
        let msg = crypto::deposit_message(CHAIN, id, &owner, asset, amount);
        Tx::System(SystemTx::BridgeDeposit {
            deposit_id: id,
            owner,
            asset,
            amount,
            attestations: self.attest(&msg, &[0, 1, 2]),
        })
    }

    fn block(&mut self, txs: Vec<Tx>) -> BlockOutcome {
        self.ts += 1_000;
        let b = Block { height: self.chain.height + 1, timestamp_ms: self.ts, txs };
        self.chain.apply_block(&b).unwrap()
    }

    fn ok(&mut self, txs: Vec<Tx>) -> BlockOutcome {
        let out = self.block(txs);
        for r in &out.results {
            assert_eq!(*r, Ok(()), "{:?}", out.results);
        }
        out
    }
}

fn market() -> MarketConfig {
    MarketConfig {
        symbol: *b"TSLA\0\0\0\0\0\0\0\0\0\0\0\0",
        tick_size: 1,
        min_base_lots: 1,
        initial_margin_bps: 1_000,
        maintenance_margin_bps: 500,
        liquidation_fee_bps: 250,
        oi_cap: 1_000_000,
        initial_price: PRICE,
        funding_interval: 3_600,
        max_funding_bps: 50,
    }
}

fn order(market: u32, side: u8, kind: u8, price: u64, size: u64) -> Action {
    Action::PlaceOrder { market, side, kind, price, size, reduce_only: false, client_id: 0 }
}

/// Deployer with a DEX and one market; two funded traders.
fn setup() -> (Net, SigningKey, SigningKey, SigningKey) {
    let mut net = Net::new();
    let (dep, a, b) = (key(1), key(2), key(3));
    let txs = vec![
        net.deposit_tx(&dep, ASSET_STAKE, 5_000),
        net.deposit_tx(&a, ASSET_QUOTE, 1_000_000),
        net.deposit_tx(&b, ASSET_QUOTE, 1_000_000),
    ];
    net.ok(txs);
    let txs = vec![net.tx(
        &dep,
        Action::DeployDex { name: [0; 16], fee_share_bps: 0, stake: 1_000, oracle_authority: crypto::pubkey(&dep) },
    )];
    net.ok(txs);
    let txs = vec![
        net.tx(&dep, Action::AddMarket { dex: 0, config: market() }),
        net.tx(&a, Action::TransferToDex { dex: 0, amount: 1_000_000 }),
        net.tx(&b, Action::TransferToDex { dex: 0, amount: 1_000_000 }),
    ];
    net.ok(txs);
    (net, dep, a, b)
}

#[test]
fn signatures_and_nonces() {
    let mut net = Net::new();
    let k = key(1);
    let t = SignedTx::sign(&k, CHAIN, 1, Action::SweepProtocolFees { dex: 0 });
    assert!(t.verify(CHAIN));
    assert!(!t.verify(CHAIN + 1), "signature is bound to the chain id");
    let mut forged = t.clone();
    forged.action = Action::SweepProtocolFees { dex: 1 };
    assert!(!forged.verify(CHAIN));

    let out = net.block(vec![Tx::User(forged)]);
    assert_eq!(out.results, vec![Err(TxError::BadSignature)]);
    // A key without an account can't act (and consumes nothing).
    let out = net.block(vec![Tx::User(t.clone())]);
    assert_eq!(out.results, vec![Err(TxError::UnknownAccount)]);
    let dep = net.deposit_tx(&k, ASSET_QUOTE, 1);
    net.ok(vec![dep]);
    let out = net.block(vec![Tx::User(t.clone())]);
    assert_eq!(
        out.results,
        vec![Err(TxError::Engine(EngineError::DexNotActive))],
        "nonce consumed even though it failed"
    );
    let out = net.block(vec![Tx::User(t)]);
    assert_eq!(out.results, vec![Err(TxError::StaleNonce)], "replay rejected");
}

#[test]
fn deposits_need_ordered_quorum_attestations() {
    let mut net = Net::new();
    let owner = crypto::pubkey(&key(1));
    let msg = crypto::deposit_message(CHAIN, 0, &owner, ASSET_QUOTE, 100);
    let two = net.attest(&msg, &[0, 1]);
    let dup = net.attest(&msg, &[0, 0, 1]);
    let wrong = {
        let m = crypto::deposit_message(CHAIN, 0, &owner, ASSET_QUOTE, 1_000_000);
        net.attest(&m, &[0, 1, 2])
    };
    let d = |id, att| {
        Tx::System(SystemTx::BridgeDeposit {
            deposit_id: id,
            owner,
            asset: ASSET_QUOTE,
            amount: 100,
            attestations: att,
        })
    };
    let out = net.block(vec![d(0, two), d(0, dup), d(0, wrong)]);
    assert_eq!(out.results, vec![Err(TxError::NoQuorum); 3]);
    let skip = {
        let m = crypto::deposit_message(CHAIN, 1, &owner, ASSET_QUOTE, 100);
        net.attest(&m, &[1, 2, 3])
    };
    let out = net.block(vec![d(1, skip)]);
    assert_eq!(out.results, vec![Err(TxError::DepositOutOfOrder)]);
    let good = net.attest(&msg, &[3, 1, 2]);
    net.ok(vec![d(0, good.clone())]);
    assert_eq!(net.chain.state.accounts[0].balance, 100);
    let out = net.block(vec![d(0, good)]);
    assert_eq!(out.results, vec![Err(TxError::DepositOutOfOrder)], "a deposit is credited once");
}

#[test]
fn trading_and_withdrawal_receipts() {
    let (mut net, _dep, a, b) = setup();
    let txs = vec![net.tx(&a, order(0, 1, 0, PRICE, 10))];
    net.ok(txs);
    let txs = vec![net.tx(&b, order(0, 0, 2, PRICE, 4))];
    let out = net.ok(txs);
    assert!(out.events.iter().any(|e| matches!(e, Event::Fill { size: 4, price: PRICE, .. })));

    let dest = [9u8; 32];
    let txs = vec![
        net.tx(&a, Action::TransferFromDex { dex: 0, amount: 500_000 }),
        net.tx(&a, Action::Withdraw { asset: ASSET_QUOTE, amount: 400_000, destination: dest }),
    ];
    let out = net.ok(txs);
    let r = out.withdrawals[0];
    assert_eq!(r, WithdrawalReceipt { id: 0, asset: ASSET_QUOTE, destination: dest, amount: 400_000 });
    // Validators sign the receipt; the Solana bridge checks the quorum.
    let sigs: Vec<_> =
        net.validators.iter().take(3).map(|v| (crypto::pubkey(v), crypto::sign(v, &r.message(CHAIN)))).collect();
    assert!(sigs.iter().all(|(pk, s)| crypto::verify(pk, &r.message(CHAIN), s)));

    let txs = vec![net.tx(&a, Action::Withdraw { asset: ASSET_QUOTE, amount: 10_000_000, destination: dest })];
    let out = net.block(txs);
    assert_eq!(out.results, vec![Err(TxError::Engine(EngineError::InsufficientCollateral))]);
    assert!(out.withdrawals.is_empty());
}

#[test]
fn cancels_and_post_only_run_first() {
    let (mut net, _dep, a, b) = setup();
    let txs = vec![net.tx(&a, order(0, 1, 0, PRICE, 10))];
    let out = net.ok(txs);
    let id = out
        .events
        .iter()
        .find_map(|e| if let Event::Placed { order_id, .. } = e { Some(*order_id) } else { None })
        .unwrap();

    // The taker's IOC was sent first, but the maker's cancel runs first, so
    // there is nothing to hit (no sniping stale quotes).
    let txs = vec![net.tx(&b, order(0, 0, 2, PRICE, 10)), net.tx(&a, Action::Cancel { market: 0, order_id: id })];
    let out = net.ok(txs);
    assert!(!out.events.iter().any(|e| matches!(e, Event::Fill { .. })));

    // A new post-only quote lands before an IOC sent ahead of it in the block.
    let txs = vec![net.tx(&b, order(0, 0, 2, PRICE + 5, 3)), net.tx(&a, order(0, 1, 1, PRICE + 5, 3))];
    let out = net.ok(txs);
    assert!(out.events.iter().any(|e| matches!(e, Event::Fill { size: 3, .. })));
}

#[test]
fn account_proofs_verify_against_the_state_root() {
    let (mut net, _dep, a, b) = setup();
    let txs = vec![net.tx(&a, order(0, 1, 0, PRICE, 10)), net.tx(&b, order(0, 0, 2, PRICE, 4))];
    let out = net.ok(txs);
    let root = out.state_root;
    let pa = net.chain.prove_account(&crypto::pubkey(&a)).unwrap();
    assert!(verify_account_proof(&root, &pa));
    // The proven leaf is the account the client expects.
    let acct = &net.chain.state.accounts[pa.id as usize];
    assert_eq!(pa.leaf, mee_engine::commit::account_leaf(pa.id, acct));
    assert_eq!(acct.margin(0).unwrap().position(0).base, -4);
    // Any tampering breaks it.
    let mut bad = pa.clone();
    bad.leaf[0] ^= 1;
    assert!(!verify_account_proof(&root, &bad));
    let mut bad = pa.clone();
    bad.globals[0] ^= 1;
    assert!(!verify_account_proof(&root, &bad));
    // A proof for one block doesn't verify against the next root.
    let sweep = net.tx(&b, Action::SweepProtocolFees { dex: 0 });
    let out = net.ok(vec![sweep]);
    assert!(!verify_account_proof(&out.state_root, &pa));
    // Validators sign the root for light clients.
    let msg = crypto::state_root_message(CHAIN, net.chain.height, &out.state_root);
    let sig = crypto::sign(&net.validators[0], &msg);
    assert!(crypto::verify(&crypto::pubkey(&net.validators[0]), &msg, &sig));
}

#[test]
fn replicas_agree_and_order_matters() {
    let (mut net, dep, a, b) = setup();
    let mut replica = net.chain.clone();
    let txs = vec![
        net.tx(&a, order(0, 1, 0, PRICE, 10)),
        net.tx(&b, order(0, 0, 2, PRICE, 3)),
        net.tx(&dep, Action::SetOracle { dex: 0, prices: vec![(0, PRICE + 100)] }),
    ];
    net.ts += 1_000;
    let block = Block { height: net.chain.height + 1, timestamp_ms: net.ts, txs: txs.clone() };
    let h1 = net.chain.apply_block(&block).unwrap().app_hash;
    let h2 = replica.apply_block(&block).unwrap().app_hash;
    assert_eq!(h1, h2, "same block, same app hash");

    let mut other = replica.clone();
    let mut swapped = txs;
    swapped.swap(0, 1);
    let next = |c: &Chain, txs: Vec<Tx>| Block { height: c.height + 1, timestamp_ms: net.ts + 1_000, txs };
    let b1 = next(&replica, vec![]);
    let b2 = next(&other, swapped);
    assert_ne!(replica.apply_block(&b1).unwrap().app_hash, other.apply_block(&b2).unwrap().app_hash);
    assert_eq!(
        net.chain.apply_block(&Block { height: 99, timestamp_ms: 0, txs: vec![] }).unwrap_err(),
        BlockError::WrongHeight
    );
}

#[test]
fn governance_slash_needs_quorum_and_fresh_nonce() {
    let (mut net, dep, _a, _b) = setup();
    let action = Governance::Slash { dex: 0, bps: 2_000, freeze: true };
    let body = borsh::to_vec(&action).unwrap();
    let msg = crypto::governance_message(CHAIN, 0, &body);
    let weak =
        Tx::System(SystemTx::Governance { nonce: 0, action: action.clone(), attestations: net.attest(&msg, &[0, 1]) });
    let strong = Tx::System(SystemTx::Governance {
        nonce: 0,
        action: action.clone(),
        attestations: net.attest(&msg, &[0, 1, 3]),
    });
    let out = net.block(vec![weak, strong.clone()]);
    assert_eq!(out.results, vec![Err(TxError::NoQuorum), Ok(())]);
    assert_eq!(net.chain.state.dexes[0].status, DEX_FROZEN);
    assert_eq!(net.chain.state.slashed_stake, 200);
    let out = net.block(vec![strong]);
    assert_eq!(out.results, vec![Err(TxError::GovernanceNonce)], "signatures can't be replayed");

    // The frozen deployer can't push prices any more.
    let txs = vec![net.tx(&dep, Action::SetOracle { dex: 0, prices: vec![(0, PRICE)] })];
    let out = net.block(txs);
    assert_eq!(out.results, vec![Err(TxError::Engine(EngineError::DexNotActive))]);
}

#[test]
fn forged_signature_inside_a_parallel_batch_is_caught() {
    let (mut net, _dep, a, b) = setup();
    let mut txs = Vec::new();
    for i in 0..200 {
        let who = if i % 2 == 0 { &a } else { &b };
        txs.push(net.tx(who, order(0, (i % 2) as u8, 1, if i % 2 == 0 { PRICE - 10 } else { PRICE + 10 }, 1)));
    }
    if let Tx::User(t) = &mut txs[137] {
        t.signature[0] ^= 1;
    }
    let out = net.block(txs);
    let bad: Vec<usize> = out.results.iter().enumerate().filter(|(_, r)| r.is_err()).map(|(i, _)| i).collect();
    assert_eq!(bad.len(), 1);
    assert_eq!(out.results[bad[0]], Err(TxError::BadSignature));
}

#[test]
fn staking_elects_the_validator_set_each_epoch() {
    let validators: Vec<SigningKey> = (100..104).map(key).collect();
    let genesis_set: Vec<Pubkey> = validators.iter().map(crypto::pubkey).collect();
    let mut net = Net::new();
    net.chain = Chain::new(Genesis {
        chain_id: CHAIN,
        params: params(),
        validators: genesis_set.clone(),
        timestamp_ms: 0,
        launch: Default::default(),
        staking: Staking { epoch_blocks: 2, max_validators: 4, min_validator_stake: 10, unbond_secs: 100 },
    });
    let delegator = key(7);
    let newcomer_key = key(200);
    let newcomer = crypto::pubkey(&newcomer_key);
    let dep = net.deposit_tx(&delegator, ASSET_STAKE, 1_000);
    let dep2 = net.deposit_tx(&newcomer_key, ASSET_QUOTE, 1);
    let out = net.ok(vec![dep, dep2]); // height 1
    assert!(out.validator_set.is_none());
    // Stake alone can't elect a key that never proved it is controlled.
    let d = net.tx(&delegator, Action::Delegate { validator: newcomer, amount: 500 });
    let out = net.ok(vec![d]); // height 2: epoch boundary
    assert!(out.validator_set.is_none(), "unregistered keys are not eligible");
    let r = net.tx(&newcomer_key, Action::RegisterValidator);
    net.ok(vec![r]); // height 3
    let out = net.ok(vec![]); // height 4: boundary
    let (epoch, set) = out.validator_set.expect("set changed");
    assert_eq!(epoch, 1);
    assert!(set.contains(&(newcomer, 500)), "weight = stake");
    assert_eq!(set.len(), 4);
    let keys: Vec<Pubkey> = set.iter().map(|(k, _)| *k).collect();
    let dropped: Vec<_> = genesis_set.iter().filter(|v| !keys.contains(v)).collect();
    assert_eq!(dropped.len(), 1, "one genesis validator (largest key among equal stakes) is out");
    assert_eq!(net.chain.validators, keys);
    assert_eq!(net.chain.total_weight(), 530);
    // The bridge rotation message the outgoing set signs.
    let msg = crypto::valset_message(CHAIN, 0, &set);
    assert_ne!(msg, crypto::valset_message(CHAIN, 0, &[(newcomer, 499)]));

    // Over-undelegating fails; a real undelegation stops counting at the
    // next election but stays locked until that election has passed.
    let bad = net.tx(&delegator, Action::Undelegate { validator: newcomer, amount: 501 });
    assert_eq!(net.block(vec![bad]).results, vec![Err(TxError::InsufficientStake)]); // height 5
    let u = net.tx(&delegator, Action::Undelegate { validator: newcomer, amount: 500 });
    let out = net.ok(vec![u]); // height 6: boundary again, newcomer has no stake
    assert_eq!(out.validator_set.map(|(e, s)| (e, s.len())), Some((2, 4)));
    assert!(!net.chain.validators.contains(&newcomer));
    let a = net.chain.state.account_id(&crypto::pubkey(&delegator)).unwrap();
    assert_eq!(net.chain.state.accounts[a as usize].stake_balance, 500);
    net.ts += 100_000;
    net.ok(vec![]); // height 7: the delay passed but the requesting block was itself a boundary
    net.ok(vec![]); // height 8: boundary
    assert_eq!(net.chain.state.accounts[a as usize].stake_balance, 500, "still backing the epoch it counted in");
    net.ok(vec![]); // height 9
    assert_eq!(net.chain.state.accounts[a as usize].stake_balance, 1_000, "unbonded after the next election");
}

#[test]
fn double_votes_slash_the_validator_and_its_delegators() {
    let mut net = Net::new();
    let delegator = key(7);
    let bad = &net.validators[2].clone();
    let bad_key = crypto::pubkey(bad);
    let dep = net.deposit_tx(&delegator, ASSET_STAKE, 10_000);
    let dep2 = net.deposit_tx(bad, ASSET_QUOTE, 1);
    net.ok(vec![dep, dep2]);
    let d = net.tx(&delegator, Action::Delegate { validator: bad_key, amount: 6_000 });
    let u = net.tx(&delegator, Action::Undelegate { validator: bad_key, amount: 2_000 });
    net.ok(vec![d, u]); // 4_000 bonded, 2_000 unbonding

    let domain = net.chain.consensus_genesis();
    let vote = |b: [u8; 32], round: u64| crypto::sign(bad, &crypto::consensus_vote_message(&domain, &b, round));
    let evidence = |block_b: [u8; 32], sig_b| {
        Tx::System(SystemTx::Evidence {
            epoch: 0,
            validator: bad_key,
            round: 9,
            block_a: [1; 32],
            sig_a: vote([1; 32], 9),
            block_b,
            sig_b,
        })
    };
    // The same vote twice, or a signature for another round, proves nothing.
    assert_eq!(net.chain.check_tx(&evidence([1; 32], vote([1; 32], 9))), Err(TxError::BadEvidence));
    assert_eq!(net.chain.check_tx(&evidence([2; 32], vote([2; 32], 10))), Err(TxError::BadEvidence));

    let out = net.ok(vec![evidence([2; 32], vote([2; 32], 9))]);
    // Ejected immediately (a forced election), not at some later epoch.
    let (epoch, set) = out.validator_set.expect("set changed");
    assert_eq!(epoch, 1);
    assert!(!set.iter().any(|(k, _)| *k == bad_key));
    assert_eq!(set.len(), 3);
    let a = net.chain.state.account_id(&crypto::pubkey(&delegator)).unwrap();
    // 5% of the bonded 4_000 and of the unbonding 2_000.
    assert_eq!(net.chain.delegations[&(a, bad_key)], 3_800);
    assert_eq!(net.chain.unbonding[0].4, 1_900);
    assert_eq!(net.chain.slashed_validator_stake, 300);
    assert!(net.chain.tombstoned.contains(&bad_key));

    // Once is enough; the key can't come back.
    let again = evidence([3; 32], vote([3; 32], 9));
    assert_eq!(net.block(vec![again]).results, vec![Err(TxError::AlreadySlashed)]);
    let r = net.tx(bad, Action::RegisterValidator);
    assert_eq!(net.block(vec![r]).results, vec![Err(TxError::AlreadySlashed)]);
}
