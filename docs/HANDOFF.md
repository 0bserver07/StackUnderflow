# StackUnderflow — Handoff doc

**Date:** 2026-05-12 (last tagged release: v0.7.0; Wave 5 follow-ups + the spec-01…10 round, all merged on `feat/post-0.7.0`, unreleased)
**Maintainer:** 0bserver07 / 0bserver07
**Branch:** `feat/post-0.7.0` (this is the live branch — base new work here, **not** `main`, which is stale relative to it); last tag `v0.7.0`
**Tests:** 2272 fast (`pytest tests/ -q`) / 2 skipped / 12 deselected, 10 slow (`pytest -m slow tests/stackunderflow/integration`) / 1 skipped, 110 frontend (`node --test stackunderflow-ui/tests/services/*.test.ts`). Was ~1747 at the start of the post-v0.7 round — +525 fast tests across the spec-01…10 work.
**Frontend:** typecheck + build clean; the dashboard build output lands in `stackunderflow/static/react/`

This doc gets a fresh agent oriented in 10 minutes. Read it before reading code.

> **Versioning note for an incoming agent.** Everything below the v0.7.0 line is **unreleased** — it's all merged on `feat/post-0.7.0` but no version number is assigned. The maintainer owns version decisions; do **not** bump `__version__.py` / `pyproject.toml` / `package.json` or move a "release" label without being asked. The CHANGELOG carries it under `## [Unreleased]`.

---

## What StackUnderflow is

A local-first knowledge base + cost dashboard for AI coding sessions. Forked from a since-rewritten codebase; **MIT, no external service dependencies, no telemetry**.

The user runs `stackunderflow start`. A FastAPI server binds `127.0.0.1:8095`, serves a React dashboard, and exposes:

- A **REST API** under `/api/*` for the dashboard
- An **MCP server** (over stdio) so Claude Desktop / Cursor / Claude Code can query session history without spinning up the dashboard
- A **CLI** (`stackunderflow ...`) for ops, exports, plan budgets, ETL ops, etc.
- A **Python public API** (`import stackunderflow; list_projects(); process(slug)`) for scripting

Source-of-truth state lives at `~/.stackunderflow/store.db` (SQLite). The dashboard is **read-only against the store** in the hot path; ingest happens in the background.

---

## Architecture map

```
┌──────────────────────── Source files (16 providers) ────────────────────────┐
│  ~/.claude/projects/                  # JSONL                                │
│  ~/.codex/sessions/                   # JSONL                                │
│  ~/Library/.../Cursor/.../state.vscdb # SQLite                               │
│  ~/Library/.../saoudrizwan.claude-dev # JSON (Cline)                         │
│  ~/.gemini/, ~/.qwen/, ~/.factory/, ... # 12 beta providers                  │
└─────────────────────────────────────────────────────────────────────────────┘
                          │
                          ▼  Adapter (per-provider parser)
┌──────────────────────────  RAW LAYER  ──────────────────────────────────────┐
│  messages, sessions, projects (SQLite)                                       │
│  one row per source-message; immutable; UNIQUE(provider, slug)               │
└─────────────────────────────────────────────────────────────────────────────┘
                          │
                          ▼  Normalizer (per-provider transform)
┌──────────────────────  NORMALIZED LAYER  ───────────────────────────────────┐
│  usage_events                                                                │
│  one row per billable event, canonical shape, cost_usd computed once         │
│  cost_source: live | rate_card | estimated | unknown                         │
└─────────────────────────────────────────────────────────────────────────────┘
                          │
                          ▼  MartBuilder.refresh(conn, since_event_id)
┌────────────────────────  MARTS LAYER (8 builders)  ─────────────────────────┐
│  daily_mart        (day, project_id, provider, model, speed)                 │
│  session_mart      (session_id, all per-session aggregates)                  │
│  project_mart      (project_id, lifetime totals)                             │
│  provider_day_mart (day, provider)                                           │
│  model_day_mart    (day, model, speed)                                       │
│  tool_mart         (day, project_id, provider, tool_name)        ← v007/v012 │
│  command_mart      (day, project_id, command_name)               ← v007      │
│  message_tool_mart (message_id, tool_name, call_index)           ← v011      │
│  mart_watermark    (mart_name → last_event_id, last_refresh_ts)              │
└─────────────────────────────────────────────────────────────────────────────┘
                          │
                          ▼
            REST routes — plain SELECTs from marts (+ aggregator fallback)
```

`tool_mart` / `command_mart` / `message_tool_mart` JOIN `usage_events` back to `messages` to read `tools_json` (the aggregate-grain marts above them never touch raw messages). `message_tool_mart` watermarks on `usage_events.id` (not `messages` — that's a UNION view post-v008 and can't be watermarked directly).

The watcher (`stackunderflow/etl/watcher.py`) ties the layers together: filesystem change → adapter.read() → writer inserts messages → normalizer inserts events → refresh_all_marts() advances watermarks. End-to-end ~400 ms. A `fcntl`/`msvcrt` single-instance lock at `~/.stackunderflow/server.lock` (see `etl/lock.py`) means a second `stackunderflow start` against the same store still serves HTTP but doesn't run the watcher; `--no-lock` / `STACKUNDERFLOW_DISABLE_LOCK=1` skips it.

---

## Package layout

