"""Q-KUKEB Executor: replaces the TradingAgents Portfolio Manager.

The original PM synthesized the risk debate into a 5-tier markdown rating.
That's a dead-end — it produces human-readable text but zero executable payload.

This module:
1. Reads the trader_investment_plan (TraderProposal markdown)
2. Reads the risk_debate_state (3-way debate history)
3. Reads the investment_plan (ResearchPlan from Research Manager)
4. Calls the LLM with structured output → QKukebTradePayload (Pydantic)
5. If action == HOLD: skip gRPC, return hold decision
6. Convert Pydantic → betterproto TradeRequest
7. Submit via gRPC to Rust execution engine
8. Return the TradeResponse (including risk_gate_code, risk_snapshot)
"""

from __future__ import annotations

import logging
import time
from typing import Optional

from swarm.agents.schemas import (
    QKukebTradePayload,
    generate_request_id,
    payload_to_trade_request,
    TraderAction,
)
from swarm.agents.utils.agent_utils import (
    build_instrument_context,
    get_language_instruction,
)
from swarm.agents.utils.structured import (
    bind_structured,
    invoke_structured_or_freetext,
)

logger = logging.getLogger(__name__)


def _render_qkukeb_payload(payload: QKukebTradePayload) -> str:
    """Render a QKukebTradePayload to markdown for state logging."""
    parts = [
        f"**Action**: {payload.side.value} {payload.base_asset}",
        f"**Instrument**: {payload.instrument}",
        f"**Venue**: {payload.venue_id}",
        f"**Notional**: ${payload.notional_value:,.2f}",
        f"**Leverage**: {payload.leverage_ratio}x",
        f"**Execution**: {payload.execution_algo.value}",
        f"**Strategy**: {payload.strategy_type.value}",
        f"**Confidence**: {payload.signal_confidence:.2%}",
        "",
        f"**Reasoning**: {payload.reasoning}",
    ]
    return "\n".join(parts)


def _render_hold_decision(reasoning: str) -> str:
    """Render a hold decision for state logging."""
    return (
        "**Rating**: Hold\n\n"
        f"**Reasoning**: {reasoning}\n\n"
        "**Status**: No order submitted — signal confidence insufficient or "
        "risk debate inconclusive."
    )


