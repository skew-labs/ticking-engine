use mee_core::engine::MarketArgs;
use mee_core::{DEX_FROZEN, EngineError, EngineParams, MARKET_ACTIVE, MARKET_SETTLED, OrderKind, Side};
use mee_engine::*;

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

fn market_args() -> MarketArgs {
    MarketArgs {
        symbol: *b"NVDA\0\0\0\0\0\0\0\0\0\0\0\0",
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

struct Env {
    s: State,
    ev: Events,
    deployer: AccountId,
    dex: DexId,
    m: MarketId,
    /// Quote that entered minus quote that left the system.
    net_in: i128,
}

fn key(i: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..4].copy_from_slice(&i.to_le_bytes());
    k
}

impl Env {
    fn new() -> Env {
        Env::with(params(), market_args(), 0)
    }

    fn with(p: EngineParams, args: MarketArgs, share: u64) -> Env {
        let mut s = State::new(p);
        let mut ev = Vec::new();
        let deployer = s.deposit_stake(&key(9_999), 5_000).unwrap();
        let dex = s.deploy_dex(deployer, *b"skew\0\0\0\0\0\0\0\0\0\0\0\0", share, 1_000, key(9_999), &mut ev).unwrap();
        let m = s.add_market(deployer, dex, args, &mut ev).unwrap();
        Env { s, ev, deployer, dex, m, net_in: 0 }
    }

    fn trader(&mut self, id: u32, collateral: u64) -> AccountId {
        let a = self.s.deposit(&key(id), collateral).unwrap();
        self.net_in += collateral as i128;
        self.s.transfer_to_dex(a, self.dex, collateral).unwrap();
        a
    }

    fn order(
        &mut self,
        a: AccountId,
        side: Side,
        kind: OrderKind,
        price: u64,
        size: u64,
    ) -> Result<PlaceOutcome, EngineError> {
        let req = OrderRequest { market: self.m, side, kind, price, size, reduce_only: false, client_id: 0 };
        self.s.place_order(a, &req, &mut self.ev)
    }

    fn pos(&self, a: AccountId) -> Position {
        self.s.accounts[a as usize].margin(self.dex).map(|mg| mg.position(self.m)).unwrap_or_default()
    }

    fn col(&self, a: AccountId) -> i64 {
        self.s.accounts[a as usize].margin(self.dex).map_or(0, |mg| mg.collateral)
    }

    fn walk_oracle(&mut self, target: u64) {
        loop {
            let cur = self.s.markets[self.m as usize].oracle_price;
            if cur == target {
                return;
            }
            let step = cur * self.s.params.max_oracle_move_bps / 10_000;
            let next = if target > cur { (cur + step).min(target) } else { cur.saturating_sub(step).max(target) };
            self.s.now += 1;
            self.s.set_oracle(&key(9_999), self.dex, &[(self.m, next)]).unwrap();
        }
    }
}

fn check_invariants(s: &State, net_in: i128) {
    for m in &s.markets {
        let mut base = 0i64;
        let mut longs = 0u64;
        let d = &s.dexes[m.dex as usize];
        for &a in &d.members {
            let p = s.accounts[a as usize].margin(m.dex).unwrap().position(m.id);
            base += p.base;
            longs += p.base.max(0) as u64;
            if m.status == MARKET_ACTIVE {
                let bids: u64 = m.book.iter(Side::Bid).filter(|o| o.account == a).map(|o| o.remaining).sum();
                let asks: u64 = m.book.iter(Side::Ask).filter(|o| o.account == a).map(|o| o.remaining).sum();
                assert_eq!((bids, asks), (p.open_bids, p.open_asks), "open order accounting drifted (account {a})");
            }
        }
        assert_eq!(base, 0, "Σbase != 0 in market {}", m.id);
        assert_eq!(longs, m.open_interest, "open interest drifted in market {}", m.id);
        let bid = m.book.best_price(Side::Bid);
        let ask = m.book.best_price(Side::Ask);
        if let (Some(b), Some(a)) = (bid, ask) {
            assert!(b < a, "crossed book");
        }
    }
    let mut claims = s.treasury as i128;
    let mut unrecovered = 0i128;
    for a in &s.accounts {
        claims += a.balance as i128;
    }
    for d in &s.dexes {
        claims += s.dex_claims(d.id).unwrap();
        unrecovered += d.unrecovered_loss as i128;
    }
    assert!(net_in + unrecovered >= claims, "system owes {} more than it holds", claims - net_in - unrecovered);
}

#[test]
fn match_at_maker_price_with_fees() {
    let mut e = Env::with(params(), market_args(), 10_000);
    let maker = e.trader(1, 1_000_000);
    let taker = e.trader(2, 1_000_000);
    let r = e.order(maker, Side::Ask, OrderKind::Limit, 10_010, 10).unwrap();
    assert_eq!(r.resting, 10);
    let r = e.order(taker, Side::Bid, OrderKind::Limit, 10_050, 4).unwrap();
    assert_eq!((r.filled, r.filled_quote, r.order_id), (4, 40_040, None));
    assert_eq!(e.pos(taker).base, 4);
    assert_eq!(e.pos(maker).base, -4);
    assert_eq!(e.pos(maker).open_asks, 6);
    // taker 21 + 21 (100% share), maker 5 + 5
    assert_eq!(e.col(taker), 1_000_000 - 42);
    assert_eq!(e.col(maker), 1_000_000 - 10);
    assert_eq!(e.s.markets[0].open_interest, 4);
    check_invariants(&e.s, e.net_in);
}

#[test]
fn rejected_orders_change_nothing() {
    let mut e = Env::new();
    let mm = e.trader(1, 100_000_000);
    let t = e.trader(2, 10_000);
    for i in 0..5 {
        e.order(mm, Side::Ask, OrderKind::Limit, PRICE + i, 20).unwrap();
    }
    let before_book = e.s.markets[0].book.depth(Side::Ask, 10);
    let before = (e.pos(t), e.col(t), e.pos(mm), e.col(mm));
    // Would fill across levels and then fail the margin check.
    assert_eq!(e.order(t, Side::Bid, OrderKind::Limit, PRICE + 4, 50).unwrap_err(), EngineError::InsufficientMargin);
    assert_eq!(e.s.markets[0].book.depth(Side::Ask, 10), before_book);
    assert_eq!((e.pos(t), e.col(t), e.pos(mm), e.col(mm)), before);
    check_invariants(&e.s, e.net_in);
}

#[test]
fn self_trade_and_order_kinds() {
    let mut e = Env::new();
    let a = e.trader(1, 10_000_000);
    let b = e.trader(2, 10_000_000);
    e.order(a, Side::Ask, OrderKind::Limit, 10_010, 5).unwrap();
    e.order(b, Side::Ask, OrderKind::Limit, 10_020, 5).unwrap();
    assert_eq!(e.order(a, Side::Bid, OrderKind::PostOnly, 10_010, 1).unwrap_err(), EngineError::PostOnlyWouldCross);
    let r = e.order(a, Side::Bid, OrderKind::ImmediateOrCancel, 10_030, 8).unwrap();
    assert_eq!(r.filled, 5, "own ask cancelled, then b's ask filled; rest dropped");
    assert_eq!(e.pos(a).open_asks, 0);
    assert!(e.s.markets[0].book.is_empty());
    check_invariants(&e.s, e.net_in);
}

#[test]
fn cancel_and_cancel_all() {
    let mut e = Env::new();
    let a = e.trader(1, 10_000_000);
    let b = e.trader(2, 10_000_000);
    let id = e.order(a, Side::Bid, OrderKind::Limit, 9_900, 5).unwrap().order_id.unwrap();
    e.order(a, Side::Bid, OrderKind::Limit, 9_800, 5).unwrap();
    assert_eq!(e.s.cancel(b, e.m, id, &mut e.ev).unwrap_err(), EngineError::NotOrderOwner);
    e.s.cancel(a, e.m, id, &mut e.ev).unwrap();
    assert_eq!(e.pos(a).open_bids, 5);
    assert_eq!(e.s.cancel_all(a, e.m, CancelReason::User, &mut e.ev).unwrap(), 1);
    assert_eq!(e.pos(a).open_bids, 0);
    check_invariants(&e.s, e.net_in);
}

#[test]
fn bankrupt_position_is_auto_deleveraged() {
    let mut e = Env::with(EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, ..params() }, market_args(), 0);
    let mm = e.trader(1, 1_000_000);
    let t = e.trader(2, 10_000);
    let liq = e.trader(3, 1_000_000);
    e.s.deposit(&key(4), 1_000).unwrap();
    e.net_in += 1_000;
    e.s.fund_insurance(e.s.account_id(&key(4)).unwrap(), e.dex, 1_000).unwrap();
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    assert_eq!(e.s.liquidate(liq, t, e.m, 0, &mut e.ev).unwrap_err(), EngineError::NotLiquidatable);
    assert!(e.s.liquidatable(e.dex).is_empty());

    // Gap to 8_000: equity 10_000 − 20_000 = −10_000, insurance only 1_000.
    e.walk_oracle(8_000);
    assert_eq!(e.s.liquidatable(e.dex), vec![t]);
    e.s.liquidate(liq, t, e.m, 0, &mut e.ev).unwrap();
    // Bankruptcy price: 10_000 + 10·p − 100_000 = 0 → p = 9_000. The short
    // (the only opposite position) buys back there.
    assert_eq!(e.ev.last(), Some(&Event::Adl { market: 0, target: t, counterparty: mm, size: 10, price: 9_000 }));
    assert_eq!((e.pos(t).base, e.pos(mm).base, e.pos(liq).base), (0, 0, 0));
    assert_eq!(e.col(t), 0);
    assert_eq!(e.col(mm), 1_000_000 + 10_000);
    let d = &e.s.dexes[0];
    assert_eq!((d.insurance, d.socialized_loss, d.unrecovered_loss), (1_000, 0, 0), "nobody else pays");
    check_invariants(&e.s, e.net_in);
}

