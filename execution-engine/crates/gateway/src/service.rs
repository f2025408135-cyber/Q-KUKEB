//! Q-KUKEB gRPC service implementation.
//!
//! Implements the `TradeCommandService` trait from the generated proto stubs.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use proto_types::generated::trade_command_service_server::TradeCommandService;
use proto_types::generated::*;
use risk_engine::decimal::FixedDecimal;
use risk_engine::interceptor::{DefaultRiskInterceptor, GarchSnapshot, RiskContext, RiskInterceptor};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use crate::state::EngineState;

/// The core gRPC service that handles trade commands and risk state streaming.
pub struct QKukebService {
    state: Arc<EngineState>,
    /// Channel for broadcasting risk state updates to subscribers.
    risk_tx: tokio::sync::broadcast::Sender<RiskState>,
}

impl QKukebService {
    /// Create a new QKukeb service with the given shared engine state.
    pub fn new(state: Arc<EngineState>) -> Self {
        let (risk_tx, _) = tokio::sync::broadcast::channel(64);
        Self { state, risk_tx }
    }

    /// Convert internal portfolio state to a protobuf RiskState.
    async fn build_risk_state(&self) -> RiskState {
        // Get latest GARCH σ² from any available instrument
        let garch_sigma_sq = self.state.garch_trackers.iter().next()
            .map(|entry| entry.value().sigma_sq())
            .unwrap_or(0.0002);

        let portfolio = self.state.portfolio.read().await;
        let snapshot = portfolio.snapshot(garch_sigma_sq);
        // Release lock before building response
        drop(portfolio);

        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        RiskState {
            portfolio_drawdown_pct: Some(proto_types::generated::FixedDecimal {
                raw_units: snapshot.drawdown_pct.raw_units,
                scale: snapshot.drawdown_pct.scale,
            }),
            var_1d_99_pct: Some(proto_types::generated::FixedDecimal {
                raw_units: snapshot.var_1d_99_pct.raw_units,
                scale: snapshot.var_1d_99_pct.scale,
            }),
            garch_sigma_sq: Some(proto_types::generated::FixedDecimal {
                raw_units: FixedDecimal::from_f64(garch_sigma_sq, 8).raw_units,
                scale: 8,
            }),
            gross_exposure: Some(proto_types::generated::FixedDecimal {
                raw_units: snapshot.gross_exposure.raw_units,
                scale: snapshot.gross_exposure.scale,
            }),
            net_exposure: Some(proto_types::generated::FixedDecimal {
                raw_units: snapshot.net_exposure.raw_units,
                scale: snapshot.net_exposure.scale,
            }),
            open_position_count: snapshot.open_position_count,
            evaluated_at_ns: now_ns,
        }
    }

    /// Build a rejection response.
    async fn build_rejection(
        &self,
        req: &TradeRequest,
        gate_code: RiskGateCode,
        detail: &str,
    ) -> TradeResponse {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();

        TradeResponse {
            request_id: req.request_id.clone(),
            execution_id: String::new(),
            received_at: req.signal_detected_at.as_ref().map(|ts| prost_types::Timestamp {
                seconds: ts.seconds,
                nanos: ts.nanos,
            }),
            responded_at: Some(prost_types::Timestamp {
                seconds: now.as_secs() as i64,
                nanos: now.subsec_nanos() as i32,
            }),
            risk_gate_code: gate_code as i32,
            filled_notional: None,
            average_fill_price: None,
            realized_slippage: None,
            risk_snapshot: Some(self.build_risk_state().await),
            rejection_detail: detail.to_string(),
            exchange_order_ids: vec![],
            commission_paid: None,
        }
    }

