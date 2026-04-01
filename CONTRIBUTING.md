# Contributing to StackUnderflow

## Getting Started

```bash
git clone https://github.com/0bserver07/StackUnderflow.git
cd StackUnderflow
pip install -e .
pip install -r requirements-dev.txt
```

To work on the React frontend:

```bash
cd stackunderflow-ui
npm install
npm run dev    # dev server with hot reload
npm run build  # production build → stackunderflow/static/react/
```

## Running Tests

```bash
python -m pytest tests/ -v
```

## Linting

```bash
bash lint.sh
```

## Pull Requests

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Add tests if you added functionality
4. Run the test suite and linter
5. Open a PR with a clear description of what changed and why

## Adding a New Source Adapter

StackUnderflow is designed to support multiple AI coding agents. See [docs/codex-adapter-spec.md](docs/codex-adapter-spec.md) for the Codex adapter specification — it documents the adapter interface and how new sources plug into the pipeline.

## Project Structure

- `stackunderflow/pipeline/` — ETL core (reader → dedup → classify → enrich → aggregate → format)
- `stackunderflow/routes/` — FastAPI route modules
- `stackunderflow/services/` — search, Q&A, tags, bookmarks, etc.
- `stackunderflow/infra/` — cache, discovery, costs
- `stackunderflow-ui/` — React frontend

## Code Style

- Python: follow ruff defaults, 120 char line length
- TypeScript: follow the existing patterns in `stackunderflow-ui/`
- No docstrings needed for obvious functions
- Comments only where the logic isn't self-evident
