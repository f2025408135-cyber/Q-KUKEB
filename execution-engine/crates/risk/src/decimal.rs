// FixedDecimal arithmetic helpers — NOT inlined into gate logic.
// All monetary and ratio arithmetic lives here.

use std::cmp::Ordering;

/// Mirror of the protobuf FixedDecimal for Rust-side arithmetic.
/// value = raw_units × 10^(-scale)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FixedDecimal {
    pub raw_units: i64,
    pub scale: u32,
}

impl FixedDecimal {
    pub const ZERO: Self = Self { raw_units: 0, scale: 0 };

    pub fn new(raw_units: i64, scale: u32) -> Self {
        Self { raw_units, scale }
    }

    /// Convert to f64 for GARCH arithmetic only.
    /// WARNING: This MUST NOT appear on the gRPC hot path.
    #[inline]
    pub fn as_f64(&self) -> f64 {
        if self.scale == 0 {
            return self.raw_units as f64;
        }
        self.raw_units as f64 / 10f64.powi(self.scale as i32)
    }

    /// Construct from f64 with explicit scale (rounds to nearest).
    pub fn from_f64(val: f64, scale: u32) -> Self {
        let multiplier = 10f64.powi(scale as i32);
        let raw = (val * multiplier).round() as i64;
        Self { raw_units: raw, scale }
    }

    /// Rescale to a common denominator, returning (numerator, denominator_scale).
    /// Useful before adding/subtracting two FixedDecimals with different scales.
    pub fn normalize_pair(a: &Self, b: &Self) -> (i64, i64, u32) {
        let target_scale = a.scale.max(b.scale);
        let a_rescaled = a.rescale(target_scale);
        let b_rescaled = b.rescale(target_scale);
        (a_rescaled.raw_units, b_rescaled.raw_units, target_scale)
    }

    /// Rescale this decimal to a new scale.
    fn rescale(&self, target: u32) -> Self {
        match self.scale.cmp(&target) {
            Ordering::Equal => *self,
            Ordering::Less => {
                let diff = target - self.scale;
                Self {
                    raw_units: self.raw_units * 10i64.pow(diff),
                    scale: target,
                }
            }
            Ordering::Greater => {
                let diff = self.scale - target;
                let divisor = 10i64.pow(diff);
                Self {
                    raw_units: self.raw_units / divisor,
                    scale: target,
                }
            }
        }
    }
}

impl std::ops::Add for FixedDecimal {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        let (a, b, scale) = Self::normalize_pair(&self, &rhs);
        Self { raw_units: a + b, scale }
    }
}

impl std::ops::Sub for FixedDecimal {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        let (a, b, scale) = Self::normalize_pair(&self, &rhs);
        Self { raw_units: a - b, scale }
    }
}

impl PartialOrd for FixedDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let (a, b, _) = Self::normalize_pair(self, other);
        a.partial_cmp(&b)
    }
}

impl Ord for FixedDecimal {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixed_decimal_basic() {
        let a = FixedDecimal::new(12345, 2); // 123.45
        let b = FixedDecimal::new(10, 0);    // 10
        let sum = a + b;
        assert_eq!(sum.raw_units, 13345);
        assert_eq!(sum.scale, 2);

        let diff = a - b;
        assert_eq!(diff.raw_units, 11345);
        assert_eq!(diff.scale, 2);
    }

    #[test]
    fn test_f64_roundtrip() {
        let val = 123.4567f64;
        let fd = FixedDecimal::from_f64(val, 4);
        assert_eq!(fd.raw_units, 1234567);
        assert_eq!(fd.as_f64() * 10000.0, 1234567.0);
    }

    #[test]
    fn test_comparison() {
        let a = FixedDecimal::new(1000, 1); // 100.0
        let b = FixedDecimal::new(9999, 2); // 99.99
        assert!(a > b);
    }
}
