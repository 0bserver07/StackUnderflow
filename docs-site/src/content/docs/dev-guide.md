---
title: Development guide
description: Hack on StackUnderflow locally.
---

# StackUnderflow Development Guide

Guide for contributors. Covers architecture, dev setup, testing, and release.

## What this is

StackUnderflow is a single-process local-first app:

- **Python backend**: FastAPI server in `stackunderflow/` that ingests coding-agent session logs through a pluggable adapter layer (Claude Code today) into a local SQLite store, and exposes a JSON API on top of it.
- **React frontend**: Vite + TypeScript + Tailwind in `stackunderflow-ui/`. Built output is written to `stackunderflow/static/react/` and served by the backend.

Everything runs on the user's machine. There is no cloud component, no sharing feature, no multi-tenant deployment. Data never leaves the host.

## Prerequisites

- Python 3.10 – 3.12
- Node.js 18+ and npm (for the frontend)
- `rsync` on the system `PATH` (used by `stackunderflow backup create`; falls back to `shutil.copytree` if missing)

## Setup

```bash
git clone https://github.com/0bserver07/StackUnderflow
cd StackUnderflow

# Python (use any venv manager — conda, venv, pyenv-virtualenv)
python -m venv .venv
source .venv/bin/activate
pip install -e .
pip install -r requirements-dev.txt

# Frontend
cd stackunderflow-ui
npm install
```

`pip install -e .` installs the package in editable mode so Python changes take effect immediately. The frontend is a separate build step (see below).

## Running in development

There are two processes: the Python backend and the Vite dev server.

**Backend** (port 8081):

```bash
stackunderflow start          # also aliased as `stackunderflow init`
# or
python -m stackunderflow.server
```

**Frontend** (port 5175, proxies `/api/*` to port 8081):

```bash
cd stackunderflow-ui
npm run dev
```

Visit `http://localhost:5175` during development. The Vite proxy is defined in `stackunderflow-ui/vite.config.ts`.

For a production-shaped run, build the frontend once and visit the backend directly at `http://localhost:8081`:

```bash
cd stackunderflow-ui && npm run build   # writes to stackunderflow/static/react/
```

## Repository layout

