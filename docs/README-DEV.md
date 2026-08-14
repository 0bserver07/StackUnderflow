# staxtrace Development Guide

Contributor guide: architecture, local setup, testing, and release.

## What this is

staxtrace is a single-process, local-first app:

- **Python backend**: a FastAPI server in `stackunderflow/` that ingests coding-agent session logs through a pluggable adapter layer into a local SQLite store and exposes a JSON API over it. All twenty adapters are enabled by default — the registry self-discovers adapter modules, so there's no opt-in flag.
- **React frontend**: Vite + TypeScript + Tailwind in `stackunderflow-ui/`. The build output is written to `stackunderflow/static/react/` and served by the backend.

Everything runs on the user's machine; data never leaves the host.

## Prerequisites

- Python 3.11 or 3.12 (`pyproject.toml` sets `requires-python = ">=3.11"`; CI runs 3.11 and 3.12).
- Node.js 20+ for the frontend build. The frontend test suite needs Node 22+.
- `rsync` on `PATH` for `stax backup create` — it falls back to `shutil.copytree` if missing.

## Setup

```bash
git clone https://github.com/0bserver07/staxtrace
cd staxtrace

# Python — use any virtualenv manager (venv, conda, pyenv-virtualenv)
python -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"

# Frontend
cd stackunderflow-ui
npm install
```

`pip install -e ".[dev]"` installs the package in editable mode with the
test and lint tooling (pytest, ruff, mypy, build, twine). Dependencies
are declared in `pyproject.toml`; the `dev` extra is the dependency
group CI installs. Editable mode means Python changes take effect
without reinstalling.

Semantic search (`memory ask`, `search-past-decisions
--use-embeddings`) uses vector embeddings served by Ollama — a
configured endpoint (`STACKUNDERFLOW_OLLAMA_URL` +
`STACKUNDERFLOW_OLLAMA_API_KEY`) or a local daemon at
`localhost:11434`. There is no Python dependency to install; without a
reachable Ollama the feature degrades to keyword/substring search, so
most contributors don't need one.

## Running in development

There are two processes: the Python backend and the Vite dev server.

**Backend** (port 8081):

```bash
stax start          # init also starts the dashboard
# or
python -m stackunderflow.server
```

**Frontend** (port 5175, proxies `/api/*` to port 8081):

```bash
cd stackunderflow-ui
npm run dev
```

Visit `http://localhost:5175` during development. The Vite proxy is
defined in `stackunderflow-ui/vite.config.ts`.

For a production-shaped run, build the frontend once and visit the
backend directly at `http://localhost:8081`:

```bash
cd stackunderflow-ui && npm run build   # writes to stackunderflow/static/react/
```

## Repository layout

```
staxtrace/
├── stackunderflow/              # Python package
│   ├── __init__.py              # Public API re-export (list_projects, list_sessions, process)
│   ├── __version__.py
│   ├── cli.py                   # Click CLI — every `stackunderflow` subcommand
│   ├── server.py                # FastAPI app, lifespan, router registration
│   ├── deps.py                  # Shared singletons (config, store_path, services, watcher handles)
│   ├── settings.py              # Descriptor-based Settings (env > file > default)
│   ├── api/                     # Public library API (__init__.py) + HTTP message helpers
│   ├── adapters/                # Source adapters, one per coding agent, + the SourceAdapter protocol
│   ├── ingest/                  # Drives adapters into the store (enumerate, writer, run_ingest)
│   ├── stats/                   # Pure transforms: classifier → enricher → aggregator → formatter
│   ├── store/                   # SQLite store: db, schema + migrations/, queries, mart_queries, types
│   ├── etl/                     # Filesystem watcher, normalizers, mart builders, backfill
│   ├── reports/                 # CLI reporting (aggregate, optimize, scope, render, export)
│   ├── routes/                  # FastAPI routers — one module per concern (29 modules)
│   ├── services/                # Stateful services initialised at startup (search, qa, tags, …)
│   ├── hooks/                   # Claude Code hook install / repair / handlers
│   ├── infra/                   # Discovery, cost/pricing math, currency, caches
│   ├── cli_helpers/             # Shared CLI helpers (ingest-on-read)
│   ├── skills/                  # Shipped Claude Code SKILL.md files
│   └── static/react/            # Frontend build output (gitignored contents)
├── stackunderflow-ui/           # React + TypeScript + Tailwind source
│   ├── src/                     # App, pages, components, services, types
│   ├── tests/services/          # Frontend tests (Node test runner)
│   ├── vite.config.ts           # Dev server :5175, proxies /api → :8081
│   └── package.json
├── tests/                       # pytest suite — mirrors the package layout
│   ├── mock-data/               # Fixture JSONL + pricing.json
│   ├── fixtures/                # Beta-normalizer input/expected fixtures
│   └── stackunderflow/          # Tests, one subdirectory per package area
├── docs/                        # This guide, CLI/API reference, specs
├── docs-site/                   # Astro Starlight site published to GitHub Pages
├── lint.sh                      # ruff check + ruff format check + mypy
├── flake.nix                    # Nix package + dev shell
└── pyproject.toml               # Package metadata, dependencies, tool config
```

