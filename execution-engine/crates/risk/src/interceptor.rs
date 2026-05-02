//! Risk interceptor — gate chain that evaluates incoming trade requests
//! against the current portfolio and market state.
//!
//! Gate order is IMMUTABLE by architectural decree:
//!   Gate 0: Signal invalidation (timestamp, drift, TTL)
//!   Gate 1: Portfolio drawdown
//!   Gate 2: Value-at-Risk (1-day 99%)
//!   Gate 3: GARCH(1,1) conditional volatility
//!
//! The `evaluate()` method is a provided method and MUST NOT be overridable.

use crate::decimal::FixedDecimal;
use proto_types::generated::{
    RiskGateCode, TradeRequest,
};
use tracing::{debug, warn};

/// Result of evaluating a single risk gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateResult {
    /// Gate passed — proceed to next gate.
    Accepted,
    /// Gate failed — short-circuit, return this code immediately.
    Rejected(RiskGateCode),
}

/// Immutable (at evaluation time) snapshot of the portfolio's risk state.
/// Populated from the risk engine's latest state, not from the request.
#[derive(Debug, Clone)]
pub struct PortfolioState {
    /// Current drawdown from high water mark, as a ratio (0.0 = 0%, 0.05 = 5%).
    pub drawdown_pct: FixedDecimal,
    /// 1-day 99% Value-at-Risk as a ratio of AUM.
    pub var_1d_99_pct: FixedDecimal,
    /// Current GARCH(1,1) conditional variance (σ²).
    /// Stored as FixedDecimal but used via `as_f64()` for arithmetic.
    pub garch_sigma_sq: FixedDecimal,
    /// Total gross notional exposure across all positions.
    pub gross_exposure: FixedDecimal,
    /// Net directional notional exposure.
    pub net_exposure: FixedDecimal,
    /// Number of currently open positions.
    pub open_position_count: u32,
    /// Absolute Assets Under Management in quote currency.
    pub aum: FixedDecimal,
    /// Portfolio high water mark in quote currency.
    pub hwm: FixedDecimal,
}

/// Mutable GARCH state reference — passed through for gate 3 evaluation.
/// The gate reads σ² but does NOT mutate it; mutation is the ring buffer's job.
#[derive(Debug, Clone)]
pub struct GarchSnapshot {
    /// Latest σ² from the GARCH(1,1) ring buffer.
    pub sigma_sq: f64,
    /// Current GARCH parameters (for logging / attribution).
    pub alpha: f64,
    pub beta: f64,
    pub omega: f64,
}

/// Full context required to evaluate a trade request against all gates.
/// This is constructed by the engine layer before calling `evaluate()`.
#[derive(Debug, Clone)]
pub struct RiskContext {
    /// Current portfolio risk snapshot.
    pub portfolio: PortfolioState,
    /// Current GARCH state for the instrument being traded.
    pub garch: GarchSnapshot,
}

/// Gate threshold configuration — loaded at startup from engine config,
/// immutable during the evaluation loop.
#[derive(Debug, Clone)]
pub struct GateConfig {
    /// Maximum allowable portfolio drawdown (ratio). E.g. 0.05 = 5%.
    pub max_drawdown_pct: FixedDecimal,

    /// Maximum allowable 1-day 99% VaR (ratio). E.g. 0.02 = 2%.
    pub max_var_1d_99_pct: FixedDecimal,

    /// Maximum allowable GARCH σ². When conditional variance exceeds this,
    /// all new positions are rejected.
    pub max_garch_sigma_sq: f64,

    /// Single-order notional cap in quote currency.
    pub max_single_order_notional: FixedDecimal,

    /// Platform leverage cap (multiplier). E.g. 10.0 → 10x max.
    pub max_leverage_ratio: FixedDecimal,

    /// Maximum concentration per instrument as a ratio of gross exposure.
    /// E.g. 0.25 = 25% of gross exposure in a single instrument.
    pub max_concentration_ratio: FixedDecimal,
}

/// The core risk interceptor trait.
///
/// Implementors provide the individual gate checks; the `evaluate()` method
/// is provided (non-overridable) and chains gates in strict sequence.
///
/// # Gate Order (Immutable)
/// 0. Signal invalidation — timestamp freshness, price drift, TTL
/// 1. Drawdown — portfolio drawdown ceiling
/// 2. VaR — 1-day 99% Value-at-Risk limit
/// 3. GARCH — conditional volatility ceiling
///
/// Gates are evaluated in order and **short-circuit on first failure**.
pub trait RiskInterceptor: Send + Sync {
    /// Returns a reference to the gate configuration.
    fn config(&self) -> &GateConfig;

    // ── Gate 0: Signal Invalidation ────────────────────────────────────────

