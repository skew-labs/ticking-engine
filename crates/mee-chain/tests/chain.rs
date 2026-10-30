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
            bridge_program: [0; 32],
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
        let msg = crypto::deposit_message(CHAIN, &[0; 32], id, &owner, asset, amount);
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
fn batch_verification_and_pre_verified_skips() {
    let mut net = Net::new();
    let good: Vec<Tx> =
        (0..200u8).map(|i| Tx::User(SignedTx::sign(&key(i), CHAIN, 1, Action::SweepProtocolFees { dex: 0 }))).collect();
    let mut txs = good.clone();
    // A bad signature in the middle of a batch is pinpointed.
    if let Tx::User(t) = &mut txs[77] {
        t.signature[5] ^= 1;
    }
    let mut bad_deposit = net.deposit_tx(&key(1), ASSET_QUOTE, 1);
    if let Tx::System(SystemTx::BridgeDeposit { amount, .. }) = &mut bad_deposit {
        *amount += 1; // attestations no longer match
    }
    txs.push(bad_deposit);
    let v = net.chain.verify_parallel(&txs);
    assert_eq!(v.iter().filter(|r| r.is_err()).count(), 2);
    assert_eq!(v[77], Err(TxError::BadSignature));
    assert_eq!(v[200], Err(TxError::NoQuorum));
    // Marked known: user signatures are trusted (the node verified those
    // exact bytes before), system transactions are still checked.
    let v = net.chain.verify_parallel_except(&txs, &vec![true; txs.len()]);
    assert_eq!(v[77], Ok(()));
    assert_eq!(v[200], Err(TxError::NoQuorum));
    // Only some known: the rest are verified as usual.
    let known: Vec<bool> = (0..txs.len()).map(|i| i % 2 == 0).collect();
    let v = net.chain.verify_parallel_except(&txs, &known);
    assert_eq!(v[77], Err(TxError::BadSignature), "odd index, not known");
    assert_eq!(v.iter().filter(|r| r.is_err()).count(), 2);
}