```
stackunderflow/
  adapters/          # Per-provider source parsers (16 of them; 4 default-on)
    base.py          # SourceAdapter Protocol; SessionRef + Record dataclasses
    claude.py codex.py cursor.py cline.py                       # default-on
    cursor_agent.py opencode.py qwen.py gemini.py               # beta
    copilot.py codeium.py continue_adapter.py                   # beta
    droid.py kiro.py openclaw.py pi.py                          # beta
    kilocode.py roocode.py                                      # beta (cline-family)
    claude_teams.py  # materialize_team_metadata() — ingests ~/.claude/teams/ + tasks/ (v013)  ← post-v0.7
    _streaming.py    # 128 MB cap + 8 MB stream threshold for JSONL
  api/               # Public Python API surface (list_projects/process/list_sessions)
  etl/                                                            ← v0.7
    normalize/       # Per-provider transforms messages → usage_events
      base.py        # Normalizer ABC + cost_source constants + _build_event helper
      __init__.py    # last-wins registry: register/get/all + 16 normalizers wire here
      claude.py codex.py cursor.py cline.py                     # default-on
      <12 beta normalizers>
    marts/           # MartBuilder ABC + 8 builders
      base.py        # ABC; concrete rebuild_from_scratch default
      __init__.py    # last-wins registry; 8 builders wire here
      daily.py session.py project.py provider_day.py model_day.py
      tool.py command.py                                                      # v007 (Wave 5); tool gains calls_total in v012
      message_tool.py                                                         # v011 — per-(message,tool,call_index) grain  ← post-v0.7
    backfill.py      # Streams messages → events → marts; idempotent; --force rebuild
    backfill_jobs.py # Process-local lock + single-slot job state for POST /api/etl/backfill   # Wave 5
    lock.py          # fcntl/msvcrt watcher single-instance lock + stale-PID detection         # Wave 5
    watcher.py       # watchfiles daemon; debounced 200 ms; per-adapter dispatch
    watermark.py     # get/set/refresh_all_marts; persists last_event_id + last_refresh_ts
    status.py        # Shared assembler for /api/etl/status + `stackunderflow etl status` (+ current_job/last_job/lock_held_by)
  hooks/                                                          ← post-v0.7 — opt-in Claude Code lifecycle hooks
    _install.py      # idempotent merge into .claude/settings.json (project or user scope) + backup
    _repair.py       # rewrite stale hook commands to the portable `stackunderflow hooks run <id>` form; --scope all walks $HOME
    handlers.py      # `hooks run <id>` body — reads payload on stdin, writes a captured_events row, always exits 0
    templates.py     # canonical hooks block emitted by `hooks install`
  ingest/
    writer.py        # INSERT INTO messages + normalize+insert hook + messages_YYYYMM partition routing (Wave 5) + claude-teams materialize call
    enumerate.py     # Discovery wrapper around all registered adapters
    __init__.py      # run_ingest(conn, adapters)
  infra/
    costs.py         # compute_cost(tokens, model, provider, *, speed) → dict
    cache.py         # TieredCache — hot LRU + cold disk JSON
    currency.py      # Frankfurter live + 24h cache + ECB snapshot fallback
    cursor_cache.py  # Fingerprint cache for vscdb (3-8× cold-start speedup)
    discovery.py     # Filesystem scan helpers (legacy file-scan path — distinct from services/discovery.py)
    providers/       # Per-provider Pricers (anthropic, openai, cursor, etc.)
  mcp/
    server.py        # FastMCP server; 8 tools — session_query/list_sessions/list_projects + 3 discovery + 2 outcome  ← +5 post-v0.7
    store_reader.py  # Read-only store helpers shared with the MCP server
  reports/           # CLI report renderers (text/json/csv) + optimize patterns (mart-backed detectors via message_tool_mart)
  routes/            # FastAPI routes (one file per concern, 16 of them)
    cfg.py compare.py context_budget.py cost.py data.py etl.py
    export.py optimize.py plan.py projects.py sessions.py yield_route.py
    bookmarks.py commands.py misc.py qa.py search.py tags.py
    playback.py agent_teams.py                                                # ← post-v0.7
  services/          # compare, plans, yield_tracker, pricing, search, qa, tags, bookmarks
    discovery.py     # discovery / outcome queries (shared by CLI + MCP); SessionMatch/OutcomeMatch/BudgetedResult; pack_within_budget   ← post-v0.7
    discovery_telemetry.py  # loaded_count/cited_count recording + demote-uncited sweep                                                   ← post-v0.7
    skill_synth.py   # mine the store for project workflow patterns → auto-* SKILL.md files                                               ← post-v0.7
    playback.py      # per-session / per-project tool-call timeline (read-side over messages.raw_json + captured_events)                   ← post-v0.7
    agent_teams.py   # Claude Code agent-team graph — v013-JOIN-backed, falls back to is_sidechain/raw_json heuristic                      ← post-v0.7
  skills/                                                         ← post-v0.7 — static SKILL.md files shipped with the package
    check-prior-work/  find-related-sessions/  recall-past-decisions/   # one SKILL.md each
  store/
    schema.py        # CURRENT_VERSION = 13; applies SQL + .py migrations idempotently; _ADD_COLUMN_GUARDS for v003/v012/v013
    queries.py       # Typed query helpers (one place for all SQL)
    mart_queries.py  # Read helpers used by route migrations (Wave 3A/4A/5A + message_tool_mart helpers)
    db.py types.py
    migrations/      # v001 → v013 (v005 + v008 are .py, rest are .sql)
  cli.py server.py deps.py settings.py __version__.py

stackunderflow-ui/    # React dashboard (Vite); output → ../stackunderflow/static/react/
  src/
    pages/           # Overview, ProjectDashboard, Settings (Settings has the "Backfill now" button wired to POST /api/etl/backfill)
    components/
      common/         FilterBar, EtlStatusBadge (shows backfill / failure state), ExportButton, ...
      dashboard/      one Tab per top-level view (Overview/Sessions/Cost/Compare/Yield/Playback*/Agents*/...; * = beta-flagged)
      cost/           # Cost-tab widgets including CostByProviderCard
      analytics/, charts/, layout/, qa/
    services/        # API client (incl. EtlBackfillInProgressError) + format/currency/filters/providerStyle/navigation helpers
    types/api.ts     # Backend response shapes mirrored as TypeScript

tests/                # backend tests; integration/ has the slow-marker e2e + perf
docs/
  HANDOFF.md         # This file
  specs/             # Architecture specs (multi-provider, etl, agent-teams, messages-partitioning, ...)
  cli-reference.md  api-reference.md  mcp.md  skills.md  hooks.md  multi-provider.md  beta-normalizer-drift.md  ...
```

---

## Recent history (v0.5 → now)

