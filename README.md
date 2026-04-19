# StackUnderflow

A local-first knowledge base for your AI coding sessions. Browse, search, and analyse conversations from Claude Code.

[Quickstart](#quickstart) | [Features](#features) | [Configuration](#configuration) | [Architecture](#architecture) | [Contributing](#contributing)

![StackUnderflow Dashboard](assets/dashboard.png)

## Quickstart

**Requirements:** Python 3.10+, Node 18+, and an existing `~/.claude/` directory from using Claude Code.

```bash
git clone https://github.com/0bserver07/StackUnderflow.git
cd StackUnderflow

# 1. Build the React UI (one-time)
cd stackunderflow-ui && npm install && npm run build && cd ..

# 2. Install the Python package
pip install -e .

# 3. Launch the dashboard
stackunderflow init
```

Your browser opens to `http://localhost:8081` with every project under `~/.claude/projects/` indexed and ready to browse.

**Common knobs:**

```bash
stackunderflow init --no-browser      # don't auto-open the browser
stackunderflow cfg set port 8090      # change the port
stackunderflow backup create          # snapshot ~/.claude/ before risky changes
stackunderflow --help                 # everything else
```

If port 8081 is taken: `stackunderflow cfg set port <free-port>` then re-run `init`.

> PyPI release coming soon. For now, install from source.

## Features

- **Analytics dashboard** — token usage, cost breakdown, model distribution, error patterns, hourly activity
- **Session viewer** — browse individual JSONL session files with conversation replay, sub-agent grouping, per-session cost
- **Full-text search** — across all sessions, with filters for date, model, and role
- **Q&A pair detection** — heuristic extraction of question-answer pairs based on text patterns and follow-up cues
- **Auto-tagging** — tags sessions by language, framework, topic, and intent (`build`, `fix`, `explore`, `refactor`, `test`, `ops`) using keyword and pattern matching
- **Resolution status** — flags Q&A pairs as `resolved`, `looped`, or `abandoned` based on follow-up patterns, with loop counts surfaced in the dashboard
- **Bookmarks** — save and organise important conversations
- **Incremental backups** — `stackunderflow backup create` snapshots `~/.claude/` with hard-linked `rsync --link-dest` (use `backup auto` on macOS for daily scheduling)
- **Multi-project** — switch between projects, view cross-project statistics
- **Legacy project recovery** — pre-January 2026 Claude Code stored prompts in `~/.claude/history.jsonl` instead of per-project JSONL files. StackUnderflow auto-detects these old projects and surfaces them from that file (prompts and timestamps only — token/model data wasn't stored locally in the old format).

## Using as a Library

StackUnderflow also works as a Python package for scripting and automation:

```python
import stackunderflow

# List all Claude Code projects on your machine
projects = stackunderflow.list_projects()
# [{"dir_name": "...", "log_path": "...", "file_count": 15, ...}, ...]

# Process a project's logs → (messages, statistics)
path = projects[0]["log_path"]
messages, stats = stackunderflow.process(path)

tokens = stats["overview"]["total_tokens"]
print(f"Sessions: {stats['overview']['sessions']}")
print(f"Tokens: {tokens['input']:,} in / {tokens['output']:,} out")
print(f"Total cost: ${stats['overview']['total_cost']:.2f}")

# Limit messages or adjust timezone
messages, stats = stackunderflow.process(path, limit=100, tz_offset=-480)
```

The pipeline stages are also importable for custom workflows:

```python
from stackunderflow.pipeline import reader, dedup, classifier, enricher, aggregator
from stackunderflow.infra.cache import TieredCache
from stackunderflow.infra.discovery import locate_logs
```

## Configuration

```bash
# Change port (default: 8081)
stackunderflow cfg set port 8090

# Disable auto-opening browser
stackunderflow cfg set auto_browser false

# Show current settings
stackunderflow cfg ls

# Reset a setting to default
stackunderflow cfg rm port
```

| Key | Default | Description |
|-----|---------|-------------|
| `port` | 8081 | Server port |
| `host` | 127.0.0.1 | Server host |
| `auto_browser` | true | Auto-open browser on start |
| `cache_max_projects` | 5 | Max projects in memory cache |
| `cache_max_mb_per_project` | 500 | Max MB per project in cache |
| `max_date_range_days` | 30 | Default date range for analytics |

See [CLI reference](docs/cli-reference.md) for all commands.

## Architecture

```
stackunderflow/
  adapters/       # source-adapter layer (one adapter per AI tool)
    base.py       #   LogAdapter ABC — discover() + stream_messages() protocol
    claude.py     #   Claude Code adapter — reads ~/.claude/projects/ + history.jsonl
  ingest/         # mtime-gated incremental import into the store
    enumerate.py  #   fan all adapters' SessionRefs into one iterable
    writer.py     #   transactional writer — one file → one transaction → one ingest_log row
  store/          # SQLite session store (~/.stackunderflow/sessions.db)
    db.py         #   connection factory (WAL mode, row_factory)
    schema.py     #   CREATE TABLE migrations
    queries.py    #   typed read helpers (list_projects, get_session_messages, …)
    types.py      #   frozen dataclasses returned by query helpers
  pipeline/       # JSONL → messages + statistics (legacy ETL — used by dashboard endpoints)
    reader.py     #   scan .jsonl files into raw entries (recursive, sub-agent aware)
    dedup.py      #   collapse streaming duplicates
    classifier.py #   tag message types and error patterns
    enricher.py   #   build dataset with interaction chains
    aggregator.py #   compute statistics (one pass, collector-based)
    formatter.py  #   shape messages for the REST API
  infra/
    cache.py      # TieredCache — hot (memory LRU with weighted eviction) + cold (disk JSON)
    discovery.py  # find and enumerate Claude log directories
    costs.py      # per-model cost estimation
    preloader.py  # background cache warming
  routes/         # FastAPI route modules
    projects.py   #   project selection and listing
    data.py       #   stats, dashboard-data, messages, refresh
    sessions.py   #   session browsing (store-backed)
    search.py     #   full-text search
    qa.py         #   Q&A pair browsing
    tags.py       #   auto-tags and manual tagging
    bookmarks.py  #   bookmark CRUD + session metadata enrichment
    misc.py       #   pricing, related, health, static
  services/       # search, Q&A, tags, bookmarks, related, pricing
  deps.py         # shared state (cache, config, services, store_path)
  server.py       # thin shell — app creation, middleware, lifespan
  settings.py     # env → file → default config resolution (descriptor-based)
  cli.py          # click CLI (init, start, cfg, backup, clear-cache, reindex)

stackunderflow-ui/  # React + TypeScript + Tailwind frontend
```

### How refresh works

`stackunderflow reindex` (also run at server startup) fans every registered adapter's discovered files through `ingest/enumerate.py`. For each file, the ingest runner checks the stored `last_offset` and `last_mtime` and only reads bytes beyond the last offset. New messages are written transactionally to the SQLite store. The store is the source of truth for session browsing, cross-project aggregation (`reports/aggregate.py`), and the waste-finding heuristic (`reports/optimize.py`).

### Source adapters

Currently supports Claude Code only — JSONL logs under `~/.claude/projects/` plus the legacy `~/.claude/history.jsonl` for pre-January-2026 sessions. A proposal sketch for adding OpenAI Codex (`~/.codex/`) lives at [docs/codex-adapter-spec.md](docs/codex-adapter-spec.md); it is an unimplemented RFC, not a roadmap commitment.

## Privacy

StackUnderflow processes all your Claude Code logs locally.

**What it reads on your machine:**
- `~/.claude/projects/<slug>/*.jsonl` — per-project conversation logs (new format)
- `~/.claude/history.jsonl` — centralized prompt history (legacy format, pre-January 2026)
- `~/.claude/settings*.json` — only via the `backup` command, for snapshots

**What it does with that data:**
- Parsing, search indexing, and analytics run locally — nothing is uploaded
- A cache is written to `~/.stackunderflow/` (hot analytics + config)
- Backups (opt-in) are written to `~/.stackunderflow/backups/` unencrypted — protect this directory like you would your `~/.claude/`

**What leaves your machine (only if you enable it):**
- **Pricing** — fetches model cost data from a public GitHub source (no user data sent)

No telemetry, no tracking, no crash reports. No sharing.

## Contributing

```bash
# Install with dev dependencies
pip install -e ".[dev]"

# Run tests
python -m pytest tests/stackunderflow/ -v

# Lint
bash lint.sh
```

See [docs/README-DEV.md](docs/README-DEV.md) for architecture details.

## License

MIT — see [LICENSE](LICENSE).
