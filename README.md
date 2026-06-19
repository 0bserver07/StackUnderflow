# StackUnderflow

**Offline, local-first observability toolkit for AI coding agents.**

StackUnderflow ingests and indexes session logs from 19 coding agent providers to surface cost analytics, interactive session playback (with step-by-step filesystem reconstruction), and a searchable knowledge base that both developers and agents can query to learn from past decisions and failures. Everything runs locally with zero external dependencies or telemetry.

<p align="center">
  <kbd><img src="https://www.google.com/s2/favicons?domain=anthropic.com&sz=64" width="16" valign="middle" /> Claude Code</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=openai.com&sz=64" width="16" valign="middle" /> OpenAI Codex</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=cursor.com&sz=64" width="16" valign="middle" /> Cursor</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=cline.bot&sz=64" width="16" valign="middle" /> Cline</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=github.com&sz=64" width="16" valign="middle" /> Copilot</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=gemini.google.com&sz=64" width="16" valign="middle" /> Gemini / Antigravity</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=continue.dev&sz=64" width="16" valign="middle" /> Continue</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=codeium.com&sz=64" width="16" valign="middle" /> Codeium</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=qwen.ai&sz=64" width="16" valign="middle" /> Qwen</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=roocode.com&sz=64" width="16" valign="middle" /> Roo Code</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=hermes-agent.org&sz=64" width="16" valign="middle" /> Hermes</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=openclaw.ai&sz=64" width="16" valign="middle" /> OpenClaw</kbd> &nbsp;
  <kbd><img src="https://www.google.com/s2/favicons?domain=pi.ai&sz=64" width="16" valign="middle" /> Pi</kbd>
</p>

### The Four Pillars
*   **Cost Analytics & Yield Attribution**: Parses raw session files into SQLite reporting marts to track spending/token mix, and correlates sessions with `git log` to classify runs (productive vs. abandoned).
*   **Time-Travel & Playback**: Reconstructs the precise state of the filesystem at any step of an AI session, letting you scrub through tool-call event streams and visualize how files evolved.
*   **Local Agent Memory**: Exposes a CLI so that active coding agents can query past sessions, decisions, and failure modes to reuse knowledge and avoid repeating errors.
*   **Offline Chat Sidebar**: Connects to a local Ollama instance (e.g., `qwen2.5-coder`) to discuss project history, query past decisions, and replay filesystem mutations without data leaving the machine.

19 providers supported (7 default-on, 12 opt-in beta). Sub-second sync (~400ms) from source-file write to dashboard data fresh. Everything stays private in `~/.stackunderflow/`.

