//! State transitions. Every public operation is atomic: it validates (or
//! simulates) first and only then mutates, so an `Err` leaves the state
//! untouched. That lets a node apply a block transaction by transaction
//! without snapshots.

use mee_core::engine::MarketArgs;
use mee_core::margin::Health;
use mee_core::math::{self, BPS, FUNDING_SCALE};
use mee_core::{
    DEX_ACTIVE, DEX_FROZEN, DEX_RETIRED, Key, MARKET_ACTIVE, MARKET_HALTED, MARKET_SETTLED, OrderKind, Side,
};

use crate::book::Order;
use crate::state::*;

pub use mee_core::EngineError as Error;
pub type Result<T> = core::result::Result<T, Error>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OrderRequest {
    pub market: MarketId,
    pub side: Side,
    pub kind: OrderKind,
    pub price: u64,
    pub size: u64,
    pub reduce_only: bool,
    pub client_id: u64,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaceOutcome {
    pub order_id: Option<OrderId>,
    pub filled: u64,
    pub filled_quote: i128,
    pub resting: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CancelReason {
    User,
    SelfTrade,
    Liquidation,
    MarketHalted,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Placed {
        market: MarketId,
        order_id: OrderId,
        account: AccountId,
        side: Side,
        price: u64,
        size: u64,
    },
    Fill {
        market: MarketId,
        maker_order: OrderId,
        maker: AccountId,
        taker: AccountId,
        taker_side: Side,
        price: u64,
        size: u64,
        taker_fee: i64,
        maker_fee: i64,
    },
    Canceled {
        market: MarketId,
        order_id: OrderId,
        account: AccountId,
        remaining: u64,
        reason: CancelReason,
    },
    Liquidated {
        market: MarketId,
        liquidator: AccountId,
        liquidatee: AccountId,
        size: u64,
        price: u64,
        penalty: i64,
    },
    BadDebt {
        dex: DexId,
        account: AccountId,
        amount: i64,
        from_insurance: i64,
        socialized: i64,
        unrecovered: i64,
    },
    Funding {
        market: MarketId,
        rate_bps: i64,
    },
    MarketSettled {
        market: MarketId,
        price: u64,
    },
    DexDeployed {
        dex: DexId,
        deployer: AccountId,
        stake: u64,
    },
    MarketAdded {
        dex: DexId,
        market: MarketId,
    },
    Slashed {
        dex: DexId,
        amount: u64,
        frozen: bool,
    },
    /// A resting order shrunk in place by `modify_order` (it keeps its
    /// queue position).
    Reduced {
        market: MarketId,
        order_id: OrderId,
        account: AccountId,
        remaining: u64,
    },
    /// Auto-deleveraging: `counterparty` closed `size` against a bankrupt
    /// `target` at the bankruptcy price.
    Adl {
        market: MarketId,
        target: AccountId,
        counterparty: AccountId,
        size: u64,
        price: u64,
    },
}

/// Owner key of the protocol account that runs protocol DEXes.
pub const PROTOCOL_KEY: Key = [0xFE; 32];

pub type Events = Vec<Event>;

fn within_bps(price: u64, reference: u64, bps: u64) -> bool {
    price.abs_diff(reference) as u128 * BPS as u128 <= reference as u128 * bps as u128
}

fn long(base: i64) -> u64 {
    base.max(0) as u64
}

fn crosses(side: Side, limit: u64, price: u64) -> bool {
    match side {
        Side::Bid => limit >= price,
        Side::Ask => limit <= price,
    }
}

fn pending_funding(m: &Market, p: &Position) -> Result<i128> {
    if p.base == 0 {
        return Ok(0);
    }
    let delta = math::sub(m.cum_funding, p.last_funding)?;
    Ok(math::div_ceil(math::mul(p.base as i128, delta)?, FUNDING_SCALE))
}

fn settle_position_funding(m: &Market, p: &mut Position) -> Result<()> {
    let owed = pending_funding(m, p)?;
    p.quote = math::sub(p.quote, owed)?;
    p.last_funding = m.cum_funding;
    Ok(())
}

/// Protocol fee and the deployer's share on top of it.
fn fees(notional: i128, bps: u64, share_bps: u64) -> Result<(i128, i128)> {
    let protocol = math::notional_bps_ceil(notional, bps)?;
    let deployer = math::notional_bps_ceil(protocol, share_bps)?;
    Ok((protocol, deployer))
}

enum Step {
    SelfCancel { order: Order },
    Fill { order: Order, size: u64 },
}

impl State {
    // -----------------------------------------------------------------------
    // Read helpers
    // -----------------------------------------------------------------------

    pub fn market(&self, m: MarketId) -> Result<&Market> {
        self.markets.get(m as usize).ok_or(Error::InvalidMarket)
    }

    fn active_market(&self, m: MarketId) -> Result<&Market> {
        let market = self.market(m)?;
        if market.status != MARKET_ACTIVE {
            return Err(Error::MarketNotActive);
        }
        Ok(market)
    }

    pub fn dex(&self, d: DexId) -> Result<&Dex> {
        self.dexes.get(d as usize).ok_or(Error::DexNotActive)
    }

    fn account(&self, a: AccountId) -> Result<&Account> {
        self.accounts.get(a as usize).ok_or(Error::TraderNotFound)
    }

    fn oracle_fresh(&self, m: &Market) -> bool {
        self.now.saturating_sub(m.oracle_ts) <= self.params.max_oracle_staleness
    }

    /// Median of oracle, best bid and best ask, clamped to oracle ± band.
    pub fn mark_price(&self, m: &Market) -> u64 {
        if m.status == MARKET_HALTED || m.status == MARKET_SETTLED {
            return m.settlement_price;
        }
        let o = m.oracle_price;
        let raw = match (m.book.best_price(Side::Bid), m.book.best_price(Side::Ask)) {
            (Some(b), Some(a)) => math::median3(o, b, a),
            _ => o,
        };
        let band = (o as u128 * self.params.mark_band_bps as u128 / BPS as u128) as u64;
        raw.clamp(o.saturating_sub(band), o.saturating_add(band))
    }

    /// Account health in one DEX. `overlay` substitutes one position and
    /// adjusts collateral, to check a trade before applying it.
    pub fn health(&self, a: AccountId, dex: DexId, overlay: Option<(MarketId, Position, i64)>) -> Result<Health> {
        let empty = Margin::default();
        let margin = self.account(a)?.margin(dex).unwrap_or(&empty);
        let mut h = Health { equity: margin.collateral as i128, ..Health::default() };
        let mut add = |mid: MarketId, p: &Position| -> Result<()> {
            if p.is_empty() {
                return Ok(());
            }
            let m = self.market(mid)?;
            let mark = self.mark_price(m) as i128;
            let base = p.base as i128;
            h.equity = math::add(h.equity, math::add(math::mul(base, mark)?, p.quote)?)?;
            h.equity = math::sub(h.equity, pending_funding(m, p)?)?;
            let worst = if m.status == MARKET_ACTIVE {
                (base + p.open_bids as i128).abs().max((base - p.open_asks as i128).abs())
            } else {
                base.abs()
            };
            h.initial =
                math::add(h.initial, math::notional_bps_ceil(math::mul(worst, mark)?, m.cfg.initial_margin_bps)?)?;
            h.maintenance = math::add(
                h.maintenance,
                math::notional_bps_ceil(math::mul(base.abs(), mark)?, m.cfg.maintenance_margin_bps)?,
            )?;
            Ok(())
        };
        let mut overlaid = false;
        for (mid, p) in &margin.positions {
            match overlay {
                Some((om, op, _)) if om == *mid => {
                    overlaid = true;
                    add(om, &op)?
                }
                _ => add(*mid, p)?,
            }
        }
        if let Some((om, op, dc)) = overlay {
            if !overlaid {
                add(om, &op)?;
            }
            h.equity = math::add(h.equity, dc as i128)?;
        }
        Ok(h)
    }

    /// Quote the DEX owes: collateral, open cash flows net of unsettled
    /// funding, insurance and unclaimed fees.
    pub fn dex_claims(&self, dex: DexId) -> Result<i128> {
        let d = self.dex(dex)?;
        let mut sum = d.insurance as i128 + d.protocol_fees as i128 + d.deployer_fees as i128;
        for &a in &d.members {
            if let Some(mg) = self.accounts[a as usize].margin(dex) {
                sum = math::add(sum, mg.collateral as i128)?;
                for (mid, p) in &mg.positions {
                    sum = math::add(sum, math::sub(p.quote, pending_funding(self.market(*mid)?, p)?)?)?;
                }
            }
        }
        Ok(sum)
    }

    // -----------------------------------------------------------------------
    // Mutation helpers (callers have already validated)
    // -----------------------------------------------------------------------

    /// Creates the account's margin in `dex` if needed and records membership.
    fn join(&mut self, a: AccountId, dex: DexId) {
        let (_, created) = self.accounts[a as usize].margin_mut(dex);
        if created {
            let d = &mut self.dexes[dex as usize];
            d.members.push(a);
            d.members_hash = crate::commit::sha256(&[&d.members_hash, &a.to_le_bytes()]);
        }
    }

    fn settle_funding(&mut self, a: AccountId, dex: DexId, m: MarketId) -> Result<()> {
        let market = &self.markets[m as usize];
        let (mg, _) = self.accounts[a as usize].margin_mut(dex);
        settle_position_funding(market, mg.position_mut(m))
    }

    fn realize_if_flat(&mut self, a: AccountId, dex: DexId, m: MarketId) -> Result<()> {
        let (mg, _) = self.accounts[a as usize].margin_mut(dex);
        let p = mg.position_mut(m);
        if p.base == 0 && p.quote != 0 {
            let q = p.quote;
            p.quote = 0;
            mg.collateral = math::to_i64(math::add(mg.collateral as i128, q)?)?;
        }
        Ok(())
    }

    /// `buyer` takes `size` lots from `seller` at `price`. Funding must
    /// already be settled for both on this market.
    fn trade(&mut self, m: MarketId, buyer: AccountId, seller: AccountId, size: u64, price: u64) -> Result<()> {
        let dex = self.markets[m as usize].dex;
        let q = i64::try_from(size).map_err(|_| Error::MathOverflow)?;
        let mut oi = self.markets[m as usize].open_interest;
        for (a, signed) in [(buyer, q), (seller, -q)] {
            let (mg, _) = self.accounts[a as usize].margin_mut(dex);
            let p = mg.position_mut(m);
            let before = long(p.base);
            p.base = p.base.checked_add(signed).ok_or(Error::MathOverflow)?;
            p.quote = math::sub(p.quote, math::mul(signed as i128, price as i128)?)?;
            oi = (oi + long(p.base)).checked_sub(before).ok_or(Error::MathOverflow)?;
        }
        self.markets[m as usize].open_interest = oi;
        self.realize_if_flat(buyer, dex, m)?;
        self.realize_if_flat(seller, dex, m)
    }

    /// Fee share of a market and who receives it (a launch creator, or the
    /// DEX deployer when `None`).
    fn fee_share(&self, m: MarketId) -> (u64, Option<AccountId>) {
        let market = &self.markets[m as usize];
        match market.creator {
            Some(c) => (market.creator_fee_share_bps, Some(c)),
            None => (self.dexes[market.dex as usize].fee_share_bps, None),
        }
    }

    fn charge_fee(
        &mut self,
        a: AccountId,
        dex: DexId,
        protocol: i128,
        deployer: i128,
        creator: Option<AccountId>,
    ) -> Result<i64> {
        let total = math::add(protocol, deployer)?;
        let (mg, _) = self.accounts[a as usize].margin_mut(dex);
        mg.collateral = math::to_i64(math::sub(mg.collateral as i128, total)?)?;
        let d = &mut self.dexes[dex as usize];
        d.protocol_fees = math::to_i64(math::add(d.protocol_fees as i128, protocol)?)?;
        match creator {
            Some(c) => {
                let acct = &mut self.accounts[c as usize];
                acct.balance = math::to_i64(math::add(acct.balance as i128, deployer)?)?;
            }
            None => d.deployer_fees = math::to_i64(math::add(d.deployer_fees as i128, deployer)?)?,
        }
        math::to_i64(total)
    }

    fn release_open(&mut self, a: AccountId, dex: DexId, m: MarketId, side: Side, size: u64) {
        let (mg, _) = self.accounts[a as usize].margin_mut(dex);
        let p = mg.position_mut(m);
        match side {
            Side::Bid => p.open_bids -= size,
            Side::Ask => p.open_asks -= size,
        }
    }
}

// ---------------------------------------------------------------------------
// Balances (bridge side)
// ---------------------------------------------------------------------------

impl State {
    pub fn deposit(&mut self, owner: &Key, amount: u64) -> Result<AccountId> {
        let a = self.account_or_create(owner);
        let acct = &mut self.accounts[a as usize];
        acct.balance = acct
            .balance
            .checked_add(i64::try_from(amount).map_err(|_| Error::MathOverflow)?)
            .ok_or(Error::MathOverflow)?;
        Ok(a)
    }

    pub fn deposit_stake(&mut self, owner: &Key, amount: u64) -> Result<AccountId> {
        let a = self.account_or_create(owner);
        let acct = &mut self.accounts[a as usize];
        acct.stake_balance = acct.stake_balance.checked_add(amount).ok_or(Error::MathOverflow)?;
        Ok(a)
    }

    /// Debits the free balance; the chain turns this into a bridge receipt.
    pub fn withdraw(&mut self, a: AccountId, amount: u64) -> Result<()> {
        let acct = self.accounts.get_mut(a as usize).ok_or(Error::TraderNotFound)?;
        if amount == 0 {
            return Err(Error::InvalidAmount);
        }
        if (amount as i128) > acct.balance as i128 {
            return Err(Error::InsufficientCollateral);
        }
        acct.balance -= amount as i64;
        Ok(())
    }

    pub fn withdraw_stake(&mut self, a: AccountId, amount: u64) -> Result<()> {
        let acct = self.accounts.get_mut(a as usize).ok_or(Error::TraderNotFound)?;
        if amount == 0 || amount > acct.stake_balance {
            return Err(Error::InvalidAmount);
        }
        acct.stake_balance -= amount;
        Ok(())
    }

    /// Moves free balance into a DEX's margin.
    pub fn transfer_to_dex(&mut self, a: AccountId, dex: DexId, amount: u64) -> Result<()> {
        if self.dex(dex)?.status != DEX_ACTIVE {
            return Err(Error::DexNotActive);
        }
        let acct = self.account(a)?;
        if amount == 0 || (amount as i128) > acct.balance as i128 {
            return Err(Error::InsufficientCollateral);
        }
        self.join(a, dex);
        let acct = &mut self.accounts[a as usize];
        acct.balance -= amount as i64;
        let (mg, _) = acct.margin_mut(dex);
        mg.collateral = mg.collateral.checked_add(amount as i64).ok_or(Error::MathOverflow)?;
        Ok(())
    }

    /// Moves free collateral out of a DEX; the account must still meet
    /// initial margin there.
    pub fn transfer_from_dex(&mut self, a: AccountId, dex: DexId, amount: u64) -> Result<()> {
        let mg = self.account(a)?.margin(dex).ok_or(Error::TraderNotFound)?;
        if amount == 0 || (amount as i128) > mg.collateral as i128 {
            return Err(Error::InsufficientCollateral);
        }
        for (mid, p) in &mg.positions {
            let m = self.market(*mid)?;
            if m.status == MARKET_ACTIVE && !p.is_empty() && !self.oracle_fresh(m) {
                return Err(Error::OracleStale);
            }
        }
        let h = self.health(a, dex, None)?;
        if h.equity - (amount as i128) < h.initial {
            return Err(Error::InsufficientMargin);
        }
        let acct = &mut self.accounts[a as usize];
        acct.margin_mut(dex).0.collateral -= amount as i64;
        acct.balance = acct.balance.checked_add(amount as i64).ok_or(Error::MathOverflow)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HIP-3 deployer lifecycle
// ---------------------------------------------------------------------------

impl State {
    pub fn deploy_dex(
        &mut self,
        a: AccountId,
        name: [u8; 16],
        fee_share_bps: u64,
        stake: u64,
        oracle_authority: Key,
        out: &mut Events,
    ) -> Result<DexId> {
        if stake < self.params.min_deployer_stake {
            return Err(Error::StakeTooLow);
        }
        if fee_share_bps > self.params.max_fee_share_bps {
            return Err(Error::FeeShareTooHigh);
        }
        if stake > self.account(a)?.stake_balance {
            return Err(Error::InsufficientCollateral);
        }
        self.accounts[a as usize].stake_balance -= stake;
        let id = self.dexes.len() as DexId;
        self.dexes.push(Dex {
            id,
            name,
            deployer: a,
            oracle_authority,
            status: DEX_ACTIVE,
            fee_share_bps,
            stake,
            deployed_ts: self.now,
            ..Dex::default()
        });
        out.push(Event::DexDeployed { dex: id, deployer: a, stake });
        Ok(id)
    }

    fn deployer_dex(&self, a: AccountId, dex: DexId) -> Result<&Dex> {
        let d = self.dex(dex)?;
        if d.deployer != a {
            return Err(Error::NotOrderOwner);
        }
        if d.status != DEX_ACTIVE {
            return Err(Error::DexNotActive);
        }
        Ok(d)
    }

    pub fn set_fee_share(&mut self, a: AccountId, dex: DexId, bps: u64) -> Result<()> {
        self.deployer_dex(a, dex)?;
        if bps > self.params.max_fee_share_bps {
            return Err(Error::FeeShareTooHigh);
        }
        self.dexes[dex as usize].fee_share_bps = bps;
        Ok(())
    }

    pub fn set_oracle_authority(&mut self, a: AccountId, dex: DexId, authority: Key) -> Result<()> {
        self.deployer_dex(a, dex)?;
        self.dexes[dex as usize].oracle_authority = authority;
        Ok(())
    }

    pub fn add_market(&mut self, a: AccountId, dex: DexId, cfg: MarketArgs, out: &mut Events) -> Result<MarketId> {
        self.deployer_dex(a, dex)?;
        self.insert_market(dex, cfg, None, 0, out)
    }

    fn insert_market(
        &mut self,
        dex: DexId,
        cfg: MarketArgs,
        creator: Option<AccountId>,
        creator_fee_share_bps: u64,
        out: &mut Events,
    ) -> Result<MarketId> {
        let p = &self.params;
        let valid = cfg.tick_size > 0
            && cfg.min_base_lots > 0
            && cfg.initial_margin_bps >= p.min_initial_margin_bps
            && cfg.initial_margin_bps <= BPS as u64
            && cfg.maintenance_margin_bps > 0
            && cfg.maintenance_margin_bps < cfg.initial_margin_bps
            && cfg.liquidation_fee_bps <= cfg.maintenance_margin_bps
            && cfg.oi_cap > 0
            && cfg.initial_price > 0
            && cfg.funding_interval > 0
            && cfg.max_funding_bps <= cfg.maintenance_margin_bps
            && creator_fee_share_bps <= p.max_fee_share_bps;
        if !valid {
            return Err(Error::InvalidMarketParams);
        }
        let id = self.markets.len() as MarketId;
        self.markets.push(Market {
            id,
            dex,
            cfg,
            status: MARKET_ACTIVE,
            oracle_price: cfg.initial_price,
            oracle_ts: self.now,
            cum_funding: 0,
            last_funding_ts: self.now,
            open_interest: 0,
            last_trade_price: 0,
            settlement_price: 0,
            creator,
            creator_fee_share_bps,
            premium_acc: 0,
            premium_ts: self.now,
            book: Default::default(),
        });
        self.dexes[dex as usize].markets.push(id);
        out.push(Event::MarketAdded { dex, market: id });
        Ok(id)
    }

    /// A DEX run by the protocol itself: no stake, prices from validator
    /// attestations. The launchpad lists graduated tokens here.
    pub fn create_protocol_dex(&mut self, name: [u8; 16], out: &mut Events) -> DexId {
        let deployer = self.account_or_create(&PROTOCOL_KEY);
        let id = self.dexes.len() as DexId;
        self.dexes.push(Dex {
            id,
            name,
            deployer,
            status: DEX_ACTIVE,
            deployed_ts: self.now,
            attested_oracle: true,
            ..Dex::default()
        });
        out.push(Event::DexDeployed { dex: id, deployer, stake: 0 });
        id
    }

    /// Lists a market on a protocol DEX with a creator who earns the fee share.
    pub fn add_protocol_market(
        &mut self,
        dex: DexId,
        cfg: MarketArgs,
        creator: AccountId,
        creator_fee_share_bps: u64,
        out: &mut Events,
    ) -> Result<MarketId> {
        let d = self.dex(dex)?;
        if !d.attested_oracle || d.status != DEX_ACTIVE {
            return Err(Error::DexNotActive);
        }
        self.account(creator)?;
        self.insert_market(dex, cfg, Some(creator), creator_fee_share_bps, out)
    }

    /// Validator-attested prices for a protocol DEX.
    pub fn set_oracle_attested(&mut self, dex: DexId, prices: &[(MarketId, u64)]) -> Result<()> {
        if !self.dex(dex)?.attested_oracle {
            return Err(Error::NotOrderOwner);
        }
        self.apply_oracle(dex, prices)
    }

    /// Open interest cap of a protocol market, scaled by the chain to the
    /// token's spot liquidity.
    pub fn set_oi_cap_attested(&mut self, m: MarketId, cap: u64) -> Result<()> {
        let market = self.market(m)?;
        if !self.dex(market.dex)?.attested_oracle {
            return Err(Error::NotOrderOwner);
        }
        if cap == 0 {
            return Err(Error::InvalidMarketParams);
        }
        self.markets[m as usize].cfg.oi_cap = cap;
        Ok(())
    }

    /// The DEX oracle push. All prices are validated before any is applied.
    pub fn set_oracle(&mut self, signer: &Key, dex: DexId, prices: &[(MarketId, u64)]) -> Result<()> {
        let d = self.dex(dex)?;
        if d.attested_oracle || &d.oracle_authority != signer {
            return Err(Error::NotOrderOwner);
        }
        self.apply_oracle(dex, prices)
    }

    fn apply_oracle(&mut self, dex: DexId, prices: &[(MarketId, u64)]) -> Result<()> {
        if self.dex(dex)?.status != DEX_ACTIVE {
            return Err(Error::DexNotActive);
        }
        for &(m, price) in prices {
            let market = self.active_market(m)?;
            if market.dex != dex {
                return Err(Error::InvalidMarket);
            }
            if price == 0 {
                return Err(Error::InvalidPrice);
            }
            if self.now.saturating_sub(market.oracle_ts) < self.params.min_oracle_interval {
                return Err(Error::OracleTooFrequent);
            }
            if !within_bps(price, market.oracle_price, self.params.max_oracle_move_bps) {
                return Err(Error::OracleMoveTooLarge);
            }
        }
        for &(m, price) in prices {
            let market = &mut self.markets[m as usize];
            market.oracle_price = price;
            market.oracle_ts = self.now;
        }
        Ok(())
    }

    /// Halts a market and settles every position at `price` in one go.
    /// `governance` lets the protocol wind down a frozen DEX.
    pub fn halt_market(&mut self, a: Option<AccountId>, m: MarketId, price: u64, out: &mut Events) -> Result<()> {
        let market = self.active_market(m)?;
        let dex = market.dex;
        let d = self.dex(dex)?;
        match a {
            Some(a) if d.deployer == a && d.status == DEX_ACTIVE => {}
            None if d.status == DEX_FROZEN || d.attested_oracle => {}
            _ => return Err(Error::NotOrderOwner),
        }
        if price == 0 || !within_bps(price, market.oracle_price, self.params.max_oracle_move_bps) {
            return Err(Error::InvalidPrice);
        }
        for o in self.markets[m as usize].book.clear() {
            self.release_open(o.account, dex, m, o.side, o.remaining);
            out.push(Event::Canceled {
                market: m,
                order_id: o.id,
                account: o.account,
                remaining: o.remaining,
                reason: CancelReason::MarketHalted,
            });
        }
        let market = &mut self.markets[m as usize];
        market.status = MARKET_HALTED;
        market.settlement_price = price;

        let members = self.dexes[dex as usize].members.clone();
        for a in members {
            self.settle_funding(a, dex, m)?;
            let (mg, _) = self.accounts[a as usize].margin_mut(dex);
            let p = mg.position(m);
            if p.base == 0 && p.quote == 0 {
                continue;
            }
            let pnl = math::add(math::mul(p.base as i128, price as i128)?, p.quote)?;
            mg.collateral = math::to_i64(math::add(mg.collateral as i128, pnl)?)?;
            let pos = mg.position_mut(m);
            pos.base = 0;
            pos.quote = 0;
            self.markets[m as usize].open_interest -= long(p.base);
            self.resolve_bad_debt(a, dex, out)?;
        }
        self.markets[m as usize].status = MARKET_SETTLED;
        out.push(Event::MarketSettled { market: m, price });
        Ok(())
    }

    pub fn request_unstake(&mut self, a: AccountId, dex: DexId) -> Result<()> {
        let d = self.dex(dex)?;
        if d.deployer != a {
            return Err(Error::NotOrderOwner);
        }
        if d.status == DEX_RETIRED {
            return Err(Error::DexNotActive);
        }
        if d.unstake_requested_ts != 0 {
            return Err(Error::UnstakePending);
        }
        if d.markets.iter().any(|&m| self.markets[m as usize].status != MARKET_SETTLED) {
            return Err(Error::MarketsOpen);
        }
        if self.now < d.deployed_ts.saturating_add(self.params.min_stake_hold_secs) {
            return Err(Error::StakeLocked);
        }
        self.dexes[dex as usize].unstake_requested_ts = self.now;
        Ok(())
    }

    pub fn complete_unstake(&mut self, a: AccountId, dex: DexId) -> Result<u64> {
        let d = self.dex(dex)?;
        if d.deployer != a {
            return Err(Error::NotOrderOwner);
        }
        if d.unstake_requested_ts == 0 {
            return Err(Error::UnstakeNotRequested);
        }
        if self.now < d.unstake_requested_ts.saturating_add(self.params.unstake_delay_secs) {
            return Err(Error::UnstakeDelay);
        }
        let d = &mut self.dexes[dex as usize];
        let amount = d.stake;
        d.stake = 0;
        d.status = DEX_RETIRED;
        let acct = &mut self.accounts[a as usize];
        acct.stake_balance = acct.stake_balance.checked_add(amount).ok_or(Error::MathOverflow)?;
        Ok(amount)
    }

    /// Applied by the chain once a validator quorum signed the slash.
    pub fn slash(&mut self, dex: DexId, bps: u64, freeze: bool, out: &mut Events) -> Result<u64> {
        let d = self.dexes.get_mut(dex as usize).ok_or(Error::DexNotActive)?;
        if d.status == DEX_RETIRED {
            return Err(Error::DexNotActive);
        }
        if bps == 0 || bps > BPS as u64 {
            return Err(Error::InvalidAmount);
        }
        let amount = (d.stake as u128 * bps as u128 / BPS as u128) as u64;
        d.stake -= amount;
        if freeze {
            d.status = DEX_FROZEN;
        }
        self.slashed_stake += amount;
        out.push(Event::Slashed { dex, amount, frozen: freeze });
        Ok(amount)
    }

    pub fn claim_deployer_fees(&mut self, a: AccountId, dex: DexId) -> Result<u64> {
        self.deployer_dex(a, dex)?;
        let d = &mut self.dexes[dex as usize];
        let fees = d.deployer_fees.max(0);
        d.deployer_fees -= fees;
        let acct = &mut self.accounts[a as usize];
        acct.balance = acct.balance.checked_add(fees).ok_or(Error::MathOverflow)?;
        Ok(fees as u64)
    }

    pub fn sweep_protocol_fees(&mut self, dex: DexId) -> Result<u64> {
        let d = self.dexes.get_mut(dex as usize).ok_or(Error::DexNotActive)?;
        let fees = d.protocol_fees.max(0);
        d.protocol_fees -= fees;
        self.treasury = self.treasury.checked_add(fees).ok_or(Error::MathOverflow)?;
        Ok(fees as u64)
    }

    pub fn fund_insurance(&mut self, a: AccountId, dex: DexId, amount: u64) -> Result<()> {
        self.dex(dex)?;
        let acct = self.accounts.get_mut(a as usize).ok_or(Error::TraderNotFound)?;
        if amount == 0 || (amount as i128) > acct.balance as i128 {
            return Err(Error::InsufficientCollateral);
        }
        acct.balance -= amount as i64;
        let d = &mut self.dexes[dex as usize];
        d.insurance = d.insurance.checked_add(amount as i64).ok_or(Error::MathOverflow)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Trading
// ---------------------------------------------------------------------------

impl State {
    /// Places an order. The whole match is simulated first (fills, fees,
    /// open interest, the taker's margin), then applied; an `Err` changes
    /// nothing.
    pub fn place_order(&mut self, a: AccountId, req: &OrderRequest, out: &mut Events) -> Result<PlaceOutcome> {
        let m = req.market;
        let market = self.active_market(m)?;
        self.account(a)?;
        let dex = market.dex;
        let cfg = market.cfg;
        if req.price == 0 {
            return Err(Error::InvalidPrice);
        }
        if !req.price.is_multiple_of(cfg.tick_size) {
            return Err(Error::TickSize);
        }
        if req.size == 0 || (!req.reduce_only && req.size < cfg.min_base_lots) {
            return Err(Error::InvalidSize);
        }
        if !req.reduce_only && !self.oracle_fresh(market) {
            return Err(Error::OracleStale);
        }
        if !within_bps(req.price, market.oracle_price, self.params.order_band_bps) {
            return Err(Error::PriceBand);
        }
        let pos0 = self.accounts[a as usize].margin(dex).map(|mg| mg.position(m)).unwrap_or_default();
        let side = req.side;
        let opposite = side.opposite();
        let mut size = req.size;
        if req.reduce_only {
            let reducible = match side {
                Side::Bid if pos0.base < 0 => pos0.base.unsigned_abs(),
                Side::Ask if pos0.base > 0 => pos0.base.unsigned_abs(),
                _ => return Err(Error::NoPosition),
            };
            size = size.min(reducible);
        }
        if req.kind == OrderKind::PostOnly
            && let Some(best) = market.book.best_price(opposite)
            && crosses(side, req.price, best)
        {
            return Err(Error::PostOnlyWouldCross);
        }

        // --- simulate ------------------------------------------------------
        let max_steps = self.params.max_fills_per_order.max(1);
        let mut plan: Vec<Step> = Vec::new();
        let mut remaining = size;
        let mut capped = false;
        for o in market.book.iter(opposite) {
            if remaining == 0 || !crosses(side, req.price, o.price) {
                break;
            }
            if plan.len() as u64 >= max_steps {
                capped = true;
                break;
            }
            if o.account == a {
                plan.push(Step::SelfCancel { order: *o });
                continue;
            }
            let q = remaining.min(o.remaining);
            plan.push(Step::Fill { order: *o, size: q });
            remaining -= q;
        }
        let rests = remaining > 0 && !req.reduce_only && req.kind != OrderKind::ImmediateOrCancel && !capped;

        let (share, creator) = self.fee_share(m);
        let mut p = pos0;
        settle_position_funding(market, &mut p)?;
        let mut taker_fees: i128 = 0;
        let mut maker_base: Vec<(AccountId, i64, i64)> = Vec::new(); // (account, base before, base after)
        let mut out_filled = 0u64;
        let mut out_quote: i128 = 0;
        for step in &plan {
            match step {
                Step::SelfCancel { order } => match opposite {
                    Side::Bid => p.open_bids -= order.remaining,
                    Side::Ask => p.open_asks -= order.remaining,
                },
                Step::Fill { order, size } => {
                    let notional = math::mul(*size as i128, order.price as i128)?;
                    let (pf, df) = fees(notional, self.params.taker_fee_bps, share)?;
                    taker_fees = math::add(taker_fees, math::add(pf, df)?)?;
                    let signed = if side == Side::Bid { *size as i64 } else { -(*size as i64) };
                    p.base = p.base.checked_add(signed).ok_or(Error::MathOverflow)?;
                    p.quote = math::sub(p.quote, math::mul(signed as i128, order.price as i128)?)?;
                    out_filled += size;
                    out_quote = math::add(out_quote, notional)?;
                    let mbase = match maker_base.iter_mut().find(|(id, _, _)| *id == order.account) {
                        Some(e) => e,
                        None => {
                            let b =
                                self.accounts[order.account as usize].margin(dex).map_or(0, |mg| mg.position(m).base);
                            maker_base.push((order.account, b, b));
                            maker_base.last_mut().unwrap()
                        }
                    };
                    mbase.2 = mbase.2.checked_sub(signed).ok_or(Error::MathOverflow)?;
                }
            }
        }
        if rests {
            match side {
                Side::Bid => p.open_bids += remaining,
                Side::Ask => p.open_asks += remaining,
            }
        }
        let oi_before = market.open_interest;
        let mut oi = oi_before as i128 + long(p.base) as i128 - long(pos0.base) as i128;
        for (_, before, after) in &maker_base {
            oi += long(*after) as i128 - long(*before) as i128;
        }
        if oi > cfg.oi_cap as i128 && oi > oi_before as i128 {
            return Err(Error::OpenInterestCap);
        }
        if !req.reduce_only {
            let h = self.health(a, dex, Some((m, p, math::to_i64(-taker_fees)?)))?;
            if h.equity < h.initial {
                return Err(Error::InsufficientMargin);
            }
        }

        // --- apply ---------------------------------------------------------
        self.join(a, dex);
        self.settle_funding(a, dex, m)?;
        let mut last_price = None;
        for step in plan {
            match step {
                Step::SelfCancel { order } => {
                    self.markets[m as usize].book.remove(order.id);
                    self.release_open(a, dex, m, opposite, order.remaining);
                    out.push(Event::Canceled {
                        market: m,
                        order_id: order.id,
                        account: a,
                        remaining: order.remaining,
                        reason: CancelReason::SelfTrade,
                    });
                }
                Step::Fill { order, size } => {
                    let maker = order.account;
                    self.settle_funding(maker, dex, m)?;
                    let (buyer, seller) = if side == Side::Bid { (a, maker) } else { (maker, a) };
                    self.trade(m, buyer, seller, size, order.price)?;
                    self.release_open(maker, dex, m, opposite, size);
                    let notional = math::mul(size as i128, order.price as i128)?;
                    let (pf, df) = fees(notional, self.params.taker_fee_bps, share)?;
                    let taker_fee = self.charge_fee(a, dex, pf, df, creator)?;
                    let (pf, df) = fees(notional, self.params.maker_fee_bps, share)?;
                    let maker_fee = self.charge_fee(maker, dex, pf, df, creator)?;
                    let book = &mut self.markets[m as usize].book;
                    if size == order.remaining {
                        book.remove(order.id);
                    } else {
                        book.reduce(order.id, size);
                    }
                    last_price = Some(order.price);
                    out.push(Event::Fill {
                        market: m,
                        maker_order: order.id,
                        maker,
                        taker: a,
                        taker_side: side,
                        price: order.price,
                        size,
                        taker_fee,
                        maker_fee,
                    });
                }
            }
        }
        if let Some(px) = last_price {
            self.markets[m as usize].last_trade_price = px;
        }
        let mut outcome = PlaceOutcome { filled: out_filled, filled_quote: out_quote, ..PlaceOutcome::default() };
        if rests {
            let id = self.next_order_id;
            self.next_order_id += 1;
            self.markets[m as usize].book.insert(Order {
                id,
                account: a,
                side,
                price: req.price,
                remaining,
                client_id: req.client_id,
            });
            let (mg, _) = self.accounts[a as usize].margin_mut(dex);
            let pos = mg.position_mut(m);
            match side {
                Side::Bid => pos.open_bids += remaining,
                Side::Ask => pos.open_asks += remaining,
            }
            outcome.order_id = Some(id);
            outcome.resting = remaining;
            out.push(Event::Placed { market: m, order_id: id, account: a, side, price: req.price, size: remaining });
        }
        Ok(outcome)
    }

    pub fn cancel(&mut self, a: AccountId, m: MarketId, order_id: OrderId, out: &mut Events) -> Result<()> {
        let market = self.market(m)?;
        let o = *market.book.get(order_id).ok_or(Error::OrderNotFound)?;
        if o.account != a {
            return Err(Error::NotOrderOwner);
        }
        let dex = market.dex;
        self.markets[m as usize].book.remove(order_id);
        self.release_open(a, dex, m, o.side, o.remaining);
        out.push(Event::Canceled {
            market: m,
            order_id,
            account: a,
            remaining: o.remaining,
            reason: CancelReason::User,
        });
        Ok(())
    }

    /// Atomic cancel-replace of a resting order (same side and client id).
    /// Shrinking at the same price reduces the order in place and keeps its
    /// time priority. Otherwise the old order is cancelled and the new one
    /// placed; if placing fails, the old order is put back exactly where it
    /// was and nothing changes.
    #[allow(clippy::too_many_arguments)]
    pub fn modify_order(
        &mut self,
        a: AccountId,
        m: MarketId,
        order_id: OrderId,
        price: u64,
        size: u64,
        kind: OrderKind,
        reduce_only: bool,
        out: &mut Events,
    ) -> Result<PlaceOutcome> {
        let market = self.active_market(m)?;
        let o = *market.book.get(order_id).ok_or(Error::OrderNotFound)?;
        if o.account != a {
            return Err(Error::NotOrderOwner);
        }
        let (dex, min_lots) = (market.dex, market.cfg.min_base_lots);
        if price == o.price && size <= o.remaining && kind != OrderKind::ImmediateOrCancel {
            // The resting order's own flags aren't stored, so no exemption:
            // a shrink never leaves an order below the minimum size.
            if size < min_lots.max(1) {
                return Err(Error::InvalidSize);
            }
            if size < o.remaining {
                self.markets[m as usize].book.reduce(order_id, o.remaining - size);
                self.release_open(a, dex, m, o.side, o.remaining - size);
                out.push(Event::Reduced { market: m, order_id, account: a, remaining: size });
            }
            return Ok(PlaceOutcome { order_id: Some(order_id), resting: size, ..PlaceOutcome::default() });
        }
        let behind = market.book.next_in_level(order_id);
        self.markets[m as usize].book.remove(order_id);
        self.release_open(a, dex, m, o.side, o.remaining);
        let req = OrderRequest { market: m, side: o.side, kind, price, size, reduce_only, client_id: o.client_id };
        let mut events = Vec::new();
        match self.place_order(a, &req, &mut events) {
            Ok(r) => {
                out.push(Event::Canceled {
                    market: m,
                    order_id,
                    account: a,
                    remaining: o.remaining,
                    reason: CancelReason::User,
                });
                out.extend(events);
                Ok(r)
            }
            Err(e) => {
                self.markets[m as usize].book.insert_before(o, behind);
                let (mg, _) = self.accounts[a as usize].margin_mut(dex);
                let p = mg.position_mut(m);
                match o.side {
                    Side::Bid => p.open_bids += o.remaining,
                    Side::Ask => p.open_asks += o.remaining,
                }
                Err(e)
            }
        }
    }

    /// Cancels every order of `a` in market `m`. O(orders in the book).
    pub fn cancel_all(&mut self, a: AccountId, m: MarketId, reason: CancelReason, out: &mut Events) -> Result<usize> {
        let market = self.market(m)?;
        let dex = market.dex;
        let mine: Vec<Order> = market
            .book
            .iter(Side::Bid)
            .chain(market.book.iter(Side::Ask))
            .filter(|o| o.account == a)
            .copied()
            .collect();
        for o in &mine {
            self.markets[m as usize].book.remove(o.id);
            self.release_open(a, dex, m, o.side, o.remaining);
            out.push(Event::Canceled { market: m, order_id: o.id, account: a, remaining: o.remaining, reason });
        }
        Ok(mine.len())
    }

    // -----------------------------------------------------------------------
    // Liquidation and bad debt
    // -----------------------------------------------------------------------

    /// Backstop liquidation: `liquidator` takes over up to `max_size` lots of
    /// the position at the mark price (0 = all) and earns part of the
    /// penalty. Atomic: margins are snapshotted and restored if the
    /// liquidator ends up under initial margin.
    pub fn liquidate(
        &mut self,
        liquidator: AccountId,
        target: AccountId,
        m: MarketId,
        max_size: u64,
        out: &mut Events,
    ) -> Result<u64> {
        self.account(liquidator)?;
        self.account(target)?;
        if liquidator == target {
            return Err(Error::SelfLiquidation);
        }
        let market = self.active_market(m)?;
        if !self.oracle_fresh(market) {
            return Err(Error::OracleStale);
        }
        let dex = market.dex;
        let h = self.health(target, dex, None)?;
        if !h.liquidatable() {
            return Err(Error::NotLiquidatable);
        }
        let base = self.accounts[target as usize].margin(dex).map_or(0, |mg| mg.position(m).base);
        if base == 0 {
            return Err(Error::NoPosition);
        }
        // Bankrupt beyond what the insurance fund can cover: deleverage the
        // most profitable opposite positions instead of socializing the loss.
        if h.equity < 0 && (self.dexes[dex as usize].insurance.max(0) as i128) < -h.equity {
            return self.auto_deleverage(target, m, out);
        }
        let size = if max_size == 0 { base.unsigned_abs() } else { base.unsigned_abs().min(max_size) };
        let price = self.mark_price(market);
        let liq_fee_bps = market.cfg.liquidation_fee_bps;

        let snap_target = self.accounts[target as usize].margin(dex).cloned();
        let snap_liq = self.accounts[liquidator as usize].margin(dex).cloned();
        let snap_oi = self.markets[m as usize].open_interest;
        let snap_insurance = self.dexes[dex as usize].insurance;
        let members_len = self.dexes[dex as usize].members.len();
        let members_hash = self.dexes[dex as usize].members_hash;

        let result = (|| -> Result<i64> {
            self.join(liquidator, dex);
            self.settle_funding(target, dex, m)?;
            self.settle_funding(liquidator, dex, m)?;
            if base > 0 {
                self.trade(m, liquidator, target, size, price)?;
            } else {
                self.trade(m, target, liquidator, size, price)?;
            }
            let notional = math::mul(size as i128, price as i128)?;
            let left = self.health(target, dex, None)?.equity.max(0);
            let penalty = math::notional_bps_ceil(notional, liq_fee_bps)?.min(left);
            let to_insurance =
                math::notional_bps_ceil(penalty, self.params.liquidation_insurance_share_bps)?.min(penalty);
            let t = self.accounts[target as usize].margin_mut(dex).0;
            t.collateral = math::to_i64(math::sub(t.collateral as i128, penalty)?)?;
            let l = self.accounts[liquidator as usize].margin_mut(dex).0;
            l.collateral = math::to_i64(math::add(l.collateral as i128, penalty - to_insurance)?)?;
            let d = &mut self.dexes[dex as usize];
            d.insurance = math::to_i64(math::add(d.insurance as i128, to_insurance)?)?;
            let lh = self.health(liquidator, dex, None)?;
            if lh.equity < lh.initial {
                return Err(Error::InsufficientMargin);
            }
            math::to_i64(penalty)
        })();

        let penalty = match result {
            Ok(p) => p,
            Err(e) => {
                let restore = |acct: &mut Account, snap: Option<Margin>| match snap {
                    Some(mg) => *acct.margin_mut(dex).0 = mg,
                    None => acct.margins.retain(|(id, _)| *id != dex),
                };
                restore(&mut self.accounts[target as usize], snap_target);
                restore(&mut self.accounts[liquidator as usize], snap_liq);
                self.markets[m as usize].open_interest = snap_oi;
                self.dexes[dex as usize].insurance = snap_insurance;
                self.dexes[dex as usize].members.truncate(members_len);
                self.dexes[dex as usize].members_hash = members_hash;
                return Err(e);
            }
        };

        self.cancel_all(target, m, CancelReason::Liquidation, out)?;
        out.push(Event::Liquidated { market: m, liquidator, liquidatee: target, size, price, penalty });
        self.resolve_bad_debt(target, dex, out)?;
        Ok(size)
    }

    /// Closes `target`'s whole position in `m` against opposite positions,
    /// most profitable first (unrealized PnL per unit of collateral), at the
    /// bankruptcy price: the price at which `target` ends with zero equity.
    fn auto_deleverage(&mut self, target: AccountId, m: MarketId, out: &mut Events) -> Result<u64> {
        let dex = self.markets[m as usize].dex;
        self.cancel_all(target, m, CancelReason::Liquidation, out)?;
        self.settle_funding(target, dex, m)?;
        let p = self.accounts[target as usize].margin(dex).expect("member").position(m);
        let mark = self.mark_price(&self.markets[m as usize]) as i128;
        let equity = self.health(target, dex, None)?.equity;
        let others = equity - (p.base as i128 * mark + p.quote);
        // base·pb + quote + others = 0, rounded so the target keeps >= 0.
        let num = -(p.quote + others);
        let pb =
            if p.base > 0 { math::div_ceil(num, p.base as i128) } else { math::div_floor(-num, -(p.base as i128)) };
        let pb = pb.clamp(1, u64::MAX as i128) as u64;

        let market = &self.markets[m as usize];
        let mut cands: Vec<(i128, AccountId, u64)> = Vec::new();
        for &x in &self.dexes[dex as usize].members {
            if x == target {
                continue;
            }
            let mg = self.accounts[x as usize].margin(dex).expect("member");
            let cp = mg.position(m);
            if cp.base == 0 || (cp.base > 0) == (p.base > 0) {
                continue;
            }
            let pnl = cp.base as i128 * mark + cp.quote - pending_funding(market, &cp)?;
            let score = math::mul(pnl, BPS)? / (mg.collateral.max(1) as i128);
            cands.push((score, x, cp.base.unsigned_abs()));
        }
        cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        let mut remaining = p.base.unsigned_abs();
        for (_, cp, cp_size) in cands {
            if remaining == 0 {
                break;
            }
            let q = remaining.min(cp_size);
            self.settle_funding(cp, dex, m)?;
            if p.base > 0 {
                self.trade(m, cp, target, q, pb)?;
            } else {
                self.trade(m, target, cp, q, pb)?;
            }
            out.push(Event::Adl { market: m, target, counterparty: cp, size: q, price: pb });
            remaining -= q;
        }
        debug_assert_eq!(remaining, 0, "open interest is balanced, so the other side covers the position");
        self.resolve_bad_debt(target, dex, out)?;
        Ok(p.base.unsigned_abs() - remaining)
    }

    /// Accounts in `dex` currently below maintenance margin (for keepers).
    pub fn liquidatable(&self, dex: DexId) -> Vec<AccountId> {
        let Ok(d) = self.dex(dex) else { return Vec::new() };
        d.members.iter().copied().filter(|&a| self.health(a, dex, None).is_ok_and(|h| h.liquidatable())).collect()
    }

    /// Flat account with negative collateral: insurance first, then pro rata
    /// from other members' positive collateral, the rest is unrecovered.
    fn resolve_bad_debt(&mut self, a: AccountId, dex: DexId, out: &mut Events) -> Result<()> {
        let Some(mg) = self.accounts[a as usize].margin(dex) else { return Ok(()) };
        if mg.collateral >= 0 || mg.positions.iter().any(|(_, p)| p.base != 0 || p.quote != 0) {
            return Ok(());
        }
        let debt = -(mg.collateral as i128);
        self.accounts[a as usize].margin_mut(dex).0.collateral = 0;
        let d = &mut self.dexes[dex as usize];
        let from_insurance = debt.min(d.insurance.max(0) as i128);
        d.insurance -= from_insurance as i64;
        let mut left = debt - from_insurance;

        let members = d.members.clone();
        let collateral_of = |acct: &Account| acct.margin(dex).map_or(0, |mg| mg.collateral.max(0)) as i128;
        let pool: i128 = members.iter().filter(|&&x| x != a).map(|&x| collateral_of(&self.accounts[x as usize])).sum();
        let mut socialized = 0i128;
        if left > 0 && pool > 0 {
            let target = left.min(pool);
            for &x in members.iter().filter(|&&x| x != a) {
                let c = collateral_of(&self.accounts[x as usize]);
                if c == 0 {
                    continue;
                }
                let share = math::div_ceil(math::mul(target, c)?, pool).min(c);
                self.accounts[x as usize].margin_mut(dex).0.collateral -= share as i64;
                socialized += share;
            }
            left = (left - socialized).max(0);
        }
        let d = &mut self.dexes[dex as usize];
        d.socialized_loss = math::to_i64(math::add(d.socialized_loss as i128, socialized)?)?;
        d.unrecovered_loss = math::to_i64(math::add(d.unrecovered_loss as i128, left)?)?;
        out.push(Event::BadDebt {
            dex,
            account: a,
            amount: math::to_i64(debt)?,
            from_insurance: from_insurance as i64,
            socialized: math::to_i64(socialized)?,
            unrecovered: math::to_i64(left)?,
        });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Block hooks
    // -----------------------------------------------------------------------

    /// Advances the clock and runs funding for every market that is due.
    /// Called by the chain at the start of each block, so funding needs no
    /// crank transactions.
    pub fn begin_block(&mut self, now: i64, out: &mut Events) -> Result<()> {
        self.now = now.max(self.now);
        for i in 0..self.markets.len() {
            let m = &self.markets[i];
            if m.status != MARKET_ACTIVE {
                continue;
            }
            let fresh = self.oracle_fresh(m);
            let oracle = m.oracle_price as i128;
            let cap = m.cfg.max_funding_bps as i128;
            // Sample the premium over the time since the last block: funding
            // pays the time-weighted average, so a single block can't game it.
            let premium = if fresh { math::mul(self.mark_price(m) as i128 - oracle, BPS)? / oracle } else { 0 };
            let dt = (self.now - m.premium_ts).max(0) as i128;
            let due = self.now >= m.last_funding_ts.saturating_add(m.cfg.funding_interval) && fresh;
            let elapsed = (self.now - m.last_funding_ts).max(1) as i128;
            let m = &mut self.markets[i];
            m.premium_acc = math::add(m.premium_acc, math::mul(premium, dt)?)?;
            m.premium_ts = self.now;
            if !due {
                continue;
            }
            let rate = (m.premium_acc / elapsed).clamp(-cap, cap);
            let delta = math::mul(math::mul(oracle, rate)?, FUNDING_SCALE)? / BPS;
            m.cum_funding = math::add(m.cum_funding, delta)?;
            m.last_funding_ts = self.now;
            m.premium_acc = 0;
            out.push(Event::Funding { market: m.id, rate_bps: rate as i64 });
        }
        Ok(())
    }
}