```
StackUnderflow/
├── stackunderflow/              # Python package
│   ├── __init__.py              # Public API: list_projects()
│   ├── __version__.py
│   ├── cli.py                   # Click CLI (start/init, cfg, reports, backup)
│   ├── server.py                # FastAPI app, lifespan, router registration
│   ├── deps.py                  # Shared singletons (config, services, store_path)
│   ├── settings.py              # Descriptor-based Settings (env > file > default)
│   ├── adapters/                # Source adapters — normalise on-disk formats
│   │   ├── base.py              # SourceAdapter protocol, SessionRef, Record dataclasses
│   │   └── claude.py            # Claude Code JSONL + legacy history.jsonl
│   ├── ingest/                  # Drives adapters into the store
│   │   ├── enumerate.py         # Walk all registered adapters, yield SessionRefs
│   │   └── writer.py            # One file → one transaction → one ingest_log row
│   ├── store/                   # SQLite session store (~/.stackunderflow/store.db)
│   │   ├── db.py                # connect() + WAL pragma
│   │   ├── schema.py            # CREATE TABLE / migrations entry point
│   │   ├── migrations/          # Versioned schema migrations
│   │   ├── queries.py           # Typed read helpers (list_projects, messages_in_range, …)
│   │   └── types.py             # ProjectRow / SessionRow / MessageRow dataclasses
│   ├── stats/                   # Pure transforms over query results — no I/O
│   │   ├── classifier.py        # Tag entries (user/assistant/tool/summary/...)
│   │   ├── enricher.py          # Derived fields (costs, continuation links)
│   │   ├── aggregator.py        # Per-day, per-model, per-tool stats
│   │   └── formatter.py         # Shape for the wire
│   ├── reports/                 # CLI reporting (report / today / month / export / optimize)
│   │   ├── aggregate.py         # build_report()
│   │   ├── optimize.py          # find_waste()
│   │   ├── scope.py             # parse_period()
│   │   └── render.py            # text / JSON / CSV output
│   ├── routes/                  # FastAPI routers — one module per concern
│   │   ├── projects.py          # /api/project, /api/projects, /api/global-stats
│   │   ├── data.py              # /api/stats, /api/dashboard-data, /api/messages, /api/refresh
│   │   ├── sessions.py          # /api/jsonl-files, /api/jsonl-content
│   │   ├── search.py            # /api/search (+ reindex, stats)
│   │   ├── qa.py                # /api/qa Q&A extraction
│   │   ├── tags.py              # /api/tags session tagging
│   │   ├── bookmarks.py         # /api/bookmarks
│   │   └── misc.py              # /api/health, /api/pricing, /ollama-api proxy
│   ├── services/                # Stateful services initialised at startup
│   │   ├── search_service.py    # Full-text search over messages
│   │   ├── qa_service.py        # Question/answer extraction
│   │   ├── tag_service.py       # Session tagging
│   │   ├── bookmark_service.py  # User bookmarks
│   │   └── pricing_service.py   # Token cost lookup
│   ├── api/
│   │   └── messages.py          # Message helpers
│   ├── infra/
│   │   ├── discovery.py         # project_metadata(): list projects under ~/.claude/projects/
│   │   └── costs.py             # Pricing math
│   └── static/
│       └── react/               # Frontend build output (gitignored contents)
├── stackunderflow-ui/           # React + TypeScript + Tailwind source
│   ├── src/
│   │   ├── App.tsx
│   │   ├── main.tsx
│   │   ├── pages/
│   │   ├── components/
│   │   ├── services/
│   │   └── types/
│   ├── vite.config.ts           # Dev server :5175, proxies /api → :8081
│   └── package.json
├── tests/
│   ├── mock-data/               # Fixture JSONL + pricing.json
│   └── stackunderflow/
│       ├── adapters/            # Adapter protocol + Claude adapter
│       ├── ingest/              # enumerate, writer, incremental behaviour
│       ├── store/               # db, schema, queries, types
│       ├── stats/               # classifier, enricher, aggregator, formatter
│       ├── reports/             # aggregate, optimize, render, scope
│       ├── utils/               # log discovery
│       ├── test_cli.py
│       ├── test_cli_data_commands.py
│       ├── test_server.py
│       ├── test_pricing_service.py
│       ├── test_qa_service_resolution.py
│       └── test_tag_service_intent.py
├── docs/                         # This guide, CLI reference, etc.
├── docs-site/                    # Astro Starlight site published to GitHub Pages
├── lint.sh                       # Runs ruff + mypy
├── pyproject.toml
├── requirements.txt
└── requirements-dev.txt
```

## Data flow

The 0.3.0 rewrite replaced the in-process cache with a SQLite-backed session store. The pipeline is split into two halves: a **pre-ingest** path that normalises on-disk session data into rows, and a **post-ingest** path of pure transforms over query results.

```
~/.claude/projects/*.jsonl
       ↓
adapters/claude.py  (enumerate() → SessionRef, read() → Record stream)
       ↓
ingest/writer.py    (incremental, mtime+size gated, one txn per file)
       ↓
~/.stackunderflow/store.db   (SQLite, WAL mode)
       ↓
store/queries.py    (typed read helpers, all SQL lives here)
       ↓
stats/ {classifier → enricher → aggregator → formatter}  (pure, no I/O)
       ↓
routes/*.py         (FastAPI) — or — reports/*.py (CLI)
       ↓
React UI or CLI output
```

Key properties:

- **Adapters** are the only code that reads session files. A `SourceAdapter` (see `adapters/base.py`) implements `enumerate() -> Iterable[SessionRef]` and `read(ref, *, since_offset) -> Iterable[Record]`. The Claude adapter handles modern per-project JSONL and the pre-Jan-2026 centralised `~/.claude/history.jsonl`. New providers plug in by implementing the same protocol and calling `adapters.register()`.
- **Ingest** is incremental. `run_ingest()` compares `(mtime, size)` against the `ingest_log` table and either skips the file, tail-reads from `processed_offset`, or reparses from zero on truncation. Each file's records land in a single transaction.
- **The store** is the single source of truth at runtime. It's created lazily at `~/.stackunderflow/store.db`, opened in WAL mode (`store/db.py`), and migrated on startup via `store.schema.apply()`.
- **Stats modules** are pure functions over query results. No file reads, no HTTP, no clock calls outside the data that's passed in. Easy to test.
- **Routes and CLI reports** both read through `store.queries`; neither touches `sqlite3` directly.

