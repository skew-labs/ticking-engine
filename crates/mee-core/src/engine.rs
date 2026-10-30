//! State transitions: traders, matching, liquidation, oracle, funding,
//! halting and settlement. Every fill settles atomically in the same call
//! (no event queue, no crank).

use crate::book;
use crate::error::{EngineError, Result};
use crate::margin::{self, health};
use crate::math::{self, BPS};
use crate::state::*;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderRequest {
    pub market: usize,
    pub side: Side,
    pub kind: OrderKind,
    /// Limit price in quote atoms per base lot. IOC "market" orders pass the
    /// worst acceptable price.
    pub price: u64,
    pub base_lots: u64,
    /// Only shrinks an existing position; never rests.
    pub reduce_only: bool,
    pub client_order_id: u64,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaceOutcome {
    pub order_id: Option<u64>,
    pub filled_lots: u64,
    pub filled_quote: i128,
    pub resting_lots: u64,
    pub fills: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MarketArgs {
    pub symbol: [u8; 16],
    pub tick_size: u64,
    pub min_base_lots: u64,
    pub initial_margin_bps: u64,
    pub maintenance_margin_bps: u64,
    pub liquidation_fee_bps: u64,
    pub oi_cap: u64,
    pub initial_price: u64,
    pub funding_interval: i64,
    pub max_funding_bps: u64,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LiquidationOutcome {
    pub base_lots: u64,
    pub price: u64,
    pub penalty: i64,
    pub bad_debt: i64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn market_index(dex: &DexState, m: usize) -> Result<usize> {
    if m < dex.header.market_count as usize { Ok(m) } else { Err(EngineError::InvalidMarket) }
}

fn active_market(dex: &DexState, m: usize) -> Result<&Market> {
    let market = &dex.markets[market_index(dex, m)?];
    if market.status != MARKET_ACTIVE {
        return Err(EngineError::MarketNotActive);
    }
    Ok(market)
}

fn check_trader(dex: &DexState, t: usize) -> Result<()> {
    if t < MAX_TRADERS && dex.traders[t].active == 1 { Ok(()) } else { Err(EngineError::TraderNotFound) }
}

fn oracle_fresh(m: &Market, params: &EngineParams, now: i64) -> bool {
    now.saturating_sub(m.oracle_ts) <= params.max_oracle_staleness
}

fn within_bps(price: u64, reference: u64, bps: u64) -> bool {
    let diff = price.abs_diff(reference) as u128;
    diff * BPS as u128 <= reference as u128 * bps as u128
}

fn long_lots(base: i64) -> u64 {
    base.max(0) as u64
}

/// Applies funding accrued since the position was last touched to its quote.
pub fn settle_funding(dex: &mut DexState, t: usize, m: usize) -> Result<()> {
    let cum = dex.markets[m].cum_funding;
    let owed = margin::pending_funding(&dex.markets[m], &dex.traders[t].positions[m])?;
    let p = &mut dex.traders[t].positions[m];
    if owed != 0 {
        p.quote.set(math::sub(p.quote.get(), owed)?);
    }
    p.last_funding = cum;
    Ok(())
}

pub fn settle_all_funding(dex: &mut DexState, t: usize) -> Result<()> {
    for m in 0..dex.header.market_count as usize {
        settle_funding(dex, t, m)?;
    }
    Ok(())
}

/// Moves `quote` into collateral once a position is flat.
fn realize_if_flat(dex: &mut DexState, t: usize, m: usize) -> Result<()> {
    let trader = &mut dex.traders[t];
    let p = &mut trader.positions[m];
    if p.base == 0 && p.quote.get() != 0 {
        trader.collateral = math::to_i64(math::add(trader.collateral as i128, p.quote.get())?)?;
        p.quote.set(0);
    }
    Ok(())
}

/// `buyer` gains `qty` base lots from `seller` at `price`. Funding for both
/// must already be settled on this market.
fn transfer_position(dex: &mut DexState, m: usize, buyer: usize, seller: usize, qty: u64, price: u64) -> Result<()> {
    debug_assert_ne!(buyer, seller);
    let notional = math::mul(qty as i128, price as i128)?;
    let q = i64::try_from(qty).map_err(|_| EngineError::MathOverflow)?;
    let before = long_lots(dex.traders[buyer].positions[m].base) + long_lots(dex.traders[seller].positions[m].base);

    let b = &mut dex.traders[buyer].positions[m];
    b.base = b.base.checked_add(q).ok_or(EngineError::MathOverflow)?;
    b.quote.set(math::sub(b.quote.get(), notional)?);
    let s = &mut dex.traders[seller].positions[m];
    s.base = s.base.checked_sub(q).ok_or(EngineError::MathOverflow)?;
    s.quote.set(math::add(s.quote.get(), notional)?);

    let after = long_lots(dex.traders[buyer].positions[m].base) + long_lots(dex.traders[seller].positions[m].base);
    let market = &mut dex.markets[m];
    market.open_interest = (market.open_interest + after).checked_sub(before).ok_or(EngineError::MathOverflow)?;

    realize_if_flat(dex, buyer, m)?;
    realize_if_flat(dex, seller, m)
}

/// Charges the protocol fee plus the deployer's share on top of it.
fn charge_fee(dex: &mut DexState, t: usize, notional: i128, bps: u64) -> Result<()> {
    if bps == 0 {
        return Ok(());
    }
    let protocol = math::notional_bps_ceil(notional, bps)?;
    let deployer = math::notional_bps_ceil(protocol, dex.header.fee_share_bps)?;
    let total = math::add(protocol, deployer)?;
    let trader = &mut dex.traders[t];
    trader.collateral = math::to_i64(math::sub(trader.collateral as i128, total)?)?;
    let h = &mut dex.header;
    h.protocol_fees = math::to_i64(math::add(h.protocol_fees as i128, protocol)?)?;
    h.deployer_fees = math::to_i64(math::add(h.deployer_fees as i128, deployer)?)?;
    Ok(())
}

fn add_open(dex: &mut DexState, t: usize, m: usize, side: Side, lots: u64) -> Result<()> {
    let p = &mut dex.traders[t].positions[m];
    let v = match side {
        Side::Bid => &mut p.open_bids,
        Side::Ask => &mut p.open_asks,
    };
    *v = v.checked_add(lots).ok_or(EngineError::MathOverflow)?;
    Ok(())
}

fn release_open(dex: &mut DexState, t: usize, m: usize, side: Side, lots: u64) -> Result<()> {
    let p = &mut dex.traders[t].positions[m];
    let v = match side {
        Side::Bid => &mut p.open_bids,
        Side::Ask => &mut p.open_asks,
    };
    *v = v.checked_sub(lots).ok_or(EngineError::MathOverflow)?;
    Ok(())
}

fn refresh_top(dex: &mut DexState, book: &Book, m: usize) {
    let market = &mut dex.markets[m];
    market.best_bid = book.bids.best().map_or(0, |o| o.price);
    market.best_ask = book.asks.best().map_or(0, |o| o.price);
}

fn check_book(book: &Book, m: usize) -> Result<()> {
    if book.market_index as usize == m { Ok(()) } else { Err(EngineError::InvalidMarket) }
}

// ---------------------------------------------------------------------------
// Traders and collateral
// ---------------------------------------------------------------------------

pub fn find_trader(dex: &DexState, owner: &Key) -> Option<usize> {
    dex.traders.iter().position(|t| t.active == 1 && &t.owner == owner)
}

pub fn register_trader(dex: &mut DexState, owner: &Key) -> Result<usize> {
    if find_trader(dex, owner).is_some() {
        return Err(EngineError::TraderExists);
    }
    let idx = dex.traders.iter().position(|t| t.active == 0).ok_or(EngineError::TradersFull)?;
    let t = &mut dex.traders[idx];
    *t = Trader::default();
    t.owner = *owner;
    t.active = 1;
    Ok(idx)
}

/// Frees the slot of a trader with nothing left in the DEX.
pub fn close_trader(dex: &mut DexState, t: usize) -> Result<()> {
    check_trader(dex, t)?;
    let trader = &dex.traders[t];
    if trader.collateral != 0 || trader.positions.iter().any(|p| !p.is_empty()) {
        return Err(EngineError::TraderNotEmpty);
    }
    dex.traders[t] = Trader::default();
    Ok(())
}

pub fn deposit(dex: &mut DexState, t: usize, amount: u64) -> Result<()> {
    check_trader(dex, t)?;
    if amount == 0 {
        return Err(EngineError::InvalidAmount);
    }
    let trader = &mut dex.traders[t];
    trader.collateral = math::to_i64(math::add(trader.collateral as i128, amount as i128)?)?;
    Ok(())
}

/// Withdraws free collateral. Unrealized profit stays in the position until
/// it is closed; the account must still meet initial margin afterwards.
pub fn withdraw(dex: &mut DexState, params: &EngineParams, t: usize, amount: u64, now: i64) -> Result<()> {
    check_trader(dex, t)?;
    if amount == 0 {
        return Err(EngineError::InvalidAmount);
    }
    for m in 0..dex.header.market_count as usize {
        let market = &dex.markets[m];
        let p = &dex.traders[t].positions[m];
        if market.status == MARKET_ACTIVE && !p.is_empty() && !oracle_fresh(market, params, now) {
            return Err(EngineError::OracleStale);
        }
    }
    settle_all_funding(dex, t)?;
    let trader = &mut dex.traders[t];
    if (amount as i128) > trader.collateral as i128 {
        return Err(EngineError::InsufficientCollateral);
    }
    trader.collateral -= amount as i64;
    let h = health(dex, params, t)?;
    if h.equity < h.initial {
        return Err(EngineError::InsufficientMargin);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Markets (deployer)
// ---------------------------------------------------------------------------

pub fn add_market(
    dex: &mut DexState,
    params: &EngineParams,
    book_key: &Key,
    args: &MarketArgs,
    now: i64,
) -> Result<usize> {
    let idx = dex.header.market_count as usize;
    if idx >= MAX_MARKETS {
        return Err(EngineError::MarketsFull);
    }
    let valid = args.tick_size > 0
        && args.min_base_lots > 0
        && args.initial_margin_bps >= params.min_initial_margin_bps
        && args.initial_margin_bps <= BPS as u64
        && args.maintenance_margin_bps > 0
        && args.maintenance_margin_bps < args.initial_margin_bps
        && args.liquidation_fee_bps <= args.maintenance_margin_bps
        && args.oi_cap > 0
        && args.initial_price > 0
        && args.funding_interval > 0
        && args.max_funding_bps <= args.maintenance_margin_bps;
    if !valid {
        return Err(EngineError::InvalidMarketParams);
    }
    dex.markets[idx] = Market {
        symbol: args.symbol,
        status: MARKET_ACTIVE,
        book: *book_key,
        tick_size: args.tick_size,
        min_base_lots: args.min_base_lots,
        initial_margin_bps: args.initial_margin_bps,
        maintenance_margin_bps: args.maintenance_margin_bps,
        liquidation_fee_bps: args.liquidation_fee_bps,
        oi_cap: args.oi_cap,
        oracle_price: args.initial_price,
        oracle_ts: now,
        funding_interval: args.funding_interval,
        last_funding_ts: now,
        max_funding_bps: args.max_funding_bps,
        ..Market::default()
    };
    dex.header.market_count += 1;
    Ok(idx)
}

/// The deployer's oracle push (HIP-3: deployers own their oracle). Bounded in
/// step size and frequency so a compromised key cannot jump the price in one go.
pub fn set_oracle(dex: &mut DexState, params: &EngineParams, m: usize, price: u64, now: i64) -> Result<()> {
    let market = active_market(dex, m)?;
    if price == 0 {
        return Err(EngineError::InvalidPrice);
    }
    if now.saturating_sub(market.oracle_ts) < params.min_oracle_interval {
        return Err(EngineError::OracleTooFrequent);
    }
    if !within_bps(price, market.oracle_price, params.max_oracle_move_bps) {
        return Err(EngineError::OracleMoveTooLarge);
    }
    let market = &mut dex.markets[m];
    market.oracle_price = price;
    market.oracle_ts = now;
    Ok(())
}

/// Permissionless funding crank: one step per interval, rate = premium of
/// mark over oracle, clamped. Returns the rate in bps.
pub fn update_funding(dex: &mut DexState, params: &EngineParams, m: usize, now: i64) -> Result<i128> {
    let market = active_market(dex, m)?;
    if now < market.last_funding_ts.saturating_add(market.funding_interval) {
        return Err(EngineError::FundingNotDue);
    }
    if !oracle_fresh(market, params, now) {
        return Err(EngineError::OracleStale);
    }
    let oracle = market.oracle_price as i128;
    let mark = margin::mark_price(market, params) as i128;
    let cap = market.max_funding_bps as i128;
    let rate_bps = (math::mul(mark - oracle, BPS)? / oracle).clamp(-cap, cap);
    let delta = math::mul(math::mul(oracle, rate_bps)?, math::FUNDING_SCALE)? / BPS;
    let market = &mut dex.markets[m];
    market.cum_funding.set(math::add(market.cum_funding.get(), delta)?);
    market.last_funding_ts = now;
    Ok(rate_bps)
}

/// Stops a market (HIP-3 deployer halt): voids every resting order and fixes
/// the settlement price, which must sit within one oracle step of the oracle.
pub fn halt_market(
    dex: &mut DexState,
    book: &mut Book,
    params: &EngineParams,
    m: usize,
    settlement_price: u64,
) -> Result<()> {
    check_book(book, m)?;
    let market = active_market(dex, m)?;
    if settlement_price == 0 || !within_bps(settlement_price, market.oracle_price, params.max_oracle_move_bps) {
        return Err(EngineError::InvalidPrice);
    }
    for side in [Side::Bid, Side::Ask] {
        while let Some(o) = book.side_mut(side).pop_best() {
            release_open(dex, o.trader as usize, m, side, o.base_lots)?;
        }
    }
    let market = &mut dex.markets[m];
    market.status = MARKET_HALTED;
    market.settlement_price = settlement_price;
    market.best_bid = 0;
    market.best_ask = 0;
    Ok(())
}

/// Permissionless, paged settlement of a halted market: closes up to
/// `max_traders` slots at the settlement price. Returns true once done.
pub fn settle_market(dex: &mut DexState, m: usize, max_traders: usize) -> Result<bool> {
    market_index(dex, m)?;
    if dex.markets[m].status != MARKET_HALTED {
        return Err(EngineError::MarketNotHalted);
    }
    let start = dex.markets[m].settle_cursor as usize;
    let end = (start + max_traders.max(1)).min(MAX_TRADERS);
    let price = dex.markets[m].settlement_price as i128;
    for t in start..end {
        if dex.traders[t].active == 0 {
            continue;
        }
        settle_funding(dex, t, m)?;
        let p = dex.traders[t].positions[m];
        if p.base == 0 && p.quote.get() == 0 {
            continue;
        }
        let pnl = math::add(math::mul(p.base as i128, price)?, p.quote.get())?;
        let market = &mut dex.markets[m];
        market.open_interest -= long_lots(p.base);
        let trader = &mut dex.traders[t];
        trader.collateral = math::to_i64(math::add(trader.collateral as i128, pnl)?)?;
        let pos = &mut trader.positions[m];
        pos.base = 0;
        pos.quote.set(0);
        resolve_bad_debt(dex, t)?;
    }
    let market = &mut dex.markets[m];
    market.settle_cursor = end as u64;
    if end == MAX_TRADERS {
        debug_assert_eq!(market.open_interest, 0);
        market.status = MARKET_SETTLED;
        return Ok(true);
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Trading
// ---------------------------------------------------------------------------

pub fn place_order(
    dex: &mut DexState,
    book: &mut Book,
    params: &EngineParams,
    taker: usize,
    req: &OrderRequest,
    now: i64,
) -> Result<PlaceOutcome> {
    let m = req.market;
    check_book(book, m)?;
    check_trader(dex, taker)?;
    let market = *active_market(dex, m)?;

    if req.price == 0 {
        return Err(EngineError::InvalidPrice);
    }
    if !req.price.is_multiple_of(market.tick_size) {
        return Err(EngineError::TickSize);
    }
    if req.base_lots == 0 || (!req.reduce_only && req.base_lots < market.min_base_lots) {
        return Err(EngineError::InvalidSize);
    }
    if !req.reduce_only && !oracle_fresh(&market, params, now) {
        return Err(EngineError::OracleStale);
    }
    if !within_bps(req.price, market.oracle_price, params.order_band_bps) {
        return Err(EngineError::PriceBand);
    }

    settle_all_funding(dex, taker)?;

    let side = req.side;
    let mut remaining = req.base_lots;
    if req.reduce_only {
        let base = dex.traders[taker].positions[m].base;
        let reducible = match side {
            Side::Bid if base < 0 => base.unsigned_abs(),
            Side::Ask if base > 0 => base.unsigned_abs(),
            _ => return Err(EngineError::NoPosition),
        };
        remaining = remaining.min(reducible);
    }

    let opposite = side.opposite();
    if req.kind == OrderKind::PostOnly
        && let Some(best) = book.side(opposite).best()
        && book::crosses(side, req.price, best.price)
    {
        return Err(EngineError::PostOnlyWouldCross);
    }

    let oi_before = dex.markets[m].open_interest;
    let mut out = PlaceOutcome::default();
    let max_fills = params.max_fills_per_order.max(1);

    while remaining > 0 && out.fills < max_fills {
        let Some(best) = book.side(opposite).best().copied() else { break };
        if !book::crosses(side, req.price, best.price) {
            break;
        }
        out.fills += 1;
        let maker = best.trader as usize;
        if maker == taker {
            // Self-trade prevention: cancel the resting order.
            book.side_mut(opposite).pop_best();
            release_open(dex, maker, m, opposite, best.base_lots)?;
            continue;
        }

        let qty = remaining.min(best.base_lots);
        settle_funding(dex, maker, m)?;
        let (buyer, seller) = if side == Side::Bid { (taker, maker) } else { (maker, taker) };
        transfer_position(dex, m, buyer, seller, qty, best.price)?;
        release_open(dex, maker, m, opposite, qty)?;

        let notional = math::mul(qty as i128, best.price as i128)?;
        charge_fee(dex, taker, notional, params.taker_fee_bps)?;
        charge_fee(dex, maker, notional, params.maker_fee_bps)?;

        if qty == best.base_lots {
            book.side_mut(opposite).pop_best();
        } else if let Some(o) = book.side_mut(opposite).best_mut() {
            o.base_lots -= qty;
        }
        remaining -= qty;
        out.filled_lots += qty;
        out.filled_quote = math::add(out.filled_quote, notional)?;
        dex.markets[m].last_trade_price = best.price;
    }

    let still_crosses = book.side(opposite).best().is_some_and(|o| book::crosses(side, req.price, o.price));
    let rests = remaining > 0 && !req.reduce_only && req.kind != OrderKind::ImmediateOrCancel && !still_crosses;
    if rests {
        let order_id = book.next_order_id;
        book.next_order_id += 1;
        let order = Order {
            order_id,
            trader: taker as u32,
            price: req.price,
            base_lots: remaining,
            client_order_id: req.client_order_id,
            ..Order::default()
        };
        if let Some(ev) = book.side_mut(side).insert(side, order)? {
            release_open(dex, ev.trader as usize, m, side, ev.base_lots)?;
        }
        add_open(dex, taker, m, side, remaining)?;
        out.order_id = Some(order_id);
        out.resting_lots = remaining;
    }

    refresh_top(dex, book, m);

    let market = &dex.markets[m];
    if market.open_interest > market.oi_cap && market.open_interest > oi_before {
        return Err(EngineError::OpenInterestCap);
    }
    if !req.reduce_only {
        let h = health(dex, params, taker)?;
        if h.equity < h.initial {
            return Err(EngineError::InsufficientMargin);
        }
    }
    Ok(out)
}

pub fn cancel_order(dex: &mut DexState, book: &mut Book, trader: usize, order_id: u64) -> Result<Order> {
    let m = book.market_index as usize;
    market_index(dex, m)?;
    check_trader(dex, trader)?;
    for side in [Side::Bid, Side::Ask] {
        if let Some(idx) = book.side(side).find(order_id) {
            if book.side(side).orders[idx].trader as usize != trader {
                return Err(EngineError::NotOrderOwner);
            }
            let o = book.side_mut(side).remove_at(idx);
            release_open(dex, trader, m, side, o.base_lots)?;
            refresh_top(dex, book, m);
            return Ok(o);
        }
    }
    Err(EngineError::OrderNotFound)
}

pub fn cancel_all(dex: &mut DexState, book: &mut Book, trader: usize) -> Result<usize> {
    let m = book.market_index as usize;
    market_index(dex, m)?;
    check_trader(dex, trader)?;
    let mut n = 0;
    for side in [Side::Bid, Side::Ask] {
        let mut i = 0;
        while i < book.side(side).len() {
            if book.side(side).orders[i].trader as usize == trader {
                let o = book.side_mut(side).remove_at(i);
                release_open(dex, trader, m, side, o.base_lots)?;
                n += 1;
            } else {
                i += 1;
            }
        }
    }
    refresh_top(dex, book, m);
    Ok(n)
}

// ---------------------------------------------------------------------------
// Liquidation and bad debt
// ---------------------------------------------------------------------------

/// Permissionless liquidation of one market's position. The liquidator takes
/// the position over at the mark price (backstop style, so it works even when
/// the book is empty) and earns part of the penalty; the rest feeds the
/// insurance fund. `max_base == 0` liquidates the whole position.
pub fn liquidate(
    dex: &mut DexState,
    book: &mut Book,
    params: &EngineParams,
    liquidator: usize,
    liquidatee: usize,
    max_base: u64,
    now: i64,
) -> Result<LiquidationOutcome> {
    let m = book.market_index as usize;
    check_trader(dex, liquidator)?;
    check_trader(dex, liquidatee)?;
    if liquidator == liquidatee {
        return Err(EngineError::SelfLiquidation);
    }
    let market = *active_market(dex, m)?;
    if !oracle_fresh(&market, params, now) {
        return Err(EngineError::OracleStale);
    }
    if !health(dex, params, liquidatee)?.liquidatable() {
        return Err(EngineError::NotLiquidatable);
    }

    cancel_all(dex, book, liquidatee)?;
    settle_funding(dex, liquidatee, m)?;
    settle_funding(dex, liquidator, m)?;

    let base = dex.traders[liquidatee].positions[m].base;
    if base == 0 {
        return Err(EngineError::NoPosition);
    }
    let qty = if max_base == 0 { base.unsigned_abs() } else { base.unsigned_abs().min(max_base) };
    let price = margin::mark_price(&market, params);
    if base > 0 {
        transfer_position(dex, m, liquidator, liquidatee, qty, price)?;
    } else {
        transfer_position(dex, m, liquidatee, liquidator, qty, price)?;
    }

    // The penalty never exceeds what the liquidatee has left, so it can't
    // grow bad debt that other traders would end up paying.
    let notional = math::mul(qty as i128, price as i128)?;
    let remaining_equity = health(dex, params, liquidatee)?.equity.max(0);
    let penalty = math::notional_bps_ceil(notional, market.liquidation_fee_bps)?.min(remaining_equity);
    let to_insurance = math::notional_bps_ceil(penalty, params.liquidation_insurance_share_bps)?.min(penalty);
    let to_liquidator = penalty - to_insurance;
    let t = &mut dex.traders[liquidatee];
    t.collateral = math::to_i64(math::sub(t.collateral as i128, penalty)?)?;
    let l = &mut dex.traders[liquidator];
    l.collateral = math::to_i64(math::add(l.collateral as i128, to_liquidator)?)?;
    let h = &mut dex.header;
    h.insurance = math::to_i64(math::add(h.insurance as i128, to_insurance)?)?;

    let bad_debt = resolve_bad_debt(dex, liquidatee)?;

    let lh = health(dex, params, liquidator)?;
    if lh.equity < lh.initial {
        return Err(EngineError::InsufficientMargin);
    }
    // A takeover moves risk between accounts, so open interest never grows
    // and the OI cap needs no check here.
    Ok(LiquidationOutcome { base_lots: qty, price, penalty: math::to_i64(penalty)?, bad_debt })
}

/// When a trader is flat everywhere with negative collateral, cover the hole
/// from the insurance fund, then pro rata from every other trader's positive
/// collateral. Returns the bad debt that was resolved.
pub fn resolve_bad_debt(dex: &mut DexState, t: usize) -> Result<i64> {
    let trader = &dex.traders[t];
    if trader.collateral >= 0 || trader.positions.iter().any(|p| p.base != 0 || p.quote.get() != 0) {
        return Ok(0);
    }
    let debt = -(trader.collateral as i128);
    dex.traders[t].collateral = 0;

    let from_insurance = debt.min(dex.header.insurance.max(0) as i128);
    dex.header.insurance -= from_insurance as i64;
    let mut left = debt - from_insurance;
    if left == 0 {
        return math::to_i64(debt);
    }

    let pool: i128 = dex
        .traders
        .iter()
        .enumerate()
        .filter(|(i, x)| *i != t && x.active == 1 && x.collateral > 0)
        .map(|(_, x)| x.collateral as i128)
        .sum();
    if pool > 0 {
        let target = left.min(pool);
        let mut taken = 0i128;
        for (i, x) in dex.traders.iter_mut().enumerate() {
            if i == t || x.active == 0 || x.collateral <= 0 {
                continue;
            }
            let share = math::div_ceil(math::mul(target, x.collateral as i128)?, pool).min(x.collateral as i128);
            x.collateral -= share as i64;
            taken += share;
        }
        dex.header.socialized_loss = math::to_i64(math::add(dex.header.socialized_loss as i128, taken)?)?;
        left = (left - taken).max(0);
    }
    dex.header.unrecovered_loss = math::to_i64(math::add(dex.header.unrecovered_loss as i128, left)?)?;
    math::to_i64(debt)
}

// ---------------------------------------------------------------------------
// Fee and insurance accounting
// ---------------------------------------------------------------------------

pub fn fund_insurance(dex: &mut DexState, amount: u64) -> Result<()> {
    if amount == 0 {
        return Err(EngineError::InvalidAmount);
    }
    dex.header.insurance = math::to_i64(math::add(dex.header.insurance as i128, amount as i128)?)?;
    Ok(())
}

pub fn take_protocol_fees(dex: &mut DexState) -> u64 {
    let v = dex.header.protocol_fees.max(0) as u64;
    dex.header.protocol_fees = 0;
    v
}

pub fn take_deployer_fees(dex: &mut DexState) -> u64 {
    let v = dex.header.deployer_fees.max(0) as u64;
    dex.header.deployer_fees = 0;
    v
}

/// Quote atoms the vault owes: collateral, open-position cash flows net of
/// unsettled funding, the insurance fund and unclaimed fees. The vault must
/// always hold at least this, minus `unrecovered_loss`.
pub fn total_claims(dex: &DexState) -> Result<i128> {
    let mut sum = dex.header.insurance as i128 + dex.header.protocol_fees as i128 + dex.header.deployer_fees as i128;
    for t in dex.traders.iter().filter(|t| t.active == 1) {
        sum = math::add(sum, t.collateral as i128)?;
        for (m, p) in t.positions.iter().enumerate() {
            sum = math::add(sum, math::sub(p.quote.get(), margin::pending_funding(&dex.markets[m], p)?)?)?;
        }
    }
    Ok(sum)
}
