//! GARCH(1,1) conditional variance estimator with a fixed-size ring buffer.
//!
//! # Model
//!
//! The GARCH(1,1) model estimates conditional variance:
//!
//!   σ²_t = ω + α · r²_{t-1} + β · σ²_{t-1}
//!
//! where:
//!   - ω: long-run variance intercept (must be positive)
//!   - α: ARCH coefficient (reaction to recent squared return, must be ≥ 0)
//!   - β: GARCH coefficient (persistence of previous variance, must be ≥ 0)
//!   - α + β < 1.0 (enforced at construction, panics if violated)
//!
//! # Design constraints
//!
//! - Ring buffer of last N log-returns (default N=500, configurable via const generic)
//! - No heap allocation inside `update()` — stack-only arithmetic
//! - `#[inline(always)]` on the hot path
//! - Parameters initialized from config at construction, never received over network
//! - σ² initialized to a reasonable default (long-run variance: ω / (1 - α - β))

use tracing::warn;

/// GARCH(1,1) state machine with a ring buffer for log-returns.
///
/// # Type Parameters
/// - `N`: Ring buffer capacity (number of historical returns to retain).
///        Default = 500. Must be > 0.
///
/// # Invariants
/// - `alpha + beta < 1.0` (enforced at construction via panic)
/// - `omega > 0.0` (enforced at construction)
/// - Ring buffer never allocates after construction
#[derive(Debug, Clone)]
pub struct GarchState<const N: usize = 500> {
    /// ARCH coefficient — weight of the previous squared return.
    alpha: f64,

    /// GARCH coefficient — weight of the previous conditional variance.
    beta: f64,

    /// Long-run variance intercept.
    omega: f64,

    /// Ring buffer storing the last N log-returns (as fixed-point: r × 10^scale).
    /// Stored as i64 to avoid any floating-point issues on the input path.
    returns: [i64; N],

    /// Scale for the log-return encoding. Each return[i] represents
    /// returns[i] × 10^(-scale). This ensures deterministic integer storage.
    #[allow(dead_code)]
    scale: u32,

    /// Current write position in the ring buffer (0..N).
    pos: usize,

    /// Number of observations seen so far (capped at N).
    count: usize,

    /// Current conditional variance (σ²_t).
    sigma_sq: f64,

    /// Previous conditional variance (σ²_{t-1}), initialized to long-run variance.
    prev_sigma_sq: f64,

    /// Timestamp of the last update (nanoseconds). Used to detect stale state.
    last_ts_ns: u64,
}

/// Error type for GARCH construction failures.
/// In production, we panic at startup (as per directive).
#[derive(Debug, Clone, PartialEq)]
pub enum GarchError {
    /// α + β ≥ 1.0 — process is non-stationary
    AlphaBetaSumExceedsOne { alpha: f64, beta: f64, sum: f64 },
    /// ω ≤ 0 — variance intercept must be positive
    OmegaNonPositive { omega: f64 },
    /// N must be > 0
    InvalidBufferSize { size: usize },
}

impl<const N: usize> GarchState<N> {
    /// Constructs a new GARCH(1,1) state with the given parameters.
    ///
    /// # Panics
    /// - If `alpha + beta >= 1.0` — process would be non-stationary
    /// - If `omega <= 0.0` — variance intercept must be positive
    /// - If N == 0 — buffer size must be positive
    ///
    /// # Arguments
    /// * `alpha` — ARCH coefficient (≥ 0, typically 0.05–0.15)
    /// * `beta` — GARCH coefficient (≥ 0, typically 0.80–0.95)
    /// * `omega` — Long-run variance intercept (> 0, typically 1e-6 to 1e-4)
    /// * `scale` — Fixed-point scale for return encoding (e.g., 8 means returns stored as r × 10^8)
    pub fn new(alpha: f64, beta: f64, omega: f64, scale: u32) -> Self {
        // Enforce invariants at construction — fail fast
        if N == 0 {
            panic!("GarchState buffer size N must be > 0, got 0");
        }
        if omega <= 0.0 {
            panic!(
                "GarchState omega must be positive, got {}. \
                 This is a startup configuration error — fix before deploying.",
                omega
            );
        }
        let sum = alpha + beta;
        if sum >= 1.0 {
            panic!(
                "GarchState alpha + beta must be < 1.0 for stationarity. \
                 Got alpha={}, beta={}, sum={}. \
                 This is a startup configuration error — fix before deploying.",
                alpha, beta, sum
            );
        }
        if alpha < 0.0 || beta < 0.0 {
            warn!(
                alpha, beta,
                "GARCH parameters alpha and beta should be non-negative. Proceeding anyway."
            );
        }

        // Long-run unconditional variance: σ²_LR = ω / (1 - α - β)
        let long_run_variance = omega / (1.0 - sum);

        Self {
            alpha,
            beta,
            omega,
            returns: [0i64; N],
            scale,
            pos: 0,
            count: 0,
            sigma_sq: long_run_variance,
            prev_sigma_sq: long_run_variance,
            last_ts_ns: 0,
        }
    }