    /// Build a response using the deterministic fill model.
    /// Returns (TradeResponse, actual gate code used).
    /// If slippage exceeds the limit, returns a rejection response.
    async fn build_fill_response(
        &self,
        req: &TradeRequest,
        garch_sigma_sq: f64,
    ) -> (TradeResponse, i32) {
        use crate::fill::{FillResult, simulate_fill};

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let execution_id = uuid::Uuid::new_v4().to_string();

        let fill = simulate_fill(req, garch_sigma_sq);

        match fill {
            FillResult::Filled { avg_price, slippage_bps } => {
                let filled_notional = req.notional_value.as_ref().map(|fd| proto_types::generated::FixedDecimal {
                    raw_units: fd.raw_units,
                    scale: fd.scale,
                });

                let response = TradeResponse {
                    request_id: req.request_id.clone(),
                    execution_id: execution_id.clone(),
                    received_at: req.signal_detected_at.as_ref().map(|ts| prost_types::Timestamp {
                        seconds: ts.seconds,
                        nanos: ts.nanos,
                    }),
                    responded_at: Some(prost_types::Timestamp {
                        seconds: now.as_secs() as i64,
                        nanos: now.subsec_nanos() as i32,
                    }),
                    risk_gate_code: RiskGateCode::RiskGateAccepted as i32,
                    filled_notional,
                    average_fill_price: Some(proto_types::generated::FixedDecimal {
                        raw_units: avg_price.raw_units,
                        scale: avg_price.scale,
                    }),
                    realized_slippage: Some(proto_types::generated::FixedDecimal {
                        raw_units: slippage_bps.raw_units,
                        scale: slippage_bps.scale,
                    }),
                    risk_snapshot: Some(self.build_risk_state().await),
                    rejection_detail: String::new(),
                    exchange_order_ids: vec![format!("SIM-{}", execution_id)],
                    commission_paid: Some(proto_types::generated::FixedDecimal {
                        raw_units: 10,
                        scale: 2,
                    }),
                };

                (response, RiskGateCode::RiskGateAccepted as i32)
            }
            FillResult::SlippageExceeded { computed_slippage_bps, limit_bps } => {
                let detail = format!(
                    "Slippage exceeded: computed {:.2} bps > limit {} bps",
                    computed_slippage_bps, limit_bps
                );

                let response = TradeResponse {
                    request_id: req.request_id.clone(),
                    execution_id: String::new(),
                    received_at: req.signal_detected_at.as_ref().map(|ts| prost_types::Timestamp {
                        seconds: ts.seconds,
                        nanos: ts.nanos,
                    }),
                    responded_at: Some(prost_types::Timestamp {
                        seconds: now.as_secs() as i64,
                        nanos: now.subsec_nanos() as i32,
                    }),
                    risk_gate_code: RiskGateCode::RiskGateExecutionError as i32,
                    filled_notional: None,
                    average_fill_price: None,
                    realized_slippage: None,
                    risk_snapshot: Some(self.build_risk_state().await),
                    rejection_detail: detail,
                    exchange_order_ids: vec![],
                    commission_paid: None,
                };

                (response, RiskGateCode::RiskGateExecutionError as i32)
            }
        }
    }

    /// Publish a RISK frame via ZeroMQ after every TradeResponse.
    ///
    /// **Constraint:** Uses `.await.ok()` pattern — a ZMQ publish failure
    /// must NEVER block or fail a TradeResponse. Logs with `tracing::warn!` only.
    async fn publish_risk_frame(
        &self,
        venue: &str,
        instrument: &str,
        response: &TradeResponse,
        request_id: &str,
    ) {
        let publisher = match &self.state.publisher {
            Some(p) => p,
            None => return, // No publisher configured — skip silently
        };

        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        // Get portfolio snapshot for the frame
        let sigma_sq = self.state.get_sigma_sq(instrument);
        let portfolio = self.state.portfolio.read().await;
        let snapshot = portfolio.snapshot(sigma_sq);
        drop(portfolio);

        let gate_code = response.risk_gate_code;
        let all_gates_open = gate_code == RiskGateCode::RiskGateAccepted as i32;

        // Resolve the gate code name from the proto enum
        let gate_name = RiskGateCode::try_from(gate_code)
            .map(|g| g.as_str_name().to_string())
            .unwrap_or_else(|_| format!("UNKNOWN_{}", gate_code));

        let frame = transport::RiskFrame {
            venue: venue.to_string(),
            instrument: instrument.to_string(),
            garch_sigma_sq: sigma_sq.to_string(),
            garch_last_ts_ns: 0, // Would need per-tracker ts; leave 0 for now
            drawdown_raw: snapshot.drawdown_pct.raw_units,
            drawdown_scale: snapshot.drawdown_pct.scale,
            var_1d_99_raw: snapshot.var_1d_99_pct.raw_units,
            var_1d_99_scale: snapshot.var_1d_99_pct.scale,
            gross_exposure_raw: snapshot.gross_exposure.raw_units,
            gross_exposure_scale: snapshot.gross_exposure.scale,
            net_exposure_raw: snapshot.net_exposure.raw_units,
            net_exposure_scale: snapshot.net_exposure.scale,
            open_position_count: snapshot.open_position_count,
            all_gates_open,
            gates: vec![transport::GateStatus {
                gate_code,
                gate_name,
            }],
            ts_ns: now_ns,
            request_id: request_id.to_string(),
        };

        // Non-fatal publish — MUST NOT block or fail the gRPC response
        if let Err(e) = publisher.publish_risk(venue, instrument, &frame).await {
            warn!(
                error = %e,
                venue = %venue,
                instrument = %instrument,
                "ZeroMQ RISK frame publish failed (non-fatal)"
            );
        }
    }
}

