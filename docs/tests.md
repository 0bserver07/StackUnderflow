# StackUnderflow Test Documentation

## Overview
This document describes the test suite for StackUnderflow. The suite currently runs **340 passing + 2 skipped** tests covering the adapter layer, ingest pipeline, SQLite store, statistics pipeline, report rendering, and the CLI/HTTP surfaces.

## Test Structure

The test directory structure mirrors the `stackunderflow` module structure for better organization:

```
tests/
├── stackunderflow/
│   ├── adapters/                       # Adapter contract + Claude adapter
│   │   ├── contract.py                 # Shared contract helpers (not a test module)
│   │   ├── test_base.py                # BaseAdapter shared behaviour (3 tests)
│   │   ├── test_claude.py              # Claude JSONL adapter (12 tests)
│   │   └── test_registry.py            # Adapter registry (2 tests)
│   ├── ingest/                         # Log discovery + incremental ingest
│   │   ├── test_enumerate.py           # Log enumeration (2 tests)
│   │   ├── test_incremental.py         # Incremental watermark logic (4 tests)
│   │   └── test_writer.py              # Ingest writer + rollback (5 tests)
│   ├── store/                          # SQLite session store
│   │   ├── test_db.py                  # Connection + migrations (4 tests)
│   │   ├── test_queries.py             # Read queries (10 tests)
│   │   ├── test_schema.py              # Schema definitions (4 tests)
│   │   └── test_types.py               # Row dataclasses (4 tests)
│   ├── stats/                          # Classifier → enricher → aggregator → formatter
│   │   ├── test_classifier.py          # Message classification (50 tests)
│   │   ├── test_enricher.py            # Token / cost enrichment (36 tests)
│   │   ├── test_aggregator.py          # Daily / model / tool aggregation (63 tests)
│   │   └── test_formatter.py           # Output formatting (10 tests)
│   ├── reports/                        # Report pipeline
│   │   ├── test_scope.py               # Scope filtering (9 tests)
│   │   ├── test_aggregate.py           # Report-level aggregation (6 tests)
│   │   ├── test_optimize.py            # Optimization step (3 tests)
│   │   └── test_render.py              # Renderers (5 tests)
│   ├── utils/
│   │   └── test_log_finder.py          # Log discovery helpers (6 tests)
│   ├── test_cli.py                     # CLI command parsing (20 tests)
│   ├── test_cli_data_commands.py       # `data` subcommands (13 tests)
│   ├── test_server.py                  # FastAPI endpoints (26 tests)
│   ├── test_pricing_service.py         # Pricing service (5 tests)
│   ├── test_qa_service_resolution.py   # Q&A resolution service (19 tests)
│   ├── test_tag_service_intent.py      # Tag / intent service (21 tests)
│   └── baseline_phase2.json            # Baseline fixture used by pipeline tests
├── mock-data/
│   ├── -Users-test-dev-ai-music/       # Sample Claude JSONL logs
│   └── pricing.json                    # Pricing fixture
└── baseline_results.json               # Baseline fixture for regression checks
```

### What Each Test Suite Covers

#### Adapter Tests (`tests/stackunderflow/adapters/`)

**test_base.py** (3 tests):
- Shared `BaseAdapter` behaviour and default implementations

**test_claude.py** (12 tests):
- Parsing Claude JSONL logs
- Message extraction, role mapping, and tool-use records
- Token / cost field normalization

**test_registry.py** (2 tests):
- Adapter registration and lookup
- Defensive copy of registered adapters

#### Ingest Tests (`tests/stackunderflow/ingest/`)

**test_enumerate.py** (2 tests):
- Enumerating log files for ingest

**test_incremental.py** (4 tests):
- Incremental ingest watermarks
- Skipping already-ingested rows

**test_writer.py** (5 tests):
- Writing parsed rows into the SQLite store
- Transactional rollback on failure

#### Store Tests (`tests/stackunderflow/store/`)

**test_db.py** (4 tests):
- Connection management and migrations

**test_queries.py** (10 tests):
- Read queries against the session store

**test_schema.py** (4 tests):
- Schema definitions and column types

**test_types.py** (4 tests):
- Row dataclasses (e.g. `DayTotals`) field shape

#### Stats Pipeline Tests (`tests/stackunderflow/stats/`)

**test_classifier.py** (50 tests):
- Role, error, and interaction classification
- Tool-use and streaming-message handling

**test_enricher.py** (36 tests):
- Token counting and cost enrichment
- Model resolution and pricing lookups

**test_aggregator.py** (63 tests):
- Daily / hourly activity aggregation
- Per-model and per-tool rollups
- Project and cross-project summaries

**test_formatter.py** (10 tests):
- Output shaping for the HTTP layer
- Error-flag mapping

#### Report Tests (`tests/stackunderflow/reports/`)

**test_scope.py** (9 tests):
- Scope / date-range filters
- Malformed timestamp handling

**test_aggregate.py** (6 tests):
- Report-level aggregation over stats output

**test_optimize.py** (3 tests):
- Report optimization pass

**test_render.py** (5 tests):
- Renderer output

#### Utility Tests (`tests/stackunderflow/utils/`)

**test_log_finder.py** (6 tests):
- Claude log directory discovery
- Project path ↔ log path conversion
- Cross-platform path handling
- Log file filtering

#### CLI & Server Tests

**test_cli.py** (20 tests):
- CLI command parsing (`start`, `init`, `cfg`, `config`, `clear-cache`)
- Settings management and persistence
- Environment variable handling
- Command output formatting

**test_cli_data_commands.py** (13 tests):
- `data` subcommands (optimize, etc.)
- Project-loop behaviour

**test_server.py** (26 tests):
- FastAPI endpoint behaviour
- Request / response validation
- Error handling
- Store-backed data routes

#### Service Tests

**test_pricing_service.py** (5 tests):
- Pricing refresh, staleness flag, error paths

**test_qa_service_resolution.py** (19 tests):
- Q&A route filters (resolved, unresolved)
- Resolution state transitions

**test_tag_service_intent.py** (21 tests):
- Tag browse by intent
- Intent tag resolution

### Test Data
- `tests/mock-data/-Users-test-dev-ai-music/` — sample JSONL files mirroring a Claude project directory
- `tests/mock-data/pricing.json` — pricing fixture for enricher tests
- `tests/baseline_results.json` — baseline fixture for regression checks
- `tests/stackunderflow/baseline_phase2.json` — pipeline baseline fixture

## Running Tests

### All Tests
```bash
python -m pytest
```

Quiet mode (one line per file):
```bash
python -m pytest -q
```

### Specific Test Modules
```bash
python -m pytest tests/stackunderflow/stats/test_aggregator.py
python -m pytest tests/stackunderflow/store/
python -m pytest tests/stackunderflow/test_server.py
```

### Selecting By Name
```bash
python -m pytest -k "classifier and error"
```

### With Coverage
```bash
python -m pytest --cov=stackunderflow --cov-report=html
```
