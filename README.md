# StackUnderflow

**Local cost dashboard for AI coding agents.** Watches the JSONL / vscdb / SQLite files your coding tools write, parses them in-process, and serves a dashboard that shows what you're spending, where it goes, and which sessions actually shipped code.

17 providers supported (4 default-on, 13 opt-in beta). Sub-second sync from source-file write to dashboard data fresh. No telemetry, no upload, no cloud — everything stays in `~/.stackunderflow/`.

[Quickstart](#quickstart) · [What it does](#what-it-does) · [Architecture](#architecture) · [Library API](#library-api) · [MCP](#mcp-server) · [Configuration](#configuration) · [Privacy](#privacy)

![StackUnderflow Dashboard](assets/dashboard.png)

---

## Quickstart

Requires Python 3.11+. The first run picks up whatever local sessions you already have under `~/.claude/`, `~/.codex/`, etc.

```bash
pip install stackunderflow
stackunderflow init
```

Browser opens to `http://localhost:8081` with every project the local store knows about, indexed and ready. Background ingest + watcher start immediately; the dashboard is interactive while ingest runs.

If port 8081 is taken: `stackunderflow cfg set port 8090` then re-run.

```bash
# common knobs
stackunderflow cfg set port 8090            # change the port
stackunderflow cfg set currency GBP         # display costs in another currency
stackunderflow plan set claude-pro          # track against a monthly budget
stackunderflow init --no-browser            # don't auto-open the browser
stackunderflow --help                       # full CLI
```

### Nix

```bash
nix run github:0bserver07/StackUnderflow      # launch the dashboard
nix build github:0bserver07/StackUnderflow    # build, output at ./result
nix develop                                   # dev shell
```

### From source

```bash
git clone https://github.com/0bserver07/StackUnderflow.git
cd StackUnderflow
cd stackunderflow-ui && npm install && npm run build && cd ..
pip install -e ".[dev]"
stackunderflow init
```

---

## What it does

### Multi-provider ingest
17 coding agents have adapters in the registry. Four ship default-on:

| Provider | Source |
|---|---|
| Claude Code | `~/.claude/projects/<slug>/*.jsonl` (+ legacy `~/.claude/history.jsonl`) |
| Codex | `~/.codex/sessions/{YYYY}/{MM}/{DD}/rollout-*.jsonl` |
| Cursor | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` |
| Cline | `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/` |

Thirteen more (KiloCode, Roo Code, OpenCode, Cursor Agent, Qwen, Gemini, Copilot, Codeium, Continue, Droid, Kiro, OpenClaw, Pi+OMP) opt in via env var:

```bash
STACKUNDERFLOW_BETA_GEMINI=1 STACKUNDERFLOW_BETA_QWEN=1 stackunderflow start
```

See [docs/multi-provider.md](docs/multi-provider.md) for the per-provider source paths and the cost-source semantics each one uses (rate-card vs estimated).

### Cost analysis
- **Cost tab** — top sessions by cost, most expensive commands (click → Messages tab), tool-cost ranking, token composition (donut + stacked daily), cache ROI, outliers, retry-loop signals, week-over-week trends, error-cost estimate. Filters (range / session / tool) URL-encoded.
- **Compare** — side-by-side model metrics over a window: one-shot rate, retry rate, cache hit rate, $/call, $/session. Group by `(provider, model)` (Agent × Model) or just model.
- **Plan budgets** — set a monthly budget from a preset (Claude Pro $20, Claude Max $200, Cursor Pro/Max) or a custom amount. Shows used / remaining / projected month-end.
- **Yield analysis** — correlates sessions with `git log` per cwd: productive (commit followed within 24h) / reverted / abandoned / no-repo. Use it to find which sessions actually shipped code.
- **Optimize** — eight waste detectors: looped Q&A, bloated CLAUDE.md, unused MCP servers, ghost agents, low read-to-edit ratio, junk reads, cache overhead, bash-output limits. Each finding ships with a one-line suggested fix.
- **Context-budget estimator** — what your system prompt + MCP servers + skills + memory files cost on every turn before you type anything.
- **Multi-currency** — pick any 3-letter ISO code; FX rates from the public Frankfurter API (24h cached, ECB snapshot fallback when offline).
- **Model aliases** — for proxied model ids (OpenRouter, Replicate, internal gateways): `cfg model-alias set openrouter/claude-opus claude-opus-4-6` and the cost layer prices it at the canonical rate.
- **Fast-mode multiplier** — Claude Opus priority tier (`service_tier="priority"`) bills at 6×; detected from the JSONL and threaded through the cost layer end-to-end.

### Search, Q&A, tags
- **Full-text search** across every ingested message. Filter by date / model / role.
- **Q&A pair extraction** — heuristic detection of question/answer pairs with resolution status (`resolved` / `looped` / `abandoned`).
- **Auto-tagging** — sessions get tagged by language, framework, topic, intent (`build`, `fix`, `explore`, `refactor`, `test`, `ops`).

### Meta agent (Ask StackUnderflow)
A **right-docked sidebar** lets you talk to your local Ollama LLM about your own coding history. It calls a catalogue of read-only backend tools (search past decisions, find sessions touching a file, get a project's cost summary, replay a session's filesystem mutations, …) and answers in prose. Recommended models: `qwen2.5-coder`, `llama3.2`. Everything runs locally — there is **no fallback to a remote LLM**; if Ollama is down the sidebar surfaces a banner. See [docs/meta-agent.md](docs/meta-agent.md).

### Playback (time-travel)
- **Event-stream timeline** — scrub through every tool call a session made, in order, with payload excerpts.
- **Virtual-FS reconstruction** (v0.7.3+) — at any timestamp in the scrub, see the reconstructed content of every file the session touched. Replays Read / Write / Edit / MultiEdit / NotebookEdit calls; marks partial reconstructions where no initial Read was seen.

### Self-referential discovery (for coding agents)
- **`find-sessions-in-path` / `-touching-file`** + **`search-past-decisions`** — CLI + MCP tools that let a Claude Code / Cursor / Codex agent query its own session history before doing work ("what did I learn here last time?"). Token-budgeted output ranks by recency + cost + relevance; opt-in **`--use-embeddings`** (`pip install stackunderflow[embeddings]`) re-ranks by cosine similarity with a local sentence-transformers model.
- **`find-sessions-where-action-worked` / `find-failure-modes-for-file`** — outcome-aware variants. Returns sessions whose subsequent turns confirmed (or contradicted) the action, with a confidence score so silence isn't mistaken for success.
- **`skills generate`** — mines this store for project-specific workflow patterns and emits Claude Code `SKILL.md` files. Project-scoped by default.
- **Bookmarks** — pin conversations you want to find later.

### Real-time sync
A `watchfiles`-backed daemon thread watches every registered adapter's source paths. On any change → ingest the new bytes → normalize → refresh marts. Source-file write to dashboard data fresh in ~400ms. Disable with `--no-watcher`.

### Export
```bash
stackunderflow export -f csv -o usage.csv -p month
stackunderflow export -f json -o usage.json   # multi-period rollup (today + 7d + 30d)
```

The dashboard's "Download" button hits the same `/api/export` endpoint.

### Backup
```bash
stackunderflow backup create               # snapshot ~/.claude/ via rsync --link-dest
stackunderflow backup auto --enable        # daily on macOS via launchd
stackunderflow backup list
stackunderflow backup restore <name>
```

Snapshots land under `~/.stackunderflow/backups/<ts>[-label]/`. Unchanged files are hard-linked from the previous snapshot, so a daily backup of a quiet `~/.claude/` is roughly zero on-disk delta. Full surface in [docs/backup.md](docs/backup.md).

### Chat sidebar
A header toggle slides in a chat drawer that streams from a **local** Ollama instance (proxied through `/api/ollama-api/*`, default upstream `http://localhost:11434`). Pick a pulled model, type, get a streamed reply — nothing leaves the machine. Empty model list = Ollama not running. See [docs/chat.md](docs/chat.md).

---

## Architecture

The pipeline is three layers tied together by a watermarked refresh loop and a filesystem watcher.

```
                ┌─ Source files (17 providers) ─┐
                │  ~/.claude/projects/           │
                │  ~/.codex/sessions/            │
                │  state.vscdb (Cursor)          │
                │  saoudrizwan.claude-dev (Cline)│
                │  ...                           │
                └─────────────┬──────────────────┘
                              │  per-provider adapter
                              ▼
               ┌─────────  RAW LAYER  ─────────┐
               │  messages, sessions, projects │
               │  one row per source-message    │
               └─────────────┬──────────────────┘
                              │  per-provider Normalizer
                              ▼
               ┌──── NORMALIZED LAYER ─────────┐
               │  usage_events                  │
               │  canonical shape, cost_usd     │
               │  computed once + stored        │
               └─────────────┬──────────────────┘
                              │  watermarked MartBuilders
                              ▼
               ┌──────  MARTS LAYER  ──────────┐
               │  daily_mart                    │
               │  session_mart                  │
               │  project_mart                  │
               │  provider_day_mart             │
               │  model_day_mart                │
               │  tool_mart                     │
               │  command_mart                  │
               │  message_tool_mart             │
               └─────────────┬──────────────────┘
                              ▼
               REST routes — plain SELECTs

                ↑↑↑ filesystem watcher ties
                    layers together: 200ms
                    debounce, ~400ms total
                    end-to-end latency
```

Every dashboard route reads from the marts. On a 247K-message store the cold-load went from 2.5s to <50ms warm. A new install starts on the empty-mart fallback path (still functional, just slower); the first watcher cycle or `stackunderflow etl backfill` populates the marts.

```
stackunderflow/
  adapters/         # 17 source-file parsers (4 default-on, 13 beta)
  etl/              # ETL pipeline (v0.7+)
    normalize/      #   Normalizer ABC + per-provider transforms (18 normalizers — pi and omp register separately, one more than the 17 adapters)
    marts/          #   MartBuilder ABC + 8 mart builders
    backfill.py     #   streams messages → events → marts
    watcher.py      #   watchfiles daemon, debounced 200ms
    watermark.py    #   per-mart last_event_id tracking
    status.py       #   shared assembler for /api/etl/status + CLI
  api/              # public Python API (list_projects/process/list_sessions)
  ingest/           # writer + per-record normalize hook
  store/            # SQLite at ~/.stackunderflow/store.db
    migrations/     #   v001 → v017 (additive; v015 intentionally skipped)
    queries.py      #   typed read helpers (raw layer)
    mart_queries.py #   typed read helpers (marts)
  infra/
    costs.py        # compute_cost(tokens, model, provider, *, speed)
    currency.py     # Frankfurter + 24h cache + ECB snapshot fallback
    cursor_cache.py # fingerprint cache for vscdb (3-8x cold-start speedup)
    providers/      # per-provider Pricers (one file per provider)
  mcp/              # FastMCP server (12 tools, multi-provider)
  reports/          # CLI report renderers + 8 optimize patterns
  routes/           # FastAPI route modules — 23, one per concern
  services/         # compare, plans, yield_tracker, search, qa, tags, ...
  cli.py            # click CLI — dashboard, ETL ops, exports, plan budgets, discovery
  server.py         # thin shell — app + lifespan + watcher + bg ingest
  settings.py       # env → file → default resolution (descriptor pattern)

stackunderflow-ui/  # React + TypeScript + Tailwind + Recharts
```

For the deeper design rationale see `docs/specs/etl-architecture.md`. For the on-disk schema as a versioned spec other tools can target: [docs/specs/session-schema-v1.md](docs/specs/session-schema-v1.md) (+ [adapter-contract.md](docs/specs/adapter-contract.md) for the source-adapter Protocol). For the state-of-the-codebase walkthrough (recent history, gotchas, real-data state, what's left) see [docs/HANDOFF.md](docs/HANDOFF.md).

---

## Library API

```python
import stackunderflow

# Every project the local store knows about, provider-tagged.
projects = stackunderflow.list_projects()
# [{"slug": ..., "provider": "claude" | "codex" | "cursor" | ...,
#   "display_name": ..., "path": ..., "first_seen": ..., "last_modified": ...}]

# Filter to one provider:
codex_only = stackunderflow.list_projects(provider="codex")

# Sessions for a project:
sessions = stackunderflow.list_sessions("project-slug")
# [{"session_id": ..., "first_ts": ..., "last_ts": ..., "message_count": ...}]

# Pipeline-formatted messages + statistics for one project:
messages, stats = stackunderflow.process(projects[0]["slug"])
print(f"Sessions: {stats['overview']['sessions']}")
print(f"Cost: ${stats['overview']['total_cost']:.2f}")
```

`list_projects()` returns `[]` rather than raising when the store doesn't exist yet. `process()` raises `KeyError` when the slug isn't found.

For lower-level access:

```python
from stackunderflow.store import db, queries, mart_queries
from stackunderflow.etl import backfill, watermark
from stackunderflow.etl.normalize import get as get_normalizer
from stackunderflow.infra.discovery import locate_logs
```

---

## MCP server

StackUnderflow ships an [MCP](https://modelcontextprotocol.io/) server that reads the local store. Twelve tools, covering every ingested provider (no longer Claude-only):

*Session & project*
- `session_query(session_id, kind="all"|"tool_calls"|"errors")` — pull messages from a specific session
- `list_sessions(provider=None, limit=50, since=None)` — recent sessions across providers
- `list_projects(provider=None)` — provider-tagged project catalogue

*Discovery & outcomes* — query your own history before doing work
- `find_sessions_in_path` / `find_sessions_touching_file` — sessions that worked in a directory or touched a file
- `search_past_decisions` — free-text search across past transcripts
- `find_sessions_where_action_worked` / `find_failure_modes_for_file` — outcome-aware variants, each with a confidence score

*Recommenders*
- `recommend_skills` — repeated patterns worth turning into a project skill
- `recommend_mode` — the cheapest model that fits a task, from your own history
- `file_risk` — how often past edits to a file were reverted or failed
- `get_burn_projection` — projected month-end spend against your plan budget

```bash
stackunderflow-mcp     # console script
stackunderflow mcp     # equivalent CLI subcommand
```

Wire into Claude Desktop via `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "stackunderflow": {
      "command": "stackunderflow-mcp"
    }
  }
}
```

See [docs/mcp.md](docs/mcp.md) for the full tool reference + Cursor / Claude Code wiring.

---

## Claude Code skills

StackUnderflow ships a set of [Claude Code skills](https://code.claude.com/docs/en/skills) that turn the local store into a reflex: Claude Code automatically surfaces prior session context when you start work in a project, mention a specific file, or reference a past decision. Install with `cp -r stackunderflow/skills/* ~/.claude/skills/` — see [docs/skills.md](docs/skills.md) for trigger semantics and example transcripts.

---

## ETL operations

The pipeline is incremental + idempotent. Most users never need to think about it. For when you do:

```bash
# Health check — watcher status, mart watermarks vs max event id, lag
stackunderflow etl status

# Populate marts from existing messages (one-time on first install or after a crash)
stackunderflow etl backfill          # incremental — skips converted msgs
stackunderflow etl backfill --force  # drop + rebuild from scratch

# Same backfill, kicked off in the background from HTTP (used by the
# Settings page "Backfill now" button); poll /api/etl/status to follow it
curl -X POST http://127.0.0.1:8081/api/etl/backfill

# Disable the watcher (headless / debugging)
stackunderflow start --no-watcher
# or via env var:
STACKUNDERFLOW_DISABLE_WATCHER=1 stackunderflow start

# Skip the watcher single-instance lock (multi-server, or stale lock file)
stackunderflow start --no-lock
# or via env var:
STACKUNDERFLOW_DISABLE_LOCK=1 stackunderflow start
```

Watcher state (including the PID currently holding the watcher lock),
watermarks, per-provider event counts, and any in-flight backfill job
are also at `GET /api/etl/status` and visible as a badge in the
dashboard header.

---

## Configuration

```bash
stackunderflow cfg ls                   # show current settings
stackunderflow cfg set port 8090
stackunderflow cfg rm port              # reset to default
```

Selected keys (full list in [docs/cli-reference.md](docs/cli-reference.md)):

| Key | Default | Description |
|---|---|---|
| `port` | `8081` | Server port |
| `host` | `127.0.0.1` | Bind address |
| `auto_browser` | `true` | Open browser on start |
| `currency` | `USD` | Display currency (any 3-letter ISO) |
| `model_aliases` | `{}` | Proxy id → canonical (manage via `cfg model-alias`) |
| `plan_name` | unset | Active plan preset (`claude-pro`, `claude-max`, `cursor-pro`, `cursor-max`, `custom`) |
| `plan_monthly_usd` | `0.0` | Monthly budget (USD) |
| `plan_reset_day` | `1` | Day of month the budget resets |
| `auto_reindex_on_ingest` | `true` | Refresh search/qa/tags after each ingest |

Env vars override the persisted file. The Python descriptor in `stackunderflow/settings.py` resolves env → file → default lazily on every read.

---

## Privacy

Everything runs locally. Nothing about your sessions, prompts, or code leaves the machine.

**What StackUnderflow reads on disk** — only the source paths the registered adapters point at. The 4 default-on roots:
- `~/.claude/projects/`, `~/.claude/history.jsonl` (legacy)
- `~/.codex/sessions/`
- `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
- `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/`

The 13 beta adapters add more source roots when their env vars are set. Full path list in [docs/multi-provider.md](docs/multi-provider.md).

**What it writes** — `~/.stackunderflow/` only.
- `store.db` — SQLite, WAL mode, the source of truth
- `cache/` — currency rates (24h), Cursor vscdb fingerprint cache
- `backups/` — only when you run `backup create`. Plain copy of `~/.claude/` snapshots — protect this directory.

**What leaves your machine** — only when explicitly enabled:
- Pricing snapshot from `github.com/BerriAI/litellm` (no user data sent; hardcoded fallback in `infra/costs.py`)
- FX rates from `api.frankfurter.app` when `currency != USD` (no user data sent; ECB snapshot fallback embedded in `infra/currency.py`)

No telemetry. No tracking. No crash reports. No analytics. The app is a single binary that talks to your filesystem and your browser.

---

## Development

```bash
git clone https://github.com/0bserver07/StackUnderflow.git
cd StackUnderflow
pip install -e ".[dev]"
cd stackunderflow-ui && npm install && npm run build && cd ..

# Backend tests — fast suite (pytest tests/ -q collects 2781; slow tests deselected by default)
pytest tests/ -q

# Slow integration + perf-regression suite (opt-in via the `slow` marker)
pytest -m slow tests/stackunderflow/integration -q

# Lint
ruff check stackunderflow/

# Frontend
cd stackunderflow-ui
npm run typecheck
npm run build                          # outputs to ../stackunderflow/static/react/
node --test tests/services/*.test.ts   # unit tests via Node 22+ built-in runner
```

For an architecture walkthrough oriented at a new contributor or agent: [docs/HANDOFF.md](docs/HANDOFF.md).

For per-component design specs: [docs/specs/](docs/specs/).

For adapters: [docs/adapters.md](docs/adapters.md) walks through writing one.

---

## License

MIT — see [LICENSE](LICENSE).