#[test]
fn deposits_need_ordered_quorum_attestations() {
    let mut net = Net::new();
    let owner = crypto::pubkey(&key(1));
    let msg = crypto::deposit_message(CHAIN, &[0; 32], 0, &owner, ASSET_QUOTE, 100);
    let two = net.attest(&msg, &[0, 1]);
    let dup = net.attest(&msg, &[0, 0, 1]);
    let wrong = {
        let m = crypto::deposit_message(CHAIN, &[0; 32], 0, &owner, ASSET_QUOTE, 1_000_000);
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
        let m = crypto::deposit_message(CHAIN, &[0; 32], 1, &owner, ASSET_QUOTE, 100);
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
    let sigs: Vec<_> = net
        .validators
        .iter()
        .take(3)
        .map(|v| (crypto::pubkey(v), crypto::sign(v, &r.message(CHAIN, &[0; 32]))))
        .collect();
    assert!(sigs.iter().all(|(pk, s)| crypto::verify(pk, &r.message(CHAIN, &[0; 32]), s)));

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
        bridge_program: [0; 32],
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
    let msg = crypto::valset_message(CHAIN, &[0; 32], 0, &set);
    assert_ne!(msg, crypto::valset_message(CHAIN, &[0; 32], 0, &[(newcomer, 499)]));

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

#[test]
fn stop_loss_fires_on_the_mark_and_batches_report_partial_failure() {
    let (mut net, dep, a, b) = setup();
    // a goes long 10.
    let txs = vec![net.tx(&b, order(0, 1, 1, PRICE, 10)), net.tx(&a, order(0, 0, 2, PRICE, 10))];
    net.ok(txs);
    assert_eq!(net.chain.state.accounts[1].margins[0].1.position(0).base, 10);

    // Stop-loss below, take-profit above; b bids below to absorb the stop.
    let stop = Action::PlaceTrigger {
        market: 0,
        side: 1,
        above: false,
        trigger_price: PRICE - 200,
        limit_price: PRICE - 400,
        size: 10,
        reduce_only: true,
    };
    let tp = Action::PlaceTrigger {
        market: 0,
        side: 1,
        above: true,
        trigger_price: PRICE + 300,
        limit_price: PRICE + 250,
        size: 10,
        reduce_only: true,
    };
    // Triggers protect a position: none without one, and only reduce-only.
    let squat = net.tx(&dep, stop.clone());
    assert_eq!(net.block(vec![squat]).results, vec![Err(TxError::BadAction)]);
    let Action::PlaceTrigger { market, side, above, trigger_price, limit_price, size, .. } = stop.clone() else {
        unreachable!()
    };
    let entry = Action::PlaceTrigger { market, side, above, trigger_price, limit_price, size, reduce_only: false };
    let t = net.tx(&a, entry);
    assert_eq!(net.block(vec![t]).results, vec![Err(TxError::BadAction)]);
    let txs = vec![net.tx(&a, stop), net.tx(&a, tp), net.tx(&b, order(0, 0, 1, PRICE - 300, 10))];
    net.ok(txs);
    assert_eq!(net.chain.triggers.len(), 2);
    // Only the owner can cancel a trigger.
    let steal = net.tx(&b, Action::CancelTrigger { id: 1 });
    assert_eq!(net.block(vec![steal]).results, vec![Err(TxError::UnknownTrigger)]);

    // The oracle drops: nothing fires in the block that moves it (marks are
    // read at the start of a block), the stop fires at the start of the next.
    let drop = net.tx(&dep, Action::SetOracle { dex: 0, prices: vec![(0, PRICE - 250)] });
    let out = net.ok(vec![drop]);
    assert!(out.triggered.is_empty());
    let out = net.ok(vec![]);
    assert_eq!(out.triggered, vec![(0, Ok(()))]);
    assert_eq!(net.chain.state.accounts[1].margins[0].1.position(0).base, 0, "position closed at b's bid");
    assert_eq!(net.chain.triggers.keys().copied().collect::<Vec<_>>(), vec![1]);
    let cancel = net.tx(&a, Action::CancelTrigger { id: 1 });
    net.ok(vec![cancel]);
    assert!(net.chain.triggers.is_empty());

    // A batch: two good orders and a bad one; the good ones stand.
    let batch = Action::Batch(BatchActions(vec![
        order(0, 1, 1, PRICE + 50, 1),
        order(0, 1, 1, 0, 1), // price 0: rejected
        order(0, 1, 1, PRICE + 60, 1),
    ]));
    assert_eq!(batch.priority(), 2, "all post-only: the post-only class");
    let t = net.tx(&a, batch);
    let out = net.block(vec![t]);
    assert_eq!(out.results, vec![Err(TxError::PartialBatch { failed: 1 })]);
    assert_eq!(net.chain.state.markets[0].book.depth(mee_core::Side::Ask, 5), vec![(PRICE + 50, 1), (PRICE + 60, 1)]);
    // Bundling a cancel with a taker order doesn't move it into the cancel class.
    let mixed = Action::Batch(BatchActions(vec![Action::CancelAll { market: 0 }, order(0, 0, 2, PRICE, 1)]));
    assert_eq!(mixed.priority(), 3);
    let nested = Action::Batch(BatchActions(vec![Action::Batch(BatchActions(vec![]))]));
    let t = net.tx(&a, nested.clone());
    assert_eq!(net.block(vec![t]).results, vec![Err(TxError::BadAction)]);
    // Untrusted bytes can't even decode a nested batch, however deep: no
    // recursion, so no stack overflow.
    use borsh::BorshDeserialize;
    assert!(Action::try_from_slice(&borsh::to_vec(&nested).unwrap()).is_err());
    let tag = borsh::to_vec(&Action::Batch(BatchActions(vec![]))).unwrap()[0];
    let mut deep = Vec::new();
    for _ in 0..200_000 {
        deep.push(tag);
        deep.extend_from_slice(&1u32.to_le_bytes());
    }
    assert!(Action::try_from_slice(&deep).is_err());
    let mut long = vec![tag];
    long.extend_from_slice(&(MAX_BATCH as u32 + 1).to_le_bytes());
    assert!(Action::try_from_slice(&long).is_err(), "too many items refused before reading them");
}

#[test]
fn snapshot_restores_a_chain_that_continues_identically() {
    let (mut net, _dep, a, b) = setup();
    let txs = vec![net.tx(&b, order(0, 1, 1, PRICE, 10)), net.tx(&a, order(0, 0, 2, PRICE, 4))];
    net.ok(txs);
    let stop = Action::PlaceTrigger {
        market: 0,
        side: 1,
        above: false,
        trigger_price: PRICE - 200,
        limit_price: PRICE - 400,
        size: 4,
        reduce_only: true,
    };
    let w = Action::Withdraw { asset: ASSET_QUOTE, amount: 1, destination: [8; 32] };
    let txs = vec![
        net.tx(&a, stop),
        net.tx(&b, Action::TransferFromDex { dex: 0, amount: 10 }),
        net.tx(&b, w),
        net.tx(&b, order(0, 0, 0, PRICE - 50, 3)),
    ];
    net.ok(txs);
    let bytes = net.chain.to_snapshot();
    let genesis = Genesis {
        chain_id: CHAIN,
        params: params(),
        validators: net.validators.iter().map(crypto::pubkey).collect(),
        timestamp_ms: 0,
        launch: Default::default(),
        staking: Default::default(),
        bridge_program: [0; 32],
    };
    let mut back = Chain::from_snapshot(&genesis, &bytes).unwrap();
    assert_eq!(
        (back.height, back.state_root, back.app_hash),
        (net.chain.height, net.chain.state_root, net.chain.app_hash)
    );
    assert_eq!(back.to_snapshot(), bytes);

    // The same next block gives the same result on both.
    let next = Block {
        height: net.chain.height + 1,
        timestamp_ms: net.ts + 1_000,
        txs: vec![net.tx(&a, order(0, 1, 2, PRICE - 50, 2)), net.tx(&b, Action::CancelAll { market: 0 })],
    };
    let o1 = net.chain.apply_block(&next).unwrap();
    let o2 = back.apply_block(&next).unwrap();
    assert_eq!((o1.app_hash, o1.state_root), (o2.app_hash, o2.state_root));

    // Another network's genesis or a flipped byte is refused.
    let other = Genesis { chain_id: CHAIN + 1, ..genesis.clone() };
    assert_eq!(Chain::from_snapshot(&other, &bytes).err(), Some(mee_chain::snapshot::SnapshotError::WrongGenesis));
    // Every byte but the app hash (a running hash of history, not a function
    // of the state, so it can't be recomputed) is checked: engine params
    // against the genesis, the rest through hash chains and the state root.
    let app_hash = Chain::from_snapshot(&genesis, &bytes).unwrap().app_hash;
    let at_hash = bytes.windows(32).position(|w| w == app_hash).unwrap();
    for at in 0..bytes.len() {
        let mut bad = bytes.clone();
        bad[at] ^= 1;
        if Chain::from_snapshot(&genesis, &bad).is_ok() {
            assert!((at_hash..at_hash + 32).contains(&at), "tampered byte {at} accepted");
        }
    }
}

#[test]
fn a_slash_only_removes_the_offender_and_never_reelects() {
    // Fixed genesis set (no scheduled elections). Outsiders stake and
    // register; then a genesis validator double-signs on purpose.
    let mut net = Net::new();
    let outsiders: Vec<SigningKey> = (50..54).map(key).collect();
    let mut txs = Vec::new();
    for o in &outsiders {
        txs.push(net.deposit_tx(o, ASSET_STAKE, 1_000));
    }
    net.ok(txs);
    let mut txs = Vec::new();
    for o in &outsiders {
        let k = crypto::pubkey(o);
        txs.push(net.tx(o, Action::RegisterValidator));
        txs.push(net.tx(o, Action::Delegate { validator: k, amount: 1_000 }));
    }
    net.ok(txs);
    let bad = net.validators[0].clone();
    let domain = net.chain.consensus_genesis();
    let vote = |b: [u8; 32]| crypto::sign(&bad, &crypto::consensus_vote_message(&domain, &b, 3));
    let ev = Tx::System(SystemTx::Evidence {
        epoch: 0,
        validator: crypto::pubkey(&bad),
        round: 3,
        block_a: [1; 32],
        sig_a: vote([1; 32]),
        block_b: [2; 32],
        sig_b: vote([2; 32]),
    });
    let before: Vec<_> = net.chain.committee();
    let out = net.ok(vec![ev]);
    let (_, set) = out.validator_set.expect("offender removed");
    let expected: Vec<_> = {
        let mut v: Vec<_> = before.into_iter().filter(|(k, _)| *k != crypto::pubkey(&bad)).collect();
        v.sort();
        v
    };
    assert_eq!(set, expected, "the other three keep their seats and weights; no outsider gets in");
}

#[test]
fn consensus_domain_binds_the_whole_genesis() {
    // Same chain id and keys, any other difference: votes never transfer.
    let base = Net::new().chain;
    let g = |f: &dyn Fn(&mut Genesis)| {
        let mut g = Genesis {
            chain_id: CHAIN,
            params: params(),
            validators: (100..104).map(|i| crypto::pubkey(&key(i))).collect(),
            timestamp_ms: 0,
            launch: Default::default(),
            staking: Default::default(),
            bridge_program: [0; 32],
        };
        f(&mut g);
        Chain::new(g).consensus_genesis()
    };
    assert_eq!(g(&|_| {}), base.consensus_genesis());
    assert_ne!(g(&|g| g.bridge_program = [1; 32]), base.consensus_genesis());
    assert_ne!(g(&|g| g.timestamp_ms = 1), base.consensus_genesis());
    assert_ne!(g(&|g| g.validators.pop().map(|_| ()).unwrap()), base.consensus_genesis());
    assert_ne!(g(&|g| g.staking.epoch_blocks = 10), base.consensus_genesis());
}