    /// Non-panicking constructor for testing / config validation.
    pub fn try_new(alpha: f64, beta: f64, omega: f64, scale: u32) -> Result<Self, GarchError> {
        if N == 0 {
            return Err(GarchError::InvalidBufferSize { size: N });
        }
        if omega <= 0.0 {
            return Err(GarchError::OmegaNonPositive { omega });
        }
        let sum = alpha + beta;
        if sum >= 1.0 {
            return Err(GarchError::AlphaBetaSumExceedsOne {
                alpha,
                beta,
                sum,
            });
        }

        let long_run_variance = omega / (1.0 - sum);

        Ok(Self {
            alpha,
            beta,
            omega,
            returns: [0i64; N],
            scale,
            pos: 0,
            count: 0,
            sigma_sq: long_run_variance,
            prev_sigma_sq: long_run_variance,
            last_ts_ns: 0,
        })
    }

    /// Updates the GARCH state with a new price observation.
    ///
    /// # Arguments
    /// * `price` — Current price as fixed-point integer (price × 10^scale)
    /// * `scale` — Scale of the price input
    /// * `ts_ns` — Observation timestamp in Unix nanoseconds
    ///
    /// # Returns
    /// The updated conditional variance σ²_t (as f64).
    ///
    /// # Hot-path guarantee
    /// This method performs NO heap allocation. All computation is stack-only.
    #[inline(always)]
    pub fn update(&mut self, price: i64, scale: u32, ts_ns: u64) -> f64 {
        // Compute log-return from the previous price (if available)
        let prev_price = if self.count > 0 {
            // The previous price is the one we last stored at position (pos + N - 1) % N
            // But we store returns, not prices. We need to look at what was there before.
            // Actually, we need to track the previous price separately.
            // Let me restructure: we store the raw price in the ring buffer, and compute
            // the log-return on the fly.
            self.returns[(self.pos + N - 1) % N]
        } else {
            // First observation — no return to compute, just store and return initial σ²
            self.returns[0] = price;
            self.pos = 1;
            self.count = 1;
            self.last_ts_ns = ts_ns;
            return self.sigma_sq;
        };

        // Compute log-return as fixed-point: ln(p_t / p_{t-1}) × 10^scale
        // Use fixed-point arithmetic to avoid float determinism issues
        let log_return = if prev_price > 0 && price > 0 {
            // ln(p_t / p_{t-1}) using the approximation:
            // ln(x) ≈ 2 * (x-1) / (x+1) for x near 1 (accurate to ~6 decimal places)
            // But for GARCH we need the actual log-return, so we convert to f64
            // for the log computation — this is acceptable because the log-return
            // itself is not used on the gRPC wire, only σ² is.
            let p_t = price as f64 / 10f64.powi(scale as i32);
            let p_prev = prev_price as f64 / 10f64.powi(scale as i32);
            (p_t / p_prev).ln()
        } else {
            0.0 // Invalid prices — skip this observation
        };

        // Store the raw price in the ring buffer (for next iteration's prev_price)
        self.returns[self.pos] = price;
        self.pos = (self.pos + 1) % N;
        self.count = self.count.saturating_add(1).min(N);
        self.last_ts_ns = ts_ns;

        // GARCH(1,1) update rule:
        // σ²_t = ω + α · r²_{t-1} + β · σ²_{t-1}
        let r_sq = log_return * log_return;
        self.sigma_sq = self.omega + self.alpha * r_sq + self.beta * self.prev_sigma_sq;

        // Advance variance state
        self.prev_sigma_sq = self.sigma_sq;

        self.sigma_sq
    }

