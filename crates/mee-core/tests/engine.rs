use mee_core::engine::{self, MarketArgs, OrderRequest};
use mee_core::margin::health;
use mee_core::math::FUNDING_SCALE;
use mee_core::*;

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
        max_fills_per_order: 32,
        liquidation_insurance_share_bps: 5_000,
        min_deployer_stake: 1_000,
        min_stake_hold_secs: 100,
        unstake_delay_secs: 10,
        min_initial_margin_bps: 500,
    }
}

fn market_args() -> MarketArgs {
    MarketArgs {
        symbol: *b"TSLA-PERP\0\0\0\0\0\0\0",
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
    dex: Box<DexState>,
    book: Box<Book>,
    p: EngineParams,
    now: i64,
    /// What the token vault would hold.
    vault: i128,
}

impl Env {
    fn new() -> Env {
        Env::with(params(), market_args(), 0)
    }

    fn with(p: EngineParams, args: MarketArgs, fee_share_bps: u64) -> Env {
        let mut dex: Box<DexState> = bytemuck::zeroed_box();
        let mut book: Box<Book> = bytemuck::zeroed_box();
        governance::deploy(&mut dex, &p, 1_000, fee_share_bps, 0).unwrap();
        let m = engine::add_market(&mut dex, &p, &[7; 32], &args, 0).unwrap();
        book.market_index = m as u64;
        Env { dex, book, p, now: 0, vault: 0 }
    }

    fn trader(&mut self, id: u8, collateral: u64) -> usize {
        let t = engine::register_trader(&mut self.dex, &[id; 32]).unwrap();
        if collateral > 0 {
            engine::deposit(&mut self.dex, t, collateral).unwrap();
            self.vault += collateral as i128;
        }
        t
    }

    /// Runs `f`, restoring state on error the way the Solana runtime would.
    fn atomic<T>(&mut self, f: impl FnOnce(&mut Env) -> Result<T>) -> Result<T> {
        let snapshot = (self.dex.clone(), self.book.clone(), self.vault);
        let r = f(self);
        if r.is_err() {
            (self.dex, self.book, self.vault) = snapshot;
        }
        r
    }

    fn order(&mut self, t: usize, side: Side, kind: OrderKind, price: u64, lots: u64) -> Result<engine::PlaceOutcome> {
        let req =
            OrderRequest { market: 0, side, kind, price, base_lots: lots, reduce_only: false, client_order_id: 0 };
        self.atomic(|e| engine::place_order(&mut e.dex, &mut e.book, &e.p, t, &req, e.now))
    }

    fn reduce(&mut self, t: usize, side: Side, price: u64, lots: u64) -> Result<engine::PlaceOutcome> {
        let req = OrderRequest {
            market: 0,
            side,
            kind: OrderKind::ImmediateOrCancel,
            price,
            base_lots: lots,
            reduce_only: true,
            client_order_id: 0,
        };
        self.atomic(|e| engine::place_order(&mut e.dex, &mut e.book, &e.p, t, &req, e.now))
    }

    fn pos(&self, t: usize) -> Position {
        self.dex.traders[t].positions[0]
    }

    fn col(&self, t: usize) -> i64 {
        self.dex.traders[t].collateral
    }

    fn oracle(&mut self, price: u64) {
        engine::set_oracle(&mut self.dex, &self.p, 0, price, self.now).unwrap();
    }

    /// Walks the oracle to `target` in allowed steps.
    fn walk_oracle(&mut self, target: u64) {
        loop {
            let cur = self.dex.markets[0].oracle_price;
            if cur == target {
                return;
            }
            let step = cur * self.p.max_oracle_move_bps / 10_000;
            let next = if target > cur { (cur + step).min(target) } else { cur.saturating_sub(step).max(target) };
            self.now += 1;
            self.oracle(next);
        }
    }

    fn check_invariants(&self) {
        check_invariants(&self.dex, &[&self.book], self.vault);
    }
}

fn check_invariants(dex: &DexState, books: &[&Book], vault: i128) {
    for (m, market) in dex.markets.iter().enumerate().take(dex.header.market_count as usize) {
        let base: i64 = dex.traders.iter().map(|t| t.positions[m].base).sum();
        assert_eq!(base, 0, "Σbase != 0 in market {m}");
        let longs: u64 = dex.traders.iter().map(|t| t.positions[m].base.max(0) as u64).sum();
        assert_eq!(longs, market.open_interest, "open interest drifted");
        let book = books.iter().find(|b| b.market_index as usize == m).unwrap();
        for (side, s) in [(Side::Bid, &book.bids), (Side::Ask, &book.asks)] {
            let orders = s.orders();
            for w in orders.windows(2) {
                assert!(mee_core::book::better(side, &w[1], &w[0]), "book side unsorted");
            }
            for (i, t) in dex.traders.iter().enumerate() {
                let resting: u64 = orders.iter().filter(|o| o.trader as usize == i).map(|o| o.base_lots).sum();
                let open = if side == Side::Bid { t.positions[m].open_bids } else { t.positions[m].open_asks };
                assert_eq!(resting, open, "open order accounting drifted for trader {i}");
            }
        }
        if market.status == MARKET_ACTIVE {
            assert_eq!(market.best_bid, book.bids.best().map_or(0, |o| o.price));
            assert_eq!(market.best_ask, book.asks.best().map_or(0, |o| o.price));
        }
    }
    let claims = engine::total_claims(dex).unwrap();
    let slack = vault + dex.header.unrecovered_loss as i128 - claims;
    assert!(slack >= 0, "vault short by {}", -slack);
}

#[test]
fn resting_order_fills_at_maker_price_and_mirrors_positions() {
    let mut e = Env::new();
    let maker = e.trader(1, 1_000_000);
    let taker = e.trader(2, 1_000_000);

    let r = e.order(maker, Side::Ask, OrderKind::Limit, 10_010, 10).unwrap();
    assert_eq!(r.resting_lots, 10);
    assert_eq!(e.dex.markets[0].best_ask, 10_010);
    assert_eq!(e.pos(maker).open_asks, 10);

    let r = e.order(taker, Side::Bid, OrderKind::Limit, 10_050, 4).unwrap();
    assert_eq!(r.filled_lots, 4);
    assert_eq!(r.filled_quote, 4 * 10_010);
    assert_eq!(r.order_id, None);

    assert_eq!(e.pos(taker).base, 4);
    assert_eq!(e.pos(maker).base, -4);
    assert_eq!(e.pos(taker).quote.get(), -40_040);
    assert_eq!(e.pos(maker).quote.get(), 40_040);
    assert_eq!(e.pos(maker).open_asks, 6);
    assert_eq!(e.dex.markets[0].open_interest, 4);
    assert_eq!(e.dex.markets[0].last_trade_price, 10_010);

    // taker 5 bps of 40_040 = 20.02 → 21, maker 1 bp = 4.004 → 5
    assert_eq!(e.col(taker), 1_000_000 - 21);
    assert_eq!(e.col(maker), 1_000_000 - 5);
    assert_eq!(e.dex.header.protocol_fees, 26);
    e.check_invariants();
}

#[test]
fn deployer_fee_share_is_charged_on_top() {
    // 100% share: deployer earns as much as the protocol.
    let mut e = Env::with(params(), market_args(), 10_000);
    let maker = e.trader(1, 1_000_000);
    let taker = e.trader(2, 1_000_000);
    e.order(maker, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(taker, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    // notional 100_000: taker 50 + 50, maker 10 + 10
    assert_eq!(e.col(taker), 1_000_000 - 100);
    assert_eq!(e.col(maker), 1_000_000 - 20);
    assert_eq!(e.dex.header.protocol_fees, 60);
    assert_eq!(e.dex.header.deployer_fees, 60);
    assert_eq!(engine::take_deployer_fees(&mut e.dex), 60);
    e.vault -= 60;
    e.check_invariants();
}

#[test]
fn price_time_priority_and_sweep() {
    let mut e = Env::new();
    let a = e.trader(1, 1_000_000);
    let b = e.trader(2, 1_000_000);
    let c = e.trader(3, 1_000_000);
    let taker = e.trader(4, 1_000_000);
    e.order(a, Side::Ask, OrderKind::Limit, 10_020, 5).unwrap();
    e.order(b, Side::Ask, OrderKind::Limit, 10_010, 5).unwrap();
    e.order(c, Side::Ask, OrderKind::Limit, 10_010, 5).unwrap();

    let r = e.order(taker, Side::Bid, OrderKind::ImmediateOrCancel, 10_020, 12).unwrap();
    assert_eq!(r.fills, 3);
    assert_eq!(e.pos(b).base, -5, "earlier order at the best price fills first");
    assert_eq!(e.pos(c).base, -5);
    assert_eq!(e.pos(a).base, -2);
    assert_eq!(e.pos(taker).base, 12);
    assert_eq!(r.filled_quote, 10 * 10_010 + 2 * 10_020);
    assert_eq!(e.book.asks.len(), 1);
    e.check_invariants();
}

#[test]
fn post_only_and_ioc() {
    let mut e = Env::new();
    let maker = e.trader(1, 1_000_000);
    let taker = e.trader(2, 1_000_000);
    e.order(maker, Side::Ask, OrderKind::Limit, 10_010, 5).unwrap();
    assert_eq!(e.order(taker, Side::Bid, OrderKind::PostOnly, 10_010, 1).unwrap_err(), EngineError::PostOnlyWouldCross);

    let r = e.order(taker, Side::Bid, OrderKind::ImmediateOrCancel, 10_010, 8).unwrap();
    assert_eq!(r.filled_lots, 5);
    assert_eq!(r.resting_lots, 0);
    assert!(e.book.bids.is_empty());
    e.check_invariants();
}

#[test]
fn self_trade_cancels_resting_order() {
    let mut e = Env::new();
    let t = e.trader(1, 1_000_000);
    let other = e.trader(2, 1_000_000);
    e.order(t, Side::Ask, OrderKind::Limit, 10_010, 5).unwrap();
    e.order(other, Side::Ask, OrderKind::Limit, 10_020, 5).unwrap();
    let r = e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, 10_020, 5).unwrap();
    assert_eq!(e.pos(t).open_asks, 0);
    assert_eq!(r.filled_lots, 5);
    assert_eq!(e.pos(other).base, -5);
    assert_eq!(e.pos(t).base, 5);
    e.check_invariants();
}

#[test]
fn closing_realizes_pnl() {
    let mut e = Env::with(EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, ..params() }, market_args(), 0);
    let mm = e.trader(1, 10_000_000);
    let t = e.trader(2, 1_000_000);
    e.order(mm, Side::Ask, OrderKind::Limit, 10_000, 10).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, 10_000, 10).unwrap();
    e.walk_oracle(10_400);
    e.order(mm, Side::Bid, OrderKind::Limit, 10_400, 10).unwrap();
    e.order(t, Side::Ask, OrderKind::ImmediateOrCancel, 10_400, 10).unwrap();
    assert_eq!(e.pos(t), Position { last_funding: e.pos(t).last_funding, ..Position::default() });
    assert_eq!(e.col(t), 1_000_000 + 4_000);
    assert_eq!(e.col(mm), 10_000_000 - 4_000);
    e.check_invariants();
}