#[test]
fn bad_debt_at_settlement_uses_insurance_then_socializes() {
    let mut e = Env::with(EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, ..params() }, market_args(), 0);
    let mm = e.trader(1, 1_000_000);
    let t = e.trader(2, 10_000);
    let other = e.trader(3, 1_000_000);
    e.s.deposit(&key(4), 1_000).unwrap();
    e.net_in += 1_000;
    e.s.fund_insurance(e.s.account_id(&key(4)).unwrap(), e.dex, 1_000).unwrap();
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    e.walk_oracle(8_000);
    e.s.halt_market(Some(e.deployer), e.m, 8_000, &mut e.ev).unwrap();
    let d = &e.s.dexes[0];
    // 9_000 owed after insurance; pro-rata shares round up by at most an atom
    // per member, so the venue is never short.
    assert_eq!((d.insurance, d.unrecovered_loss), (0, 0));
    assert!((9_000..=9_002).contains(&d.socialized_loss), "{}", d.socialized_loss);
    let (cm, co) = (e.col(mm), e.col(other));
    assert_eq!(cm + co, 1_020_000 + 1_000_000 - d.socialized_loss);
    check_invariants(&e.s, e.net_in);
}

#[test]
fn failed_liquidation_restores_state() {
    let mut e = Env::new();
    let mm = e.trader(1, 100_000_000);
    let t = e.trader(2, 12_000);
    let poor = e.trader(3, 100);
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    e.walk_oracle(9_200);
    let before = (e.pos(t), e.col(t), e.pos(poor), e.col(poor), e.s.markets[0].open_interest);
    assert_eq!(e.s.liquidate(poor, t, e.m, 0, &mut e.ev).unwrap_err(), EngineError::InsufficientMargin);
    assert_eq!((e.pos(t), e.col(t), e.pos(poor), e.col(poor), e.s.markets[0].open_interest), before);
    // A brand-new liquidator margin is rolled back too.
    let fresh = e.s.deposit(&key(50), 1).unwrap();
    let members = e.s.dexes[0].members.len();
    assert!(e.s.liquidate(fresh, t, e.m, 0, &mut e.ev).is_err());
    assert_eq!(e.s.dexes[0].members.len(), members);
    assert!(e.s.accounts[fresh as usize].margin(e.dex).is_none());
    check_invariants(&e.s, e.net_in + 1);
}

