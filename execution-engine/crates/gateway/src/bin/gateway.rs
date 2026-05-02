//! Q-KUKEB Gateway binary — starts the gRPC server.
//!
//! Usage:
//!   qkukeb-gateway [OPTIONS]
//!
//! Options (env vars take precedence):
//!   --addr <HOST:PORT>       Bind address (default: 0.0.0.0:50051)
//!   --aum <AMOUNT>           Initial AUM in quote currency (default: 1000000)
//!   --max-drawdown <PCT>     Max drawdown as decimal (default: 0.05)
//!   --max-var <PCT>          Max 1d 99% VaR as decimal (default: 0.02)
//!   --max-garch-sigma-sq <V> Max GARCH σ² (default: 0.04)
//!   --max-leverage <N>       Max leverage ratio (default: 20)
//!   --zmq-pub <ENDPOINT>     ZeroMQ PUB endpoint (default: disabled)

use gateway::QKukebService;
use proto_types::generated::trade_command_service_server::TradeCommandServiceServer;
use risk_engine::decimal::FixedDecimal;
use risk_engine::interceptor::GateConfig;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,qkukeb=debug"))
        )
        .with_target(true)
        .with_thread_ids(true)
        .init();

    info!("Q-KUKEB Gateway starting...");

    // ── Configuration ──────────────────────────────────────────────────
    let addr = std::env::var("QKUKEB_BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".to_string());

    let aum: i64 = std::env::var("QKUKEB_AUM")
        .unwrap_or_else(|_| "1000000".to_string())
        .parse()?;

    let max_drawdown: f64 = std::env::var("QKUKEB_MAX_DRAWDOWN")
        .unwrap_or_else(|_| "0.05".to_string())
        .parse()?;

    let max_var: f64 = std::env::var("QKUKEB_MAX_VAR")
        .unwrap_or_else(|_| "0.02".to_string())
        .parse()?;

    let max_garch_sigma_sq: f64 = std::env::var("QKUKEB_MAX_GARCH_SIGMA_SQ")
        .unwrap_or_else(|_| "0.04".to_string())
        .parse()?;

    let max_leverage: i64 = std::env::var("QKUKEB_MAX_LEVERAGE")
        .unwrap_or_else(|_| "20".to_string())
        .parse()?;

    let zmq_endpoint = std::env::var("QKUKEB_ZMQ_PUB_ENDPOINT").ok();

    // ── ZeroMQ Publisher (optional) ────────────────────────────────────
    let publisher = match &zmq_endpoint {
        Some(endpoint) => {
            match transport::MarketStatePublisher::new(endpoint).await {
                Ok(pub_) => {
                    info!(endpoint = %endpoint, "ZeroMQ PUB publisher initialized");
                    Some(pub_)
                }
                Err(e) => {
                    warn!(error = %e, endpoint = %endpoint, "Failed to bind ZeroMQ PUB — continuing without publisher");
                    None
                }
            }
        }
        None => {
            info!("No ZeroMQ PUB endpoint configured — RISK frames will not be published");
            None
        }
    };

    // ── Gate Config ────────────────────────────────────────────────────
    let gate_config = GateConfig {
        max_drawdown_pct: FixedDecimal::from_f64(max_drawdown, 4),
        max_var_1d_99_pct: FixedDecimal::from_f64(max_var, 4),
        max_garch_sigma_sq,
        max_single_order_notional: FixedDecimal::new(1_000_000, 0),
        max_leverage_ratio: FixedDecimal::new(max_leverage, 0),
        max_concentration_ratio: FixedDecimal::new(250, 3), // 25%
    };

    // ── Engine State ───────────────────────────────────────────────────
    let state = std::sync::Arc::new(gateway::EngineState::new(
        gate_config,
        FixedDecimal::new(aum, 0),
        publisher,
    ));

    // ── gRPC Server ────────────────────────────────────────────────────
    let service = QKukebService::new(state);

    info!(
        addr = %addr,
        aum = aum,
        max_drawdown = max_drawdown,
        max_var = max_var,
        max_garch = max_garch_sigma_sq,
        max_leverage = max_leverage,
        zmq_endpoint = zmq_endpoint.as_deref().unwrap_or("disabled"),
        "Starting gRPC server"
    );

    tonic::transport::Server::builder()
        .add_service(TradeCommandServiceServer::new(service))
        .serve_with_shutdown(addr.parse()?, shutdown_signal())
        .await?;

    info!("Gateway shut down gracefully");
    Ok(())
}

/// Wait for Ctrl+C or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C"),
        _ = terminate => info!("Received SIGTERM"),
    }
}
