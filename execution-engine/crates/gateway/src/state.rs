//! Engine state — shared state container for the gRPC service.
//!
//! Wraps the portfolio manager, per-instrument GARCH trackers, and the
//! ZeroMQ PUB publisher for RISK frame broadcasts.

use crate::portfolio::PortfolioStateManager;
use risk_engine::garch::GarchState;
use risk_engine::interceptor::GateConfig;
use dashmap::DashMap;
use tracing::info;

/// Shared engine state accessible from all gRPC handlers.
pub struct EngineState {
    /// Portfolio position tracker.
    pub portfolio: tokio::sync::RwLock<PortfolioStateManager>,
    /// Per-instrument GARCH ring buffers. Keyed by instrument code.
    pub garch_trackers: DashMap<String, GarchState<500>>,
    /// Gate configuration (immutable after construction).
    pub gate_config: GateConfig,
    /// ZeroMQ PUB publisher for BOOK and RISK frames (Contract 2).
    /// Wrapped in Option so the engine can start without ZMQ (e.g. tests).
    /// Set to None when ZMQ endpoint is not configured.
    pub publisher: Option<transport::MarketStatePublisher>,
}

impl EngineState {
    /// Create a new engine state with the given gate configuration and initial AUM.
    pub fn new(
        gate_config: GateConfig,
        initial_aum: risk_engine::decimal::FixedDecimal,
        publisher: Option<transport::MarketStatePublisher>,
    ) -> Self {
        info!(
            max_drawdown = gate_config.max_drawdown_pct.raw_units,
            max_var = gate_config.max_var_1d_99_pct.raw_units,
            max_garch_sigma_sq = gate_config.max_garch_sigma_sq,
            max_leverage = gate_config.max_leverage_ratio.raw_units,
            aum = initial_aum.raw_units,
            has_publisher = publisher.is_some(),
            "Engine state initialized"
        );
        Self {
            portfolio: tokio::sync::RwLock::new(PortfolioStateManager::new(initial_aum)),
            garch_trackers: DashMap::new(),
            gate_config,
            publisher,
        }
    }

    /// Get or create a GARCH tracker for the given instrument.
    /// Uses default parameters: α=0.10, β=0.85, ω=0.00001, scale=8.
    pub fn get_or_create_garch(&self, instrument: &str) {
        self.garch_trackers.entry(instrument.to_string())
            .or_insert_with(|| {
                // α=0.10, β=0.85, ω=0.00001 → α+β=0.95 < 1.0 ✓
                let garch = GarchState::new(0.10, 0.85, 0.00001, 8);
                info!(instrument = %instrument, "Created new GARCH tracker");
                garch
            });
    }

    /// Feed a price observation into the GARCH tracker for an instrument.
    /// Returns the latest σ².
    pub fn feed_price(&self, instrument: &str, price: i64, scale: u32, ts_ns: u64) -> f64 {
        self.get_or_create_garch(instrument);
        let mut garch = self.garch_trackers.get_mut(instrument).unwrap();
        garch.update(price, scale, ts_ns)
    }

    /// Get the latest σ² for an instrument (or default if no tracker exists).
    pub fn get_sigma_sq(&self, instrument: &str) -> f64 {
        if let Some(garch) = self.garch_trackers.get(instrument) {
            garch.sigma_sq()
        } else {
            // Default: long-run variance with our default params
            // ω / (1 - α - β) = 0.00001 / 0.05 = 0.0002
            0.0002
        }
    }
}