`server.py` runs one ingest pass inside the FastAPI `lifespan` at boot. The CLI exposes `stackunderflow reindex` to rebuild the store from scratch.

## Shared state (`deps.py`)

Route modules import singletons from `stackunderflow.deps`:

- `config` — the `Settings` instance
- `store_path` — `~/.stackunderflow/store.db`
- `current_project_path`, `current_log_path`, `is_reindexing` — mutable server state
- `search_service`, `tag_service`, `qa_service`, `bookmark_service`, `pricing_service` — all `None` at import time, populated by the FastAPI `lifespan` handler in `server.py`

Services initialise inside `lifespan` (not at import time) because some open SQLite files. Initialising at import would trigger I/O on any tooling that imports the package (pytest collection, build, CLI startup, etc.).

## Settings

`stackunderflow/settings.py` uses a descriptor (`_Opt`) that resolves on every read:

1. Environment variable (e.g. `PORT`)
2. `~/.stackunderflow/config.json`
3. Declared default

Available keys (from `settings.py`):

| Key                       | Env                      | Default     |
| ------------------------- | ------------------------ | ----------- |
| `port`                    | `PORT`                   | `8081`      |
| `host`                    | `HOST`                   | `127.0.0.1` |
| `auto_browser`            | `AUTO_BROWSER`           | `True`      |
| `max_date_range_days`     | `MAX_DATE_RANGE_DAYS`    | `30`        |
| `messages_initial_load`   | `MESSAGES_INITIAL_LOAD`  | `500`       |
| `log_level`               | `LOG_LEVEL`              | `"INFO"`    |

Managed from the CLI:

```bash
stackunderflow cfg ls                    # show all settings with source
stackunderflow cfg set port 9000         # persist to ~/.stackunderflow/config.json
stackunderflow cfg rm port               # remove from config file
```

The hidden `config` group is still wired as an alias (`stackunderflow config show|set|unset`) for backward compatibility.

## CLI reference

Defined in `stackunderflow/cli.py`. Runs via the `stackunderflow` entry point.

| Command                                | Purpose                                                            |
| -------------------------------------- | ------------------------------------------------------------------ |
| `stackunderflow start`                 | Launch the dashboard (primary command).                            |
| `stackunderflow init`                  | Alias for `start`.                                                 |
| `stackunderflow start --fresh`         | Wipe disk cache before starting.                                   |
| `stackunderflow start --headless`      | Don't auto-open the browser.                                       |
| `stackunderflow cfg ls`                | Show settings with their source (`env`/`file`/`default`).          |
| `stackunderflow cfg set KEY VALUE`     | Persist setting to config file.                                    |
| `stackunderflow cfg rm KEY`            | Remove persisted setting.                                          |
| `stackunderflow clear-cache`           | Informational; cache is cleared on restart with `--fresh`.         |
| `stackunderflow reindex`               | Rebuild the session store from scratch.                            |
| `stackunderflow report`                | Dashboard-style summary over a date range.                         |
| `stackunderflow today` / `month`       | Shortcuts for common report periods.                               |
| `stackunderflow status`                | One-liner: today + month cost and message counts.                  |
| `stackunderflow export`                | Export aggregated data as CSV or JSON.                             |
| `stackunderflow optimize`              | Surface sessions with repeated retry loops.                        |
| `stackunderflow backup create`         | Incremental rsync-based backup of `~/.claude/` (hard-links).       |
| `stackunderflow backup list`           | List existing backups.                                             |
| `stackunderflow backup restore NAME`   | Restore `~/.claude/` from a named backup (confirms first).         |
| `stackunderflow backup auto --enable`  | Install a daily launchd job (macOS) or print cron line (Linux).    |

Full details: [cli-reference.md](cli-reference.md).

## Public Python API

```python
import stackunderflow

projects = stackunderflow.list_projects()
# [{"log_path": ..., "display_name": ..., ...}, ...]
```

Lower-level entry points:

```python
from stackunderflow.adapters import registered, register
from stackunderflow.adapters.base import SourceAdapter, SessionRef, Record
from stackunderflow.ingest import run_ingest
from stackunderflow.store import db, schema, queries
from stackunderflow.infra.discovery import project_metadata, ProjectInfo
from stackunderflow.settings import Settings
```

## Testing