def create_qkukeb_executor(
    llm,
    grpc_client=None,
    agent_id: str = "qkukeb-swarm-0",
):
    """Create the Q-KUKEB executor node.

    Args:
        llm: The deep-thinking LLM for structured output generation.
        grpc_client: Optional TradeCommandClient for submitting to Rust engine.
                    If None, runs in dry-run mode (logs payload, no gRPC call).
        agent_id: The originating agent ID for gRPC requests.

    Returns:
        A LangGraph node function.
    """
    structured_llm = bind_structured(llm, QKukebTradePayload, "Q-KUKEB Executor")

    def qkukeb_executor_node(state) -> dict:
        instrument_context = build_instrument_context(state["company_of_interest"])

        history = state["risk_debate_state"]["history"]
        risk_debate_state = state["risk_debate_state"]
        research_plan = state["investment_plan"]
        trader_plan = state["trader_investment_plan"]

        past_context = state.get("past_context", "")
        lessons_line = (
            f"- Lessons from prior decisions and outcomes:\n{past_context}\n"
            if past_context
            else ""
        )

        prompt = f"""As the Q-KUKEB Portfolio Executor, your job is to translate the analysts' debate and the trader's proposal into an executable trade command.

{instrument_context}

---

**Context:**
- Research Manager's investment plan: **{research_plan}**
- Trader's transaction proposal: **{trader_plan}**
{lessons_line}
**Risk Analysts Debate History:**
{history}

---

Your task:
1. Assess the overall conviction from the debate. If the evidence is genuinely balanced or conflicting, output side=BUY with a very small notional and low confidence — the Rust risk engine will reject it. But if there is a clear directional thesis, size the position appropriately.
2. Determine the optimal instrument format (SPOT vs PERP). Use PERP for leveraged positions.
3. Choose leverage carefully: 1x for cautious, 5-10x for moderate conviction, 10-20x for high conviction. Never exceed 20x.
4. Set signal_confidence based on: debate unanimity, evidence strength, and consensus quality.
5. Pick the execution algorithm: VWAP for orders >$100k, AGGRESSIVE for urgent entries, TWAP for steady builds.

Be precise with the instrument code format: BASE-QUOTE-CLASS (e.g. NVDA-USD-SPOT, BTC-USDT-PERP).
The notional_value is the TOTAL position size in USD, BEFORE leverage is applied.{get_language_instruction()}"""

        # Generate structured trade payload via LLM
        trade_payload = invoke_structured_or_freetext(
            structured_llm,
            llm,
            prompt,
            _render_qkukeb_payload,
            "Q-KUKEB Executor",
        )

        # Extract the parsed payload (may be rendered markdown if structured output failed)
        if isinstance(trade_payload, QKukebTradePayload):
            payload = trade_payload
        else:
            # Structured output failed — log and produce a hold
            logger.warning(
                "Structured output failed for Q-KUKEB Executor, got raw text. "
                "Producing HOLD decision."
            )
            return {
                "risk_debate_state": _update_risk_state(risk_debate_state, "HOLD (parse failure)"),
                "final_trade_decision": _render_hold_decision(
                    "Structured output parse failure — holding."
                ),
            }

        # ── Check for HOLD-like conditions ──
        # The LLM should output BUY or SELL, but if confidence is very low,
        # we can short-circuit here to avoid a round-trip to Rust.
        if payload.signal_confidence < 0.3:
            logger.info(
                "Signal confidence %.2f below threshold 0.30 — HOLD",
                payload.signal_confidence,
            )
            return {
                "risk_debate_state": _update_risk_state(risk_debate_state, "HOLD (low confidence)"),
                "final_trade_decision": _render_hold_decision(
                    f"Signal confidence {payload.signal_confidence:.0%} below threshold. "
                    f"Reasoning: {payload.reasoning}"
                ),
            }

        # ── Submit to Rust via gRPC ──
        request_id = generate_request_id()
        trade_request = payload_to_trade_request(
            payload, agent_id=agent_id, request_id=request_id
        )

        gate_code_str = "UNKNOWN"
        rejection_detail = ""

        if grpc_client is not None:
            try:
                logger.info(
                    "Submitting trade: %s %s %s notional=%.2f leverage=%.1fx",
                    payload.side.value,
                    payload.instrument,
                    payload.strategy_type.value,
                    payload.notional_value,
                    payload.leverage_ratio,
                )
                response = grpc_client.submit_trade_sync(trade_request)

                gate_code_str = _risk_gate_code_to_str(response.risk_gate_code)
                rejection_detail = response.rejection_detail or ""

                if response.risk_gate_code == 1:  # RISK_GATE_ACCEPTED
                    logger.info(
                        "Trade ACCEPTED: execution_id=%s filled=%.2f avg_px=%s",
                        response.execution_id,
                        response.filled_notional.raw_units / (10 ** response.filled_notional.scale),
                        response.average_fill_price.raw_units / (10 ** response.average_fill_price.scale) if response.average_fill_price else "N/A",
                    )
                else:
                    logger.warning(
                        "Trade REJECTED: code=%s detail=%s",
                        gate_code_str,
                        rejection_detail,
                    )
            except Exception as e:
                logger.error("gRPC submission failed: %s", e)
                gate_code_str = "GRPC_ERROR"
                rejection_detail = str(e)
        else:
            logger.info(
                "DRY RUN: would submit %s %s notional=%.2f",
                payload.side.value,
                payload.instrument,
                payload.notional_value,
            )
            gate_code_str = "DRY_RUN_ACCEPTED"

        # Build final trade decision for state
        final_decision = _render_qkukeb_payload(payload)
        final_decision += f"\n\n**Risk Gate**: {gate_code_str}"
        if rejection_detail:
            final_decision += f"\n**Rejection Detail**: {rejection_detail}"

        return {
            "risk_debate_state": _update_risk_state(
                risk_debate_state, gate_code_str
            ),
            "final_trade_decision": final_decision,
        }

    return qkukeb_executor_node


def _update_risk_state(risk_debate_state: dict, judge_decision: str) -> dict:
    """Update the risk debate state with the executor's decision."""
    return {
        "judge_decision": judge_decision,
        "history": risk_debate_state["history"],
        "aggressive_history": risk_debate_state["aggressive_history"],
        "conservative_history": risk_debate_state["conservative_history"],
        "neutral_history": risk_debate_state["neutral_history"],
        "latest_speaker": "Judge",
        "current_aggressive_response": risk_debate_state.get(
            "current_aggressive_response", ""
        ),
        "current_conservative_response": risk_debate_state.get(
            "current_conservative_response", ""
        ),
        "current_neutral_response": risk_debate_state.get(
            "current_neutral_response", ""
        ),
        "count": risk_debate_state["count"],
    }


def _risk_gate_code_to_str(code: int) -> str:
    """Convert protobuf RiskGateCode enum int to human-readable string."""
    _codes = {
        0: "UNSPECIFIED",
        1: "ACCEPTED",
        2: "REJECTED_MAX_DRAWDOWN",
        3: "REJECTED_VAR_BREACH",
        4: "REJECTED_GARCH_VOLATILITY",
        5: "REJECTED_NOTIONAL_LIMIT",
        6: "REJECTED_LEVERAGE_LIMIT",
        7: "REJECTED_CONCENTRATION",
        8: "REJECTED_SIGNAL_STALE",
        9: "REJECTED_INSUFFICIENT_LIQ",
        10: "EXECUTION_ERROR",
    }
    return _codes.get(code, f"UNKNOWN({code})")
