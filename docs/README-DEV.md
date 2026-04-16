# StackUnderflow Development Guide

Guide for contributors. Covers architecture, dev setup, testing, and release.

## What this is

StackUnderflow is a single-process local-first app:

- **Python backend**: FastAPI server in `stackunderflow/` that reads Claude Code JSONL logs from `~/.claude/projects/`, processes them through a pipeline, and exposes a JSON API.
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
│   ├── __init__.py              # Public API: process(), list_projects()
│   ├── __version__.py
│   ├── cli.py                   # Click CLI (start/init, cfg, clear-cache, backup)
│   ├── server.py                # FastAPI app, lifespan, router registration
│   ├── deps.py                  # Shared singletons (cache, config, services)
│   ├── settings.py              # Descriptor-based Settings (env > file > default)
│   ├── api/
│   │   └── messages.py          # Message helpers
│   ├── pipeline/                # ETL: reader → dedup → classifier → enricher → aggregator → formatter
│   │   ├── __init__.py          # process(log_dir) entry point
│   │   ├── reader.py            # JSONL discovery + parsing (recursive)
│   │   ├── history_reader.py    # Legacy ~/.claude/history.jsonl support (pre-Jan-2026)
│   │   ├── dedup.py             # Collapse duplicate entries across resumed sessions
│   │   ├── classifier.py        # Tag entries (user/assistant/tool/summary/...)
│   │   ├── enricher.py          # Attach derived fields (costs, continuations)
│   │   ├── aggregator.py        # Statistics (tokens, models, per-day, per-tool)
│   │   ├── formatter.py         # Shape message dicts for the API
│   │   └── cross_project.py     # Background aggregation across all projects
│   ├── routes/                  # FastAPI routers — one module per concern
│   │   ├── projects.py          # /api/project, /api/projects, /api/global-stats
│   │   ├── data.py              # /api/stats, /api/dashboard-data, /api/messages, /api/refresh
│   │   ├── sessions.py          # /api/jsonl-files, /api/jsonl-content
│   │   ├── search.py            # /api/search (+ reindex, stats)
│   │   ├── qa.py                # /api/qa Q&A extraction
│   │   ├── tags.py              # /api/tags session tagging
│   │   ├── bookmarks.py         # /api/bookmarks
│   │   └── misc.py              # /api/health, /api/pricing, /ollama-api proxy
│   ├── services/                # Stateful services initialized at startup
│   │   ├── search_service.py    # Full-text search over messages
│   │   ├── qa_service.py        # Question/answer extraction
│   │   ├── tag_service.py       # Session tagging
│   │   ├── bookmark_service.py  # User bookmarks
│   │   └── pricing_service.py   # Token cost lookup
│   ├── infra/
│   │   ├── cache.py             # TieredCache (hot LRU + cold disk JSON)
│   │   ├── discovery.py         # Find projects under ~/.claude/projects/
│   │   ├── costs.py             # Pricing math
│   │   └── preloader.py         # Warm cache on startup
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
│   └── stackunderflow/
│       ├── core/                # Pipeline, processor, history_reader, stats
│       ├── utils/                # Cache, log discovery
│       ├── test_cli.py
│       ├── test_server.py
│       ├── test_performance.py
│       └── test_processor_*.py
├── docs/                         # This guide, CLI reference, etc.
├── lint.sh                       # Runs ruff + mypy
├── pyproject.toml
├── requirements.txt
└── requirements-dev.txt
```

## How sessions are processed

Call site: `stackunderflow.pipeline.process(log_dir) -> (messages, statistics)`.

1. **reader** (`reader.py`). Walks `log_dir` recursively, reads every `*.jsonl`, and yields `RawEntry(payload, session_id, origin)`. Handles three file shapes:
   - `<project>/<uuid>.jsonl` — main sessions
   - `<project>/agent-<hash>.jsonl` — top-level sub-agents
   - `<project>/<uuid>/subagents/agent-<hash>.jsonl` — nested sub-agents
   - Detects session continuations (resumed sessions) across files.

2. **history_reader** (`history_reader.py`). Fallback for legacy projects. Before January 2026, Claude Code wrote only user prompts to a centralized `~/.claude/history.jsonl` instead of per-project JSONL conversation logs. When `reader.scan()` finds no JSONL under a project directory, it falls back to this module, which:
   - Parses `~/.claude/history.jsonl` once and caches the result by project slug.
   - Groups entries without a `sessionId` into synthetic sessions using a 2-hour gap heuristic.
   - Converts each entry into the same `RawEntry` shape the rest of the pipeline expects.
   - Legacy entries contain prompt text and timestamp only — no token counts, model, or assistant response.

3. **dedup** (`dedup.py`). Collapses duplicate entries that appear when a session is resumed (Claude Code rewrites prefix lines).

4. **classifier** (`classifier.py`). Tags each merged entry by role/kind (user, assistant, tool_use, tool_result, summary, system).

5. **enricher** (`enricher.py`). Attaches derived fields: token cost from pricing data, session continuation links, normalized model names.

6. **aggregator** (`aggregator.py`). Walks the enriched dataset once and returns the stats dict: per-day, per-model, per-tool, token totals, dollar totals, first/last message timestamps.

7. **formatter** (`formatter.py`). Converts entries into the wire format the frontend expects and applies the `limit` argument.

Results are cached by `TieredCache` (hot in-memory LRU + cold disk JSON under `~/.stackunderflow/cache/`). The hot cache is configurable via `cache_max_projects` and `cache_max_mb_per_project`. At startup, `infra/preloader.warm()` pre-processes the `cache_warm_on_startup` most-recent projects so the first page load is instant.

## Shared state (`deps.py`)

Route modules import singletons from `stackunderflow.deps`:

- `cache` — the `TieredCache` instance
- `config` — the `Settings` instance
- `current_project_path`, `current_log_path` — the currently selected project (mutated by `POST /api/project`)
- `search_service`, `tag_service`, `qa_service`, `bookmark_service`, `pricing_service` — all `None` at import time, populated by the FastAPI `lifespan` handler in `server.py`

Services initialize inside `lifespan` (not at import time) because some of them open SQLite files. Initializing at import would trigger I/O on any tooling that imports the package (pytest collection, build, CLI startup, etc.).

## Settings

`stackunderflow/settings.py` uses a descriptor (`_Opt`) that resolves on every read:

1. Environment variable (e.g. `PORT`)
2. `~/.stackunderflow/config.json`
3. Declared default

Available keys:

| Key                            | Env                             | Default       |
| ------------------------------ | ------------------------------- | ------------- |
| `port`                         | `PORT`                          | `8081`        |
| `host`                         | `HOST`                          | `127.0.0.1`   |
| `cache_max_projects`           | `CACHE_MAX_PROJECTS`            | `5`           |
| `cache_max_mb_per_project`     | `CACHE_MAX_MB_PER_PROJECT`      | `500`         |
| `auto_browser`                 | `AUTO_BROWSER`                  | `True`        |
| `max_date_range_days`          | `MAX_DATE_RANGE_DAYS`           | `30`          |
| `messages_initial_load`        | `MESSAGES_INITIAL_LOAD`         | `500`         |
| `enable_background_processing` | `ENABLE_BACKGROUND_PROCESSING`  | `True`        |
| `cache_warm_on_startup`        | `CACHE_WARM_ON_STARTUP`         | `3`           |
| `log_level`                    | `LOG_LEVEL`                     | `"INFO"`      |

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
| `stackunderflow backup create`         | Incremental rsync-based backup of `~/.claude/` (hard-links).       |
| `stackunderflow backup list`           | List existing backups.                                             |
| `stackunderflow backup restore NAME`   | Restore `~/.claude/` from a named backup (confirms first).         |
| `stackunderflow backup auto --enable`  | Install a daily launchd job (macOS) or print cron line (Linux).    |

Full details: [cli-reference.md](cli-reference.md).

## Public Python API

```python
import stackunderflow