#[tonic::async_trait]
impl TradeCommandService for QKukebService {
    /// Handle SubmitTradeCommand — evaluate risk gates and return response.
    async fn submit_trade_command(
        &self,
        request: Request<TradeRequest>,
    ) -> Result<Response<TradeResponse>, Status> {
        let req = request.into_inner();
        let request_id = req.request_id.clone();
        let venue = req.asset.as_ref()
            .map(|a| a.venue_id.clone())
            .unwrap_or_else(|| "UNKNOWN".to_string());
        let instrument = req.asset.as_ref()
            .map(|a| a.instrument.clone())
            .unwrap_or_default();

        info!(
            request_id = %request_id,
            instrument = %instrument,
            agent_id = %req.originating_agent_id,
            side = req.side,
            "Received trade command"
        );

        // Current time in nanoseconds
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        // Get GARCH σ² for the instrument
        let sigma_sq = self.state.get_sigma_sq(&instrument);

        // Build risk context from current portfolio + GARCH state
        let portfolio_snapshot = {
            let portfolio = self.state.portfolio.read().await;
            portfolio.snapshot(sigma_sq)
        };

        let risk_ctx = RiskContext {
            portfolio: portfolio_snapshot,
            garch: GarchSnapshot {
                sigma_sq,
                alpha: 0.10,
                beta: 0.85,
                omega: 0.00001,
            },
        };

        // Evaluate all gates via the interceptor
        let interceptor = DefaultRiskInterceptor::new(self.state.gate_config.clone());
        let gate_code = interceptor.evaluate(&req, &risk_ctx, now_ns);

        match gate_code {
            RiskGateCode::RiskGateAccepted => {
                info!(
                    request_id = %request_id,
                    instrument = %instrument,
                    "Trade ACCEPTED — all gates passed, running fill simulation"
                );

                // Run deterministic fill simulation
                let (response, actual_gate_code) = self.build_fill_response(&req, sigma_sq).await;

                // If fill was rejected due to slippage, still publish RISK frame
                if actual_gate_code != RiskGateCode::RiskGateAccepted as i32 {
                    warn!(
                        request_id = %request_id,
                        instrument = %instrument,
                        "Fill REJECTED — slippage exceeded after gates passed"
                    );
                    self.publish_risk_frame(&venue, &instrument, &response, &request_id).await;
                    return Ok(Response::new(response));
                }

                // Record the position in portfolio (simulated)
                if let Some(asset) = &req.asset {
                    if let Some(notional) = &req.notional_value {
                        let leverage = req.leverage_ratio.as_ref()
                            .map(|l| FixedDecimal::new(l.raw_units, l.scale))
                            .unwrap_or(FixedDecimal::new(1, 0));

                        let entry_price = FixedDecimal::new(1, 0);

                        let mut portfolio = self.state.portfolio.write().await;
                        portfolio.open_position(
                            &asset.instrument,
                            req.side,
                            FixedDecimal::new(notional.raw_units, notional.scale),
                            entry_price,
                            leverage,
                        );
                    }
                }

                // ── Directive 4B: Publish RISK frame via ZeroMQ (non-fatal) ──
                self.publish_risk_frame(&venue, &instrument, &response, &request_id).await;

                // Broadcast updated risk state
                let _ = self.risk_tx.send(self.build_risk_state().await);

                Ok(Response::new(response))
            }
            rejection_code => {
                let detail = format!("Rejected by gate: {}", rejection_code.as_str_name());
                warn!(
                    request_id = %request_id,
                    instrument = %instrument,
                    gate = rejection_code.as_str_name(),
                    "Trade REJECTED"
                );

                let response = self.build_rejection(&req, rejection_code, &detail).await;

                // ── Directive 4B: Publish RISK frame via ZeroMQ (non-fatal) ──
                self.publish_risk_frame(&venue, &instrument, &response, &request_id).await;

                Ok(Response::new(response))
            }
        }
    }

