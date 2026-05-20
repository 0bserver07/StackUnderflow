# Contributing to StackUnderflow

## Getting Started

```bash
git clone https://github.com/0bserver07/StackUnderflow.git
cd StackUnderflow
pip install -e ".[dev]"
```

The `[dev]` extra pulls in pytest, ruff, mypy, and the build tooling. To work
on the React frontend:

```bash
cd stackunderflow-ui
npm install
npm run dev    # dev server with hot reload
npm run build  # production build → stackunderflow/static/react/
```

## Running Tests

```bash
pytest tests/ -q                                       # fast suite; slow tests are deselected by default
pytest -m slow tests/stackunderflow/integration -q     # slow e2e + perf-regression suite
```

Frontend tests use the Node built-in test runner (no vitest dependency):

```bash
cd stackunderflow-ui && node --test tests/services/*.test.ts
```

## Linting

```bash
bash lint.sh        # ruff check + ruff format --check + mypy
```

## Pull Requests

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Add tests if you added functionality
4. Run the test suite and linter
5. Open a PR with a clear description of what changed and why

## Adding a New Source Adapter

See [docs/adapters.md](docs/adapters.md) for the contract (`SessionRef`, `Record`, the `SourceAdapter` protocol), the `enumerate()` / `read()` semantics, the `ProviderPricer` extension point, and the beta-flag wiring pattern. Working references: `stackunderflow/adapters/claude.py` (file-mode), `stackunderflow/adapters/cursor.py` (database-mode).

## Project Structure

- `stackunderflow/adapters/` — per-provider source-file parsers
- `stackunderflow/etl/` — the normalize → marts pipeline, the filesystem watcher, backfill
- `stackunderflow/store/` — SQLite store, schema, migrations, query helpers
- `stackunderflow/routes/` — FastAPI route modules
- `stackunderflow/services/` — search, Q&A, tags, bookmarks, discovery, recommenders
- `stackunderflow/infra/` — cache, costs, currency, provider pricers
- `stackunderflow/mcp/` — the MCP server
- `stackunderflow-ui/` — React frontend

`docs/HANDOFF.md` has the full package map and the design rationale.

## Code Style

- Python: follow ruff defaults, 120 char line length
- TypeScript: follow the existing patterns in `stackunderflow-ui/`
- No docstrings needed for obvious functions
- Comments only where the logic isn't self-evident
