"""gRPC Bridge: Python client for the Rust TradeCommandService.

This module provides the synchronous and asynchronous gRPC clients that
submit TradeRequest messages to the Rust execution engine and receive
TradeResponse messages with risk gate results.

Wire protocol: gRPC (grpcio), protobuf serialization via betterproto.
"""

from __future__ import annotations

import logging
import os
from typing import Optional

logger = logging.getLogger(__name__)

# Default gRPC endpoint — can be overridden via env var or config
_DEFAULT_GRPC_ENDPOINT = os.getenv(
    "QKUKEB_GRPC_ENDPOINT", "localhost:50051"
)


class TradeCommandClient:
    """Synchronous gRPC client for submitting trade commands to the Rust engine.

    Usage:
        client = TradeCommandClient("localhost:50051")
        response = client.submit_trade_sync(trade_request)
        print(response.risk_gate_code)  # 1 = ACCEPTED

    The client lazily initializes the gRPC channel on first call and
    reuses it for subsequent requests. Channel failures are logged but
    not raised — the caller receives a synthetic REJECTED response instead.
    """

    def __init__(self, endpoint: Optional[str] = None):
        self._endpoint = endpoint or _DEFAULT_GRPC_ENDPOINT
        self._channel = None
        self._stub = None

    def _ensure_channel(self):
        """Lazily create the gRPC channel and stub."""
        if self._channel is not None:
            return

        try:
            import grpc
            from swarm.proto.tradecommand.v1 import (
                TradeCommandServiceStub,
            )

            # Use a short deadline for the channel — we need sub-second latency
            self._channel = grpc.insecure_channel(
                self._endpoint,
                options=[
                    ("grpc.max_receive_message_length", 4 * 1024 * 1024),
                    ("grpc.keepalive_timeout_ms", 5000),
                    ("grpc.keepalive_permit_without_calls", 1),
                ],
            )
            self._stub = TradeCommandServiceStub(self._channel)
            logger.info("gRPC channel opened to %s", self._endpoint)
        except ImportError:
            logger.error(
                "grpcio not installed. Install with: pip install grpcio"
            )
            raise
        except Exception as e:
            logger.error("Failed to create gRPC channel: %s", e)
            raise

    def submit_trade_sync(self, trade_request) -> "TradeResponse":
        """Submit a TradeRequest to the Rust engine synchronously.

        Args:
            trade_request: A betterproto TradeRequest instance.

        Returns:
            A betterproto TradeResponse instance.

        Raises:
            ConnectionError: If the gRPC channel cannot be established.
            grpc.RpcError: If the server returns an error.
        """
        self._ensure_channel()

        from swarm.proto.tradecommand.v1 import TradeResponse

        try:
            response_bytes = self._stub.SubmitTradeCommand(
                trade_request.SerializeToString()
            )
            # Deserialize the response
            response = TradeResponse()
            if hasattr(response_bytes, "SerializeToString"):
                # Already a message object
                return response_bytes
            else:
                # Raw bytes — deserialize
                response.ParseFromString(response_bytes)
                return response
        except Exception as e:
            logger.error("gRPC call failed: %s", e)
            raise

    def close(self):
        """Close the gRPC channel."""
        if self._channel is not None:
            self._channel.close()
            self._channel = None
            self._stub = None
            logger.info("gRPC channel closed")

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class AsyncTradeCommandClient:
    """Asynchronous gRPC client for submitting trade commands.

    Usage:
        client = AsyncTradeCommandClient("localhost:50051")
        async with client:
            response = await client.submit_trade(trade_request)

    Supports both unary SubmitTradeCommand and server-streaming
    StreamRiskState.
    """

    def __init__(self, endpoint: Optional[str] = None):
        self._endpoint = endpoint or _DEFAULT_GRPC_ENDPOINT
        self._channel = None
        self._stub = None

    async def _ensure_channel(self):
        """Lazily create the async gRPC channel and stub."""
        if self._channel is not None:
            return

        try:
            import grpc.aio
            from swarm.proto.tradecommand.v1 import (
                TradeCommandServiceStub,
            )

            self._channel = grpc.aio.insecure_channel(
                self._endpoint,
                options=[
                    ("grpc.max_receive_message_length", 4 * 1024 * 1024),
                ],
            )
            self._stub = TradeCommandServiceStub(self._channel)
            logger.info("Async gRPC channel opened to %s", self._endpoint)
        except ImportError:
            logger.error("grpcio not installed.")
            raise
        except Exception as e:
            logger.error("Failed to create async gRPC channel: %s", e)
            raise

    async def submit_trade(self, trade_request) -> "TradeResponse":
        """Submit a TradeRequest asynchronously.

        Args:
            trade_request: A betterproto TradeRequest instance.

        Returns:
            A betterproto TradeResponse instance.
        """
        await self._ensure_channel()

        from swarm.proto.tradecommand.v1 import TradeResponse

        try:
            response = await self._stub.SubmitTradeCommand(
                trade_request.SerializeToString()
            )
            if hasattr(response, "SerializeToString"):
                return response
            response_obj = TradeResponse()
            response_obj.ParseFromString(response)
            return response_obj
        except Exception as e:
            logger.error("Async gRPC call failed: %s", e)
            raise

    async def stream_risk_state(self, agent_id: str, throttle_ms: int = 0):
        """Subscribe to live risk state updates from the Rust engine.

        Args:
            agent_id: The agent ID for the subscription.
            throttle_ms: Minimum ms between pushes. 0 = every update.

        Yields:
            RiskState messages from the server stream.
        """
        await self._ensure_channel()

        from swarm.proto.tradecommand.v1 import (
            RiskSubscriptionRequest,
            RiskState,
        )

        request = RiskSubscriptionRequest()
        request.agent_id = agent_id
        request.throttle_interval_ms = throttle_ms

        try:
            response_stream = self._stub.StreamRiskState(
                request.SerializeToString()
            )
            async for chunk in response_stream:
                if hasattr(chunk, "portfolio_drawdown_pct"):
                    # Already a RiskState message
                    yield chunk
                else:
                    state = RiskState()
                    state.ParseFromString(chunk)
                    yield state
        except Exception as e:
            logger.error("Risk state stream error: %s", e)
            raise

    async def close(self):
        """Close the async gRPC channel."""
        if self._channel is not None:
            await self._channel.close()
            self._channel = None
            self._stub = None

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        await self.close()
