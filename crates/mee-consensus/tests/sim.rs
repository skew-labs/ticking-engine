use mee_consensus::sim::{SimConfig, run};

#[test]
fn honest_network_commits_fast() {
    let r = run(&SimConfig::default());
    assert!(r.safe());
    // 20s of 5-50ms links: hundreds of blocks.
    let c = r.min_commits(0..4);
    println!("honest 4 nodes, 20s: {c} commits, {} messages", r.messages);
    assert!(c > 200, "only {c} commits");
}

#[test]
fn deterministic_for_a_seed() {
    let a = run(&SimConfig { duration_ms: 3_000, ..SimConfig::default() });
    let b = run(&SimConfig { duration_ms: 3_000, ..SimConfig::default() });
    assert_eq!(a.commits, b.commits);
}

#[test]
fn survives_a_crashed_validator() {
    for crashed in 0..4u16 {
        let r = run(&SimConfig { crashed: vec![crashed], ..SimConfig::default() });
        assert!(r.safe());
        let c = r.min_commits((0..4).filter(|&i| i != crashed as usize));
        assert!(c > 20, "crashed {crashed}: only {c} commits");
    }
}

#[test]
fn safe_and_live_with_message_loss() {
    for seed in 1..=10 {
        let r = run(&SimConfig { seed, drop_per_mille: 100, n: 7, ..SimConfig::default() });
        assert!(r.safe(), "seed {seed} forked");
        assert!(r.min_commits(0..7) > 10, "seed {seed}: {:?}", r.commits.iter().map(|c| c.len()).collect::<Vec<_>>());
    }
}

#[test]
fn heals_after_partition() {
    // Two of four nodes are cut off for 5s: no quorum on either side, so
    // nothing may commit then; after healing, progress resumes.
    let cfg = SimConfig { partition: Some((2_000, 7_000, vec![0, 1])), duration_ms: 20_000, ..SimConfig::default() };
    let r = run(&cfg);
    assert!(r.safe());
    let during = run(&SimConfig { duration_ms: 6_900, ..cfg.clone() });
    let before = run(&SimConfig { duration_ms: 2_000, ..cfg.clone() });
    assert!(r.min_commits(0..4) > during.min_commits(0..4) + 50, "no progress after healing");
    assert!(
        during.commits.iter().map(|c| c.len()).max().unwrap()
            <= before.commits.iter().map(|c| c.len()).max().unwrap() + 2
    );
}

#[test]
fn equivocating_leader_cannot_fork() {
    for seed in 1..=10 {
        let r = run(&SimConfig { seed, n: 4, equivocator: Some(1), drop_per_mille: 20, ..SimConfig::default() });
        assert!(r.safe(), "seed {seed} forked");
        assert!(r.min_commits([0usize, 2, 3].into_iter()) > 20);
        for p in &r.payloads[0] {
            assert!(!p.ends_with(b"-equivocated") || r.payloads.iter().all(|q| q.contains(p)));
        }
    }
}

#[test]
fn no_commits_without_quorum() {
    // Two of four crashed: more than f faults. The protocol must halt, not fork.
    let r = run(&SimConfig { crashed: vec![1, 3], duration_ms: 10_000, ..SimConfig::default() });
    assert!(r.safe());
    assert_eq!(r.min_commits([0usize, 2].into_iter()), 0);
}

#[test]
fn randomized_fault_sweep_never_forks() {
    for seed in 100..140u64 {
        let n = [4usize, 7][(seed % 2) as usize];
        let f = (n - 1) / 3;
        let cfg = SimConfig {
            seed,
            n,
            drop_per_mille: seed % 300,
            max_delay_ms: 20 + seed % 200,
            crashed: if seed % 3 == 0 { vec![(seed % n as u64) as u16] } else { vec![] },
            equivocator: if seed % 3 == 1 && f > 0 { Some(((seed + 1) % n as u64) as u16) } else { None },
            partition: if seed % 5 == 0 { Some((1_000, 4_000, vec![0])) } else { None },
            duration_ms: 8_000,
            ..SimConfig::default()
        };
        let r = run(&cfg);
        assert!(r.safe(), "fork with {cfg:?}");
    }
}