| Tag | Date | Highlights |
|---|---|---|
| v0.5.0 | 2026-04-30 | All 16 codeburn-catalog providers as adapters; 4 default-on (claude/codex/cursor/cline) + 12 beta-flag-gated |
| v0.6.0 | 2026-05-01 | Currency, export, model aliases, plan budgets, compare, yield, optimize patterns, context budget, fast-mode SQLite, streaming reader, cursor cache. Multi-provider Python API + MCP. Cursor v3 conversationId fix. UI surfaces wired |
| v0.6.1 | 2026-05-01 | Currency snapshot fallback, cursor pricing for `composer-*`, per-workspace cursor slugs, `<synthetic>` cleanup, defensive adapter coverage |
| v0.6.x patches | 2026-05-04 to 2026-05-05 | Provider/model FilterBar URL-synced, `formatModelName` normalizer, `Annotated[..., Query()]` filter binding fix, non-blocking startup ingest, `bulk_*` SQL helpers replacing N+1 in `/api/projects` |
| v0.7.0 | 2026-05-06 | ETL pipeline (Waves 1–4): usage_events + 5 marts + watermarked refresh + filesystem watcher + every dashboard route migrated to mart reads + status surface + UI badge |
| **Wave 5 (unreleased)** | **2026-05-06** | tool_mart + command_mart (v007) + POST /api/etl/backfill route + watcher single-instance lock + messages_YYYYMM partitioning behind UNION view (v008, NOT auto-applied) + 13 beta normalizers validated against real-shape fixtures + 1 copilot drift fix |
| **post-v0.7 specs 01–10 (unreleased)** | **2026-05-12** | Self-referential discovery for coding agents (5 CLI commands + 5 MCP tools), token-budgeted discovery output, outcome-aware discovery, auto-generated skills (`skills generate/list/clean`) + 3 static SKILL.md files shipped, citation-feedback telemetry (`discovery_telemetry`, v009), opt-in Claude Code lifecycle hooks (`captured_events`, v010), `message_tool_mart` per-message-grain mart + mart-backed optimize detectors (v011), `tool_mart.calls_total` (v012), multi-agent (agent-teams) FS recognition + `/api/agent-teams` + Agents tab (v013), per-session playback timeline + `/api/playback` + Playback tab. Schema `CURRENT_VERSION = 13` |

---

## What changed in the Wave 5 follow-ups (merged, unreleased)

The five HANDOFF follow-ups deferred from v0.7.0 all landed together. See `CHANGELOG.md` for the deep dive.

