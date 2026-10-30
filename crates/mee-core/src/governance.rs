//! HIP-3 style deployer lifecycle: stake to deploy, hold the stake while
//! markets run, unstake through a delay during which the stake stays
//! slashable, and guardian-vote slashing.

use crate::error::{EngineError, Result};
use crate::math::BPS;
use crate::state::*;

pub fn deploy(dex: &mut DexState, params: &EngineParams, stake: u64, fee_share_bps: u64, now: i64) -> Result<()> {
    if stake < params.min_deployer_stake {
        return Err(EngineError::StakeTooLow);
    }
    if fee_share_bps > params.max_fee_share_bps {
        return Err(EngineError::FeeShareTooHigh);
    }
    let h = &mut dex.header;
    h.status = DEX_ACTIVE;
    h.stake_amount = stake;
    h.deployed_ts = now;
    h.fee_share_bps = fee_share_bps;
    Ok(())
}

pub fn require_active(dex: &DexState) -> Result<()> {
    if dex.header.status == DEX_ACTIVE { Ok(()) } else { Err(EngineError::DexNotActive) }
}

pub fn set_fee_share(dex: &mut DexState, params: &EngineParams, fee_share_bps: u64) -> Result<()> {
    require_active(dex)?;
    if fee_share_bps > params.max_fee_share_bps {
        return Err(EngineError::FeeShareTooHigh);
    }
    dex.header.fee_share_bps = fee_share_bps;
    Ok(())
}

/// Starts the unstake delay. Every market must be settled (or never created)
/// and the minimum hold since deployment must have passed.
pub fn request_unstake(dex: &mut DexState, params: &EngineParams, now: i64) -> Result<()> {
    if dex.header.status == DEX_RETIRED {
        return Err(EngineError::DexNotActive);
    }
    if dex.header.unstake_requested_ts != 0 {
        return Err(EngineError::UnstakePending);
    }
    let open = dex.markets[..dex.header.market_count as usize].iter().any(|m| m.status != MARKET_SETTLED);
    if open {
        return Err(EngineError::MarketsOpen);
    }
    if now < dex.header.deployed_ts.saturating_add(params.min_stake_hold_secs) {
        return Err(EngineError::StakeLocked);
    }
    dex.header.unstake_requested_ts = now;
    Ok(())
}

/// Ends the delay and returns the stake to pay out.
pub fn complete_unstake(dex: &mut DexState, params: &EngineParams, now: i64) -> Result<u64> {
    let h = &mut dex.header;
    if h.unstake_requested_ts == 0 {
        return Err(EngineError::UnstakeNotRequested);
    }
    if now < h.unstake_requested_ts.saturating_add(params.unstake_delay_secs) {
        return Err(EngineError::UnstakeDelay);
    }
    let amount = h.stake_amount;
    h.stake_amount = 0;
    h.status = DEX_RETIRED;
    h.slash_votes = 0;
    Ok(amount)
}

/// Records one guardian's vote for slashing `slash_bps` of the stake. All
/// votes on an open proposal must agree on the terms. Returns the vote count.
pub fn vote_slash(
    dex: &mut DexState,
    guardian: usize,
    guardian_count: usize,
    slash_bps: u64,
    freeze: bool,
) -> Result<u32> {
    if guardian >= guardian_count || guardian >= MAX_GUARDIANS {
        return Err(EngineError::InvalidGuardian);
    }
    if slash_bps == 0 || slash_bps > BPS as u64 {
        return Err(EngineError::InvalidAmount);
    }
    let h = &mut dex.header;
    if h.status == DEX_RETIRED {
        return Err(EngineError::DexNotActive);
    }
    if h.slash_votes == 0 {
        h.slash_bps = slash_bps;
        h.slash_freeze = freeze as u8;
    } else if h.slash_bps != slash_bps || h.slash_freeze != freeze as u8 {
        return Err(EngineError::ProposalMismatch);
    }
    let bit = 1u64 << guardian;
    if h.slash_votes & bit != 0 {
        return Err(EngineError::AlreadyVoted);
    }
    h.slash_votes |= bit;
    Ok(h.slash_votes.count_ones())
}

/// Executes a proposal that reached `threshold` votes. Returns the stake
/// amount to move to the treasury. A freezing slash hands the DEX to
/// governance: the deployer can no longer push oracles, add or halt markets.
pub fn execute_slash(dex: &mut DexState, threshold: u32) -> Result<u64> {
    let h = &mut dex.header;
    if h.slash_votes == 0 {
        return Err(EngineError::NoProposal);
    }
    if h.slash_votes.count_ones() < threshold.max(1) {
        return Err(EngineError::ThresholdNotMet);
    }
    let amount = (h.stake_amount as u128 * h.slash_bps as u128 / BPS as u128) as u64;
    h.stake_amount -= amount;
    if h.slash_freeze == 1 {
        h.status = DEX_FROZEN;
    }
    h.slash_votes = 0;
    h.slash_bps = 0;
    h.slash_freeze = 0;
    h.slash_nonce += 1;
    Ok(amount)
}

pub fn clear_slash(dex: &mut DexState) {
    let h = &mut dex.header;
    h.slash_votes = 0;
    h.slash_bps = 0;
    h.slash_freeze = 0;
}