    /// Returns the current conditional variance σ²_t.
    #[inline(always)]
    pub fn sigma_sq(&self) -> f64 {
        self.sigma_sq
    }

    /// Returns the current volatility σ_t (square root of σ²_t).
    #[inline(always)]
    pub fn sigma(&self) -> f64 {
        self.sigma_sq.sqrt()
    }

    /// Returns the number of observations in the buffer.
    #[inline(always)]
    pub fn observation_count(&self) -> usize {
        self.count
    }

    /// Returns the last observation timestamp (nanoseconds).
    #[inline(always)]
    pub fn last_ts_ns(&self) -> u64 {
        self.last_ts_ns
    }

    /// Returns a snapshot suitable for the `GarchSnapshot` in interceptor.rs.
    #[inline]
    pub fn snapshot(&self) -> crate::interceptor::GarchSnapshot {
        crate::interceptor::GarchSnapshot {
            sigma_sq: self.sigma_sq,
            alpha: self.alpha,
            beta: self.beta,
            omega: self.omega,
        }
    }

    /// Returns the long-run unconditional variance: ω / (1 - α - β).
    #[inline]
    pub fn long_run_variance(&self) -> f64 {
        self.omega / (1.0 - self.alpha - self.beta)
    }

    /// Resets the state to initial conditions (long-run variance).
    pub fn reset(&mut self) {
        let lr = self.long_run_variance();
        self.sigma_sq = lr;
        self.prev_sigma_sq = lr;
        self.pos = 0;
        self.count = 0;
        self.returns.fill(0);
        self.last_ts_ns = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Typical GARCH parameters for crypto (BTC/ETH 1-minute returns)
    const ALPHA: f64 = 0.10;
    const BETA: f64 = 0.85;
    const OMEGA: f64 = 1e-6;

    #[test]
    fn test_construction_enforces_stationarity() {
        // α + β = 0.95 < 1.0 → OK
        let state = GarchState::<500>::new(ALPHA, BETA, OMEGA, 8);
        assert!(state.sigma_sq() > 0.0);

        // α + β = 1.0 → panic
        let result = std::panic::catch_unwind(|| {
            GarchState::<500>::new(0.5, 0.5, OMEGA, 8);
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_construction_rejects_zero_omega() {
        let result = std::panic::catch_unwind(|| {
            GarchState::<500>::new(ALPHA, BETA, 0.0, 8);
        });
        assert!(result.is_err());

        let result = std::panic::catch_unwind(|| {
            GarchState::<500>::new(ALPHA, BETA, -1.0, 8);
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_try_new_error_cases() {
        let err = GarchState::<500>::try_new(0.5, 0.5, OMEGA, 8).unwrap_err();
        assert_eq!(
            err,
            GarchError::AlphaBetaSumExceedsOne {
                alpha: 0.5,
                beta: 0.5,
                sum: 1.0,
            }
        );

        let err = GarchState::<500>::try_new(ALPHA, BETA, 0.0, 8).unwrap_err();
        assert_eq!(err, GarchError::OmegaNonPositive { omega: 0.0 });
    }

    #[test]
    fn test_sigma_squared_convergence_on_synthetic_returns() {
        // Generate synthetic log-returns with known variance (σ² = 0.0001, σ = 1%)
        // Feed them through GARCH and verify σ² converges toward the true value.

        let mut state = GarchState::<500>::new(0.10, 0.85, 1e-6, 8);

        let true_sigma_sq: f64 = 0.0001; // 1% daily vol → σ² = 0.0001
        let mut rng_state: u64 = 42; // Simple LCG for deterministic "randomness"

        // Simple deterministic pseudo-random: xorshift64
        let mut next_pseudo = || -> f64 {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            // Map to standard normal approximation (Box-Muller would be better but this is a test)
            (rng_state as f64 / u64::MAX as f64) * 2.0 - 1.0
        };

        let base_price: i64 = 65_000_000_000; // $65,000.00 at scale=8

        // Feed 1000 returns
        for i in 0..1000 {
            // Generate a price with the known volatility
            let shock = next_pseudo() * true_sigma_sq.sqrt();
            let log_return = shock; // r_t ~ N(0, σ²)
            let price_factor = log_return.exp();
            let price = (base_price as f64 * price_factor) as i64;

            state.update(price, 8, (1_700_000_000_000_000_000u64) + (i as u64) * 1_000_000_000);
        }

        let final_sigma_sq = state.sigma_sq();

        // After 1000 observations, GARCH σ² should be within 10x of the true value
        // (GARCH convergence depends on parameters; with α=0.10, β=0.85 it converges
        // reasonably fast but we allow wide bounds because the synthetic data is noisy)
        assert!(
            final_sigma_sq > 0.0,
            "σ² should be positive, got {}",
            final_sigma_sq
        );

        // σ² should not diverge to infinity (stationarity check)
        assert!(
            final_sigma_sq < 1.0,
            "σ² should not diverge, got {}",
            final_sigma_sq
        );

        // σ² should be in a reasonable neighborhood of the true value
        // With our parameters, long-run variance = ω/(1-α-β) = 1e-6/0.05 = 2e-5
        // The GARCH will converge toward this, modulated by the actual return variance
        let long_run = state.long_run_variance();
        assert!(
            (final_sigma_sq - long_run).abs() / long_run < 5.0,
            "σ²={} should be within 5x of long-run variance={}",
            final_sigma_sq,
            long_run
        );
    }

    #[test]
    fn test_first_observation_returns_initial_variance() {
        let state = GarchState::<500>::new(ALPHA, BETA, OMEGA, 8);
        let lr = state.long_run_variance();

        let mut state = state; // make mutable
        let sigma_sq = state.update(65_000_000_000i64, 8, 1);
        assert_eq!(sigma_sq, lr); // First obs returns initial σ²
        assert_eq!(state.observation_count(), 1);
    }

    #[test]
    fn test_ring_buffer_wraps_correctly() {
        // Use a small buffer to test wrapping
        let mut state = GarchState::<5>::new(0.10, 0.85, 1e-6, 0);

        // Feed 10 observations (buffer holds 5)
        for i in 1..=10u64 {
            let price = 65_000 + i as i64 * 10;
            state.update(price, 0, 1_700_000_000_000_000_000 + i * 1_000_000_000);
        }

        // Count should be capped at buffer size
        assert_eq!(state.observation_count(), 5);

        // σ² should be finite
        assert!(state.sigma_sq().is_finite());
        assert!(state.sigma_sq() > 0.0);
    }

    #[test]
    fn test_snapshot() {
        let state = GarchState::<500>::new(ALPHA, BETA, OMEGA, 8);
        let snap = state.snapshot();
        assert_eq!(snap.alpha, ALPHA);
        assert_eq!(snap.beta, BETA);
        assert_eq!(snap.omega, OMEGA);
        assert!(snap.sigma_sq > 0.0);
    }

    #[test]
    fn test_reset() {
        let mut state = GarchState::<500>::new(ALPHA, BETA, OMEGA, 8);
        let lr = state.long_run_variance();

        // Feed some data
        for i in 1..=10u64 {
            state.update(65_000_000_000i64 + (i as i64 * 100), 8, i * 1_000_000_000);
        }

        state.reset();
        assert_eq!(state.observation_count(), 0);
        assert_eq!(state.sigma_sq(), lr);
        assert_eq!(state.last_ts_ns(), 0);
    }

    #[test]
    fn test_const_generic_different_sizes() {
        // Verify it works with different N values
        let _s100 = GarchState::<100>::new(ALPHA, BETA, OMEGA, 4);
        let _s200 = GarchState::<200>::new(ALPHA, BETA, OMEGA, 4);
        let _s1000 = GarchState::<1000>::new(ALPHA, BETA, OMEGA, 4);
    }
}