```bash
python -m pytest -q                                         # full suite
python -m pytest -v                                         # verbose
python -m pytest -k history                                 # subset by name
python -m pytest tests/stackunderflow/adapters/ -q          # one subtree
python -m pytest tests/stackunderflow/store/ -q
python -m pytest tests/stackunderflow/stats/ -q
python -m pytest --cov=stackunderflow                       # coverage
```

Current suite: **340 passed, 2 skipped**. The two skips cover interactive `init` flows that require a running server.

Mock data: `tests/mock-data/-Users-test-dev-ai-music/` plus `tests/mock-data/pricing.json`.

See [tests.md](tests.md) for layout and conventions.

## Lint and type-check

```bash
./lint.sh                         # runs the block below

ruff check stackunderflow/        # lint
ruff format stackunderflow/       # format
mypy stackunderflow/ --ignore-missing-imports
```

`pyproject.toml` configures:
- Line length 120
- Ruff target Python 3.11
- Ruff replaces Black for formatting

## Frontend (`stackunderflow-ui/`)

Stack: React 18, TypeScript, Tailwind, Vite, react-router-dom, @tanstack/react-query, recharts, react-markdown, react-syntax-highlighter.

```bash
cd stackunderflow-ui
npm run dev         # Vite dev server on :5175, proxies /api → :8081
npm run build       # tsc + vite build, outputs to ../stackunderflow/static/react/
npm run typecheck   # tsc --noEmit
```

The backend serves the built React app from `stackunderflow/static/react/index.html` with a catch-all for client-side routing (`/project/{path:path}`).

The Vite config also proxies `/ollama-api/*` to `http://localhost:11434/api/*` so the UI can talk to a local Ollama instance if the user has one running. Ollama is optional; the proxy silently returns 502 when it's not reachable.

## GitHub Actions

Workflows live in `.github/workflows/`:

- `test.yml` — pytest on Python 3.10/3.11/3.12. Runs on every push and PR.
- `lint.yml` — ruff + mypy.
- `build.yml` — `python -m build` + `pip install dist/*.whl` on Ubuntu, macOS, Windows × Python 3.10, 3.12.
- `publish.yml` — publishes to PyPI on GitHub release or manual dispatch.
- `docs.yml` — builds and deploys `docs-site/` to GitHub Pages.

## Release

1. Bump `stackunderflow/__version__.py` (semver: MAJOR.MINOR.PATCH).
2. Update `CHANGELOG.md`.
3. Run locally:
   ```bash
   python -m pytest -q
   ./lint.sh
   rm -rf dist/ build/ *.egg-info
   python -m build
   twine check dist/*
   ```
4. Optional local install test:
   ```bash
   pip install dist/stackunderflow-*.whl
   stackunderflow --version
   ```
5. Tag and push:
   ```bash
   git tag -a v0.x.y -m "Release v0.x.y"
   git push origin main
   git push origin v0.x.y
   ```
6. Create a GitHub release from the tag. `publish.yml` uploads to PyPI.

Once on PyPI, `uvx stackunderflow init` works immediately; no separate publish step for `uv`.

## Debugging

- Server won't start: `lsof -i :8081` to check the port.
- Stale Python bytecode after a refactor: `find . -name __pycache__ -type d -exec rm -rf {} +`.
- Verbose logs: `LOG_LEVEL=DEBUG stackunderflow start`.
- Store looks wrong / out of date: `stackunderflow reindex` rebuilds `~/.stackunderflow/store.db` from scratch. `stackunderflow start --fresh` also wipes any residual JSON cache at `~/.stackunderflow/cache/`.
- Frontend not reflecting API changes: confirm the Vite proxy target matches the backend port (`stackunderflow-ui/vite.config.ts` hardcodes `:8081`).

## Contributing

- Add tests for new behavior.
- Keep functions small and type-hinted.
- Conventional commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`).
- Run `./lint.sh` and `python -m pytest -q` before pushing.

## Other docs

- [cli-reference.md](cli-reference.md) — full CLI options and examples.
- [claude-logs-structure-and-processing.md](claude-logs-structure-and-processing.md) — JSONL format details.
- [memory-and-latency-optimization.md](memory-and-latency-optimization.md) — store / latency notes.
- [tests.md](tests.md) — test suite walk-through.
- [codex-adapter-spec.md](codex-adapter-spec.md) — design sketch for optional OpenAI Codex ingestion.
