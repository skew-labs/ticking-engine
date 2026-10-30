use bytemuck::{Pod, Zeroable};
use core::fmt;

use crate::error::{EngineError, Result};

pub const BPS: i128 = 10_000;
/// Funding indices are quote atoms per base lot, scaled by this factor.
pub const FUNDING_SCALE: i128 = 1_000_000_000;

/// `i128` stored as little-endian bytes so `Pod` structs keep 8-byte alignment
/// (Solana account data is only guaranteed 8-byte aligned).
#[repr(transparent)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct I128(pub [u8; 16]);

impl I128 {
    pub const ZERO: I128 = I128([0; 16]);

    pub fn new(v: i128) -> Self {
        I128(v.to_le_bytes())
    }

    pub fn get(&self) -> i128 {
        i128::from_le_bytes(self.0)
    }

    pub fn set(&mut self, v: i128) {
        self.0 = v.to_le_bytes();
    }
}

impl fmt::Debug for I128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

pub fn add(a: i128, b: i128) -> Result<i128> {
    a.checked_add(b).ok_or(EngineError::MathOverflow)
}

pub fn sub(a: i128, b: i128) -> Result<i128> {
    a.checked_sub(b).ok_or(EngineError::MathOverflow)
}

pub fn mul(a: i128, b: i128) -> Result<i128> {
    a.checked_mul(b).ok_or(EngineError::MathOverflow)
}

/// Division rounded toward +infinity. `d` must be positive.
pub fn div_ceil(n: i128, d: i128) -> i128 {
    debug_assert!(d > 0);
    let q = n / d;
    if n % d > 0 { q + 1 } else { q }
}

/// Division rounded toward -infinity. `d` must be positive.
pub fn div_floor(n: i128, d: i128) -> i128 {
    debug_assert!(d > 0);
    let q = n / d;
    if n % d < 0 { q - 1 } else { q }
}

pub fn to_i64(v: i128) -> Result<i64> {
    i64::try_from(v).map_err(|_| EngineError::MathOverflow)
}

pub fn to_u64(v: i128) -> Result<u64> {
    u64::try_from(v).map_err(|_| EngineError::MathOverflow)
}

/// `ceil(lots * price * bps / BPS)`, all non-negative.
pub fn notional_bps_ceil(notional: i128, bps: u64) -> Result<i128> {
    Ok(div_ceil(mul(notional, bps as i128)?, BPS))
}

pub fn median3(a: u64, b: u64, c: u64) -> u64 {
    a.max(b).min(a.min(b).max(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        assert_eq!(div_ceil(7, 2), 4);
        assert_eq!(div_ceil(-7, 2), -3);
        assert_eq!(div_ceil(6, 2), 3);
        assert_eq!(div_floor(7, 2), 3);
        assert_eq!(div_floor(-7, 2), -4);
        assert_eq!(div_floor(-6, 2), -3);
    }

    #[test]
    fn median() {
        assert_eq!(median3(1, 2, 3), 2);
        assert_eq!(median3(3, 1, 2), 2);
        assert_eq!(median3(2, 3, 1), 2);
        assert_eq!(median3(5, 5, 1), 5);
    }

    #[test]
    fn i128_roundtrip() {
        let mut x = I128::ZERO;
        x.set(-123_456_789_012_345_678_901_234_567);
        assert_eq!(x.get(), -123_456_789_012_345_678_901_234_567);
    }
}
