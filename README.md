# Q-KUKEB — Hybrid Algorithmic Trading Engine

A high-frequency, hybrid trading system combining a **Rust execution engine** with a **Python intelligence swarm**. The Rust side handles risk gating, order execution, and market data ingestion with microsecond latency. The Python side runs agent-based signal generation strategies connected via gRPC and ZeroMQ.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        PYTHON SWARM                                 │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                         │
│  │ Agent:   │  │ Agent:   │  │ Agent:   │   ← Strategy agents     │
│  │ Funding  │  │ Cross-Ven│  │ Mean-Rev │                         │
│  │ Arb      │  │ Stat Arb │  │ L2 Book  │                         │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘                         │
│       │              │              │                                │
│  ─────┴──────────────┴──────────────┴─────                          │
│  │          DAG Runner / Consensus         │                        │
│  ─────┬───────────────────────────────────┘                        │
│       │                                                        gRPC  │
│  ─────┴────────────────────────────────────────────────────────── │
└──────────────────────────────────────────────────────────────────┼─┘
                                                                   │
                        TradeRequest (protobuf)                     │
                              │                                     │
                              ▼                                     │
┌───────────────────────────────────────────────────────────────────┐
│                     RUST EXECUTION ENGINE                          │
│                                                                   │
│  ┌─────────────┐    ┌──────────────┐    ┌─────────────────────┐   │
│  │   Gateway   │───▶│    Risk      │───▶│    Transport        │   │
│  │ (WS ingest) │    │ Interceptor  │    │ (gRPC server +      │   │
│  │             │    │              │    │  ZeroMQ PUB)        │   │
│  └─────────────┘    │ Gate 0: TTL  │    └─────────────────────┘   │
│                     │ Gate 1: DD    │                             │
│  ┌─────────────┐    │ Gate 2: VaR   │                             │
│  │   GARCH     │    │ Gate 3: σ²    │                             │
│  │ (ring buf)  │───▶│              │                             │
│  └─────────────┘    └──────────────┘                             │
└───────────────────────────────────────────────────────────────────┘
                              │
                         ZeroMQ PUB
                        (BOOK/*, RISK/*)
                              │
                              ▼
                    ┌──────────────────┐
                    │   Monitoring /    │
                    │   Dashboard       │
                    └──────────────────┘
```

## Communication Contracts

### 1. gRPC — `trade_command.proto`

The Python swarm submits structured trade theses via gRPC. The Rust engine evaluates them through the risk gate chain and returns fill details or rejection codes.

| RPC | Direction | Purpose |
|-----|-----------|---------|
| `SubmitTradeCommand` | Python → Rust | Unary: submit a trade thesis |
| `StreamRiskState` | Rust → Python | Server-streaming: live risk metrics |

### 2. ZeroMQ PUB — Market State

The Rust engine publishes two topic streams:

| Topic | Payload | Frequency |
|-------|---------|-----------|
| `BOOK/{venue}/{instrument}` | L2 order book snapshot + delta | Every depth change |
| `RISK/{venue}/{instrument}` | GARCH tick, drawdown, gate states | On risk metric change |

### 3. Risk Gate Chain

Gates are evaluated in **strict immutable order**, short-circuiting on first failure:

| Gate | Check | Rejection Code |
|------|-------|---------------|
| 0 | Signal TTL freshness | `REJECTED_SIGNAL_STALE` |
| 1 | Portfolio drawdown ceiling | `REJECTED_MAX_DRAWDOWN` |
| 2 | 1-day 99% VaR limit | `REJECTED_VAR_BREACH` |
| 3 | GARCH(1,1) σ² threshold | `REJECTED_GARCH_VOLATILITY` |

## Key Design Decisions

- **`FixedDecimal` everywhere** — All monetary values use `(raw_units: i64, scale: u32)` integer pairs. No floats cross the gRPC boundary. This prevents determinism bugs in GARCH arithmetic during volatility spikes.
- **`signal_ttl_ms` is Gate 0** — Checked before any portfolio math. A stale signal that passes portfolio gates is a latency problem, not alpha.
- **GARCH(1,1) ring buffer** — Stack-only, `#[inline(always)]` on the hot path. No heap allocation inside `update()`. Parameters are config-time only, never received over the wire.
- **`α + β < 1.0` enforced at startup** — Panics at construction if the stationarity condition is violated. Better to crash at boot than produce garbage risk metrics.

## Repository Structure

```
algo-hybrid/
├── execution-engine/              # Rust workspace
│   ├── Cargo.toml                 # Workspace manifest (tonic 0.11, prost 0.12)
│   └── crates/
│       ├── risk/                  # RiskInterceptor, GARCH, FixedDecimal
│       │   └── src/
│       │       ├── interceptor.rs # Gate chain trait + DefaultRiskInterceptor
│       │       ├── garch.rs       # GARCH(1,1) ring buffer (const generic N=500)
│       │       └── decimal.rs     # FixedDecimal arithmetic helpers
│       ├── gateway/               # WebSocket ingestion, tick normalization
│       ├── transport/             # ZeroMQ PUB, gRPC server
│       └── proto/                 # tonic build.rs, generated stubs
│           └── build.rs           # Proto compilation from /proto/
├── intelligence-swarm/            # Python package
│   ├── pyproject.toml             # betterproto, grpcio, pyzmq, numpy
│   └── swarm/
│       ├── agents/                # Strategy agent base + implementations
│       ├── dag/                   # DAG runner, consensus logic
│       ├── transport/             # gRPC client, ZeroMQ SUB
│       └── proto/                 # betterproto generated stubs
│           └── tradecommand/
│               └── v1.py          # TradeRequest, TradeResponse, etc.
├── proto/                         # Single source of truth
│   └── trade_command.proto        # Protobuf schema (Contract 1)
└── docker-compose.yml             # zmq-broker, rust-engine, python-swarm
```

## Build & Test

### Prerequisites

- Rust 1.75+
- Python 3.11+
- protoc 25.x
- Docker (for containerized deployment)

### Rust Engine

```bash
cd execution-engine
cargo build              # Build all crates
cargo test -p risk-engine  # Run risk interceptor + GARCH tests (17 tests)
cargo test               # Run all tests
```

### Python Swarm

```bash
cd intelligence-swarm
pip install -e ".[dev]"
python -m pytest swarm/  # Run swarm tests
```

### Proto Codegen

Rust (via tonic-build, triggered on `cargo build`):
```bash
export PROTOC=/path/to/protoc
cd execution-engine && cargo build -p proto-types
```

Python (via betterproto):
```bash
cd intelligence-swarm/swarm/proto
protoc --python_betterproto_out=. \
  -I../../../proto \
  ../../../proto/trade_command.proto
```

### Docker

```bash
docker-compose up --build
```

## Test Results — Sprint Block 1

```
running 17 tests
test decimal::tests::test_f64_roundtrip .............. ok
test decimal::tests::test_fixed_decimal_basic ......... ok
test decimal::tests::test_comparison ................ ok
test garch::tests::test_const_generic_different_sizes  ok
test garch::tests::test_first_observation_returns_initial_variance  ok
test garch::tests::test_ring_buffer_wraps_correctly .. ok
test garch::tests::test_reset ....................... ok
test garch::tests::test_construction_enforces_stationarity  ok
test garch::tests::test_construction_rejects_zero_omega  ok
test garch::tests::test_snapshot ................... ok
test garch::tests::test_try_new_error_cases ......... ok
test garch::tests::test_sigma_squared_convergence_on_synthetic_returns  ok
test interceptor::tests::test_drawdown_gate_rejects  ok
test interceptor::tests::test_all_gates_pass ......... ok
test interceptor::tests::test_garch_gate_rejects .... ok
test interceptor::tests::test_gate_order_is_drawdown_before_var  ok
test interceptor::tests::test_var_gate_rejects ...... ok

test result: ok. 17 passed; 0 failed; 0 ignored
```

## Supported Strategies

| Strategy | Type | Description |
|----------|------|-------------|
| `FUNDING_RATE_ARB` | Market-making | Perpetual vs spot funding spread capture |
| `MEV_LIQUIDATION` | Exploitative | Cascading liquidation front-running |
| `STAT_ARB_CROSSVENUE` | Statistical | Cross-venue price discrepancy |
| `MEAN_REVERSION_L2` | Microstructure | L2 order book imbalance mean reversion |

## Execution Algorithms

| Algo | Behavior |
|------|----------|
| `TWAP` | Time-weighted average price |
| `VWAP` | Volume-weighted average price |
| `ICEBERG` | Hidden order, slice exposure |
| `AGGRESSIVE` | IOC / sweep liquidity |
| `PASSIVE` | Post-only, maker-only |

## License

Proprietary — All rights reserved.