projects = stackunderflow.list_projects()
messages, stats = stackunderflow.process(projects[0]["log_path"])
```

- `list_projects()` returns `[{"log_path": ..., "display_name": ..., ...}]` for every project under `~/.claude/projects/` (plus legacy projects discovered via `history.jsonl`).
- `process(log_dir, *, limit=None, tz_offset=0)` runs the full pipeline.

Lower-level entry points:

```python
from stackunderflow.pipeline import reader, dedup, classifier, enricher, aggregator, formatter
from stackunderflow.infra.discovery import project_metadata, ProjectInfo
from stackunderflow.infra.cache import TieredCache
from stackunderflow.settings import Settings
```

## Testing

```bash
pytest                                  # full suite
pytest -q                               # quiet
pytest -v                               # verbose
pytest -k history                       # subset by name
pytest tests/stackunderflow/core/test_processor.py
pytest --cov=stackunderflow             # coverage
pytest tests/stackunderflow/test_performance.py    # perf (local only)
```

Current suite: **142 passed, 2 skipped**. The two skips cover interactive `init` flows that require a running server.

Mock data: `tests/mock-data/-Users-test-dev-ai-music/`.

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

## Release

1. Bump `stackunderflow/__version__.py` (semver: MAJOR.MINOR.PATCH).
2. Update `CHANGELOG.md`.
3. Run locally:
   ```bash
   pytest
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
- Cache looks wrong: `stackunderflow start --fresh` wipes `~/.stackunderflow/cache/` on boot.
- Frontend not reflecting API changes: confirm the Vite proxy target matches the backend port (`stackunderflow-ui/vite.config.ts` hardcodes `:8081`).

## Contributing

- Add tests for new behavior.
- Keep functions small and type-hinted.
- Conventional commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`).
- Run `./lint.sh` and `pytest` before pushing.

## Other docs

- [cli-reference.md](cli-reference.md) — full CLI options and examples.
- [claude-logs-structure-and-processing.md](claude-logs-structure-and-processing.md) — JSONL format details.
- [memory-and-latency-optimization.md](memory-and-latency-optimization.md) — cache architecture.
- [tests.md](tests.md) — test suite walk-through.
- [codex-adapter-spec.md](codex-adapter-spec.md) — design sketch for optional OpenAI Codex ingestion.
