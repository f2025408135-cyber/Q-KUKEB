"""Pydantic schemas used by agents that produce structured output.

Original schemas from TradingAgents (PortfolioRating, TraderAction, ResearchPlan,
TraderProposal, PortfolioDecision) are preserved for upstream compatibility.

New: QKukebTradePayload — maps 1:1 to the Rust TradeRequest protobuf.
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone
from enum import Enum
from typing import Optional

from pydantic import BaseModel, Field


# ---------------------------------------------------------------------------
# Shared rating types (original TradingAgents)
# ---------------------------------------------------------------------------


class PortfolioRating(str, Enum):
    """5-tier rating used by the Research Manager and Portfolio Manager."""

    BUY = "Buy"
    OVERWEIGHT = "Overweight"
    HOLD = "Hold"
    UNDERWEIGHT = "Underweight"
    SELL = "Sell"


class TraderAction(str, Enum):
    """3-tier transaction direction used by the Trader."""

    BUY = "Buy"
    HOLD = "Hold"
    SELL = "Sell"


# ---------------------------------------------------------------------------
# Research Manager (original)
# ---------------------------------------------------------------------------


class ResearchPlan(BaseModel):
    recommendation: PortfolioRating = Field(
        description=(
            "The investment recommendation. Exactly one of Buy / Overweight / "
            "Hold / Underweight / Sell."
        ),
    )
    rationale: str = Field(
        description="Conversational summary of the key debate points.",
    )
    strategic_actions: str = Field(
        description="Concrete steps for the trader to implement.",
    )


def render_research_plan(plan: ResearchPlan) -> str:
    return "\n".join([
        f"**Recommendation**: {plan.recommendation.value}",
        "",
        f"**Rationale**: {plan.rationale}",
        "",
        f"**Strategic Actions**: {plan.strategic_actions}",
    ])


# ---------------------------------------------------------------------------
# Trader (original)
# ---------------------------------------------------------------------------


class TraderProposal(BaseModel):
    action: TraderAction = Field(
        description="The transaction direction. Exactly one of Buy / Hold / Sell.",
    )
    reasoning: str = Field(
        description="The case for this action. Two to four sentences.",
    )
    entry_price: Optional[float] = Field(
        default=None,
        description="Optional entry price target.",
    )
    stop_loss: Optional[float] = Field(
        default=None,
        description="Optional stop-loss price.",
    )
    position_sizing: Optional[str] = Field(
        default=None,
        description="Optional sizing guidance, e.g. '5% of portfolio'.",
    )


def render_trader_proposal(proposal: TraderProposal) -> str:
    parts = [
        f"**Action**: {proposal.action.value}",
        "",
        f"**Reasoning**: {proposal.reasoning}",
    ]
    if proposal.entry_price is not None:
        parts.extend(["", f"**Entry Price**: {proposal.entry_price}"])
    if proposal.stop_loss is not None:
        parts.extend(["", f"**Stop Loss**: {proposal.stop_loss}"])
    if proposal.position_sizing:
        parts.extend(["", f"**Position Sizing**: {proposal.position_sizing}"])
    parts.extend([
        "",
        f"FINAL TRANSACTION PROPOSAL: **{proposal.action.value.upper()}**",
    ])
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# Portfolio Manager (original — kept for backward compat, not used in hot path)
# ---------------------------------------------------------------------------


class PortfolioDecision(BaseModel):
    rating: PortfolioRating = Field(
        description="The final position rating.",
    )
    executive_summary: str = Field(
        description="Concise action plan. Two to four sentences.",
    )
    investment_thesis: str = Field(
        description="Detailed reasoning anchored in debate evidence.",
    )
    price_target: Optional[float] = Field(
        default=None,
        description="Optional target price.",
    )
    time_horizon: Optional[str] = Field(
        default=None,
        description="Optional holding period, e.g. '3-6 months'.",
    )


def render_pm_decision(decision: PortfolioDecision) -> str:
    parts = [
        f"**Rating**: {decision.rating.value}",
        "",
        f"**Executive Summary**: {decision.executive_summary}",
        "",
        f"**Investment Thesis**: {decision.investment_thesis}",
    ]
    if decision.price_target is not None:
        parts.extend(["", f"**Price Target**: {decision.price_target}"])
    if decision.time_horizon:
        parts.extend(["", f"**Time Horizon**: {decision.time_horizon}"])
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# Q-KUKEB: gRPC Bridge Payload (NEW)
# ---------------------------------------------------------------------------


class QKukebAssetClass(str, Enum):
    """Maps to AssetClass in trade_command.proto."""
    SPOT = "SPOT"
    PERP = "PERP"
    DATED_FUT = "DATED_FUT"
    OPTION = "OPTION"


class QKukebOrderSide(str, Enum):
    """Maps to OrderSide in trade_command.proto."""
    BUY = "BUY"
    SELL = "SELL"


class QKukebExecutionAlgo(str, Enum):
    """Maps to ExecutionAlgo in trade_command.proto."""
    TWAP = "TWAP"
    VWAP = "VWAP"
    ICEBERG = "ICEBERG"
    AGGRESSIVE = "AGGRESSIVE"
    PASSIVE = "PASSIVE"


class QKukebStrategyType(str, Enum):
    """Maps to StrategyType in trade_command.proto."""
    FUNDING_RATE_ARB = "FUNDING_RATE_ARB"
    MEV_LIQUIDATION = "MEV_LIQUIDATION"
    STAT_ARB_CROSSVENUE = "STAT_ARB_CROSSVENUE"
    MEAN_REVERSION_L2 = "MEAN_REVERSION_L2"


class QKukebTradePayload(BaseModel):
    """The LLM-structured output that maps 1:1 to Rust TradeRequest protobuf.

    The Q-KUKEB Portfolio Manager LLM produces this payload. It is then
    converted to a betterproto TradeRequest and sent over gRPC to the Rust
    execution engine.

    All monetary values are in quote currency (USD/USDT).
    """

    base_asset: str = Field(
        description=(
            "The base asset ticker, e.g. 'BTC', 'ETH', 'NVDA'. "
            "Must match the company_of_interest from the analyst pipeline."
        ),
    )
    quote_asset: str = Field(
        default="USDT",
        description="Quote currency. Default 'USDT' for crypto, 'USD' for equities.",
    )
    asset_class: QKukebAssetClass = Field(
        default=QKukebAssetClass.SPOT,
        description="Instrument type. Use PERP for leveraged futures.",
    )
    venue_id: str = Field(
        default="BINANCE_PERP",
        description=(
            "Exchange routing key. Examples: 'BINANCE_PERP', 'DYDX_V4', "
            "'BYBIT_SPOT'. Use 'SIMULATED' for paper trading."
        ),
    )
    instrument: str = Field(
        description=(
            "Canonical instrument code in format 'BASE-QUOTE-CLASS', e.g. "
            "'BTC-USDT-PERP', 'NVDA-USD-SPOT'."
        ),
    )
    side: QKukebOrderSide = Field(
        description="Order direction. BUY or SELL. Never HOLD — skip gRPC for holds.",
    )
    notional_value: float = Field(
        description=(
            "Position size in quote currency (USD/USDT). "
            "E.g. 50000.0 means $50,000 notional exposure. "
            "Must be positive. Derive from position_sizing if available, "
            "otherwise use a reasonable default based on conviction."
        ),
    )
    leverage_ratio: float = Field(
        default=1.0,
        ge=1.0,
        le=125.0,
        description=(
            "Leverage multiplier. 1.0 = no leverage. For PERP positions, "
            "consider 5-20x depending on conviction and volatility. "
            "The Rust engine enforces a hard cap from its own config."
        ),
    )
    execution_algo: QKukebExecutionAlgo = Field(
        default=QKukebExecutionAlgo.VWAP,
        description=(
            "Execution algorithm. VWAP for large orders (>100k), "
            "AGGRESSIVE for time-sensitive entries, TWAP for steady accumulation."
        ),
    )
    slippage_bps_limit: int = Field(
        default=50,
        ge=1,
        le=500,
        description=(
            "Maximum acceptable slippage in basis points. "
            "50 bps = 0.5% max slippage. Tighter for liquid instruments, "
            "wider for illiquid or volatile ones."
        ),
    )
    strategy_type: QKukebStrategyType = Field(
        default=QKukebStrategyType.STAT_ARB_CROSSVENUE,
        description=(
            "The strategy that generated this signal. "
            "STAT_ARB_CROSSVENUE for most equity signals. "
            "FUNDING_RATE_ARB for crypto perp vs spot. "
            "MEAN_REVERSION_L2 for book imbalance signals."
        ),
    )
    signal_confidence: float = Field(
        ge=0.0,
        le=1.0,
        description=(
            "Composite confidence score (0.0-1.0) derived from the debate. "
            "Consider: Was the bull/bear debate unanimous or contested? "
            "Did all three risk analysts agree? How strong is the evidence? "
            "0.9+ = high conviction, 0.7-0.9 = moderate, <0.7 = cautious."
        ),
    )
    signal_ttl_ms: int = Field(
        default=2000,
        description=(
            "Signal validity window in milliseconds. "
            "150ms for MEV liquidation cascades, "
            "500ms for mean reversion, "
            "2000ms for funding rate arb, "
            "5000ms for slow cross-venue stat arb."
        ),
    )
    reasoning: str = Field(
        description=(
            "Executive summary of the trade decision. "
            "Synthesize: the analyst consensus, the risk debate outcome, "
            "the key risk levels (entry, stop-loss, target), and why this "
            "notional/leverage is appropriate. Two to four sentences."
        ),
    )


def generate_request_id() -> str:
    """Generate a unique idempotency key for the gRPC request."""
    return str(uuid.uuid4())


def payload_to_trade_request(
    payload: QKukebTradePayload,
    agent_id: str,
    request_id: Optional[str] = None,
) -> "swarm.proto.tradecommand.v1.TradeRequest":
    """Convert a QKukebTradePayload to a betterproto TradeRequest.

    This is the gRPC bridge serialization point. The output is sent
    over the wire to the Rust execution engine.
    """
    from swarm.proto.tradecommand.v1 import (
        TradeRequest,
        AssetIdentifier,
        FixedDecimal,
        SignalInvalidationThresholds,
        StrategyType,
        AssetClass,
        OrderSide,
        ExecutionAlgo,
    )

    now = datetime.now(timezone.utc)

    # Map enums
    _asset_class_map = {
        QKukebAssetClass.SPOT: AssetClass.ASSET_CLASS_SPOT,
        QKukebAssetClass.PERP: AssetClass.ASSET_CLASS_PERP,
        QKukebAssetClass.DATED_FUT: AssetClass.ASSET_CLASS_DATED_FUT,
        QKukebAssetClass.OPTION: AssetClass.ASSET_CLASS_OPTION,
    }
    _side_map = {
        QKukebOrderSide.BUY: OrderSide.ORDER_SIDE_BUY,
        QKukebOrderSide.SELL: OrderSide.ORDER_SIDE_SELL,
    }
    _algo_map = {
        QKukebExecutionAlgo.TWAP: ExecutionAlgo.EXECUTION_ALGO_TWAP,
        QKukebExecutionAlgo.VWAP: ExecutionAlgo.EXECUTION_ALGO_VWAP,
        QKukebExecutionAlgo.ICEBERG: ExecutionAlgo.EXECUTION_ALGO_ICEBERG,
        QKukebExecutionAlgo.AGGRESSIVE: ExecutionAlgo.EXECUTION_ALGO_AGGRESSIVE,
        QKukebExecutionAlgo.PASSIVE: ExecutionAlgo.EXECUTION_ALGO_PASSIVE,
    }
    _strategy_map = {
        QKukebStrategyType.FUNDING_RATE_ARB: StrategyType.STRATEGY_FUNDING_RATE_ARB,
        QKukebStrategyType.MEV_LIQUIDATION: StrategyType.STRATEGY_MEV_LIQUIDATION,
        QKukebStrategyType.STAT_ARB_CROSSVENUE: StrategyType.STRATEGY_STAT_ARB_CROSSVENUE,
        QKukebStrategyType.MEAN_REVERSION_L2: StrategyType.STRATEGY_MEAN_REVERSION_L2,
    }

    # Convert notional_value (float) to FixedDecimal (int, scale=4)
    notional_raw = int(round(payload.notional_value * 10000))
    leverage_raw = int(round(payload.leverage_ratio * 100))
    confidence_raw = int(round(payload.signal_confidence * 10000))

    # Build the protobuf TradeRequest
    req = TradeRequest()
    req.request_id = request_id or generate_request_id()
    req.originating_agent_id = agent_id
    req.signal_detected_at = now
    req.side = _side_map[payload.side]
    req.execution_algo = _algo_map[payload.execution_algo]
    req.slippage_bps_limit = payload.slippage_bps_limit
    req.execution_ttl_ms = payload.signal_ttl_ms * 5  # execution window = 5x signal TTL
    req.strategy_type = _strategy_map[payload.strategy_type]
    req.strategy_version = "1.0.0"

    # Asset identifier
    req.asset = AssetIdentifier()
    req.asset.base_asset = payload.base_asset
    req.asset.quote_asset = payload.quote_asset
    req.asset.asset_class = _asset_class_map[payload.asset_class]
    req.asset.venue_id = payload.venue_id
    req.asset.instrument = payload.instrument

    # Notional value as FixedDecimal
    req.notional_value = FixedDecimal()
    req.notional_value.raw_units = notional_raw
    req.notional_value.scale = 4

    # Leverage as FixedDecimal
    req.leverage_ratio = FixedDecimal()
    req.leverage_ratio.raw_units = leverage_raw
    req.leverage_ratio.scale = 2

    # Signal confidence as FixedDecimal
    req.signal_confidence = FixedDecimal()
    req.signal_confidence.raw_units = confidence_raw
    req.signal_confidence.scale = 4

    # Invalidation thresholds
    req.invalidation = SignalInvalidationThresholds()
    req.invalidation.signal_ttl_ms = payload.signal_ttl_ms
    req.invalidation.max_price_drift_bps = 100  # 1% max drift
    req.invalidation.min_funding_spread_bps_e2 = 0  # not applicable for non-funding

    return req