#[test]
fn funding_runs_in_begin_block() {
    let p = EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, max_oracle_staleness: 10_000, ..params() };
    let mut e = Env::with(p, market_args(), 0);
    let long = e.trader(1, 1_000_000);
    let short = e.trader(2, 1_000_000);
    let mm = e.trader(3, 1_000_000);
    e.order(short, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(long, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    e.order(mm, Side::Bid, OrderKind::Limit, 10_040, 1).unwrap();
    e.order(mm, Side::Ask, OrderKind::Limit, 10_060, 1).unwrap();
    e.s.begin_block(1_800, &mut e.ev).unwrap();
    assert_eq!(e.s.markets[0].cum_funding, 0, "not due yet");
    let before = (e.s.health(long, e.dex, None).unwrap().equity, e.s.health(short, e.dex, None).unwrap().equity);
    e.s.now = 3_600;
    e.s.set_oracle(&key(9_999), e.dex, &[(e.m, PRICE)]).unwrap();
    e.s.begin_block(3_600, &mut e.ev).unwrap();
    // The premium held at +40 bps for the whole interval, so its TWAP is 40.
    assert_eq!(e.ev.last(), Some(&Event::Funding { market: 0, rate_bps: 40 }));
    let after = (e.s.health(long, e.dex, None).unwrap().equity, e.s.health(short, e.dex, None).unwrap().equity);
    // 40 bps of 10_000 per lot on 10 lots: the long pays 400, the short gets it.
    assert_eq!((after.0 - before.0, after.1 - before.1), (-400, 400));
    check_invariants(&e.s, e.net_in);
}

#[test]
fn hip3_dexes_are_isolated() {
    let mut e = Env::with(EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, ..params() }, market_args(), 0);
    // A second deployer with its own DEX and oracle.
    let d2 = e.s.deposit_stake(&key(8_888), 2_000).unwrap();
    let dex2 = e.s.deploy_dex(d2, [1; 16], 0, 1_000, key(8_888), &mut e.ev).unwrap();
    let m2 = e.s.add_market(d2, dex2, market_args(), &mut e.ev).unwrap();
    assert_eq!(e.s.add_market(e.deployer, dex2, market_args(), &mut e.ev).unwrap_err(), EngineError::NotOrderOwner);
    assert_eq!(e.s.set_oracle(&key(9_999), dex2, &[(m2, PRICE)]).unwrap_err(), EngineError::NotOrderOwner);

    let a = e.s.deposit(&key(1), 2_000_000).unwrap();
    let b = e.s.deposit(&key(2), 2_000_000).unwrap();
    e.net_in += 4_000_000;
    for x in [a, b] {
        e.s.transfer_to_dex(x, e.dex, 1_000_000).unwrap();
        e.s.transfer_to_dex(x, dex2, 1_000_000).unwrap();
    }
    let req =
        |side, kind| OrderRequest { market: m2, side, kind, price: PRICE, size: 50, reduce_only: false, client_id: 0 };
    e.s.place_order(a, &req(Side::Ask, OrderKind::Limit), &mut e.ev).unwrap();
    e.s.place_order(b, &req(Side::Bid, OrderKind::ImmediateOrCancel), &mut e.ev).unwrap();
    // Exposure in dex2 does not touch margin in dex 0.
    assert_eq!(e.s.health(b, e.dex, None).unwrap().initial, 0);
    assert!(e.s.health(b, dex2, None).unwrap().initial > 0);

    // Guardians slash and freeze dex2; governance winds it down.
    e.s.slash(dex2, 5_000, true, &mut e.ev).unwrap();
    assert_eq!(e.s.dexes[dex2 as usize].status, DEX_FROZEN);
    assert_eq!(e.s.halt_market(Some(d2), m2, PRICE, &mut e.ev).unwrap_err(), EngineError::NotOrderOwner);
    e.s.halt_market(None, m2, PRICE, &mut e.ev).unwrap();
    assert_eq!(e.s.markets[m2 as usize].status, MARKET_SETTLED);
    assert_eq!(e.s.slashed_stake, 500);
    check_invariants(&e.s, e.net_in);
}