[Quickstart](#quickstart) · [What it does](#what-it-does) · [Architecture](#architecture) · [Library API](#library-api) · [Configuration](#configuration) · [Privacy](#privacy)

![StackUnderflow — the projects overview across every coding agent the local store has indexed](assets/overview.png)

*Writeup: [Building StackUnderflow](https://yad.codes/posts/building-stackunderflow/).*

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

## CLI Tour (Live Terminal Demo)

StackUnderflow features a robust, colorful terminal interface powered by `rich`. Here is a direct look at the CLI in action, showing how you can query cost, audit waste, and query past sessions:

### 1. Cost & Ingest Status (`stackunderflow status`)
Get a quick, one-line summary of your active token spending and message counts for the day and the current billing cycle:
```bash
$ stackunderflow status
today: $35.63 (75 msg) | month: $7974.71 (31728 msg)
```

### 2. Multi-project reports (`stackunderflow report`)
Generate high-fidelity, ASCII table summaries of your spending across all active agent workspaces over a custom date range (e.g., the last 7 days):
```ansi
$ stackunderflow report
StackUnderflow — last 7 days
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┳━━━━━━━━━━┓
┃ Project                                     ┃     Cost ┃ Messages ┃ Sessions ┃
┡━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━╇━━━━━━━━━━┩
│ -Users-yadkonrad-dev-dev-year26-jan26-Stac… │ $1081.59 │    3,514 │       20 │
│ -Users-yadkonrad-dev-dev-year26-jan26-new-… │  $635.22 │      998 │        2 │
│ -Users-yadkonrad-dev-dev-year26-jan26-bour… │  $289.22 │      905 │        2 │
│ -Users-yadkonrad-dev-dev-year26-feb26-chim… │  $239.58 │    1,254 │       11 │
│ -Users-yadkonrad-dev-dev-year26-feb26-clau… │  $203.06 │      593 │        4 │
│ -Users-yadkonrad-dev-dev-year26-may26-Stud… │  $157.24 │      176 │        2 │
└─────────────────────────────────────────────┴──────────┴──────────┴──────────┘
Total: $2894.57  8,315 messages  59 sessions
```

### 3. Waste audit & cost optimization (`stackunderflow optimize`)
Run automated, offline waste detectors (looped Q&A pairs, cache thrashing, excessive file re-reads, and unused MCP servers) to cut down your active developer billing:
```ansi
$ stackunderflow optimize
Waste report — last 30 days

Q&A loops:
  -Users-yadkonrad-dev-dev-year26-feb26-claude-sessions: 6 looped pair(s)
    - "if u were to review our entire conversations, whats is the oscillation like?"

Structural patterns:
  [HIGH] cache_overhead: 241 session(s) with cache thrash
      241 session(s) where cache_create_tokens exceed 50% of total input
      ~289,497,821 wasted tokens
      fix: Bundle related questions into one session so cache writes amortise.
  [HIGH] junk_reads: 61 file(s) re-read excessively
      61 file(s) Read 5+ times in a single session — assistant likely forgot prior reads.
      fix: Cache file contents in working memory or use Grep to search.
```

### 4. Search past decisions (`stackunderflow memory decisions "<term>"`)
Active agents (or developers) can query the database directly from the CLI to view past decisions and context-rich changes to avoid duplicating work:
```ansi
$ stackunderflow memory decisions "cache"
Past decisions matching 'cache' (14 session(s))

  [claude] 18d87ee4-b01…  2026-05-20T03:21:26  msgs=445  $115.0498
      -Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow  /Users/yadkonrad/dev/dev/year26/jan26/StackUnderflow
      … remove a leaked email and force-pushed. Please garbage-collect the dangling/unreachable commits so cached SHAs stop resolving.

  [claude] 5be67015-9a4…  2026-05-20T01:56:58  msgs=198  $22.2723
      … memory-and-latency's "no in-process cache" claim was false — `/api/dashboard-data` has a memo cache plus a `project_mart` fast-path.
```

---

## What it does

### Multi-provider ingest
19 coding agents have adapters in the registry. Seven ship default-on:

| Provider | Source |
|---|---|
| Claude Code | `~/.claude/projects/<slug>/*.jsonl` (+ legacy `~/.claude/history.jsonl`) |
| Codex | `~/.codex/sessions/{YYYY}/{MM}/{DD}/rollout-*.jsonl` |
| Cursor | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` |
| Cline | `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/` |
| OpenClaw | `~/.openclaw/agents/` (+ clawdbot / moltbot / moldbot variants) |
| Pi + OMP | `~/.pi/agent/sessions/`, `~/.omp/agent/sessions/` |
| Hermes | `~/.hermes/sessions/` |

Twelve more (KiloCode, Roo Code, OpenCode, Cursor Agent, Qwen, Gemini, Copilot, Codeium, Continue, Droid, Kiro, Antigravity) opt in via env var:

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

![The Cost tab: spend by agent, cache ROI, and an error-cost breakdown by tool](assets/cost.png)

![Compare: per-model sessions, retry, cache, and unit economics side by side](assets/compare.png)

![Tools ranked by cost and the token-composition donut](assets/tools.png)

### Search, Q&A, tags
- **Full-text search** across every ingested message. Filter by date / model / role.
- **Q&A pair extraction** — heuristic detection of question/answer pairs with resolution status (`resolved` / `looped` / `abandoned`).
- **Auto-tagging** — sessions get tagged by language, framework, topic, intent (`build`, `fix`, `explore`, `refactor`, `test`, `ops`).

### Meta agent (Ask StackUnderflow)
A **right-docked sidebar** lets you talk to your local Ollama LLM about your own coding history. It calls a catalogue of read-only backend tools (search past decisions, find sessions touching a file, get a project's cost summary, replay a session's filesystem mutations, …) and answers in prose. Recommended models: `qwen2.5-coder`, `llama3.2`. Everything runs locally — there is **no fallback to a remote LLM**; if Ollama is down the sidebar surfaces a banner. See [docs/meta-agent.md](docs/meta-agent.md).

![Ask StackUnderflow: a local model answering from your own session history via read-only tools](assets/agent-sidebar.png)

### Playback (time-travel)
- **Event-stream timeline** — scrub through every tool call a session made, in order, with payload excerpts.
- **Virtual-FS reconstruction** (v0.7.3+) — at any timestamp in the scrub, see the reconstructed content of every file the session touched. Replays Read / Write / Edit / MultiEdit / NotebookEdit calls; marks partial reconstructions where no initial Read was seen.

![Step-by-step playback with the reconstructed file tree at each moment](assets/playback.png)

### Self-referential discovery (for coding agents)
- **`find-sessions-in-path` / `-touching-file`** + **`search-past-decisions`** — CLI commands that let a Claude Code / Cursor / Codex agent query its own session history before doing work ("what did I learn here last time?"). Token-budgeted output ranks by recency + cost + relevance; opt-in **`--use-embeddings`** (`pip install stackunderflow[embeddings]`) re-ranks by cosine similarity with a local sentence-transformers model.
- **`find-sessions-where-action-worked` / `find-failure-modes-for-file`** — outcome-aware variants. Returns sessions whose subsequent turns confirmed (or contradicted) the action, with a confidence score so silence isn't mistaken for success.
- **`skills generate`** — mines this store for project-specific workflow patterns and emits Claude Code `SKILL.md` files. Project-scoped by default.
- **Bookmarks** — pin conversations you want to find later.

### Real-time sync
A `watchfiles`-backed daemon thread watches every registered adapter's source paths. On any change → ingest the new bytes → normalize → refresh marts. Source-file write to dashboard data fresh in ~400ms. Disable with `--no-watcher`.

![The ETL pipeline panel: watcher status, event count, and per-mart watermarks](assets/etl.png)

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

```mermaid
flowchart TD
    %% Theme Styling for Dark & Light Mode Legibility
    classDef source fill:#1A202C,stroke:#4A5568,stroke-width:1.5px,color:#EDF2F7;
    classDef pipeline fill:#2B6CB0,stroke:#3182CE,stroke-width:2px,color:#FFF;
    classDef db fill:#2C7A7B,stroke:#319795,stroke-width:2px,color:#FFF;
    classDef interface fill:#D69E2E,stroke:#ECC94B,stroke-width:2px,color:#FFF;
    classDef cli fill:#E53E3E,stroke:#F56565,stroke-width:2px,color:#FFF;
    classDef agent fill:#805AD5,stroke:#9F7AEA,stroke-width:2px,color:#FFF;

    %% 1. Log Sources
    subgraph Sources ["📁 Input Log Sources (19 Providers)"]
        Logs["Local Session Logs<br/>• Claude Code JSONL<br/>• Cursor state.vscdb<br/>• Cline tasks JSON"]
    end
    class Logs source;

    %% 2. Background Processing
    subgraph Engine ["⚡ StackUnderflow Core Engine"]
        Watcher["Filesystem Watcher<br/>• 200ms debounce<br/>• ~400ms fresh sync"]
        Ingest["Ingest & Normalizer<br/>• Standardizes events<br/>• Computes costs offline"]
        Store[("SQLite Store<br/>~/.stackunderflow/store.db")]
        ETL["Mart Builder (ETL)<br/>• Aggregates 8 reporting marts<br/>• Correlates Git yields"]
    end
    class Watcher,Ingest,ETL pipeline;
    class Store db;

    %% 3. Interfaces & Presentation
    subgraph Frontends ["🖥️ Interfaces & Presenters"]
        API["FastAPI REST Web Server<br/>• Serving /api/* routes"]
        CLI["Command Line Interface (CLI)<br/>• stackunderflow today / month<br/>• stackunderflow optimize / report<br/>• stackunderflow memory (agent queries)"]
    end
    class API interface;
    class CLI cli;

    %% 4. Client / End User Applications
    subgraph Clients ["👥 End Users & AI Clients"]
        Dashboard["React Web Dashboard<br/>• http://localhost:8081<br/>• Analytics, playback & virtual FS"]
        Ollama["Local Ollama Chat<br/>• Offline history Q&A sidebar"]
        Agent["Active AI Agent (Claude Code / Cursor)<br/>• Queries past runs during sessions<br/>• Learns from previous failures"]
    end
    class Dashboard,Ollama interface;
    class Agent agent;

    %% Watcher Loop
    Watcher -.->|Monitors| Logs
    Watcher -.->|Triggers Ingest| Ingest

    %% Data Pipeline Flow
    Logs --> Ingest
    Ingest -->|Raw & Normalized events| Store
    Store --> ETL
    ETL -->|Aggregated reporting marts| Store

    %% Access Points
    Store --> API
    Store --> CLI

    %% Client Delivery
    API --> Dashboard
    API --> Ollama
    CLI <-->|memory CLI queries| Agent
    CLI <-->|Developer CLI Reports| Dashboard
```

Most dashboard routes read from the marts when populated, falling back to a live aggregation pass otherwise. On a 247K-message store the cold-load went from 2.5s to <50ms warm. A new install starts on the empty-mart fallback path (still functional, just slower); the first watcher cycle or `stackunderflow etl backfill` populates the marts.

```
stackunderflow/
  adapters/         # 19 source-file parsers (7 default-on, 12 beta)
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

**What StackUnderflow reads on disk** — only the source paths the registered adapters point at. The 7 default-on roots:
- `~/.claude/projects/`, `~/.claude/history.jsonl` (legacy)
- `~/.codex/sessions/`
- `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
- `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/`
- `~/.openclaw/agents/` (+ clawdbot / moltbot / moldbot variants)
- `~/.pi/agent/sessions/`, `~/.omp/agent/sessions/`
- `~/.hermes/sessions/`

The 12 beta adapters add more source roots when their env vars are set. Full path list in [docs/multi-provider.md](docs/multi-provider.md).

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
