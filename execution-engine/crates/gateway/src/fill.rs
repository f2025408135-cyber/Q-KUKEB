//! Deterministic fill simulation model.
//!
//! Replaces the previous naive fill simulation with a reproducible model
//! that the Python swarm can reason about.
//!
//! Slippage model:
//!   computed_slippage_bps = base_slippage_bps + (garch_sigma_sq * vol_multiplier)
//!   vol_multiplier = FILL_VOL_MULTIPLIER env var (default: 1000.0)
//!
//! If computed_slippage_bps > req.slippage_bps_limit → FillResult::SlippageExceeded
//! Otherwise → FillResult::Filled { avg_price, slippage_bps }
//!
//! The same TradeRequest + GarchState always produces the same fill result.

use proto_types::generated::TradeRequest;
use risk_engine::decimal::FixedDecimal;

/// Base slippage in basis points. Represents minimum market impact.
const BASE_SLIPPAGE_BPS: f64 = 0.5;

/// Volatility multiplier for GARCH σ² contribution to slippage.
/// Configurable via env: FILL_VOL_MULTIPLIER.
fn vol_multiplier() -> f64 {
    std::env::var("FILL_VOL_MULTIPLIER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000.0)
}

/// Result of the deterministic fill simulation.
#[derive(Debug, Clone, PartialEq)]
pub enum FillResult {
    /// Fill was accepted within slippage tolerance.
    Filled {
        /// Average fill price in quote currency (fixed-point).
        avg_price: FixedDecimal,
        /// Realized slippage in basis points (fixed-point, scale=4).
        slippage_bps: FixedDecimal,
    },
    /// Slippage would exceed the request's limit — fill rejected.
    SlippageExceeded {
        /// The computed slippage in basis points that would have occurred.
        computed_slippage_bps: f64,
        /// The request's maximum allowed slippage in basis points.
        limit_bps: u32,
    },
}

/// Simulate a deterministic fill based on the trade request and GARCH state.
///
/// # Arguments
/// * `req` — The trade request containing notional, side, and slippage limit.
/// * `garch_sigma_sq` — Current GARCH(1,1) conditional variance for the instrument.
///
/// # Returns
/// A `FillResult` — either `Filled` with price and slippage, or `SlippageExceeded`.
///
/// # Determinism guarantee
/// Given the same `TradeRequest` and `garch_sigma_sq`, this function always returns
/// the same `FillResult`. No randomness, no external state.
pub fn simulate_fill(req: &TradeRequest, garch_sigma_sq: f64) -> FillResult {
    let multiplier = vol_multiplier();

    // Slippage model: base + volatility component
    let computed_slippage_bps = BASE_SLIPPAGE_BPS + (garch_sigma_sq * multiplier);

    // Get the request's slippage limit (basis points)
    let limit_bps = req.slippage_bps_limit;

    if computed_slippage_bps > limit_bps as f64 {
        return FillResult::SlippageExceeded {
            computed_slippage_bps,
            limit_bps,
        };
    }

    // Compute average fill price from notional
    // For simulation: if notional = 10000 (scale=0) and we apply slippage,
    // the fill price reflects the slippage cost.
    // This is a simplified model — real fills would come from exchange execution.
    let slippage_bps_fd = FixedDecimal::from_f64(computed_slippage_bps, 4);

    // Use the notional as the avg fill price placeholder (simulated)
    let avg_price = req.notional_value.as_ref()
        .map(|nv| FixedDecimal::new(nv.raw_units, nv.scale))
        .unwrap_or(FixedDecimal::ZERO);

    FillResult::Filled {
        avg_price,
        slippage_bps: slippage_bps_fd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto_types::generated::{AssetIdentifier, FixedDecimal as ProtoFixedDecimal};

    fn make_trade_request(slippage_bps_limit: u32) -> TradeRequest {
        TradeRequest {
            request_id: "fill-test-001".to_string(),
            originating_agent_id: "test-agent".to_string(),
            signal_detected_at: None,
            asset: Some(AssetIdentifier {
                base_asset: "BTC".to_string(),
                quote_asset: "USDT".to_string(),
                asset_class: 2, // PERP
                venue_id: "BINANCE_PERP".to_string(),
                instrument: "BTC-USDT-PERP".to_string(),
            }),
            side: 1, // Buy
            notional_value: Some(ProtoFixedDecimal {
                raw_units: 10_000,
                scale: 0,
            }),
            leverage_ratio: Some(ProtoFixedDecimal {
                raw_units: 5,
                scale: 0,
            }),
            execution_algo: 1, // TWAP
            slippage_bps_limit,
            execution_ttl_ms: 5000,
            strategy_type: 1,
            strategy_version: "v1.0".to_string(),
            signal_embedding: vec![],
            signal_confidence: Some(ProtoFixedDecimal {
                raw_units: 85,
                scale: 2,
            }),
            invalidation: None,
        }
    }

    #[test]
    fn test_fill_accepted_low_volatility() {
        // σ² = 0.0002 → slippage = 0.5 + (0.0002 * 1000) = 0.7 bps
        // limit = 5 bps → accepted
        let req = make_trade_request(5);
        let result = simulate_fill(&req, 0.0002);

        match result {
            FillResult::Filled { slippage_bps, .. } => {
                // 0.7 bps at scale=4 → raw_units ≈ 7000
                let slippage_f64 = slippage_bps.as_f64();
                assert!(
                    (slippage_f64 - 0.7).abs() < 0.01,
                    "Expected ~0.7 bps, got {}",
                    slippage_f64
                );
            }
            FillResult::SlippageExceeded { computed_slippage_bps, .. } => {
                panic!("Expected fill accepted, got slippage exceeded: {} bps", computed_slippage_bps);
            }
        }
    }

    #[test]
    fn test_slippage_exceeded_high_volatility() {
        // σ² = 0.05 → slippage = 0.5 + (0.05 * 1000) = 50.5 bps
        // limit = 5 bps → rejected
        let req = make_trade_request(5);
        let result = simulate_fill(&req, 0.05);

        match result {
            FillResult::SlippageExceeded { computed_slippage_bps, limit_bps } => {
                assert!(
                    (computed_slippage_bps - 50.5).abs() < 0.01,
                    "Expected ~50.5 bps, got {}",
                    computed_slippage_bps
                );
                assert_eq!(limit_bps, 5);
            }
            FillResult::Filled { .. } => {
                panic!("Expected slippage exceeded, got fill accepted");
            }
        }
    }

    #[test]
    fn test_determinism_same_inputs_same_output() {
        // Same request + same σ² → must produce identical results
        let req = make_trade_request(10);
        let r1 = simulate_fill(&req, 0.001);
        let r2 = simulate_fill(&req, 0.001);
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_vol_multiplier_env_override() {
        std::env::set_var("FILL_VOL_MULTIPLIER", "2000.0");
        let req = make_trade_request(100);

        // σ² = 0.001 → slippage = 0.5 + (0.001 * 2000) = 2.5 bps
        let result = simulate_fill(&req, 0.001);

        match result {
            FillResult::Filled { slippage_bps, .. } => {
                let slippage_f64 = slippage_bps.as_f64();
                assert!(
                    (slippage_f64 - 2.5).abs() < 0.01,
                    "Expected ~2.5 bps with 2000x multiplier, got {}",
                    slippage_f64
                );
            }
            FillResult::SlippageExceeded { computed_slippage_bps, .. } => {
                panic!("Expected fill, got slippage exceeded: {} bps", computed_slippage_bps);
            }
        }

        std::env::remove_var("FILL_VOL_MULTIPLIER");
    }
}
