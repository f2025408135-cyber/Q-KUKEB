//! Wire frame types for ZeroMQ PUB transport (Contract 2).
//!
//! Two topic patterns:
//!   BOOK/{venue}/{instrument}   — L2 order book top-25 levels
//!   RISK/{venue}/{instrument}   — GARCH tick, portfolio snapshot, gate status
//!
//! Wire format: two frames per send — [topic_bytes, json_bytes].
//! Payload is `serde_json::to_vec()` of the corresponding frame struct.

use serde::{Deserialize, Serialize};

// ─── BOOK frame ──────────────────────────────────────────────────────────────

/// A single price level in the L2 order book.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PriceLevel {
    /// Price in quote currency (fixed-point: raw_units / 10^scale).
    pub price_raw: i64,
    pub price_scale: u32,
    /// Quantity in base asset (fixed-point: raw_units / 10^scale).
    pub qty_raw: i64,
    pub qty_scale: u32,
    /// Number of orders at this level.
    pub order_count: u32,
}

/// L2 order book snapshot — top N levels from each side.
///
/// Published on topic `BOOK/{venue}/{instrument}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookFrame {
    /// Venue identifier (e.g. "BINANCE_PERP", "DYDX_V4").
    pub venue: String,
    /// Canonical instrument code (e.g. "BTC-USDT-PERP").
    pub instrument: String,
    /// Top bid levels (best price first).
    pub bids: Vec<PriceLevel>,
    /// Top ask levels (best price first).
    pub asks: Vec<PriceLevel>,
    /// Sequence number for gap detection.
    pub sequence: u64,
    /// Timestamp of the snapshot (nanoseconds since Unix epoch).
    pub ts_ns: u64,
}

// ─── RISK frame ──────────────────────────────────────────────────────────────

/// Risk gate status for an instrument evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateStatus {
    /// Protobuf RiskGateCode enum value (1=Accepted, 2..=10=rejection codes).
    pub gate_code: i32,
    /// Human-readable gate name (e.g. "RISK_GATE_ACCEPTED").
    pub gate_name: String,
}

/// Portfolio risk snapshot published on the ZMQ bus.
///
/// Published on topic `RISK/{venue}/{instrument}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskFrame {
    /// Venue identifier.
    pub venue: String,
    /// Canonical instrument code.
    pub instrument: String,
    /// GARCH(1,1) conditional variance (σ²) as f64 string for JSON precision.
    pub garch_sigma_sq: String,
    /// GARCH timestamp of last update.
    pub garch_last_ts_ns: u64,
    /// Portfolio drawdown percentage (fixed-point: raw_units / 10^scale).
    pub drawdown_raw: i64,
    pub drawdown_scale: u32,
    /// 1-day 99% VaR percentage (fixed-point: raw_units / 10^scale).
    pub var_1d_99_raw: i64,
    pub var_1d_99_scale: u32,
    /// Gross exposure in quote currency (fixed-point: raw_units / 10^scale).
    pub gross_exposure_raw: i64,
    pub gross_exposure_scale: u32,
    /// Net exposure in quote currency (fixed-point: raw_units / 10^scale).
    pub net_exposure_raw: i64,
    pub net_exposure_scale: u32,
    /// Number of open positions.
    pub open_position_count: u32,
    /// Advisory flag — all gates open. Rust re-evaluates atomically.
    pub all_gates_open: bool,
    /// Gate-by-gate status for the last evaluation.
    pub gates: Vec<GateStatus>,
    /// Timestamp of this risk frame (nanoseconds since Unix epoch).
    pub ts_ns: u64,
    /// Request ID that triggered this frame (empty if periodic publish).
    pub request_id: String,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build the ZMQ topic string for a BOOK frame.
pub fn book_topic(venue: &str, instrument: &str) -> Vec<u8> {
    format!("BOOK/{}/{}", venue, instrument).into_bytes()
}

/// Build the ZMQ topic string for a RISK frame.
pub fn risk_topic(venue: &str, instrument: &str) -> Vec<u8> {
    format!("RISK/{}/{}", venue, instrument).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_book_frame_serialization() {
        let frame = BookFrame {
            venue: "BINANCE_PERP".to_string(),
            instrument: "BTC-USDT-PERP".to_string(),
            bids: vec![
                PriceLevel {
                    price_raw: 65000_00,
                    price_scale: 2,
                    qty_raw: 1_500_000,
                    qty_scale: 8,
                    order_count: 42,
                },
            ],
            asks: vec![
                PriceLevel {
                    price_raw: 65001_00,
                    price_scale: 2,
                    qty_raw: 800_000,
                    qty_scale: 8,
                    order_count: 17,
                },
            ],
            sequence: 12345,
            ts_ns: 1_700_000_000_000_000_000,
        };

        let json = serde_json::to_vec(&frame).unwrap();
        let deserialized: BookFrame = serde_json::from_slice(&json).unwrap();
        assert_eq!(frame, deserialized);
    }

    #[test]
    fn test_risk_frame_serialization() {
        let frame = RiskFrame {
            venue: "BINANCE_PERP".to_string(),
            instrument: "BTC-USDT-PERP".to_string(),
            garch_sigma_sq: "0.0002".to_string(),
            garch_last_ts_ns: 1_700_000_000_000_000_000,
            drawdown_raw: 10,
            drawdown_scale: 4,
            var_1d_99_raw: 50,
            var_1d_99_scale: 4,
            gross_exposure_raw: 100_000,
            gross_exposure_scale: 0,
            net_exposure_raw: 50_000,
            net_exposure_scale: 0,
            open_position_count: 2,
            all_gates_open: true,
            gates: vec![GateStatus {
                gate_code: 1,
                gate_name: "RISK_GATE_ACCEPTED".to_string(),
            }],
            ts_ns: 1_700_000_000_000_000_000,
            request_id: "test-req-001".to_string(),
        };

        let json = serde_json::to_vec(&frame).unwrap();
        let deserialized: RiskFrame = serde_json::from_slice(&json).unwrap();
        assert_eq!(frame, deserialized);
    }

    #[test]
    fn test_topic_helpers() {
        assert_eq!(book_topic("BINANCE_PERP", "BTC-USDT-PERP"), b"BOOK/BINANCE_PERP/BTC-USDT-PERP");
        assert_eq!(risk_topic("DYDX_V4", "ETH-USDT-PERP"), b"RISK/DYDX_V4/ETH-USDT-PERP");
    }
}