#[test]
fn initial_margin_blocks_oversized_orders() {
    let mut e = Env::new();
    let mm = e.trader(1, 100_000_000);
    let t = e.trader(2, 10_000);
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 100).unwrap();
    // 10% IM on 10 lots × 10_000 = 10_000 plus fees: over the limit.
    assert_eq!(
        e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap_err(),
        EngineError::InsufficientMargin
    );
    // Resting orders count as worst case too.
    assert_eq!(e.order(t, Side::Bid, OrderKind::Limit, PRICE - 10, 11).unwrap_err(), EngineError::InsufficientMargin);
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 9).unwrap();
    e.check_invariants();
}

#[test]
fn withdraw_respects_margin() {
    let mut e = Env::new();
    let mm = e.trader(1, 100_000_000);
    let t = e.trader(2, 100_000);
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 100).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 50).unwrap(); // IM 50_000
    assert_eq!(
        engine::withdraw(&mut e.dex.clone(), &e.p, t, 60_000, e.now).unwrap_err(),
        EngineError::InsufficientMargin
    );
    engine::withdraw(&mut e.dex, &e.p, t, 40_000, e.now).unwrap();
    e.vault -= 40_000;
    assert_eq!(
        engine::withdraw(&mut e.dex.clone(), &e.p, t, 1_000_000, e.now).unwrap_err(),
        EngineError::InsufficientCollateral
    );
    e.check_invariants();
}

