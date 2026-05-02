import os

_QKUKEB_HOME = os.path.join(os.path.expanduser("~"), ".qkukeb")

DEFAULT_CONFIG = {
    "project_dir": os.path.abspath(os.path.join(os.path.dirname(__file__), ".")),
    "results_dir": os.getenv("QKUKEB_RESULTS_DIR", os.path.join(_QKUKEB_HOME, "logs")),
    "data_cache_dir": os.getenv("QKUKEB_CACHE_DIR", os.path.join(_QKUKEB_HOME, "cache")),
    "memory_log_path": os.getenv("QKUKEB_MEMORY_LOG_PATH", os.path.join(_QKUKEB_HOME, "memory", "trading_memory.md")),
    # Optional cap on the number of resolved memory log entries.
    "memory_log_max_entries": None,

    # ─── LLM settings ───────────────────────────────────────────────────────
    "llm_provider": "openai",
    "deep_think_llm": "gpt-5.4",
    "quick_think_llm": "gpt-5.4-mini",
    "backend_url": None,
    # Provider-specific thinking configuration
    "google_thinking_level": None,
    "openai_reasoning_effort": None,
    "anthropic_effort": None,

    # ─── Q-KUKEB: gRPC bridge to Rust execution engine ──────────────────────
    # The gRPC endpoint for the Rust TradeCommandService.
    # Set via env var QKUKEB_GRPC_ENDPOINT or here.
    "grpc_endpoint": os.getenv("QKUKEB_GRPC_ENDPOINT", "localhost:50051"),
    # Agent ID sent in every TradeRequest.originating_agent_id
    "agent_id": os.getenv("QKUKEB_AGENT_ID", "qkukeb-swarm-0"),
    # When True, the executor runs in dry-run mode: generates payloads
    # but does NOT submit to the Rust engine. Useful for testing the
    # agent swarm without a running Rust backend.
    "dry_run": os.getenv("QKUKEB_DRY_RUN", "false").lower() == "true",

    # ─── Q-KUKEB: Default trade parameters ──────────────────────────────────
    # Default venue routing. Can be overridden by the LLM per-trade.
    "default_venue": os.getenv("QKUKEB_DEFAULT_VENUE", "BINANCE_PERP"),
    # Default quote currency
    "default_quote_asset": "USDT",
    # Default leverage for PERP positions
    "default_leverage": 5.0,
    # Maximum leverage the LLM is allowed to propose
    "max_leverage": 20.0,
    # Default signal TTL in milliseconds
    "default_signal_ttl_ms": 2000,
    # Default slippage limit in basis points
    "default_slippage_bps": 50,

    # ─── Checkpoint/resume ──────────────────────────────────────────────────
    "checkpoint_enabled": False,

    # ─── Output ─────────────────────────────────────────────────────────────
    "output_language": "English",

    # ─── Debate and discussion settings ─────────────────────────────────────
    "max_debate_rounds": 1,
    "max_risk_discuss_rounds": 1,
    "max_recur_limit": 100,

    # ─── Data vendor configuration ──────────────────────────────────────────
    "data_vendors": {
        "core_stock_apis": "yfinance",
        "technical_indicators": "yfinance",
        "fundamental_data": "yfinance",
        "news_data": "yfinance",
    },
    "tool_vendors": {},
}
