//! Transport crate — ZeroMQ PUB publisher for Q-KUKEB market data and risk frames.
//!
//! Implements Contract 2 wire format:
//!   BOOK/{venue}/{instrument} — L2 order book top-25 levels
//!   RISK/{venue}/{instrument} — GARCH tick, portfolio snapshot, gate status
//!
//! Wire format: two frames per send — [topic_bytes, json_bytes].

pub mod frames;
pub mod publisher;

pub use frames::{BookFrame, PriceLevel, RiskFrame, GateStatus, book_topic, risk_topic};
pub use publisher::MarketStatePublisher;
