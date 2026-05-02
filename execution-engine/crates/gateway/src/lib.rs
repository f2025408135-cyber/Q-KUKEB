//! Q-KUKEB Gateway — gRPC server that receives TradeRequest from the Python swarm,
//! evaluates it through the risk interceptor gate chain, and returns TradeResponse.
//!
//! Architecture:
//!   Python swarm → gRPC → [Gateway] → RiskInterceptor.evaluate() → TradeResponse → Python
//!                                   ↕
//!                            PortfolioStateManager
//!                            GarchTracker (per-instrument)
//!                            ZeroMQ PUB (RISK frames)

mod fill;
mod portfolio;
mod service;
mod state;

pub use fill::{FillResult, simulate_fill};
pub use portfolio::PortfolioStateManager;
pub use service::QKukebService;
pub use state::EngineState;
