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

    /// Build a success response with simulated fill.
    async fn build_acceptance(
        &self,
        req: &TradeRequest,
        execution_id: &str,
    ) -> TradeResponse {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();

        // Echo the notional as filled (simulated — real fills come from exchange)
        let filled_notional = req.notional_value.as_ref().map(|fd| proto_types::generated::FixedDecimal {
            raw_units: fd.raw_units,
            scale: fd.scale,
        });

        // Simulated avg fill price
        let avg_fill = req.notional_value.as_ref().map(|fd| proto_types::generated::FixedDecimal {
            raw_units: fd.raw_units,
            scale: fd.scale,
        });

        TradeResponse {
            request_id: req.request_id.clone(),
            execution_id: execution_id.to_string(),
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
            average_fill_price: avg_fill,
            realized_slippage: Some(proto_types::generated::FixedDecimal {
                raw_units: 2,
                scale: 2,
            }),
            risk_snapshot: Some(self.build_risk_state().await),
            rejection_detail: String::new(),
            exchange_order_ids: vec![format!("SIM-{}", execution_id)],
            commission_paid: Some(proto_types::generated::FixedDecimal {
                raw_units: 10,
                scale: 2,
            }),
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
                    "Trade ACCEPTED — all gates passed"
                );

                // Record the position in portfolio (simulated)
                if let Some(asset) = &req.asset {
                    if let Some(notional) = &req.notional_value {
                        let leverage = req.leverage_ratio.as_ref()
                            .map(|l| FixedDecimal::new(l.raw_units, l.scale))
                            .unwrap_or(FixedDecimal::new(1, 0));

                        // Simulated entry price
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

                let execution_id = uuid::Uuid::new_v4().to_string();
                let response = self.build_acceptance(&req, &execution_id).await;

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
                Ok(Response::new(self.build_rejection(&req, rejection_code, &detail).await))
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
