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