#[test]
fn deployer_lifecycle() {
    let mut e = Env::new();
    let a = e.trader(1, 1_000_000);
    let b = e.trader(2, 1_000_000);
    e.order(a, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(b, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 4).unwrap();
    assert_eq!(e.s.request_unstake(e.deployer, e.dex).unwrap_err(), EngineError::MarketsOpen);
    e.s.halt_market(Some(e.deployer), e.m, 10_200, &mut e.ev).unwrap();
    assert_eq!(e.pos(b).base, 0);
    assert_eq!(e.pos(a).open_asks, 0);
    assert_eq!(e.s.claim_deployer_fees(e.deployer, e.dex).unwrap(), 0);
    e.s.sweep_protocol_fees(e.dex).unwrap();
    assert_eq!(e.s.request_unstake(e.deployer, e.dex).unwrap_err(), EngineError::StakeLocked);
    e.s.now = 100;
    e.s.request_unstake(e.deployer, e.dex).unwrap();
    assert_eq!(e.s.complete_unstake(e.deployer, e.dex).unwrap_err(), EngineError::UnstakeDelay);
    e.s.now = 110;
    assert_eq!(e.s.complete_unstake(e.deployer, e.dex).unwrap(), 1_000);
    assert_eq!(e.s.accounts[e.deployer as usize].stake_balance, 5_000);
    check_invariants(&e.s, e.net_in);
}

#[test]
fn transfer_out_respects_margin() {
    let mut e = Env::new();
    let mm = e.trader(1, 100_000_000);
    let t = e.trader(2, 100_000);
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 100).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 50).unwrap();
    assert_eq!(e.s.transfer_from_dex(t, e.dex, 60_000).unwrap_err(), EngineError::InsufficientMargin);
    e.s.transfer_from_dex(t, e.dex, 40_000).unwrap();
    e.s.withdraw(t, 40_000).unwrap();
    e.net_in -= 40_000;
    assert_eq!(e.s.withdraw(t, 1).unwrap_err(), EngineError::InsufficientCollateral);
    check_invariants(&e.s, e.net_in);
}