### New tables (migrations v007 + v008)
- `v007_lower_grain_marts.sql` — `tool_mart` (per-tool, additive) + `command_mart` (per-command, additive)
- `v008_messages_partitioning.py` — `messages` becomes a VIEW over `messages_YYYYMM` partitions (+ `messages_unknown` fallback) backed by `_messages_id_seq` and an INSTEAD OF INSERT trigger; FK on `usage_events.source_message_fk` dropped (SQLite can't FK to a view; UNIQUE dedup index preserved)

### New abstractions / modules
- `etl/lock.py` — POSIX `fcntl.flock` + Windows `msvcrt.locking` watcher single-instance lock with stale-PID detection
- `etl/backfill_jobs.py` — process-local lock + single-slot job state for the new POST route
- 2 new `MartBuilder` subclasses (`tool.py`, `command.py`) wired into the existing last-wins registry

### New surfaces
- `POST /api/etl/backfill` returning `202 {job_id, started_at}` or `409 {error, job_id}` if a job is already in-flight
- `current_job` block on `/api/etl/status`
- `watcher.lock_held_by` PID on `/api/etl/status` + CLI text output
- `stackunderflow start --no-lock` flag (sets `STACKUNDERFLOW_DISABLE_LOCK=1`)
- Settings page "Backfill now" button now hits the real route (replaces the v0.7.0 CLI-fallback display)
- `EtlStatusBadge` poll cadence drops to 2 s while a backfill is running

### Routes migrated to mart reads (continuing from Wave 4)
- `/api/cost-data` `tool_costs` block now overlays from `tool_mart` (empty-mart fallback preserved)
- `/api/optimize` detectors `bash_output_limits`, `junk_reads`, `low_read_edit_ratio`, `ghost_agents` gain a `tool_mart` fast-path filter — instant return on project-scoped windows that didn't use the implicated tool (the per-message-grain replacement, `message_tool_mart`, landed in the post-v0.7 round; see below)

### Beta normalizer validation
All 13 beta normalizers (registry has 13, not 12 — `omp` aliases `pi`) graded against real-shape fixtures matching the codeburn catalog spec. Result: 12 ✅ matching, 1 ⚠️ drift fixed (copilot model-priority bug — see drift report), 0 ❌ broken. Drift report at `docs/beta-normalizer-drift.md`.

### v008 NOT auto-applied
The maintainer's real `~/.stackunderflow/store.db` is still on `user_version = 6`. Apply v008 (and the later migrations) manually per the documented rollout in `docs/specs/messages-partitioning.md`: backup → apply on `/tmp/store.test.db` copy → verify counts (`view total == sum(partition counts)`) → spot-check dashboard against the copy → swap.

---

## What changed since v0.7.0 (the spec-01…10 round, unreleased)

Ten specs landed together on `feat/post-0.7.0`. They turn the store from a passive cost dashboard into something a coding agent queries reflexively before doing work, plus close out the Wave 5 follow-up backlog. All migrations are **additive** — no existing table is mutated. `schema.CURRENT_VERSION = 13`.

### Self-referential discovery for coding agents (specs 01, 02, 03, 04)
- **5 CLI commands + 5 MCP tools** (`stackunderflow/services/discovery.py` is the shared core):
  - `find-sessions-in-path PATH` / `find_sessions_in_path` — sessions whose project root is `PATH` or an ancestor (ancestor-only).
  - `find-sessions-touching-file FILE` / `find_sessions_touching_file` — sessions whose tool calls referenced `FILE`; `--mode read|write|any`.
  - `search-past-decisions QUERY` / `search_past_decisions` — substring search across transcripts + tool args; each result carries a snippet.
  - `find-sessions-where-action-worked ACTION` / `find_sessions_where_action_worked` — sessions where `ACTION` was done *and the next user turn confirmed it*; result carries `outcome="worked"` + `outcome_evidence` + `outcome_msg_id`.
  - `find-failure-modes-for-file FILE` / `find_failure_modes_for_file` — sessions where editing `FILE` led to a revert / complaint; `outcome` ∈ `{failed, reverted}`.
- **Token-budgeted output (spec 03).** The first three are *budget-aware*: within the `--limit` hard cap, results are ranked (recency + cost + relevance, weights from `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS=0.5,0.2,0.3`) and packed greedily by `pack_within_budget` until ~`--context-budget` estimated tokens (chars/4) are used. Default `--context-budget` = `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` or `2000`; `0` disables. JSON always carries `_budget_used_tokens` / `_budget_max_tokens`; `_truncated` / `_more_available` appear when rows were dropped. The 2 outcome commands are *not* budget-aware. `BudgetedResult` / `SessionMatch` / `OutcomeMatch` dataclasses live in `services/discovery.py`.
- **Citation-feedback telemetry (spec 04 — `v009_discovery_telemetry.sql`).** New `discovery_telemetry` table (session ids + `loaded_count` / `cited_count` / `first_loaded_ts` / `demoted` — no transcript content). The three discovery commands bump `loaded_count` for every session they surface; `session_query` (MCP) bumps `cited_count` when an agent looks one up. `cite_rate = cited_count / loaded_count` feeds the ranking so cited sessions climb and uncited noise sinks. `stackunderflow discovery telemetry` inspects the table; `stackunderflow discovery demote-uncited [--dry-run] [--min-loads N] [--min-age-days M]` flags sessions surfaced N+ times over M+ days that were never cited (zeroes their cite-rate term; they stay reachable via direct lookup). Recording is gated behind `STACKUNDERFLOW_DISCOVERY_TELEMETRY` (default on).

### Auto-generated skills (spec 02)
- **`stackunderflow skills generate / list / clean`** (`stackunderflow/services/skill_synth.py`). Mines `messages` in the local store for project-specific workflow patterns — `canonical-test-command`, `always-runs-X-after-Y`, `uses-tool-flag-combo`, `avoids-X`, `never-touches-paths` — and emits Claude Code `SKILL.md` files. Pure regex, no network. **Project-scoped by default — no `--all-projects` mode**; cross-project mining is opt-in via `--scope user` with an explicit `--project`/`--projects` allowlist. Generated skills land under `<project>/.claude/skills/auto-*/` (or `~/.claude/skills/` for `--scope user`) — never into the package, never released (`pyproject.toml` `hatch.build.exclude` blocks `**/auto-*/SKILL.md`; `.gitignore` ignores `.claude/skills/auto-*/`). Idempotent: `.bak` written before any overwrite; never clobbers a hand-authored skill (skips any target whose `SKILL.md` isn't marked `auto_generated: true`).
- **3 static `SKILL.md` files shipped** under `stackunderflow/skills/`: `check-prior-work` (fires `find-sessions-in-path` on the first substantive task in a project), `find-related-sessions` (fires `find-sessions-touching-file` on a file mention), `recall-past-decisions` (fires `search-past-decisions` on a past-decision reference). Install today is a manual `cp -r stackunderflow/skills/* ~/.claude/skills/` — see `docs/skills.md`.

### Opt-in Claude Code lifecycle hooks (spec 05 — `v010_captured_events.sql`)
- **`stackunderflow/hooks/`** + **`stackunderflow hooks install / uninstall / status / repair / run`**. Nothing is installed unless the user runs `hooks install`. It merges StackUnderflow's hook entries (`PostToolUse` / `UserPromptSubmit` / `Stop` / `PreCompact`) into a `settings.json` — `<git-root>/.claude/settings.json` for `--scope project` (default) or `~/.claude/settings.json` for `--scope user` — idempotently, backing up first, never touching another tool's hooks. Each interesting event writes one row to the new `captured_events` table: `failure` (non-zero Bash exit), `correction` (a user prompt matched the correction heuristic — the prompt text is *not* stored), `boundary` (Stop — session-totals snapshot), `snapshot` (pre-compaction). `payload_json` is sanitised metadata by default; `--capture-content` stores the full payload (prompt text, tool output). `hooks run <id>` is the one command Claude Code itself invokes (reads the payload on stdin, always exits 0). `hooks repair --scope all` walks `$HOME` (bounded depth ≤ 8, prunes `node_modules`/`.git`/etc, never follows symlinks) rewriting stale hardcoded paths back to the portable `stackunderflow hooks run <id>` form. The outcome-aware discovery commands (spec 01) read `captured_events` for a deterministic success/failure flag, falling back to their transcript heuristic when the table is empty (hook-less installs) — no producer dependency. See `docs/hooks.md`.

### `message_tool_mart` + mart-backed optimize detectors (spec 07 — `v011_message_tool_mart.sql`)
- New per-`(message_id, tool_name, call_index)`-grain mart (`stackunderflow/etl/marts/message_tool.py`). The `optimize.py` detectors (`junk_reads`, `bash_output_limits`, `low_read_edit_ratio`, `ghost_agents`) needed finer signal than the aggregate marts carry — per-file-path, per-byte-count, per-tool-call sequences — and used to re-parse `messages.raw_json` directly (a scan that fans out across every monthly partition post-v008). The mart replaces that with a single indexed lookup. `byte_count` = write-payload size for write-family tools (Write→content, Edit→new_string, MultiEdit→Σ new_string, NotebookEdit→new_source), tool-*result* size (from the following message's matching `tool_result` block) for output-producing tools (Bash, Read, Grep, …); `file_path` = the obvious input key, or `subagent_type` for `Task`. Per-entity mart (each `usage_events` row → 0..N mart rows); watermarks on `usage_events.id`.

### `tool_mart.calls_total` (spec 08 — `v012_tool_mart_calls_total.sql`)
- New `tool_mart.calls_total` column alongside `event_count`. `event_count` is the *distinct* `(message, tool)` pair count (a turn that calls `Read` 3× contributes `event_count += 1` — the 1/N-attribution contract); `calls_total` restores the *non-distinct* count (`+= 3`) the pre-Wave-5 aggregator's `tool_costs` block reported. `cost_usd` stays 1/N-attributed (cost must not double for repeated calls in one turn). `/api/cost-data` `tool_costs` rows now include `calls_total`. Existing `tool_mart` rows get `calls_total = 0` via the DEFAULT until a `stackunderflow etl backfill --force` re-derives both columns; that stale-zero window is self-healing and acceptable (no consumer needed `calls_total` before this migration).

### Multi-agent (agent-teams) FS recognition (spec 09 — `v013_multi_agent_session_metadata.sql`)
- Materialises the Claude Code parallel-agent topology in the schema instead of reconstructing it by scanning `messages.is_sidechain` + parsing `raw_json` on every render. Four additive nullable columns on `sessions` (`team_id`, `spawned_by_session_id`, `spawn_prompt`, `agent_role` ∈ `{lead, subagent, NULL}`) + a new `agent_teams` table (one row per team — name, project, created-at, description, lead session, raw `config.json`). Populated by `adapters/claude_teams.py:materialize_team_metadata()` on each ingest cycle (ingests `~/.claude/teams/` + `~/.claude/tasks/`, never touched before); not auto-backfilled — sessions ingested before this migration keep NULL team metadata until the next ingest. Non-Claude adapters leave the columns NULL. The `/api/agent-teams` routes (list + per-session graph + per-agent transcript, in `routes/agent_teams.py` via `services/agent_teams.py`) are now indexed-JOIN-backed and the response bodies carry `spawn_prompt` / `agent_role` / team `description` (all `null` on stores whose team artefacts aren't ingested yet — the service falls back to the heuristic there). New beta-flagged "Agents" dashboard tab. See `docs/specs/agent-teams.md`.

### Per-session playback timeline (spec 10)
- New read-only `GET /api/playback/{session_id}` (ordered tool-call event stream for one session; params `tool_filter=Edit,Write`, `limit` ≤ 10000, `include_payload` default on → 200-char excerpts) and `GET /api/playback/project/{project_slug}` (cross-session timeline; params `since=7d`-or-ISO, `tool_filter`, `limit` ≤ 20000, `include_payload` default *off* — a project-wide stream is large). `routes/playback.py` over `services/playback.py`; pure read-side over `messages.raw_json` (+ the optional `captured_events` table for an authoritative success/failure flag) — **no schema migration**. 404 when the session/slug isn't in the store; 200 with empty `events` when it exists but issued no tool calls. New beta-flagged "Playback" dashboard tab. v1 = event stream only; v2 (virtual-filesystem reconstruction) is later. See `docs/specs/messages-partitioning.md` for the partitioning context this read path lives on.

### New / changed env vars
- `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` (config-key-backed, default `2000`) — default `--context-budget` for the budget-aware discovery commands / MCP tools.
- `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS` (config-key-backed, default `0.5,0.2,0.3`) — discovery-ranking weights `recency,cost,relevance`; malformed → default.
- `STACKUNDERFLOW_DISCOVERY_TELEMETRY` (standalone, default on) — set to `0` to disable the passive citation-feedback recording.
- `STACKUNDERFLOW_DISABLE_LOCK` (standalone, default unset) — skip the watcher single-instance lock (same as `start --no-lock`); landed in Wave 5, documented here for completeness alongside `STACKUNDERFLOW_DISABLE_WATCHER`.

---

## What changed in v0.7.0 (the ETL push)

*Historical snapshot of the v0.7.0 release itself. Wave 5 (above) added marts 6–7 + the backfill route + the lock; the post-v0.7 round added mart 8 + the discovery / skills / hooks / playback / agent-teams surfaces. Read the two "What changed" sections above this for the current picture.*

### New tables (migration `v006_etl_layer.sql`)
- `usage_events` (canonical fact, `UNIQUE(source_message_fk)` for dedup)
- `daily_mart`, `session_mart`, `project_mart`, `provider_day_mart`, `model_day_mart`
- `mart_watermark`

### New abstractions
- `Normalizer` ABC + 16 subclasses (per-provider transforms)
- `MartBuilder` ABC + 5 subclasses (per-mart rollup logic)
- Two last-wins registries (`stackunderflow.etl.normalize` and `stackunderflow.etl.marts`)
- Watermark helpers (`get_watermark`, `set_watermark`, `refresh_all_marts`)
- `BackfillReport` dataclass + `backfill(conn, *, force=False)` orchestrator

### New surfaces
- `GET /api/etl/status` returning `{watcher, marts, events, lag_seconds, health}`
- `stackunderflow etl status [--format text|json]` CLI
- `stackunderflow etl backfill [--force]` CLI (now actually populates events)
- `EtlStatusBadge` in the dashboard header
- "ETL pipeline" section on `/settings` with "Backfill now" button

### Routes migrated to mart reads
- `/api/projects?include_stats=true`
- `/api/dashboard-data`
- `/api/cost-data` (totals/by_day/by_model blocks)
- `/api/cost-data/by-provider`
- `/api/compare`
- `/api/yield`
- `/api/optimize` (cache_overhead detector only — others stay on aggregator path because they need per-message text)
- `/api/messages/summary`

Empty-mart fallback to the aggregator preserved per route — so the JSON contract is unchanged whether marts are populated or empty.

### Latency on real data
- 247K-message store; before: dashboard cold-load 2.5–2.8 s warm
- After: per-route warm latencies range 1.1 ms (cost-by-provider) to 100 ms (optimize). Median <10 ms.

### Watcher
- ~155 ms end-to-end smoke-tested on the maintainer's `~/.claude/projects` (well under the 400 ms target)
- `stackunderflow start` now non-blocking — HTTP binds in <1 s, ingest runs in a daemon thread

---

## Key gotchas + design decisions

### Migration numbering
The ETL migration shipped as `v006_etl_layer.sql` (v004 + v005 were already taken — synthetic-models cleanup + cursor-workspace redistribute). The post-v0.7 round added v007–v013, all **additive** (no existing table mutated): v007 lower-grain marts, v008 messages partitioning, v009 `discovery_telemetry`, v010 `captured_events`, v011 `message_tool_mart`, v012 `tool_mart.calls_total`, v013 multi-agent session metadata + `agent_teams`. `schema.CURRENT_VERSION = 13`. `schema.apply` runs every migration whose number exceeds `PRAGMA user_version`, so they chain correctly regardless of merge order; `_ADD_COLUMN_GUARDS` in `schema.py` makes the `ALTER TABLE ADD COLUMN` migrations (v003, v012, v013) idempotent against a crash mid-migration.

### Empty-mart fallback
Every migrated route checks if its mart is populated. If yes → mart read. If no → original aggregator path. So the dashboard works even on a fresh install before backfill runs. After a backfill or a single watcher cycle, marts populate and the fast path takes over automatically. The post-v0.7 routes (`/api/playback`, `/api/agent-teams`) are pure read-side over `messages.raw_json` and don't have an empty-mart fallback to worry about — `/api/agent-teams` does fall back from the v013 JOIN to the `is_sidechain`/`raw_json` heuristic when the team artefacts aren't ingested.

### Cost is computed once
`cost_usd` lives on every `usage_events` row. Marts SUM it, never re-apply rate cards. Currency conversion stays at the API boundary (already correct from v0.6.0). When pricing changes, re-normalize from raw messages — one code path.

### `session_count` correctness across windows
Additive marts (daily, provider_day, model_day) can't simply SUM `COUNT(DISTINCT session_id)` across refresh windows (the same session can appear in two windows). Solution: after the additive INSERT...ON CONFLICT, a follow-up UPDATE recomputes `session_count` from `usage_events` for affected keys. Bounded by number of distinct keys in the window — typically O(1)..O(few dozen). Tests lock this in.

### Per-entity vs additive marts
- `session_mart` and `project_mart` use INSERT OR REPLACE over a re-aggregated subquery for affected entities (totals stay correct when new events arrive for an existing session).
- `daily_mart`, `provider_day_mart`, `model_day_mart` use INSERT...ON CONFLICT DO UPDATE additively (because the same `(day, …)` key never appears in two refresh windows once the watermark moves forward).

### Normalizer / mart registries are in `__init__.py`
Per spec — the registries live in `stackunderflow/etl/normalize/__init__.py` and `stackunderflow/etl/marts/__init__.py`, NOT in `base.py`. Last-wins (re-registering overwrites). `_clear()` for tests. The 16 normalizer + 8 mart-builder default registrations happen at package-import time via top-level `register("name", Cls)` calls. `stackunderflow.etl.marts.all()` should list 8 keys; `stackunderflow.etl.normalize.all()` 16.

### Watcher + single-instance lock
Uses `watchfiles` (Rust-backed). Daemon thread spawned in lifespan. Catches every exception so a bad event never poisons the loop. `--no-watcher` / `STACKUNDERFLOW_DISABLE_WATCHER=1` for headless mode. Since Wave 5 a `fcntl`/`msvcrt` lock at `~/.stackunderflow/server.lock` (`etl/lock.py`) makes a second `stackunderflow start` against the same store serve HTTP without running the watcher — stale-PID detection via `os.kill(pid, 0)` clears a dead recorder's metadata; `--no-lock` / `STACKUNDERFLOW_DISABLE_LOCK=1` skips the gate.

### Discovery: two modules named `discovery.py`
`stackunderflow/infra/discovery.py` is the legacy filesystem-scan helper (walks `~/.claude*` JSONL). `stackunderflow/services/discovery.py` is the post-v0.7 store-backed query layer (`SessionMatch` / `OutcomeMatch` / `BudgetedResult`, `pack_within_budget`, the 5 `find_sessions_*` / `search_past_decisions` functions shared by the CLI commands and the MCP tools). They're unrelated — don't confuse them.

### Auto-generated skills never ship in the package
`stackunderflow skills generate` writes to `<project>/.claude/skills/auto-*/` (or `~/.claude/skills/` for `--scope user`) — never into `stackunderflow/skills/` (which holds only the 3 hand-authored static SKILL.md files). `pyproject.toml`'s `hatch.build.exclude` blocks `**/auto-*/SKILL.md` from the wheel/sdist; `.gitignore` ignores `.claude/skills/auto-*/`. This is a hard constraint.

---

## How to run / what to know

```bash
# Run the dashboard
stackunderflow start                    # binds 127.0.0.1:8095
                                          # ingest + watcher run in background
stackunderflow start --no-watcher        # headless / profiling — no fs watcher
stackunderflow start --no-lock           # skip the watcher single-instance lock

# ETL ops
stackunderflow etl status                # health + watermarks (+ current_job / lock_held_by)
stackunderflow etl status --format json
stackunderflow etl backfill              # incremental (skips already-converted msgs)
stackunderflow etl backfill --force      # drops events + marts, rebuilds (re-derives tool_mart.calls_total too)

# Discovery (also exposed as MCP tools — see docs/mcp.md, docs/cli-reference.md)
stackunderflow find-sessions-in-path "$(pwd)" --since 30d
stackunderflow find-sessions-touching-file path/to/file.py --mode write
stackunderflow search-past-decisions "watchfiles inotify"
stackunderflow find-sessions-where-action-worked "add caching" --file stackunderflow/infra/cache.py
stackunderflow find-failure-modes-for-file stackunderflow/routes/cost.py

# Auto-generated skills (docs/skills.md) — project-scoped by default
stackunderflow skills generate --dry-run --format json
stackunderflow skills list
stackunderflow skills clean --older-than 30d        # preview; --yes to delete

# Opt-in Claude Code hooks (docs/hooks.md) — nothing installs until you run this
stackunderflow hooks install                        # --scope project (cwd's git root) by default
stackunderflow hooks status
stackunderflow hooks repair --dry-run

# Discovery telemetry
stackunderflow discovery telemetry
stackunderflow discovery demote-uncited --dry-run

# Tests
pytest tests/ -q                         # fast suite (default)
pytest -m slow tests/stackunderflow/integration -q   # slow e2e + perf
ruff check stackunderflow/

# Frontend
cd stackunderflow-ui
npm run typecheck
npm run build                             # output → ../stackunderflow/static/react/
node --test tests/services/*.test.ts      # frontend unit tests (Node built-in runner; no vitest dep)
```

---

## Real-data state right now (maintainer's machine)

```
~/.stackunderflow/store.db (~1.9 GB):
  user_version: 6   ← v007 through v013 all ship on feat/post-0.7.0 but are UNAPPLIED here
                       pending manual rollout (see docs/specs/messages-partitioning.md)
  ~150K usage_events
  Marts populated for the 5 v0.7.0 marts (daily / session / project / provider_day / model_day),
  watermarks in sync.
  tool_mart / command_mart / message_tool_mart : not yet present (need v007/v011 applied + a backfill).
  agent_teams table + the 4 new `sessions` columns : not yet present (need v013).
  discovery_telemetry / captured_events : not yet present (need v009 / v010).
  Per-provider events: claude ~150K, cursor ~220, cline ~103.
```

**Practical upshot for an incoming agent:** on the maintainer's box every post-v0.7 feature that depends on a v007+ migration is *dormant* until the manual rollout runs. The code is all there and passes tests against `tmp_path` / `:memory:` stores; it just hasn't been applied to the 1.9 GB production store yet. Don't be surprised when `stackunderflow etl status` against the real store doesn't show `tool` / `command` / `message_tool` rows, or when `/api/agent-teams` falls back to the heuristic path.

---

## What's left / known follow-ups

Wave 5 follow-ups #2 (`message_tool_mart` — spec 07), #5 (`last_job` retention — `LAST_JOB_TTL_SECONDS`), and #6 (`tool_mart.calls_total` — spec 08) are now **closed** by the post-v0.7 round. The remaining live items:

| # | Item | Severity |
|---|---|---|
| 1 | **Apply v007–v013 to the real `~/.stackunderflow/store.db`.** They all ship on `feat/post-0.7.0` but none are auto-applied (1.9 GB store, manual review preferred). Follow the rollout in `docs/specs/messages-partitioning.md`: backup → apply on `/tmp/store.test.db` copy → verify counts (`view total == Σ partition counts`) → spot-check dashboard → swap. Until then the maintainer's store stays on `user_version = 6` and every post-v0.7 feature that needs a v007+ migration is dormant. | medium (not blocking; do when comfortable) |
| 2 | Migrate `optimize` per-message detectors fully onto `message_tool_mart`. The mart exists (v011) and the detectors gained a `tool_mart` fast-path filter, but on a populated store the full `messages.raw_json` re-parse still runs for the per-file/per-byte signals — wiring those reads to the new mart is the remaining work. | low (fast enough today) |
| 3 | Beta normalizer pricing-table coverage gap: `qwen-coder-plus` and `gemini-1.5-pro` (and probably others in the long tail) aren't in the canonical RATE_CARD, so they emit `cost_source=unknown`. Adapter/normalizer code is correct — it's a pricing-table population question. Track the missing models per provider and fold them in. | low (correctness for enabled betas) |
| 4 | Watcher / lock cross-platform coverage. POSIX `fcntl.flock` is smoke-tested on macOS. Windows `msvcrt.locking()` code path is written but **not verified on a real Windows box**. Linux watcher (watchfiles cross-platform) also untested on real data. Most users are on macOS so low urgency, but worth a CI matrix run before claiming Windows support. | low |
| 5 | `command_costs` per-Interaction block in `/api/cost-data` stays on the aggregator path — its shape doesn't fit `(day, project, command_name)` aggregation. `command_mart_for_project` is wired and ready; if you want a per-command-NAME rollup surfaced, route it. | low (current path works) |
| 6 | Beta normalizer fixtures (`tests/fixtures/beta_normalizers/`) are synthetic-but-spec-accurate per the codeburn catalog. They don't replace real-world parity — that needs actual session data per provider on the maintainer's machine, which most beta providers don't have. The defensive empty/malformed coverage from v0.6.1 + the spec-shape coverage from Wave 5 catch the structural failure modes; the next "Cursor v3 conversationId-in-the-key" still needs real local data. | low (no concrete bug today) |
| 7 | `stackunderflow init --install-skills` — auto-installing the 3 static `SKILL.md` files (idempotent copy, `--force` for upgrades). Today the install is a manual `cp -r stackunderflow/skills/* ~/.claude/skills/` per `docs/skills.md`. | low (UX nicety) |
| 8 | Playback v2 — the v1 surface (`/api/playback/*` + the Playback tab) is event-stream-only; v2 (virtual-filesystem reconstruction at a point in time) is sketched but not built. Beta-flagged today. | low (v1 covers the immediate need) |
| 9 | `outcome` inference quality. The outcome-aware discovery commands prefer the deterministic `captured_events` table but fall back to a transcript heuristic on hook-less installs ("no complaint before the session ended" can false-positive). Worth tightening the heuristic, or nudging users toward `stackunderflow hooks install`. | low |
| 10 | Discovery search is plain `LIKE` substring — no embeddings / fuzzy. A spec-noted "opt-in `--use-llm` refinement pass" for `skills generate` and a semantic search mode for `search-past-decisions` are both candidates if the substring matcher proves too brittle in practice. | low (substring works for distinctive keywords) |

---

## Files an incoming agent should read first

1. `docs/cli-reference.md` — every `stackunderflow` command, flag-for-flag (now incl. discovery / skills / discovery-telemetry / hooks)
2. `docs/mcp.md` — the 8 MCP tools (3 legacy + 3 discovery + 2 outcome) + the citation-feedback loop
3. `docs/skills.md` — the 3 static SKILL.md files + the `skills generate/list/clean` synthesis + the hard guardrails
4. `docs/hooks.md` — the opt-in Claude Code lifecycle hooks (what's captured, the privacy model, the Windows status)
5. `docs/specs/etl-architecture.md` — design contract for the pipeline
6. `docs/specs/messages-partitioning.md` — v008 design + rollback + ops rollout (the doc the manual migration walk-through lives in)
7. `docs/specs/agent-teams.md` — the multi-agent FS-recognition design (v013)
8. `docs/beta-normalizer-drift.md` — per-provider verdicts from the Wave 5 normalizer audit
9. `stackunderflow/services/discovery.py` — `SessionMatch` / `OutcomeMatch` / `BudgetedResult`, `pack_within_budget`, the 5 shared discovery / outcome query functions
10. `stackunderflow/etl/normalize/base.py` + `stackunderflow/etl/marts/base.py` — `Normalizer` / `MartBuilder` ABCs
11. `stackunderflow/etl/backfill.py` + `backfill_jobs.py` + `lock.py` + `watcher.py` — orchestrator, the backfill-job slot, the single-instance lock, the watchfiles dispatch
12. `stackunderflow/store/migrations/v006_etl_layer.sql` … `v013_multi_agent_session_metadata.sql` — the schema progression (every migration header documents its own rationale)
13. `stackunderflow/store/mart_queries.py` — every read helper used by routes
14. `stackunderflow/ingest/writer.py` — partition routing helpers + the claude-teams materialize call
15. Any `routes/*.py` for the JSON contracts the dashboard depends on (`playback.py` / `agent_teams.py` are the newest)
16. `tests/stackunderflow/integration/` — e2e + perf regression — the most useful single dir to understand the whole pipeline at once

---

## Conventions worth knowing

- **No version bumps without `CHANGELOG.md` + git tag + GitHub release** — done together as one PR (`release: 0.7.0`)
- **No codeburn attribution** in shipped code (the project is a clean rewrite; references stayed in `docs/specs/multi-provider/` only)
- **No backwards-compat shims** — when an API shape changes, change the consumers in the same PR
- **Tests must run on Linux CI** (no macOS-only paths in non-platform-specific tests)
- **Beta adapters opt in via `STACKUNDERFLOW_BETA_<NAME>=1`** — never on by default
- **Frontend tests use `node --test`** (Node 22+ built-in runner) not vitest — no new dev dep
- **Idempotent EVERYTHING in ETL** — every `refresh`, `backfill`, `watcher cycle` must be safe to re-run
- **The user's `~/.stackunderflow/store.db` is sacred** — tests use `tmp_path` or `:memory:`, never the real store
- **Settings file:** `~/.stackunderflow/config.json` (not `settings.json`); the descriptor pattern in `settings.py` resolves env → file → default

---

## When something breaks

| Symptom | Likely cause | Where to look |
|---|---|---|
| `/api/etl/status` shows lag > 1000 events for minutes | Watcher not running, or normalizer raising | `stackunderflow start` log; `stackunderflow etl status` |
| Marts empty after backfill | Normalizer / mart-builder for that provider/mart not registered | `stackunderflow.etl.normalize.all()` should list 16 keys; `stackunderflow.etl.marts.all()` 8 |
| `tool` / `command` / `message_tool` marts missing on the real store | v007 / v011 not applied yet (maintainer's box is on `user_version = 6`) | Run the manual rollout, then `stackunderflow etl backfill --force` |
| `/api/agent-teams` returns heuristic-only data | v013 not applied, or `~/.claude/teams/` artefacts not ingested | Apply v013; re-ingest so `claude_teams.materialize_team_metadata()` runs |
| `stackunderflow hooks run` did nothing | Expected — it always exits 0; if no row appeared, the event didn't match a handler heuristic, or `captured_events` couldn't be created | `stackunderflow.hooks.handlers`; `stackunderflow hooks status` |
| Discovery commands return nothing on a fresh store | Store empty — no `messages` ingested yet | `stackunderflow etl backfill` first; `stackunderflow etl status` |
| Dashboard cost = $0 for a provider | Pricer for the model returns `None` | `infra/providers/<provider>.py` `rates_for()` |
| Watcher spammed log: "Adapter raised in cycle" | Provider adapter has a parse bug | Look at the provider's adapter, run `adapter.read()` directly on the file |
| `health: error` in status | Mart watermark stuck + watcher dead | Restart server; `stackunderflow etl backfill --force` if persistent |
| Second `stackunderflow start` doesn't refresh data | First instance holds the watcher lock — by design (it still serves HTTP) | `stackunderflow etl status` (`watcher.lock_held_by`); `--no-lock` to override |
| New install, dashboard slow | Marts empty, fallback to aggregator. Run `stackunderflow etl backfill` to populate | One-time, then watcher keeps it fresh |

---

## What I'd do next if I had a week

1. **Apply v007–v013 to the maintainer's real store + verify on the dashboard.** Walk the documented rollout (`docs/specs/messages-partitioning.md`) end-to-end against the 1.9 GB store, then `stackunderflow etl backfill --force` to populate `tool_mart` / `command_mart` / `message_tool_mart` (+ `tool_mart.calls_total`), and re-ingest so the v013 team metadata materialises. Measure read-fanout cost on the live dashboard. This unblocks every dormant post-v0.7 feature on the production box.
2. **Finish migrating `optimize` onto `message_tool_mart`.** The mart exists (v011); wire the per-file / per-byte detector reads to it so a populated store stops re-parsing `messages.raw_json` (which fans out across monthly partitions post-v008). Closes follow-up #2.
3. **Pricing-table population sweep for the long-tail betas.** Audit which models the 12 beta providers emit in real-world fixtures and add the missing entries to the canonical RATE_CARD. Closes the `cost_source=unknown` cosmetic gap on `qwen-coder-plus`, `gemini-1.5-pro`, and friends. Probably 30–50 model entries across the 12 providers.
4. **Cross-platform CI matrix for the watcher + lock.** Linux + Windows runners. Smoke-test `watchfiles` source-file detection latency, smoke-test `msvcrt.locking` lock acquisition, document the gaps. Closes follow-up #4.
5. **`stackunderflow init --install-skills`.** Auto-install the 3 static `SKILL.md` files (idempotent copy into `~/.claude/skills/`, `--force` for upgrades) so the discovery skills aren't a manual `cp`. Closes follow-up #7.

---

That's the picture. Files referenced are absolute paths under `/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow/`. Welcome.
