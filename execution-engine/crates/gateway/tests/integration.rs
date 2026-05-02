//! Integration smoke test for Sprint Block 4 acceptance criterion.
//!
//! Test plan:
//!   1. Spin up gateway with in-process state (no real ZeroMQ bind — publisher = None)
//!   2. Submit a valid TradeRequest with signal_ttl_ms = 5000
//!   3. Assert TradeResponse.risk_gate_code == RISK_GATE_ACCEPTED
//!   4. Submit the same request with signal_detected_at = now - 10s (stale)
//!   5. Assert TradeResponse.risk_gate_code == RISK_GATE_REJECTED_SIGNAL_STALE
//!
//! This is the acceptance criterion for Sprint Block 4.

use gateway::{EngineState, QKukebService};
use proto_types::generated::trade_command_service_server::TradeCommandService;
use proto_types::generated::{
    AssetIdentifier, FixedDecimal as ProtoFixedDecimal, RiskGateCode,
    SignalInvalidationThresholds, TradeRequest,
};
use risk_engine::decimal::FixedDecimal;
use risk_engine::interceptor::GateConfig;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tonic::Request;

/// Build the gate config used in integration tests.
fn test_gate_config() -> GateConfig {
    GateConfig {
        max_drawdown_pct: FixedDecimal::new(500, 4),     // 5.00%
        max_var_1d_99_pct: FixedDecimal::new(200, 4),    // 2.00%
        max_garch_sigma_sq: 0.04,                          // σ² = 0.04
        max_single_order_notional: FixedDecimal::new(1_000_000, 0),
        max_leverage_ratio: FixedDecimal::new(20, 0),
        max_concentration_ratio: FixedDecimal::new(250, 3), // 25%
    }
}

/// Create a valid TradeRequest with a fresh timestamp.
fn make_valid_trade_request() -> TradeRequest {
    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    TradeRequest {
        request_id: "integration-test-001".to_string(),
        originating_agent_id: "qkukeb-executor".to_string(),
        signal_detected_at: Some(prost_types::Timestamp {
            seconds: now_ts.as_secs() as i64,
            nanos: now_ts.subsec_nanos() as i32,
        }),
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
        slippage_bps_limit: 50, // 50 bps — plenty of room
        execution_ttl_ms: 5000,
        strategy_type: 1, // Funding rate arb
        strategy_version: "v1.0".to_string(),
        signal_embedding: vec![],
        signal_confidence: Some(ProtoFixedDecimal {
            raw_units: 85,
            scale: 2,
        }),
        invalidation: Some(SignalInvalidationThresholds {
            signal_ttl_ms: 5000,
            max_price_drift_bps: 100,
            min_funding_spread_bps_e2: 50,
            min_fill_probability: Some(ProtoFixedDecimal {
                raw_units: 80,
                scale: 2,
            }),
            min_available_liquidity: Some(ProtoFixedDecimal {
                raw_units: 100_000,
                scale: 0,
            }),
        }),
    }
}

/// Create a stale TradeRequest (signal_detected_at = 10 seconds ago).
fn make_stale_trade_request() -> TradeRequest {
    let stale_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        - Duration::from_secs(10);

    TradeRequest {
        request_id: "integration-test-002".to_string(),
        originating_agent_id: "qkukeb-executor".to_string(),
        signal_detected_at: Some(prost_types::Timestamp {
            seconds: stale_time.as_secs() as i64,
            nanos: stale_time.subsec_nanos() as i32,
        }),
        asset: Some(AssetIdentifier {
            base_asset: "ETH".to_string(),
            quote_asset: "USDT".to_string(),
            asset_class: 2, // PERP
            venue_id: "BINANCE_PERP".to_string(),
            instrument: "ETH-USDT-PERP".to_string(),
        }),
        side: 2, // Sell
        notional_value: Some(ProtoFixedDecimal {
            raw_units: 5_000,
            scale: 0,
        }),
        leverage_ratio: Some(ProtoFixedDecimal {
            raw_units: 3,
            scale: 0,
        }),
        execution_algo: 1,
        slippage_bps_limit: 50,
        execution_ttl_ms: 5000,
        strategy_type: 1,
        strategy_version: "v1.0".to_string(),
        signal_embedding: vec![],
        signal_confidence: Some(ProtoFixedDecimal {
            raw_units: 90,
            scale: 2,
        }),
        invalidation: Some(SignalInvalidationThresholds {
            signal_ttl_ms: 5000,
            max_price_drift_bps: 100,
            min_funding_spread_bps_e2: 50,
            min_fill_probability: Some(ProtoFixedDecimal {
                raw_units: 80,
                scale: 2,
            }),
            min_available_liquidity: Some(ProtoFixedDecimal {
                raw_units: 100_000,
                scale: 0,
            }),
        }),
    }
}