// ---------------------------------------------------------------------------
// Randomized: atomicity and invariants
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn fingerprint(s: &State) -> String {
    // Cheap but thorough: accounts, dexes, market scalars and book depth.
    let mut out = format!("{:?}{:?}{}{}", s.accounts, s.dexes, s.next_order_id, s.treasury);
    for m in &s.markets {
        out += &format!(
            "{} {} {} {} {:?} {:?}",
            m.open_interest,
            m.oracle_price,
            m.cum_funding,
            m.last_trade_price,
            m.book.depth(Side::Bid, 1_000),
            m.book.depth(Side::Ask, 1_000)
        );
    }
    out
}

#[test]
fn random_actions_are_atomic_and_keep_invariants() {
    for seed in 1..=8u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut e = Env::with(params(), MarketArgs { oi_cap: 500, ..market_args() }, rng.below(30_000));
        let traders: Vec<AccountId> = (0..10).map(|i| e.trader(i + 1, 20_000 + rng.below(500_000))).collect();
        let liq = e.trader(100, 50_000_000);
        let (mut ok, mut rejected) = (0, 0);
        for step in 0..4_000 {
            let t = traders[rng.below(traders.len() as u64) as usize];
            let oracle = e.s.markets[0].oracle_price;
            let price = (oracle as i64 + rng.below(400) as i64 - 200).max(1) as u64;
            let side = if rng.below(2) == 0 { Side::Bid } else { Side::Ask };
            let checkpoint = if step % 7 == 0 { Some(fingerprint(&e.s)) } else { None };
            let mut ev = Vec::new();
            let r: Result<(), EngineError> = match rng.below(12) {
                0..=4 => {
                    let kind = OrderKind::from_u8(rng.below(3) as u8).unwrap();
                    let req = OrderRequest {
                        market: 0,
                        side,
                        kind,
                        price,
                        size: 1 + rng.below(25),
                        reduce_only: false,
                        client_id: 0,
                    };
                    e.s.place_order(t, &req, &mut ev).map(|_| ())
                }
                5 => {
                    let req = OrderRequest {
                        market: 0,
                        side,
                        kind: OrderKind::ImmediateOrCancel,
                        price,
                        size: 1 + rng.below(25),
                        reduce_only: true,
                        client_id: 0,
                    };
                    e.s.place_order(t, &req, &mut ev).map(|_| ())
                }
                6 => {
                    let ids: Vec<u64> = e.s.markets[0]
                        .book
                        .iter(Side::Bid)
                        .chain(e.s.markets[0].book.iter(Side::Ask))
                        .map(|o| o.id)
                        .collect();
                    match ids.get(rng.below(ids.len().max(1) as u64) as usize) {
                        Some(&id) => e.s.cancel(t, 0, id, &mut ev),
                        None => Ok(()),
                    }
                }
                7 => {
                    let next = (oracle as i64 + rng.below(800) as i64 - 400).max(1_000) as u64;
                    e.s.now += 1;
                    e.s.set_oracle(&key(9_999), e.dex, &[(0, next)])
                }
                8 => {
                    let amt = 1 + rng.below(30_000);
                    e.s.transfer_from_dex(t, e.dex, amt)
                        .and_then(|_| e.s.withdraw(t, amt))
                        .map(|_| e.net_in -= amt as i128)
                }
                9 => {
                    let amt = 1 + rng.below(30_000);
                    let owner = e.s.accounts[t as usize].owner;
                    e.s.deposit(&owner, amt).unwrap();
                    e.net_in += amt as i128;
                    e.s.transfer_to_dex(t, e.dex, amt)
                }
                10 => e.s.liquidate(liq, t, 0, rng.below(5), &mut ev).map(|_| ()),
                _ => {
                    let now = e.s.now + 3_600;
                    e.s.now = now;
                    let cur = e.s.markets[0].oracle_price;
                    e.s.set_oracle(&key(9_999), e.dex, &[(0, cur)]).and_then(|_| e.s.begin_block(now, &mut ev))
                }
            };
            match r {
                Ok(()) => ok += 1,
                Err(_) => {
                    rejected += 1;
                    if let Some(fp) = checkpoint {
                        assert_eq!(fp, fingerprint(&e.s), "rejected action mutated state (seed {seed}, step {step})");
                    }
                }
            }
            check_invariants(&e.s, e.net_in);
        }
        assert!(ok > 1_000 && rejected > 100, "seed {seed}: ok {ok}, rejected {rejected}");
    }
}