## Data flow

The pipeline has two halves: a **pre-ingest** path that normalises
on-disk session data into rows, and a **post-ingest** path of pure
transforms over query results.

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

Alongside this read path, the ETL layer (`etl/`) maintains precomputed
mart tables. A filesystem watcher (`etl/watcher.py`), started at server
boot, detects new session data and refreshes the store and marts
incrementally. Hot routes read marts when a project is materialised and
fall back to the `stats/` pipeline otherwise.

Key properties:

- **Adapters** are the only code that reads session files. A `SourceAdapter` (see `adapters/base.py`) implements `enumerate() -> Iterable[SessionRef]` and `read(ref, *, since_offset) -> Iterable[Record]`. The Claude adapter handles modern per-project JSONL and the pre-Jan-2026 centralised `~/.claude/history.jsonl`. New providers plug in by implementing the protocol and calling `adapters.register()`.
- **Ingest** is incremental. `run_ingest()` compares `(mtime, size)` against the `ingest_log` table and either skips the file, tail-reads from `processed_offset`, or reparses from zero on truncation. Each file's records land in a single transaction.
- **The store** is the single source of truth at runtime. It is created lazily at `~/.stackunderflow/store.db`, opened in WAL mode (`store/db.py`), and migrated on startup via `store.schema.apply()`.
- **Stats modules** are pure functions over query results — no file reads, no HTTP, no clock calls outside the data passed in. Easy to test.
- **Routes and CLI reports** both read through `store.queries` (and `store.mart_queries`); neither touches `sqlite3` directly.

`server.py` runs an ingest pass in a background thread at boot, then
starts the filesystem watcher. `stax reindex` re-applies
migrations and runs a full ingest pass on demand.

## Shared state (`deps.py`)

Route modules import singletons from `stackunderflow.deps`:

- `config` — the `Settings` instance.
- `store_path` — `~/.stackunderflow/store.db`.
- `current_project_path`, `current_log_path`, `is_reindexing` — mutable server state.
- `search_service`, `tag_service`, `qa_service`, `bookmark_service`, `pricing_service` — `None` at import time, populated by the FastAPI `lifespan` handler in `server.py`.
- `watcher_handle`, `watcher_lock_handle` — the ETL filesystem watcher and its singleton lock, also populated by `lifespan` (and left `None` for CLI subcommands that don't start the server).

Services initialise inside `lifespan`, not at import time, because some
open SQLite files. Initialising at import would trigger I/O on any
tooling that imports the package — pytest collection, builds, CLI
startup.

## Settings

`stackunderflow/settings.py` uses a descriptor (`_Opt`) that resolves on
every read:

1. Environment variable (e.g. `PORT`).
2. `~/.stackunderflow/config.json`.
3. Declared default.

| Key | Env | Default |
| --- | --- | --- |
| `port` | `PORT` | `8081` |
| `host` | `HOST` | `127.0.0.1` |
| `auto_browser` | `AUTO_BROWSER` | `True` |
| `max_date_range_days` | `MAX_DATE_RANGE_DAYS` | `30` |
| `messages_initial_load` | `MESSAGES_INITIAL_LOAD` | `500` |
| `log_level` | `LOG_LEVEL` | `INFO` |
| `auto_reindex_on_ingest` | `AUTO_REINDEX_ON_INGEST` | `True` |
| `currency` | `STACKUNDERFLOW_CURRENCY` | `USD` |
| `discovery_budget_tokens` | `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` | `2000` |
| `discovery_rank_weights` | `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS` | `0.5,0.2,0.3` |
| `model_aliases` | — | `{}` |
| `plan_name` | — | `None` |
| `plan_monthly_usd` | — | `None` |
| `plan_reset_day` | — | `1` |
| `plan_alert_thresholds` | — | `[50, 75, 90]` |

Settings with no env var are file-only — a JSON dict or list is awkward
to express in a shell variable, so they are managed through the CLI.

```bash
stax cfg ls                    # show all settings with source
stax cfg set port 9000         # persist to ~/.stackunderflow/config.json
stax cfg rm port               # remove from config file
```

The `model_aliases` map has its own `stax cfg model-alias`
subcommands, and the `plan_*` keys are managed by `stax plan`.
The hidden `config` group stays wired as an alias
(`stax config show|set|unset`) for backward compatibility.

## CLI reference

Defined in `stackunderflow/cli.py`, run via the `stackunderflow` entry
point.

| Command | Purpose |
| --- | --- |
| `stax start` | Launch the dashboard. `--fresh` wipes the disk cache first; `--headless` skips opening the browser. |
| `stax init` | Start the dashboard (alias for `start`); `--install-skills` also installs the shipped Claude Code skills. |
| `stax reindex` | Apply pending migrations and run a full ingest pass. |
| `stax cfg ls\|set\|rm` | View or change persistent settings. |
| `stax plan show\|set\|reset` | Manage the monthly plan budget. |
| `stax report` / `today` / `month` | Cost and activity summaries over a date range. |
| `stax status` | One-line today + month cost and message counts. |
| `stax export` | Export aggregated data as CSV or JSON. |
| `stax optimize` | Surface sessions with repeated retry loops. |
| `stax backup create\|verify\|list\|restore\|auto` | Snapshot and restore `~/.claude/` — see [backup.md](backup.md). |
| `stax clear-cache` | Clear the Cursor parse cache; the in-memory cache clears on restart. |

The CLI has more command groups — `memory` (the agent-facing query
namespace), `etl`, `hooks`, `guide`, `skills`, `discovery`,
`recommend`, `risk`, `ingest`, `pricing`, `analyze`, plus
`context-budget`, `compare`, and `yield`. Run `stax --help`
or see [cli-reference.md](cli-reference.md) for the full surface.

## Public Python API

Reads from the local SQLite store at `~/.stackunderflow/store.db` —
every provider, every project, in one query:

```python
import stackunderflow

projects = stackunderflow.list_projects()
# [{"slug": ..., "provider": ..., "display_name": ..., "path": ...,
#   "first_seen": ..., "last_modified": ...}, ...]

# Optional: filter to one provider.
claude_only = stackunderflow.list_projects(provider="claude")

# Pipeline-formatted messages + stats for one project.
messages, stats = stackunderflow.process(projects[0]["slug"])

# Sessions for a project (id + first/last timestamp + message count).
sessions = stackunderflow.list_sessions(projects[0]["slug"])
```

An empty store (no ingest yet) makes `list_projects()` return `[]`. An
unknown slug makes `process()` and `list_sessions()` raise `KeyError`.

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
python -m pytest tests/ -q                     # default run (slow tests deselected)
python -m pytest tests/ -v                     # verbose
python -m pytest -m slow                       # the slow integration / perf suite
python -m pytest -k history                    # select by name
python -m pytest tests/stackunderflow/store/   # one subtree
python -m pytest --cov=stackunderflow          # coverage
```

Collecting `tests/` finds 3321 tests; the default configuration
deselects the 14 `slow`-marked ones and runs the rest. The frontend
suite runs separately on the Node test runner. See [tests.md](tests.md)
for the layout of both.

## Lint and type-check

```bash
./lint.sh                         # ruff check --fix, ruff format check, mypy

ruff check stackunderflow/        # lint
ruff format stackunderflow/       # format
mypy stackunderflow/ --ignore-missing-imports
```

`pyproject.toml` sets a line length of 120 and a Ruff target of Python
3.11. Ruff handles both linting and formatting; there is no separate
Black step.

## Cost-tab analytics

The Cost tab (`stackunderflow-ui/src/pages/cost/`) renders attribution
views — top sessions, expensive commands, tool ranking, token
composition, cache ROI, outliers, retry signals, week-over-week trends,
and an error-cost estimate. It pulls from a dedicated set of endpoints
split off `/api/dashboard-data` so the initial dashboard load stays
small:

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/cost-data` | The nine analytics sections (`session_costs`, `command_costs`, `tool_costs`, `token_composition`, `outliers`, `retry_signals`, `session_efficiency`, `error_cost`, `trends`). Source: `routes/cost.py`. |
| GET | `/api/commands` | Paginated per-command list; `?offset=&limit=&sort=cost\|tokens\|tools\|steps\|time&order=desc\|asc`. Source: `routes/commands.py`. |
| GET | `/api/interaction/{interaction_id}` | One enriched interaction (command + responses + tool_results) for deep links from the Messages tab. Source: `routes/cost.py`. |
| GET | `/api/sessions/compare` | Side-by-side diff of two sessions: `?a=&b=` returns `{a, b, diff}` over cost / tokens / commands / errors / duration. Source: `routes/sessions.py`. |

Frontend conventions specific to the Cost tab:

- Filter state (range / session / tool) is encoded in the URL query string so views are shareable and survive a refresh.
- Deep-linked detail pages (a single session or interaction) render a breadcrumb and back button so users can step out of the drill-down.
- The header carries a light/dark theme toggle. The choice persists in `localStorage`, falling back to the user's `prefers-color-scheme`.

Full request/response shapes live in [api-reference.md](api-reference.md).

## Frontend (`stackunderflow-ui/`)

Stack: React 18, TypeScript, Tailwind, and Vite 6, with
react-router-dom, @tanstack/react-query, recharts, react-markdown, and
react-syntax-highlighter.

```bash
cd stackunderflow-ui
npm run dev          # Vite dev server on :5175, proxies /api → :8081
npm run build        # tsc -b && vite build → ../stackunderflow/static/react/
npm run preview      # serve the production build locally
npm run typecheck    # tsc --noEmit
node --test tests/services/*.test.ts   # frontend tests (needs Node 22+)
```

The backend serves the built React app from
`stackunderflow/static/react/index.html`, with a catch-all that returns
`index.html` for client-side routes.

The Vite config also proxies `/ollama-api/*` to
`http://localhost:11434/api/*` so the UI can talk to a local Ollama
instance during development, mirroring the backend's own
`/ollama-api/{path}` proxy in `stackunderflow/routes/misc.py`. Ollama
is optional, and both proxies return 502 when it is not reachable.

## Nix

`flake.nix` at the repo root packages both the Python backend and the
Vite-built frontend so the project builds and runs without pip or npm:

```bash
nix develop          # dev shell: Python, Node.js, ruff, mypy, pytest, rsync
nix build            # build the wheel + bundled frontend; result at ./result
nix run . -- start   # run the built `stackunderflow` CLI (here, `start`)
```

The flake pins `npmDepsHash` against
`stackunderflow-ui/package-lock.json`; update it whenever the lockfile
changes — the first build after a change prints the expected hash.

## GitHub Actions

Workflows live in `.github/workflows/`:

- `test.yml` — pytest with coverage on Ubuntu, Python 3.11 and 3.12. Coverage must stay above 60%.
- `lint.yml` — ruff (errors), a ruff-format check, and mypy.
- `build.yml` — builds the React UI, then runs `python -m build` and a wheel-install CLI smoke test on Ubuntu, macOS, and Windows × Python 3.11 and 3.12.
- `publish.yml` — on a published GitHub release, builds and uploads to PyPI.
- `docs.yml` — builds and deploys `docs-site/` to GitHub Pages.

All run on push and PR to `main`, except `publish.yml` (release only)
and `docs.yml` (also `workflow_dispatch`).

## Release

1. Bump `stackunderflow/__version__.py` (semver: MAJOR.MINOR.PATCH).
2. Update `CHANGELOG.md`.
3. Run locally:
   ```bash
   python -m pytest tests/ -q
   ./lint.sh
   rm -rf dist/ build/ *.egg-info
   python -m build
   twine check dist/*
   ```
4. Optional local install test:
   ```bash
   pip install dist/stackunderflow-*.whl
   stax --version
   ```
5. Tag and push:
   ```bash
   git tag -a v0.x.y -m "Release v0.x.y"
   git push origin main
   git push origin v0.x.y
   ```
6. Create a GitHub release from the tag. `publish.yml` uploads to PyPI.

Once on PyPI, `uvx stax init` works immediately.

## Debugging

- Server won't start: `lsof -i :8081` to check the port.
- Stale Python bytecode after a refactor: `find . -name __pycache__ -type d -exec rm -rf {} +`.
- Verbose logs: `LOG_LEVEL=DEBUG stax start`.
- Store looks wrong or out of date: `stax reindex` re-applies migrations and re-runs ingest against `~/.stackunderflow/store.db`. For a clean rebuild, delete `~/.stackunderflow/store.db` first — it is derived data and the next ingest recreates it. `stax start --fresh` separately wipes the JSON cache at `~/.stackunderflow/cache/`.
- Frontend not reflecting API changes: confirm the Vite proxy target matches the backend port (`stackunderflow-ui/vite.config.ts` hardcodes `:8081`).

## Contributing

- Add tests for new behaviour.
- Keep functions small and type-hinted.
- Conventional commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`).
- Run `./lint.sh` and `python -m pytest tests/ -q` before pushing.

## Other docs

- [cli-reference.md](cli-reference.md) — full CLI options and examples.
- [api-reference.md](api-reference.md) — HTTP request/response shapes.
- [claude-logs-structure-and-processing.md](claude-logs-structure-and-processing.md) — JSONL format details.
- [memory-and-latency-optimization.md](memory-and-latency-optimization.md) — store and latency notes.
- [tests.md](tests.md) — test suite walk-through.
- [backup.md](backup.md) — backup and restore.