    /// Stream risk state updates to subscribed agents.
    type StreamRiskStateStream = Pin<Box<
        dyn Stream<Item = Result<RiskState, Status>> + Send,
    >>;

    async fn stream_risk_state(
        &self,
        request: Request<RiskSubscriptionRequest>,
    ) -> Result<Response<Self::StreamRiskStateStream>, Status> {
        let sub_req = request.into_inner();
        let agent_id = sub_req.agent_id;
        let throttle_ms = sub_req.throttle_interval_ms;

        info!(
            agent_id = %agent_id,
            throttle_ms = throttle_ms,
            "New risk state subscriber"
        );

        let mut rx = self.risk_tx.subscribe();

        // Spawn a periodic publisher so the stream has data even without trades
        let state_clone = Arc::clone(&self.state);
        let tx_clone = self.risk_tx.clone();
        let stream_instrument = self.state.garch_trackers.iter().next()
            .map(|e| e.key().clone())
            .unwrap_or_default();

        tokio::spawn(async move {
            let interval_ms = if throttle_ms > 0 { throttle_ms as u64 } else { 1000 };
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
            loop {
                interval.tick().await;

                let sigma_sq = state_clone.get_sigma_sq(&stream_instrument);
                let portfolio = state_clone.portfolio.read().await;
                let snapshot = portfolio.snapshot(sigma_sq);
                drop(portfolio);

                let now_ns = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);

                let risk_state = RiskState {
                    portfolio_drawdown_pct: Some(proto_types::generated::FixedDecimal {
                        raw_units: snapshot.drawdown_pct.raw_units,
                        scale: snapshot.drawdown_pct.scale,
                    }),
                    var_1d_99_pct: Some(proto_types::generated::FixedDecimal {
                        raw_units: snapshot.var_1d_99_pct.raw_units,
                        scale: snapshot.var_1d_99_pct.scale,
                    }),
                    garch_sigma_sq: Some(proto_types::generated::FixedDecimal {
                        raw_units: FixedDecimal::from_f64(sigma_sq, 8).raw_units,
                        scale: 8,
                    }),
                    gross_exposure: Some(proto_types::generated::FixedDecimal {
                        raw_units: snapshot.gross_exposure.raw_units,
                        scale: snapshot.gross_exposure.scale,
                    }),
                    net_exposure: Some(proto_types::generated::FixedDecimal {
                        raw_units: snapshot.net_exposure.raw_units,
                        scale: snapshot.net_exposure.scale,
                    }),
                    open_position_count: snapshot.open_position_count,
                    evaluated_at_ns: now_ns,
                };

                // If all subscribers dropped, stop publishing
                if tx_clone.send(risk_state).is_err() {
                    break;
                }
            }
        });

        // Convert broadcast receiver to a stream
        let stream = async_stream::stream! {
            loop {
                match rx.recv().await {
                    Ok(risk_state) => {
                        yield Ok(risk_state);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!(skipped = skipped, "Risk state subscriber lagged — skipping");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(stream)))
    }
}
