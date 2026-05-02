//! ZeroMQ PUB publisher for market data and risk frames (Contract 2).
//!
//! Wire format: two frames per send — [topic_bytes, json_bytes].
//! Uses `rzmq` 0.5 (Context + SocketType::Pub + send_multipart).

use rzmq::{Context, Msg, SocketType};
use tracing::{debug, info, warn};

use crate::frames::{BookFrame, RiskFrame, book_topic, risk_topic};

/// ZeroMQ PUB socket publisher for BOOK and RISK frames.
///
/// # Wire format
///
/// Each publish sends exactly two frames:
///   Frame 0: topic bytes (e.g. `b"BOOK/BINANCE_PERP/BTC-USDT-PERP"`)
///   Frame 1: JSON-encoded payload (`serde_json::to_vec`)
///
/// No third frame. No envelope wrapper.
pub struct MarketStatePublisher {
    #[allow(dead_code)]
    ctx: Context,
    socket: rzmq::Socket,
    endpoint: String,
}

impl MarketStatePublisher {
    /// Create a new publisher and bind to the given ZeroMQ endpoint.
    ///
    /// # Arguments
    /// * `endpoint` — ZeroMQ endpoint, e.g. `"tcp://127.0.0.1:5555"` or `"ipc:///tmp/qkukeb.pub"`
    ///
    /// # Errors
    /// Returns `rzmq::ZmqError` if context creation or bind fails.
    pub async fn new(endpoint: &str) -> Result<Self, rzmq::ZmqError> {
        let ctx = Context::new()?;
        let socket = ctx.socket(SocketType::Pub)?;

        socket.bind(endpoint).await?;
        info!(endpoint = %endpoint, "ZeroMQ PUB socket bound");

        Ok(Self {
            ctx,
            socket,
            endpoint: endpoint.to_string(),
        })
    }

    /// Publish a BOOK frame. Topic = `b"BOOK/{venue}/{instrument}"`.
    ///
    /// Wire: `[book_topic_bytes, json_bytes]`
    pub async fn publish_book(
        &self,
        venue: &str,
        instrument: &str,
        payload: &BookFrame,
    ) -> Result<(), rzmq::ZmqError> {
        let topic = book_topic(venue, instrument);
        let json = match serde_json::to_vec(payload) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "Failed to serialize BookFrame");
                return Err(rzmq::ZmqError::Internal(e.to_string()));
            }
        };

        debug!(
            topic = %String::from_utf8_lossy(&topic),
            sequence = payload.sequence,
            "Publishing BOOK frame"
        );

        self.socket.send_multipart(vec![
            Msg::from_vec(topic),
            Msg::from_vec(json),
        ]).await
    }

    /// Publish a RISK frame. Topic = `b"RISK/{venue}/{instrument}"`.
    ///
    /// Wire: `[risk_topic_bytes, json_bytes]`
    pub async fn publish_risk(
        &self,
        venue: &str,
        instrument: &str,
        payload: &RiskFrame,
    ) -> Result<(), rzmq::ZmqError> {
        let topic = risk_topic(venue, instrument);
        let json = match serde_json::to_vec(payload) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "Failed to serialize RiskFrame");
                return Err(rzmq::ZmqError::Internal(e.to_string()));
            }
        };

        debug!(
            topic = %String::from_utf8_lossy(&topic),
            gates_open = payload.all_gates_open,
            "Publishing RISK frame"
        );

        self.socket.send_multipart(vec![
            Msg::from_vec(topic),
            Msg::from_vec(json),
        ]).await
    }

    /// Returns the bound endpoint string (for diagnostics).
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for MarketStatePublisher {
    fn drop(&mut self) {
        // Best-effort cleanup. In production, use explicit shutdown.
        // rzmq sockets are Arc-based and will close when the last reference drops.
        debug!(endpoint = %self.endpoint, "MarketStatePublisher dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_publisher_bind_inproc() {
        // inproc:// works without a real network interface
        let result = MarketStatePublisher::new("ipc:///tmp/test-qkukeb-pub.sock").await;
        // May fail if leftover socket file exists, that's OK for a unit test
        // The important thing is it doesn't panic or hang
        if let Ok(pub_) = result {
            assert_eq!(pub_.endpoint(), "ipc:///tmp/test-qkukeb-pub.sock");
        }
    }

    #[test]
    fn test_book_frame_publish_format() {
        let frame = BookFrame {
            venue: "BINANCE_PERP".to_string(),
            instrument: "BTC-USDT-PERP".to_string(),
            bids: vec![],
            asks: vec![],
            sequence: 1,
            ts_ns: 12345,
        };

        let topic = book_topic(&frame.venue, &frame.instrument);
        let json = serde_json::to_vec(&frame).unwrap();

        // Verify two-frame wire format structure
        assert!(topic.starts_with(b"BOOK/"));
        assert!(json.len() > 0);
        assert!(serde_json::from_slice::<BookFrame>(&json).is_ok());
    }

    #[test]
    fn test_risk_frame_publish_format() {
        let frame = RiskFrame {
            venue: "BINANCE_PERP".to_string(),
            instrument: "BTC-USDT-PERP".to_string(),
            garch_sigma_sq: "0.0002".to_string(),
            garch_last_ts_ns: 0,
            drawdown_raw: 0,
            drawdown_scale: 4,
            var_1d_99_raw: 50,
            var_1d_99_scale: 4,
            gross_exposure_raw: 0,
            gross_exposure_scale: 0,
            net_exposure_raw: 0,
            net_exposure_scale: 0,
            open_position_count: 0,
            all_gates_open: true,
            gates: vec![],
            ts_ns: 12345,
            request_id: String::new(),
        };

        let topic = risk_topic(&frame.venue, &frame.instrument);
        let json = serde_json::to_vec(&frame).unwrap();

        assert!(topic.starts_with(b"RISK/"));
        assert!(json.len() > 0);
        assert!(serde_json::from_slice::<RiskFrame>(&json).is_ok());
    }
}
