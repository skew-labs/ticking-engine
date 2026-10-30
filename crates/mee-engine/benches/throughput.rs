//! Single-thread throughput on a market-making style flow:
//! 60% passive limit orders around mid, 30% cancels, 10% crossing IOCs.
//! Every operation runs the full checks (margin, OI, fees, funding), and the
//! state is committed (account Merkle tree, book digests, roots) every
//! `BLOCK` operations (default 5,000), as a chain block would.
//!
//! `cargo bench -p mee-engine`

use std::time::Instant;

use mee_core::engine::MarketArgs;
use mee_core::{EngineParams, OrderKind, Side};
use mee_engine::*;

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..4].copy_from_slice(&i.to_le_bytes());
    k
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn main() {
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
    let mut s = State::new(params);
    let mut ev = Vec::with_capacity(1 << 20);
    let deployer = s.deposit_stake(&key(u32::MAX), 1).unwrap();
    let dex = s.deploy_dex(deployer, [0; 16], 10_000, 1, key(u32::MAX), &mut ev).unwrap();
    let args = MarketArgs {
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
    };
    let m = s.add_market(deployer, dex, args, &mut ev).unwrap();

    let n_accounts = 1_000u32;
    let accounts: Vec<AccountId> = (0..n_accounts)
        .map(|i| {
            let a = s.deposit(&key(i), 1_000_000_000_000).unwrap();
            s.transfer_to_dex(a, dex, 1_000_000_000_000).unwrap();
            a
        })
        .collect();

    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut live: Vec<(AccountId, OrderId)> = Vec::new();
    let n_ops: u64 = std::env::var("OPS").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000_000);
    let (mut places, mut cancels, mut takes, mut fills, mut errors) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let block: u64 = std::env::var("BLOCK").ok().and_then(|v| v.parse().ok()).unwrap_or(5_000);
    s.commit();
    let start = Instant::now();
    for op in 0..n_ops {
        if op % block == 0 {
            s.commit();
        }
        let a = accounts[rng.below(n_accounts as u64) as usize];
        let roll = rng.below(100);
        let mid = 100_000u64;
        if roll < 60 || live.is_empty() {
            let side = if rng.below(2) == 0 { Side::Bid } else { Side::Ask };
            let offset = 1 + rng.below(200);
            let price = if side == Side::Bid { mid - offset } else { mid + offset };
            let req = OrderRequest {
                market: m,
                side,
                kind: OrderKind::PostOnly,
                price,
                size: 1 + rng.below(10),
                reduce_only: false,
                client_id: 0,
            };
            match s.place_order(a, &req, &mut ev) {
                Ok(o) => {
                    if let Some(id) = o.order_id {
                        live.push((a, id));
                    }
                    places += 1;
                }
                Err(_) => errors += 1,
            }
        } else if roll < 90 {
            let i = rng.below(live.len() as u64) as usize;
            let (owner, id) = live.swap_remove(i);
            if s.cancel(owner, m, id, &mut ev).is_ok() {
                cancels += 1;
            }
        } else {
            let side = if rng.below(2) == 0 { Side::Bid } else { Side::Ask };
            let price = if side == Side::Bid { mid + 300 } else { mid - 300 };
            let req = OrderRequest {
                market: m,
                side,
                kind: OrderKind::ImmediateOrCancel,
                price,
                size: 1 + rng.below(30),
                reduce_only: false,
                client_id: 0,
            };
            match s.place_order(a, &req, &mut ev) {
                Ok(_) => takes += 1,
                Err(_) => errors += 1,
            }
        }
        if ev.len() > 1 << 19 {
            fills += ev.iter().filter(|e| matches!(e, Event::Fill { .. })).count() as u64;
            ev.clear();
        }
    }
    s.commit();
    let elapsed = start.elapsed();
    fills += ev.iter().filter(|e| matches!(e, Event::Fill { .. })).count() as u64;
    let rate = n_ops as f64 / elapsed.as_secs_f64();
    println!("ops: {n_ops} in {:.3}s → {:.0} ops/s ({:.0} ns/op)", elapsed.as_secs_f64(), rate, 1e9 / rate);
    println!("places {places}, cancels {cancels}, takers {takes}, fills {fills}, rejected {errors}");
    println!("resting orders at end: {}", s.markets[m as usize].book.len());
}
