# Tests

StackUnderflow has a Python backend suite (pytest) and a TypeScript
frontend suite (the Node built-in test runner). This document covers the
layout of both and how to run them.

## Backend suite

Collecting `tests/` finds 2795 tests. The default configuration
deselects 14 `slow`-marked tests (see [Slow tests](#slow-tests) below),
leaving 2781; of those, 2 are skipped — interactive `init` flows that
need a running server.

The suite covers the adapter layer, the ETL pipeline and marts, the
SQLite store and its migrations, the stats pipeline, report rendering,
the MCP server, and the CLI and HTTP surfaces.

### Layout

`tests/stackunderflow/` mirrors the `stackunderflow` package. File
counts below are the number of `test_*.py` files in each directory, not
the number of test functions.

| Directory | Files | Covers |
|---|---:|---|
| `adapters/` | 32 | Source-adapter contract and per-provider adapters. `*_defensive.py` files feed malformed input. |
| `cli/` | 15 | CLI subcommands: `compare`, `context-budget`, `discovery`, `etl status`, `export`, `hooks`, `ingest`, `plan`, `recommend`, `risk`, `skills`, `init --install-skills`. |
| `etl/` | 8 | ETL watcher, watermark, backfill, lock, registries. |
| `etl/marts/` | 9 | The eight mart builders plus a cross-mart integration test. |
| `etl/normalize/` | 18 | Per-provider normalizers. |
| `hooks/` | 3 | Claude Code hook install, repair, and handlers. |
| `infra/` | 3 | Currency, Cursor parse cache, model aliases. |
| `infra/providers/` | 20 | Per-provider pricing and cost math. |
| `ingest/` | 6 | Log enumeration, incremental ingest, the writer, auto-reindex. |
| `integration/` | 2 | Real-data end-to-end and route performance regression. Both `slow`-marked. |
| `mcp/` | 5 | MCP server and its tools. |
| `reports/` | 8 | CLI report pipeline: aggregate, optimize, render, scope. |
| `routes/` | 30 | FastAPI endpoint behaviour, including mart-overlay routes. |
| `services/` | 23 | Stateful services: agent teams, burn projection, compare, discovery, GitHub ingest, live stats, meta-agent, mode recommender, plans, playback, risk, skills, yield. |
| `stats/` | 4 | The stats pipeline: classifier, enricher, aggregator, formatter. |
| `store/` | 15 | Connection and PRAGMAs, schema, queries, types, individual migrations, partitioning, mart queries. |
| `utils/` | 1 | Log-directory discovery. |

Eleven more files sit directly under `tests/stackunderflow/`:
`test_cli.py`, `test_cli_data_commands.py`, `test_cli_model_alias.py`,
`test_cli_yield.py`, `test_mcp.py`, `test_pricing_service.py`,
`test_public_api.py`, `test_qa_service_resolution.py`,
`test_server.py`, `test_skills.py`, `test_tag_service_intent.py`.

### Slow tests

`pyproject.toml` registers a `slow` marker and sets
`addopts = -m 'not slow'`, so a plain `pytest` run skips slow tests by
default. They are the real-data integration tests under
`tests/stackunderflow/integration/` plus a few performance checks
(`services/test_yield_perf.py`, `services/test_skill_synth.py`). Run
them explicitly:

```bash
python -m pytest -m slow
```

### Test data

- `tests/mock-data/-Users-test-dev-ai-music/` — sample Claude JSONL logs laid out like a real project directory.
- `tests/mock-data/codex-sessions/` — sample OpenAI Codex session logs.
- `tests/mock-data/pricing.json` — pricing fixture for enricher and cost tests.
- `tests/fixtures/beta_normalizers/` — one input/expected fixture directory per beta provider (codeium, continue, copilot, cursor_agent, droid, gemini, kilocode, kiro, openclaw, opencode, pi, qwen, roocode).
- `tests/baseline_results.json` and `tests/stackunderflow/baseline_phase2.json` — baseline fixtures for regression checks.
- `tests/conftest.py` — `set_home_env`, a cross-platform helper for redirecting `Path.home()` to a `tmp_path`.

### Running the backend suite

```bash
python -m pytest tests/ -q                              # default run (slow tests deselected)
python -m pytest tests/ -v                              # verbose
python -m pytest -m slow                                # only the slow suite
python -m pytest tests/stackunderflow/store/            # one subtree
python -m pytest tests/stackunderflow/stats/test_aggregator.py
python -m pytest -k "classifier and error"              # select by name
python -m pytest --cov=stackunderflow --cov-report=html # coverage
```

`pytest`, `pytest-asyncio`, and `pytest-cov` install with the `dev`
extra: `pip install -e ".[dev]"`.

## Frontend suite

The frontend tests run on the Node built-in test runner — no test
framework dependency. They live in `stackunderflow-ui/tests/services/`
and exercise the pure helpers in `src/services/`. There are nine files:

| File | Covers |
|---|---|
| `agent-teams.test.ts` | Agent-teams service |
| `burn-projection.test.ts` | Burn / budget projection |
| `etl-status.test.ts` | ETL status service |
| `filters.test.ts` | Filter state and query-string encoding |
| `format.test.ts` | `formatModelName` and formatting helpers |
| `live.test.ts` | Live-stats service |
| `meta-agent.test.ts` | Meta-agent service |
| `playback-fs.test.ts` | Playback filesystem service |
| `playback.test.ts` | Playback service |

`node --test` strips TypeScript types natively, so this needs **Node
22+**. There is no `npm test` script; run the files directly from
`stackunderflow-ui/`:

```bash
cd stackunderflow-ui
node --test tests/services/*.test.ts          # all files
node --test tests/services/format.test.ts     # one file
```

`npm run typecheck` (`tsc --noEmit`) type-checks the whole UI and is the
other check to run before pushing frontend changes.
