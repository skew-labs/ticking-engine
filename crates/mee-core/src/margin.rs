//! Mark price, account equity and margin requirements (cross margin within a DEX).

use crate::error::Result;
use crate::math::{self, BPS, FUNDING_SCALE};
use crate::state::*;

/// Mark price: median of oracle, best bid and best ask (oracle alone if one
/// side is empty), clamped to oracle ± `mark_band_bps` so a thin book cannot
/// drag margin. Halted and settled markets mark at the settlement price.
pub fn mark_price(m: &Market, params: &EngineParams) -> u64 {
    if m.status == MARKET_HALTED || m.status == MARKET_SETTLED {
        return m.settlement_price;
    }
    let o = m.oracle_price;
    let raw = if m.best_bid > 0 && m.best_ask > 0 { math::median3(o, m.best_bid, m.best_ask) } else { o };
    let band = (o as u128 * params.mark_band_bps as u128 / BPS as u128) as u64;
    raw.clamp(o.saturating_sub(band), o.saturating_add(band))
}

/// Funding owed by a position since it was last settled, rounded against the
/// trader (payers pay at least the exact amount, receivers get at most it).
pub fn pending_funding(m: &Market, p: &Position) -> Result<i128> {
    if p.base == 0 {
        return Ok(0);
    }
    let delta = math::sub(m.cum_funding.get(), p.last_funding.get())?;
    Ok(math::div_ceil(math::mul(p.base as i128, delta)?, FUNDING_SCALE))
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    /// Collateral + unrealized PnL − pending funding.
    pub equity: i128,
    /// Needed to open risk: worst case over resting orders filling.
    pub initial: i128,
    /// Below this the account can be liquidated.
    pub maintenance: i128,
}

impl Health {
    pub fn liquidatable(&self) -> bool {
        self.equity < self.maintenance
    }
}

pub fn health(dex: &DexState, params: &EngineParams, trader: usize) -> Result<Health> {
    let t = &dex.traders[trader];
    let mut h = Health { equity: t.collateral as i128, ..Health::default() };
    for (i, m) in dex.markets.iter().enumerate().take(dex.header.market_count as usize) {
        let p = &t.positions[i];
        if p.is_empty() {
            continue;
        }
        let mark = mark_price(m, params) as i128;
        h.equity = math::add(h.equity, math::add(math::mul(p.base as i128, mark)?, p.quote.get())?)?;
        h.equity = math::sub(h.equity, pending_funding(m, p)?)?;

        let base = p.base as i128;
        let worst = if m.status == MARKET_ACTIVE {
            (base + p.open_bids as i128).abs().max((base - p.open_asks as i128).abs())
        } else {
            base.abs()
        };
        h.initial = math::add(h.initial, math::notional_bps_ceil(math::mul(worst, mark)?, m.initial_margin_bps)?)?;
        h.maintenance =
            math::add(h.maintenance, math::notional_bps_ceil(math::mul(base.abs(), mark)?, m.maintenance_margin_bps)?)?;
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> EngineParams {
        EngineParams { mark_band_bps: 100, ..EngineParams::default() }
    }

    #[test]
    fn mark_is_median_clamped() {
        let mut m = Market { status: MARKET_ACTIVE, oracle_price: 10_000, ..Market::default() };
        assert_eq!(mark_price(&m, &params()), 10_000);
        m.best_bid = 10_010;
        m.best_ask = 10_020;
        assert_eq!(mark_price(&m, &params()), 10_010);
        // A spoofed book far above oracle is clamped to +1%.
        m.best_bid = 20_000;
        m.best_ask = 20_001;
        assert_eq!(mark_price(&m, &params()), 10_100);
        m.status = MARKET_HALTED;
        m.settlement_price = 9_000;
        assert_eq!(mark_price(&m, &params()), 9_000);
    }

    #[test]
    fn pending_funding_rounds_against_trader() {
        let mut m = Market::default();
        m.cum_funding.set(FUNDING_SCALE + 1); // a hair over 1 atom per lot
        let long = Position { base: 3, ..Position::default() };
        let short = Position { base: -3, ..Position::default() };
        assert_eq!(pending_funding(&m, &long).unwrap(), 4); // pays 3.000…003 → 4
        assert_eq!(pending_funding(&m, &short).unwrap(), -3); // receives at most 3
    }
}