#[test]
fn reduce_only_clamps_and_never_rests() {
    let mut e = Env::new();
    let mm = e.trader(1, 100_000_000);
    let t = e.trader(2, 1_000_000);
    assert_eq!(e.reduce(t, Side::Ask, PRICE, 1).unwrap_err(), EngineError::NoPosition);
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 5).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 5).unwrap();
    e.order(mm, Side::Bid, OrderKind::Limit, PRICE - 1, 50).unwrap();
    let r = e.reduce(t, Side::Ask, PRICE - 1, 50).unwrap();
    assert_eq!(r.filled_lots, 5);
    assert_eq!(e.pos(t).base, 0);
    assert_eq!(e.pos(t).open_asks, 0);
    e.check_invariants();
}

#[test]
fn open_interest_cap() {
    let mut e = Env::with(params(), MarketArgs { oi_cap: 10, ..market_args() }, 0);
    let a = e.trader(1, 100_000_000);
    let b = e.trader(2, 100_000_000);
    e.order(a, Side::Ask, OrderKind::Limit, PRICE, 20).unwrap();
    e.order(b, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    assert_eq!(
        e.order(b, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 1).unwrap_err(),
        EngineError::OpenInterestCap
    );
    // Closing still works at the cap.
    e.order(b, Side::Ask, OrderKind::Limit, PRICE + 5, 3).unwrap();
    e.order(a, Side::Bid, OrderKind::ImmediateOrCancel, PRICE + 5, 3).unwrap();
    assert_eq!(e.dex.markets[0].open_interest, 7);
    e.check_invariants();
}

#[test]
fn liquidation_takes_over_at_mark() {
    let mut e = Env::new();
    let mm = e.trader(1, 100_000_000);
    let t = e.trader(2, 12_000);
    let liq = e.trader(3, 100_000_000);
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    e.order(t, Side::Bid, OrderKind::Limit, PRICE - 500, 1).unwrap_or_default();
    assert_eq!(
        engine::liquidate(&mut e.dex.clone(), &mut e.book.clone(), &e.p, liq, t, 0, e.now).unwrap_err(),
        EngineError::NotLiquidatable
    );

    e.walk_oracle(9_200); // equity 12_000 − 50 − 8_000 = 3_950 < MM 4_600
    let h = health(&e.dex, &e.p, t).unwrap();
    assert!(h.liquidatable(), "{h:?}");

    let out = engine::liquidate(&mut e.dex, &mut e.book, &e.p, liq, t, 0, e.now).unwrap();
    assert_eq!(out.base_lots, 10);
    assert_eq!(out.price, 9_200);
    assert_eq!(e.pos(t).base, 0);
    assert_eq!(e.pos(t).open_bids, 0, "resting orders are cancelled");
    assert_eq!(e.pos(liq).base, 10);
    // penalty = 2.5% of 92_000 = 2_300; half to insurance
    assert_eq!(out.penalty, 2_300);
    assert_eq!(e.dex.header.insurance, 1_150);
    assert_eq!(e.col(liq), 100_000_000 + 1_150);
    assert_eq!(out.bad_debt, 0);
    e.check_invariants();
}

#[test]
fn bad_debt_uses_insurance_then_socializes() {
    let mut e = Env::with(EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, ..params() }, market_args(), 0);
    let mm = e.trader(1, 1_000_000);
    let t = e.trader(2, 10_000);
    let liq = e.trader(3, 1_000_000);
    engine::fund_insurance(&mut e.dex, 1_000).unwrap();
    e.vault += 1_000;
    e.order(mm, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(t, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();

    // Gap down through the account's equity: loss 20_000 on 10_000 collateral.
    e.walk_oracle(8_000);
    let out = engine::liquidate(&mut e.dex, &mut e.book, &e.p, liq, t, 0, e.now).unwrap();
    assert_eq!(out.penalty, 0, "no penalty out of an empty account");
    assert_eq!(out.bad_debt, 10_000);
    assert_eq!(e.col(t), 0);
    assert_eq!(e.dex.header.insurance, 0);
    assert_eq!(e.dex.header.socialized_loss, 9_000);
    assert_eq!(e.dex.header.unrecovered_loss, 0);
    // mm and liq held equal collateral, so they split the remainder.
    assert_eq!(e.col(mm), 1_000_000 - 4_500);
    assert_eq!(e.col(liq), 1_000_000 - 4_500);
    e.check_invariants();
}

#[test]
fn funding_moves_from_longs_to_shorts() {
    let mut e = Env::with(EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, ..params() }, market_args(), 0);
    let long = e.trader(1, 1_000_000);
    let short = e.trader(2, 1_000_000);
    let mm = e.trader(3, 1_000_000);
    e.order(short, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(long, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 10).unwrap();
    // Book trades above oracle: bid 10_040 / ask 10_060 → mark 10_040 (+40 bps).
    e.order(mm, Side::Bid, OrderKind::Limit, 10_040, 1).unwrap();
    e.order(mm, Side::Ask, OrderKind::Limit, 10_060, 1).unwrap();

    assert_eq!(engine::update_funding(&mut e.dex, &e.p, 0, e.now).unwrap_err(), EngineError::FundingNotDue);
    e.now += 3_600;
    e.oracle(PRICE);
    let rate = engine::update_funding(&mut e.dex, &e.p, 0, e.now).unwrap();
    assert_eq!(rate, 40);
    // 40 bps of 10_000 = 40 atoms per lot.
    assert_eq!(e.dex.markets[0].cum_funding.get(), 40 * FUNDING_SCALE);

    let before = (health(&e.dex, &e.p, long).unwrap().equity, health(&e.dex, &e.p, short).unwrap().equity);
    engine::settle_all_funding(&mut e.dex, long).unwrap();
    engine::settle_all_funding(&mut e.dex, short).unwrap();
    assert_eq!(e.pos(long).quote.get(), -100_000 - 400);
    assert_eq!(e.pos(short).quote.get(), 100_000 + 400);
    let after = (health(&e.dex, &e.p, long).unwrap().equity, health(&e.dex, &e.p, short).unwrap().equity);
    assert_eq!(before, after, "settling funding does not change equity");
    e.check_invariants();
}

#[test]
fn oracle_steps_are_bounded() {
    let mut e = Env::new();
    e.now = 1;
    assert_eq!(engine::set_oracle(&mut e.dex, &e.p, 0, 10_600, e.now).unwrap_err(), EngineError::OracleMoveTooLarge);
    engine::set_oracle(&mut e.dex, &e.p, 0, 10_500, e.now).unwrap();
    let p = EngineParams { min_oracle_interval: 5, ..params() };
    assert_eq!(engine::set_oracle(&mut e.dex, &p, 0, 10_400, e.now + 1).unwrap_err(), EngineError::OracleTooFrequent);

    let t = e.trader(1, 1_000_000);
    e.now += 61;
    assert_eq!(e.order(t, Side::Bid, OrderKind::Limit, 10_500, 1).unwrap_err(), EngineError::OracleStale);
}

#[test]
fn halt_settle_unstake_lifecycle() {
    let mut e = Env::with(EngineParams { taker_fee_bps: 0, maker_fee_bps: 0, ..params() }, market_args(), 0);
    let a = e.trader(1, 1_000_000);
    let b = e.trader(2, 1_000_000);
    e.order(a, Side::Ask, OrderKind::Limit, PRICE, 10).unwrap();
    e.order(b, Side::Bid, OrderKind::ImmediateOrCancel, PRICE, 4).unwrap();

    assert_eq!(governance::request_unstake(&mut e.dex, &e.p, 1_000).unwrap_err(), EngineError::MarketsOpen);
    assert_eq!(
        engine::halt_market(&mut e.dex, &mut e.book, &e.p, 0, 20_000).unwrap_err(),
        EngineError::InvalidPrice,
        "settlement price must sit near the oracle"
    );
    engine::halt_market(&mut e.dex, &mut e.book, &e.p, 0, 10_200).unwrap();
    assert!(e.book.asks.is_empty());
    assert_eq!(e.pos(a).open_asks, 0);
    assert_eq!(e.order(b, Side::Bid, OrderKind::Limit, PRICE, 1).unwrap_err(), EngineError::MarketNotActive);

    let mut done = false;
    let mut pages = 0;
    while !done {
        done = engine::settle_market(&mut e.dex, 0, 100).unwrap();
        pages += 1;
    }
    assert_eq!(pages, MAX_TRADERS.div_ceil(100));
    assert_eq!(e.dex.markets[0].status, MARKET_SETTLED);
    assert_eq!(e.col(b), 1_000_000 + 4 * 200);
    assert_eq!(e.col(a), 1_000_000 - 4 * 200);
    e.check_invariants();

    assert_eq!(governance::request_unstake(&mut e.dex, &e.p, 50).unwrap_err(), EngineError::StakeLocked);
    governance::request_unstake(&mut e.dex, &e.p, 100).unwrap();
    assert_eq!(governance::complete_unstake(&mut e.dex, &e.p, 105).unwrap_err(), EngineError::UnstakeDelay);
    // Still slashable during the delay.
    governance::vote_slash(&mut e.dex, 0, 3, 5_000, false).unwrap();
    governance::vote_slash(&mut e.dex, 1, 3, 5_000, false).unwrap();
    assert_eq!(governance::execute_slash(&mut e.dex, 2).unwrap(), 500);
    assert_eq!(governance::complete_unstake(&mut e.dex, &e.p, 110).unwrap(), 500);
    assert_eq!(e.dex.header.status, DEX_RETIRED);
}

#[test]
fn slashing_votes() {
    let mut e = Env::new();
    assert_eq!(governance::execute_slash(&mut e.dex, 2).unwrap_err(), EngineError::NoProposal);
    assert_eq!(governance::vote_slash(&mut e.dex, 5, 3, 1_000, true).unwrap_err(), EngineError::InvalidGuardian);
    governance::vote_slash(&mut e.dex, 0, 3, 1_000, true).unwrap();
    assert_eq!(governance::vote_slash(&mut e.dex, 0, 3, 1_000, true).unwrap_err(), EngineError::AlreadyVoted);
    assert_eq!(governance::vote_slash(&mut e.dex, 1, 3, 2_000, true).unwrap_err(), EngineError::ProposalMismatch);
    assert_eq!(governance::execute_slash(&mut e.dex, 2).unwrap_err(), EngineError::ThresholdNotMet);
    governance::vote_slash(&mut e.dex, 2, 3, 1_000, true).unwrap();
    assert_eq!(governance::execute_slash(&mut e.dex, 2).unwrap(), 100);
    assert_eq!(e.dex.header.stake_amount, 900);
    assert_eq!(e.dex.header.status, DEX_FROZEN);
    assert_eq!(governance::set_fee_share(&mut e.dex, &e.p, 0).unwrap_err(), EngineError::DexNotActive);
}

#[test]
fn full_book_evicts_and_releases_margin() {
    let mut e = Env::new();
    let spammer = e.trader(1, 1_000_000_000);
    let t = e.trader(2, 1_000_000);
    for i in 0..BOOK_CAPACITY as u64 {
        e.order(spammer, Side::Bid, OrderKind::Limit, 9_000 + i, 1).unwrap();
    }
    assert_eq!(e.order(t, Side::Bid, OrderKind::Limit, 9_000, 1).unwrap_err(), EngineError::BookFull);
    e.order(t, Side::Bid, OrderKind::Limit, 9_500, 1).unwrap();
    assert_eq!(e.pos(spammer).open_bids, BOOK_CAPACITY as u64 - 1);
    e.check_invariants();
}

// ---------------------------------------------------------------------------
// Randomized invariant test
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

#[test]
fn random_actions_keep_invariants() {
    for seed in 1..=12u64 {
        run_random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 3_000);
    }
}

fn run_random(seed: u64, steps: usize) {
    let mut rng = Rng(seed);
    let p = EngineParams { min_oracle_interval: 0, max_fills_per_order: 8, ..params() };
    let mut e = Env::with(p, MarketArgs { oi_cap: 400, ..market_args() }, rng.below(30_000));
    let traders: Vec<usize> = (0..8u8).map(|i| e.trader(i + 1, 20_000 + rng.below(400_000))).collect();
    let liquidator = e.trader(100, 50_000_000);
    let mut ok = 0;

    for _ in 0..steps {
        let snapshot = (e.dex.clone(), e.book.clone(), e.vault);
        let t = traders[rng.below(traders.len() as u64) as usize];
        let oracle = e.dex.markets[0].oracle_price;
        let price = (oracle as i64 + rng.below(400) as i64 - 200).max(1) as u64;
        let side = if rng.below(2) == 0 { Side::Bid } else { Side::Ask };
        let action = rng.below(12);
        let res: Result<()> = match action {
            0..=4 => {
                let kind = OrderKind::from_u8(rng.below(3) as u8).unwrap();
                e.order(t, side, kind, price, 1 + rng.below(20)).map(|_| ())
            }
            5 => e.reduce(t, side, price, 1 + rng.below(20)).map(|_| ()),
            6 => {
                let ids: Vec<u64> =
                    e.book.bids.orders().iter().chain(e.book.asks.orders()).map(|o| o.order_id).collect();
                match ids.get(rng.below(ids.len().max(1) as u64) as usize) {
                    Some(&id) => engine::cancel_order(&mut e.dex, &mut e.book, t, id).map(|_| ()),
                    None => Ok(()),
                }
            }
            7 => {
                let next = (oracle as i64 + rng.below(800) as i64 - 400).max(1_000) as u64;
                e.now += 1;
                engine::set_oracle(&mut e.dex, &e.p, 0, next, e.now)
            }
            8 => {
                let amt = 1 + rng.below(30_000);
                engine::withdraw(&mut e.dex, &e.p, t, amt, e.now).map(|_| e.vault -= amt as i128)
            }
            9 => {
                let amt = 1 + rng.below(30_000);
                engine::deposit(&mut e.dex, t, amt).map(|_| e.vault += amt as i128)
            }
            10 => engine::liquidate(&mut e.dex, &mut e.book, &e.p, liquidator, t, rng.below(5), e.now).map(|_| ()),
            _ => {
                e.now += 3_600;
                let cur = e.dex.markets[0].oracle_price;
                engine::set_oracle(&mut e.dex, &e.p, 0, cur, e.now)
                    .and_then(|_| engine::update_funding(&mut e.dex, &e.p, 0, e.now).map(|_| ()))
            }
        };
        match res {
            Ok(()) => {
                ok += 1;
                if std::env::var("MEE_DEBUG").is_ok() {
                    let claims = engine::total_claims(&e.dex).unwrap();
                    eprintln!("action {action} slack {}", e.vault + e.dex.header.unrecovered_loss as i128 - claims);
                }
                e.check_invariants();
            }
            Err(_) => (e.dex, e.book, e.vault) = snapshot,
        }
    }
    assert!(ok > steps / 4, "too few successful actions ({ok}) for seed {seed}");

    // Wind down: halt, settle, and everyone must be able to leave.
    let settle = e.dex.markets[0].oracle_price;
    engine::halt_market(&mut e.dex, &mut e.book, &e.p, 0, settle).unwrap();
    while !engine::settle_market(&mut e.dex, 0, 64).unwrap() {}
    e.check_invariants();
    let mut paid = 0i128;
    for t in traders.iter().copied().chain([liquidator]) {
        let col = e.col(t);
        assert!(col >= 0);
        if col > 0 {
            engine::withdraw(&mut e.dex, &e.p, t, col as u64, e.now).unwrap();
            paid += col as i128;
        }
    }
    paid += engine::take_protocol_fees(&mut e.dex) as i128 + engine::take_deployer_fees(&mut e.dex) as i128;
    paid += e.dex.header.insurance as i128;
    assert!(paid <= e.vault + e.dex.header.unrecovered_loss as i128, "paid out more than the vault held");
}