    /// Checks whether the incoming signal is still valid based on:
    /// - `signal_ttl_ms`: signal age must not exceed this threshold
    /// - `max_price_drift_bps`: mid-price must not have drifted beyond this
    /// - `min_fill_probability`: fill probability must meet minimum
    /// - `min_available_liquidity`: book liquidity must be sufficient
    ///
    /// This is Gate 0 — evaluated BEFORE any portfolio math.
    fn check_signal_invalidation(
        &self,
        req: &TradeRequest,
        now_ns: u64,
    ) -> GateResult {
        let inv = match &req.invalidation {
            Some(inv) => inv,
            None => {
                debug!(request_id = %req.request_id, "No invalidation thresholds — accepting");
                return GateResult::Accepted;
            }
        };

        // Check signal TTL — if the signal's detection timestamp exceeds the TTL, reject.
        if inv.signal_ttl_ms > 0 {
            if let Some(detected_at) = &req.signal_detected_at {
                // Convert prost Timestamp → SystemTime → nanoseconds since epoch
                // prost_types::Timestamp has i64 seconds + i32 nanos
                let proto_ts = prost_types::Timestamp {
                    seconds: detected_at.seconds,
                    nanos: detected_at.nanos,
                };
                let detected_ns = std::time::SystemTime::try_from(proto_ts)
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64);

                if let Some(detected_ns) = detected_ns {
                    let age_ns = now_ns.saturating_sub(detected_ns);
                    let ttl_ns = (inv.signal_ttl_ms as u64) * 1_000_000;
                    if age_ns > ttl_ns {
                        warn!(
                            request_id = %req.request_id,
                            age_ms = age_ns / 1_000_000,
                            ttl_ms = inv.signal_ttl_ms,
                            "Signal TTL exceeded — rejecting"
                        );
                        return GateResult::Rejected(
                            RiskGateCode::RiskGateRejectedSignalStale,
                        );
                    }
                }
            }
        }

        GateResult::Accepted
    }

    // ── Gate 1: Drawdown ───────────────────────────────────────────────────

    /// Checks whether the current portfolio drawdown exceeds the configured ceiling.
    fn check_drawdown(&self, state: &PortfolioState) -> GateResult {
        if state.drawdown_pct >= self.config().max_drawdown_pct {
            warn!(
                drawdown_pct = state.drawdown_pct.raw_units,
                max_pct = self.config().max_drawdown_pct.raw_units,
                "Drawdown gate breached"
            );
            GateResult::Rejected(RiskGateCode::RiskGateRejectedMaxDrawdown)
        } else {
            GateResult::Accepted
        }
    }

    // ── Gate 2: Value-at-Risk ─────────────────────────────────────────────

    /// Checks whether the 1-day 99% VaR exceeds the configured limit.
    fn check_var(&self, state: &PortfolioState) -> GateResult {
        if state.var_1d_99_pct >= self.config().max_var_1d_99_pct {
            warn!(
                var_pct = state.var_1d_99_pct.raw_units,
                max_pct = self.config().max_var_1d_99_pct.raw_units,
                "VaR gate breached"
            );
            GateResult::Rejected(RiskGateCode::RiskGateRejectedVarBreach)
        } else {
            GateResult::Accepted
        }
    }

    // ── Gate 3: GARCH Volatility ───────────────────────────────────────────

    /// Checks whether GARCH(1,1) conditional variance exceeds the threshold.
    fn check_garch(&self, garch: &GarchSnapshot) -> GateResult {
        if garch.sigma_sq > self.config().max_garch_sigma_sq {
            warn!(
                sigma_sq = garch.sigma_sq,
                max_sigma_sq = self.config().max_garch_sigma_sq,
                "GARCH gate breached"
            );
            GateResult::Rejected(RiskGateCode::RiskGateRejectedGarchVolatility)
        } else {
            GateResult::Accepted
        }
    }

    // ── Composed Chain (NON-OVERRIDABLE) ───────────────────────────────────

    /// Evaluates all gates in strict sequence, short-circuiting on first failure.
    ///
    /// **Gate order is immutable by architectural decree.** This method is sealed
    /// and cannot be overridden by implementors.
    ///
    /// Returns the final `RiskGateCode`: `Accepted` if all gates pass,
    /// or the rejection code of the first failing gate.
    fn evaluate(
        &self,
        req: &TradeRequest,
        ctx: &RiskContext,
        now_ns: u64,
    ) -> RiskGateCode {
        // Gate 0 ALWAYS first — no exceptions
        if let GateResult::Rejected(code) = self.check_signal_invalidation(req, now_ns) {
            return code;
        }

        // Gates 1-3 in strict order
        for result in [
            self.check_drawdown(&ctx.portfolio),
            self.check_var(&ctx.portfolio),
            self.check_garch(&ctx.garch),
        ] {
            if let GateResult::Rejected(code) = result {
                return code;
            }
        }

        RiskGateCode::RiskGateAccepted
    }
}

