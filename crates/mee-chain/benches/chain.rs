//! End-to-end block throughput: signed user transactions (signature +
//! nonce + execution) in 20k-transaction blocks.
//!
//! `cargo bench -p mee-chain`

use std::time::Instant;

use ed25519_dalek::SigningKey;
use mee_chain::crypto;
use mee_chain::*;
use mee_core::EngineParams;

const CHAIN: u64 = 1;

fn main() {
    let validators: Vec<SigningKey> = (0..4u8).map(|i| SigningKey::from_bytes(&[200 + i; 32])).collect();
    let params = EngineParams {
        taker_fee_bps: 5,
        maker_fee_bps: 1,
        max_fee_share_bps: 30_000,
        max_oracle_move_bps: 500,
        max_oracle_staleness: i64::MAX / 2,
        mark_band_bps: 100,
        order_band_bps: 5_000,
        max_fills_per_order: 1_000,
        liquidation_insurance_share_bps: 5_000,
        min_deployer_stake: 1,
        min_initial_margin_bps: 500,
        ..EngineParams::default()
    };
    let mut chain = Chain::new(Genesis {
        chain_id: CHAIN,
        params,
        validators: validators.iter().map(crypto::pubkey).collect(),
        timestamp_ms: 0,
        launch: Default::default(),
    });
    let traders: Vec<SigningKey> = (0..2_000u32)
        .map(|i| {
            let mut b = [0u8; 32];
            b[..4].copy_from_slice(&i.to_le_bytes());
            b[31] = 1;
            SigningKey::from_bytes(&b)
        })
        .collect();
    let dep = SigningKey::from_bytes(&[250; 32]);
    let mut nonce = std::collections::HashMap::new();
    let mut tx = |k: &SigningKey, a: Action| {
        let n = nonce.entry(crypto::pubkey(k)).or_insert(0u64);
        *n += 1;
        Tx::User(SignedTx::sign(k, CHAIN, *n, a))
    };
    let mut deposits = Vec::new();
    for (i, k) in std::iter::once(&dep).chain(&traders).enumerate() {
        let owner = crypto::pubkey(k);
        let (asset, amount) = if i == 0 { (ASSET_STAKE, 1) } else { (ASSET_QUOTE, 1_000_000_000_000) };
        let msg = crypto::deposit_message(CHAIN, i as u64, &owner, asset, amount);
        let attestations = (0..3)
            .map(|v| Attestation { validator: v as u16, signature: crypto::sign(&validators[v], &msg) })
            .collect();
        deposits.push(Tx::System(SystemTx::BridgeDeposit { deposit_id: i as u64, owner, asset, amount, attestations }));
    }
    let mut height = 0;
    let mut apply = |chain: &mut Chain, txs: Vec<Tx>| {
        height += 1;
        chain.apply_block(&Block { height, timestamp_ms: height as i64 * 1_000, txs }).unwrap()
    };
    apply(&mut chain, deposits);
    let setup = vec![tx(
        &dep,
        Action::DeployDex { name: [0; 16], fee_share_bps: 0, stake: 1, oracle_authority: crypto::pubkey(&dep) },
    )];
    apply(&mut chain, setup);
    let mut setup = vec![tx(
        &dep,
        Action::AddMarket {
            dex: 0,
            config: MarketConfig {
                symbol: [0; 16],
                tick_size: 1,
                min_base_lots: 1,
                initial_margin_bps: 1_000,
                maintenance_margin_bps: 500,
                liquidation_fee_bps: 250,
                oi_cap: u64::MAX / 4,
                initial_price: 100_000,
                funding_interval: 3_600,
                max_funding_bps: 50,
            },
        },
    )];
    for k in &traders {
        setup.push(tx(k, Action::TransferToDex { dex: 0, amount: 1_000_000_000_000 }));
    }
    apply(&mut chain, setup);

    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let (blocks, per_block) = (10, 20_000);
    let mut signed = Vec::new();
    for _ in 0..blocks {
        let mut txs = Vec::with_capacity(per_block);
        for _ in 0..per_block {
            let k = &traders[rnd(traders.len() as u64) as usize];
            let side = rnd(2) as u8;
            let (kind, price) = if rnd(10) == 0 {
                (2, if side == 0 { 100_300 } else { 99_700 })
            } else {
                let off = 1 + rnd(200);
                (1, if side == 0 { 100_000 - off } else { 100_000 + off })
            };
            txs.push(tx(
                k,
                Action::PlaceOrder {
                    market: 0,
                    side,
                    kind,
                    price,
                    size: 1 + rnd(10),
                    reduce_only: false,
                    client_id: 0,
                },
            ));
        }
        signed.push(txs);
    }
    let t = Instant::now();
    let v = chain.verify_parallel(&signed[0]);
    assert!(v.iter().all(|r| r.is_ok()));
    println!("signature check alone: {:.0} tx/s", per_block as f64 / t.elapsed().as_secs_f64());
    let start = Instant::now();
    let mut ok = 0;
    for txs in signed {
        let out = apply(&mut chain, txs);
        ok += out.results.iter().filter(|r| r.is_ok()).count();
    }
    let secs = start.elapsed().as_secs_f64();
    let total = blocks * per_block;
    println!(
        "{total} signed txs in {blocks} blocks: {secs:.3}s → {:.0} tx/s ({ok} ok) on {} threads",
        total as f64 / secs,
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
}