#[test]
fn reputation_routes_around_a_crashed_validator() {
    let honest = run(&SimConfig::default()).min_commits(0..4);
    for crashed in 0..4u16 {
        let r = run(&SimConfig { crashed: vec![crashed], ..SimConfig::default() });
        assert!(r.safe());
        let c = r.min_commits((0..4).filter(|&i| i != crashed as usize));
        println!("crashed {crashed}: {c} commits vs {honest} honest");
        // Without reputation every fourth round (and the one voting into it)
        // times out (a few dozen commits here). With it the dead node is
        // skipped after a short warm-up. The rest is structural: with 3 of 4
        // alive the quorum needs every live node, so each round waits for the
        // slowest link instead of the fastest three of four.
        assert!(c * 2 >= honest, "crashed {crashed}: {c} vs {honest}");
    }
}

#[test]
fn restored_replica_never_votes_twice_in_a_round() {
    use mee_consensus::sim::key_for;
    use mee_consensus::{Config, Message, Out, Replica, sha256};
    let keys: Vec<_> = (0..4).map(|i| key_for(7, i)).collect();
    let vals: Vec<[u8; 32]> = keys.iter().map(|k| k.verifying_key().to_bytes()).collect();
    let genesis = sha256(&[b"g"]);
    let mk = |i: usize| Replica::new(i as u16, keys[i].clone(), vals.clone(), genesis, Config::default());
    // Round 1's leader proposes.
    let mut leader = mk(1);
    let outs = leader.start(0);
    let proposal = outs
        .iter()
        .find_map(|o| if let Out::Broadcast(m @ Message::Propose { .. }) = o { Some(m.clone()) } else { None })
        .unwrap();
    // Replica 3 votes (to round 2's leader), persisting its safety state first.
    let mut r3 = mk(3);
    r3.start(0);
    let outs = r3.on_message(1, proposal.clone(), 1);
    let pos_persist = outs.iter().position(|o| matches!(o, Out::Persist(_))).expect("persists");
    let pos_vote = outs.iter().position(|o| matches!(o, Out::Send(_, Message::Vote(_)))).expect("votes");
    assert!(pos_persist < pos_vote, "safety state hits disk before the vote leaves");
    let Out::Persist(safety) = outs[pos_persist].clone() else { unreachable!() };
    // Crash and restore: the same proposal gets no second vote.
    let mut back = mk(3);
    back.restore(safety, &[], 2);
    let outs = back.on_message(1, proposal, 3);
    assert!(!outs.iter().any(|o| matches!(o, Out::Send(_, Message::Vote(_)))));
}

#[test]
fn quorum_is_by_weight_not_by_head_count() {
    // Seven validators, weights 3,3,3,1,1,1,1 (total 13; quorum > 26/3).
    let weights = Some(vec![3, 3, 3, 1, 1, 1, 1]);
    let sim = |crashed: Vec<u16>, weights: Option<Vec<u64>>| {
        run(&SimConfig { n: 7, crashed, weights, duration_ms: 30_000, ..SimConfig::default() })
    };
    // Three light nodes down: 10 of 13 is still a quorum, so it commits
    // (with equal weights 4 of 7 would halt). Slow at first: three dead
    // leaders in a row time out until leader reputation skips them.
    let r = sim(vec![3, 4, 5], weights.clone());
    assert!(r.safe());
    let live = [0usize, 1, 2, 6];
    assert!(r.min_commits(live.into_iter()) > 20, "light nodes down: {}", r.min_commits(live.into_iter()));
    let r = sim(vec![3, 4, 5], None);
    assert_eq!(r.min_commits(live.into_iter()), 0, "equal weights: 4 of 7 is not a quorum");

    // Two heavy nodes down: five of seven nodes are up but only 7 of 13
    // weight, so it must halt (safely); equal weights would carry on.
    let r = sim(vec![0, 1], weights);
    assert!(r.safe());
    assert_eq!(r.min_commits(2..7), 0, "7 of 13 is not a quorum");
    assert!(sim(vec![0, 1], None).min_commits(2..7) > 20);
}