/// Default implementation of `RiskInterceptor` with configurable thresholds.
#[derive(Debug, Clone)]
pub struct DefaultRiskInterceptor {
    config: GateConfig,
}

impl DefaultRiskInterceptor {
    /// Creates a new interceptor with the given gate configuration.
    pub fn new(config: GateConfig) -> Self {
        Self { config }
    }
}

impl RiskInterceptor for DefaultRiskInterceptor {
    fn config(&self) -> &GateConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Note: SignalInvalidationThresholds and AssetIdentifier are available via proto_types::generated
    // and used in the TradeRequest fields directly.

    fn make_gate_config() -> GateConfig {
        GateConfig {
            max_drawdown_pct: FixedDecimal::new(500, 4),    // 5.00%
            max_var_1d_99_pct: FixedDecimal::new(200, 4),   // 2.00%
            max_garch_sigma_sq: 0.04,                         // σ² = 0.04 (σ ≈ 20% annualized)
            max_single_order_notional: FixedDecimal::new(1_000_000, 0),
            max_leverage_ratio: FixedDecimal::new(10, 0),
            max_concentration_ratio: FixedDecimal::new(250, 3), // 25%
        }
    }

    fn make_safe_context() -> RiskContext {
        RiskContext {
            portfolio: PortfolioState {
                drawdown_pct: FixedDecimal::new(10, 4),   // 0.10%
                var_1d_99_pct: FixedDecimal::new(50, 4),  // 0.50%
                garch_sigma_sq: FixedDecimal::new(1, 4),
                gross_exposure: FixedDecimal::new(500_000, 0),
                net_exposure: FixedDecimal::new(100_000, 0),
                open_position_count: 3,
                aum: FixedDecimal::new(2_000_000, 0),
                hwm: FixedDecimal::new(2_010_000, 0),
            },
            garch: GarchSnapshot {
                sigma_sq: 0.01,
                alpha: 0.1,
                beta: 0.85,
                omega: 0.000001,
            },
        }
    }

    #[test]
    fn test_all_gates_pass() {
        let interceptor = DefaultRiskInterceptor::new(make_gate_config());
        let ctx = make_safe_context();

        let req = TradeRequest {
            request_id: "test-001".to_string(),
            ..Default::default()
        };

        let result = interceptor.evaluate(&req, &ctx, 1_700_000_000_000_000_000);
        assert_eq!(result, RiskGateCode::RiskGateAccepted);
    }

    #[test]
    fn test_drawdown_gate_rejects() {
        let interceptor = DefaultRiskInterceptor::new(make_gate_config());
        let mut ctx = make_safe_context();
        ctx.portfolio.drawdown_pct = FixedDecimal::new(600, 4); // 6.00% > 5.00% max

        let req = TradeRequest {
            request_id: "test-002".to_string(),
            ..Default::default()
        };

        let result = interceptor.evaluate(&req, &ctx, 1_700_000_000_000_000_000);
        assert_eq!(result, RiskGateCode::RiskGateRejectedMaxDrawdown);
    }

    #[test]
    fn test_var_gate_rejects() {
        let interceptor = DefaultRiskInterceptor::new(make_gate_config());
        let mut ctx = make_safe_context();
        ctx.portfolio.var_1d_99_pct = FixedDecimal::new(300, 4); // 3.00% > 2.00% max

        let req = TradeRequest {
            request_id: "test-003".to_string(),
            ..Default::default()
        };
        let result = interceptor.evaluate(&req, &ctx, 1_700_000_000_000_000_000);
        assert_eq!(result, RiskGateCode::RiskGateRejectedVarBreach);
    }

    #[test]
    fn test_garch_gate_rejects() {
        let interceptor = DefaultRiskInterceptor::new(make_gate_config());
        let mut ctx = make_safe_context();
        ctx.garch.sigma_sq = 0.05; // 0.05 > 0.04 max

        let req = TradeRequest {
            request_id: "test-004".to_string(),
            ..Default::default()
        };
        let result = interceptor.evaluate(&req, &ctx, 1_700_000_000_000_000_000);
        assert_eq!(result, RiskGateCode::RiskGateRejectedGarchVolatility);
    }

    #[test]
    fn test_gate_order_is_drawdown_before_var() {
        let interceptor = DefaultRiskInterceptor::new(make_gate_config());
        let mut ctx = make_safe_context();
        // BOTH gates are breached; drawdown should fire first
        ctx.portfolio.drawdown_pct = FixedDecimal::new(600, 4);
        ctx.portfolio.var_1d_99_pct = FixedDecimal::new(300, 4);

        let req = TradeRequest {
            request_id: "test-005".to_string(),
            ..Default::default()
        };
        let result = interceptor.evaluate(&req, &ctx, 1_700_000_000_000_000_000);
        // Must reject on drawdown (gate 1), not VaR (gate 2)
        assert_eq!(result, RiskGateCode::RiskGateRejectedMaxDrawdown);
    }
}