/// Spin up an EngineState with no publisher (for test isolation).
fn make_test_engine_state() -> Arc<EngineState> {
    Arc::new(EngineState::new(
        test_gate_config(),
        FixedDecimal::new(100_000, 0), // $100k AUM
        None, // No ZeroMQ publisher for integration tests
    ))
}

#[tokio::test]
async fn test_valid_trade_accepted() {
    // 1. Spin up gateway with in-process state (no real ZeroMQ)
    let state = make_test_engine_state();
    let service = QKukebService::new(state);

    // 2. Submit a valid TradeRequest with signal_ttl_ms = 5000
    let req = make_valid_trade_request();
    let response = service
        .submit_trade_command(Request::new(req))
        .await
        .expect("gRPC call should not fail");

    let resp = response.into_inner();

    // 3. Assert risk_gate_code == RISK_GATE_ACCEPTED
    assert_eq!(
        resp.risk_gate_code,
        RiskGateCode::RiskGateAccepted as i32,
        "Expected RISK_GATE_ACCEPTED (1), got {}",
        resp.risk_gate_code
    );

    // Verify additional invariants
    assert!(!resp.execution_id.is_empty(), "execution_id should be set");
    assert!(resp.filled_notional.is_some(), "filled_notional should be set on acceptance");
    assert!(resp.realized_slippage.is_some(), "realized_slippage should be set");
    assert!(
        resp.exchange_order_ids.iter().any(|id| id.starts_with("SIM-")),
        "Should have SIM- prefixed order ID"
    );
}

#[tokio::test]
async fn test_stale_signal_rejected() {
    // 1. Spin up gateway with in-process state
    let state = make_test_engine_state();
    let service = QKukebService::new(state);

    // 4. Submit request with signal_detected_at = now - 10s (stale)
    let req = make_stale_trade_request();
    let response = service
        .submit_trade_command(Request::new(req))
        .await
        .expect("gRPC call should not fail");

    let resp = response.into_inner();

    // 5. Assert risk_gate_code == RISK_GATE_REJECTED_SIGNAL_STALE
    assert_eq!(
        resp.risk_gate_code,
        RiskGateCode::RiskGateRejectedSignalStale as i32,
        "Expected RISK_GATE_REJECTED_SIGNAL_STALE (8), got {}",
        resp.risk_gate_code
    );

    // Verify rejection invariants
    assert!(resp.execution_id.is_empty(), "execution_id should be empty on rejection");
    assert!(resp.filled_notional.is_none(), "filled_notional should be None on rejection");
    assert!(
        !resp.rejection_detail.is_empty(),
        "rejection_detail should explain the rejection"
    );
    assert!(
        resp.rejection_detail.contains("SIGNAL_STALE"),
        "rejection_detail should mention SIGNAL_STALE"
    );
}

#[tokio::test]
async fn test_risk_snapshot_in_response() {
    let state = make_test_engine_state();
    let service = QKukebService::new(state);

    let req = make_valid_trade_request();
    let response = service
        .submit_trade_command(Request::new(req))
        .await
        .expect("gRPC call should not fail");

    let resp = response.into_inner();

    // Risk snapshot should always be present
    let snapshot = resp.risk_snapshot.expect("risk_snapshot should always be set");
    assert!(snapshot.evaluated_at_ns > 0, "evaluated_at_ns should be a valid timestamp");
    assert!(snapshot.gross_exposure.is_some(), "gross_exposure should be in snapshot");
    assert!(snapshot.net_exposure.is_some(), "net_exposure should be in snapshot");
}

#[tokio::test]
async fn test_deterministic_fill_low_volatility() {
    // With default σ² (0.0002), slippage = 0.5 + (0.0002 * 1000) = 0.7 bps
    // limit = 50 bps → should fill
    let state = make_test_engine_state();
    let service = QKukebService::new(state);

    let req = make_valid_trade_request();
    let response = service
        .submit_trade_command(Request::new(req))
        .await
        .expect("gRPC call should not fail");

    let resp = response.into_inner();
    assert_eq!(resp.risk_gate_code, RiskGateCode::RiskGateAccepted as i32);

    // Realized slippage should be ~0.7 bps (scale=4 → raw_units ≈ 7000)
    let slippage = resp.realized_slippage.unwrap();
    let slippage_f64 = slippage.raw_units as f64 / 10f64.powi(slippage.scale as i32);
    assert!(
        (slippage_f64 - 0.7).abs() < 0.01,
        "Expected ~0.7 bps slippage, got {}",
        slippage_f64
    );
}