#[test]
fn protocol_market_pays_creator_and_takes_attested_prices() {
    let mut e = Env::with(params(), market_args(), 0);
    let creator = e.s.deposit(&key(500), 0).unwrap();
    let pdex = e.s.create_protocol_dex(*b"launch\0\0\0\0\0\0\0\0\0\0", &mut e.ev);
    let m = e.s.add_protocol_market(pdex, market_args(), creator, 10_000, &mut e.ev).unwrap();
    // No deployer key can push prices; only the attested path.
    assert_eq!(e.s.set_oracle(&key(9_999), pdex, &[(m, PRICE)]).unwrap_err(), EngineError::NotOrderOwner);
    e.s.now += 1;
    e.s.set_oracle_attested(pdex, &[(m, PRICE + 100)]).unwrap();
    assert_eq!(e.s.set_oracle_attested(e.dex, &[(e.m, PRICE)]).unwrap_err(), EngineError::NotOrderOwner);
    e.s.set_oi_cap_attested(m, 5).unwrap();

    let a = e.s.deposit(&key(1), 1_000_000).unwrap();
    let b = e.s.deposit(&key(2), 1_000_000).unwrap();
    e.net_in += 2_000_000;
    e.s.transfer_to_dex(a, pdex, 1_000_000).unwrap();
    e.s.transfer_to_dex(b, pdex, 1_000_000).unwrap();
    let req = |side, kind, size| OrderRequest {
        market: m,
        side,
        kind,
        price: PRICE + 100,
        size,
        reduce_only: false,
        client_id: 0,
    };
    e.s.place_order(a, &req(Side::Ask, OrderKind::Limit, 10), &mut e.ev).unwrap();
    assert_eq!(
        e.s.place_order(b, &req(Side::Bid, OrderKind::ImmediateOrCancel, 6), &mut e.ev).unwrap_err(),
        EngineError::OpenInterestCap
    );
    e.s.place_order(b, &req(Side::Bid, OrderKind::ImmediateOrCancel, 5), &mut e.ev).unwrap();
    // notional 50_500: taker 26 + maker 6 protocol; creator gets the same again (100% share).
    assert_eq!(e.s.accounts[creator as usize].balance, 26 + 6);
    assert_eq!(e.s.dexes[pdex as usize].deployer_fees, 0);
    check_invariants(&e.s, e.net_in);
}

