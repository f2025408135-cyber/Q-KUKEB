"""Extract the risk gate code from the Q-KUKEB Executor's output.

The original TradingAgents signal processor extracted a 5-tier portfolio
rating (Buy/Overweight/Hold/Underweight/Sell) from the PM's markdown.

In Q-KUKEB, the executor outputs either:
1. A structured trade payload with a risk gate result from the Rust engine
2. A HOLD decision if confidence is too low

This module parses both formats and returns a consistent signal string.
"""

from __future__ import annotations

import re
from typing import Any


class SignalProcessor:
    """Read the risk gate result out of a Q-KUKEB Executor decision."""

    def __init__(self, quick_thinking_llm: Any = None):
        # Accepted for backward compatibility with TradingAgentsGraph.__init__
        self.quick_thinking_llm = quick_thinking_llm

    def process_signal(self, full_signal: str) -> str:
        """Extract the risk gate code or HOLD from the executor's output.

        Returns one of:
        - "ACCEPTED" — trade passed all risk gates
        - "REJECTED_*" — specific rejection reason
        - "HOLD" — executor decided not to trade
        - "DRY_RUN_ACCEPTED" — dry run mode
        - "GRPC_ERROR" — gRPC communication failure
        """
        # Check for explicit Risk Gate line first
        gate_match = re.search(
            r"\*\*Risk Gate\*\*:\s*(\w+)",
            full_signal,
            re.IGNORECASE,
        )
        if gate_match:
            return gate_match.group(1).upper()

        # Check for HOLD decision
        if re.search(r"\*\*Rating\*\*:\s*Hold", full_signal, re.IGNORECASE):
            return "HOLD"

        # Check for FINAL TRANSACTION PROPOSAL line (backward compat)
        action_match = re.search(
            r"FINAL TRANSACTION PROPOSAL:\s*\*\*(\w+)\*\*",
            full_signal,
            re.IGNORECASE,
        )
        if action_match:
            action = action_match.group(1).upper()
            if action == "HOLD":
                return "HOLD"
            # If there's a buy/sell but no risk gate, it was likely a dry run
            return f"DRY_RUN_{action}"

        return "UNKNOWN"
