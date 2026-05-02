//! Portfolio state manager — tracks open positions, P&L, drawdown, exposure.
//!
//! This is a simple in-memory tracker. In production, this would be backed by
//! a persistent store and reconciled against exchange fills.

use risk_engine::decimal::FixedDecimal;
use risk_engine::interceptor::PortfolioState;
use std::collections::HashMap;
use tracing::info;

/// A single tracked position.
#[derive(Debug, Clone)]
pub struct Position {
    #[allow(dead_code)]
    pub instrument: String,
    pub side: i32,           // proto OrderSide enum (1=Buy, 2=Sell)
    pub notional: FixedDecimal,
    #[allow(dead_code)]
    pub entry_price: FixedDecimal,
    #[allow(dead_code)]
    pub leverage: FixedDecimal,
    pub unrealized_pnl: FixedDecimal,
}

/// Manages the full portfolio state for risk gate evaluation.
pub struct PortfolioStateManager {
    /// All open positions keyed by instrument.
    positions: HashMap<String, Position>,
    /// Absolute AUM in quote currency.
    aum: FixedDecimal,
    /// Portfolio high water mark.
    hwm: FixedDecimal,
}

impl PortfolioStateManager {
    pub fn new(initial_aum: FixedDecimal) -> Self {
        let hwm = initial_aum;
        Self {
            positions: HashMap::new(),
            aum: initial_aum,
            hwm,
        }
    }

    /// Open a new position or add to an existing one.
    pub fn open_position(
        &mut self,
        instrument: &str,
        side: i32,
        notional: FixedDecimal,
        entry_price: FixedDecimal,
        leverage: FixedDecimal,
    ) {
        let pos = self.positions.entry(instrument.to_string()).or_insert(Position {
            instrument: instrument.to_string(),
            side,
            notional: FixedDecimal::ZERO,
            entry_price,
            leverage,
            unrealized_pnl: FixedDecimal::ZERO,
        });
        pos.notional = pos.notional + notional;
        info!(
            instrument = %instrument,
            side = side,
            notional = notional.raw_units,
            "Position opened/added"
        );
    }

    /// Close (reduce) a position. Returns the remaining notional after reduction.
    pub fn close_position(&mut self, instrument: &str, reduce_notional: FixedDecimal) -> Option<FixedDecimal> {
        if let Some(pos) = self.positions.get_mut(instrument) {
            let (reduce_raw, pos_raw, scale) =
                FixedDecimal::normalize_pair(&reduce_notional, &pos.notional);
            if reduce_raw >= pos_raw {
                self.positions.remove(instrument);
                info!(instrument = %instrument, "Position fully closed");
                None
            } else {
                pos.notional = FixedDecimal::new(pos_raw - reduce_raw, scale);
                info!(
                    instrument = %instrument,
                    remaining = pos.notional.raw_units,
                    "Position reduced"
                );
                Some(pos.notional)
            }
        } else {
            None
        }
    }

    /// Build a `PortfolioState` snapshot for the risk interceptor.
    pub fn snapshot(&self, garch_sigma_sq: f64) -> PortfolioState {
        // Calculate gross and net exposure
        let mut gross = FixedDecimal::ZERO;
        let mut net = FixedDecimal::ZERO;
        for pos in self.positions.values() {
            gross = gross + pos.notional;
            match pos.side {
                1 => net = net + pos.notional,   // Buy = long
                2 => net = net - pos.notional,   // Sell = short
                _ => {}
            }
        }

        // Total unrealized P&L across all positions
        let total_pnl: FixedDecimal = self.positions.values()
            .map(|p| p.unrealized_pnl)
            .fold(FixedDecimal::ZERO, |acc, pnl| acc + pnl);

        let current_equity = self.aum + total_pnl;

        // Drawdown from high water mark
        let drawdown_pct = if self.hwm.raw_units > 0 && current_equity < self.hwm {
            let (eq_raw, hwm_raw, _) = FixedDecimal::normalize_pair(&current_equity, &self.hwm);
            let drawdown_raw = hwm_raw - eq_raw;
            // drawdown_pct = drawdown / hwm, scale 6 (parts per million)
            if hwm_raw > 0 {
                FixedDecimal::new(drawdown_raw * 1_000_000 / hwm_raw, 6)
            } else {
                FixedDecimal::ZERO
            }
        } else {
            FixedDecimal::ZERO
        };

        // Placeholder VaR — in production, computed from return covariance matrix
        let var_1d_99_pct = FixedDecimal::new(50, 4); // 0.50%

        // Store garch_sigma_sq as FixedDecimal (scale=8)
        let garch_fd = FixedDecimal::from_f64(garch_sigma_sq, 8);

        PortfolioState {
            drawdown_pct,
            var_1d_99_pct,
            garch_sigma_sq: garch_fd,
            gross_exposure: gross,
            net_exposure: net,
            open_position_count: self.positions.len() as u32,
            aum: self.aum,
            hwm: self.hwm,
        }
    }

    pub fn position_count(&self) -> usize {
        self.positions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_and_snapshot() {
        let mut mgr = PortfolioStateManager::new(FixedDecimal::new(100_000, 0));

        mgr.open_position(
            "BTC-USDT-PERP",
            1, // Buy
            FixedDecimal::new(10_000, 0),
            FixedDecimal::new(50000, 2), // $500.00
            FixedDecimal::new(5, 0),     // 5x
        );

        let snapshot = mgr.snapshot(0.01);
        assert_eq!(snapshot.open_position_count, 1);
        assert_eq!(snapshot.gross_exposure, FixedDecimal::new(10_000, 0));
    }

    #[test]
    fn test_close_position() {
        let mut mgr = PortfolioStateManager::new(FixedDecimal::new(100_000, 0));

        mgr.open_position(
            "ETH-USDT-SPOT",
            1,
            FixedDecimal::new(5_000, 0),
            FixedDecimal::new(30000, 2),
            FixedDecimal::new(1, 0),
        );

        let remaining = mgr.close_position("ETH-USDT-SPOT", FixedDecimal::new(3_000, 0));
        assert!(remaining.is_some());
        assert_eq!(remaining.unwrap(), FixedDecimal::new(2_000, 0));

        let snapshot = mgr.snapshot(0.01);
        assert_eq!(snapshot.gross_exposure, FixedDecimal::new(2_000, 0));
    }

    #[test]
    fn test_net_exposure_with_short() {
        let mut mgr = PortfolioStateManager::new(FixedDecimal::new(100_000, 0));

        mgr.open_position("BTC-USDT-PERP", 1, FixedDecimal::new(30_000, 0), FixedDecimal::new(50000, 2), FixedDecimal::new(5, 0));
        mgr.open_position("ETH-USDT-PERP", 2, FixedDecimal::new(20_000, 0), FixedDecimal::new(30000, 2), FixedDecimal::new(3, 0));

        let snapshot = mgr.snapshot(0.01);
        assert_eq!(snapshot.gross_exposure, FixedDecimal::new(50_000, 0));
        assert_eq!(snapshot.net_exposure, FixedDecimal::new(10_000, 0));
    }
}