#[test]
fn modify_keeps_priority_when_shrinking_and_restores_on_failure() {
    let mut e = Env::new();
    let (a, b, c) = (e.trader(1, 100_000), e.trader(2, 100_000), e.trader(3, 1_000_000));
    let ida = e.order(a, Side::Ask, OrderKind::Limit, PRICE + 10, 5).unwrap().order_id.unwrap();
    let idb = e.order(b, Side::Ask, OrderKind::Limit, PRICE + 10, 5).unwrap().order_id.unwrap();
    let book_ids = |e: &Env| e.s.markets[e.m as usize].book.iter(Side::Ask).map(|o| o.id).collect::<Vec<_>>();

    // Shrinking at the same price: still first in the queue.
    let r = e.s.modify_order(a, e.m, ida, PRICE + 10, 3, OrderKind::Limit, false, &mut e.ev).unwrap();
    assert_eq!((r.order_id, r.resting), (Some(ida), 3));
    assert_eq!(book_ids(&e), vec![ida, idb]);
    assert_eq!(e.pos(a).open_asks, 3);

    // A replace that fails (too large for a's margin) changes nothing at all,
    // not even a's place in the queue.
    let before = format!("{:?}{:?}", book_ids(&e), e.s.accounts[a as usize]);
    let digest = e.s.markets[e.m as usize].book.digest();
    let err = e.s.modify_order(a, e.m, ida, PRICE + 20, 5_000, OrderKind::Limit, false, &mut e.ev);
    assert_eq!(err, Err(EngineError::InsufficientMargin));
    assert_eq!(format!("{:?}{:?}", book_ids(&e), e.s.accounts[a as usize]), before);
    assert_eq!(e.s.markets[e.m as usize].book.digest(), digest, "book commitment restored");
    // Someone else's order can't be modified.
    let err = e.s.modify_order(b, e.m, ida, PRICE + 10, 1, OrderKind::Limit, false, &mut e.ev);
    assert_eq!(err, Err(EngineError::NotOrderOwner));

    // Repricing moves it: a new id at the back of the new level.
    let r = e.s.modify_order(a, e.m, ida, PRICE + 5, 4, OrderKind::Limit, false, &mut e.ev).unwrap();
    let new_id = r.order_id.unwrap();
    assert_ne!(new_id, ida);
    assert_eq!(book_ids(&e), vec![new_id, idb]);
    assert_eq!(e.pos(a).open_asks, 4);
    // A taker hits the repriced order first.
    e.order(c, Side::Bid, OrderKind::ImmediateOrCancel, PRICE + 10, 6).unwrap();
    assert_eq!(e.pos(a).base, -4);
    assert_eq!(e.pos(b).base, -2);
}
