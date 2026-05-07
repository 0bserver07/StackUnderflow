# StackUnderflow — Handoff doc

**Date:** 2026-05-06 (last tagged release: v0.7.0; Wave 5 follow-ups merged on `main`, unreleased)
**Maintainer:** yad.konrad@quantumrise.com / 0bserver07
**Branch:** `main` (clean, ahead of `origin/main` by Wave 5 merges + this docs commit), tag `v0.7.0`
**Tests:** 1747 passing, 2 skipped, 11 deselected (`pytest -m slow` runs the 11)
**Frontend:** typecheck + build clean, `stackunderflow-ui@0.7.0`

This doc gets a fresh agent oriented in 10 minutes. Read it before reading code.

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
┌────────────────────────  MARTS LAYER  ──────────────────────────────────────┐
│  daily_mart        (day, project_id, provider, model, speed)                 │
│  session_mart      (session_id, all per-session aggregates)                  │
│  project_mart      (project_id, lifetime totals)                             │
│  provider_day_mart (day, provider)                                           │
│  model_day_mart    (day, model, speed)                                       │
│  tool_mart         (day, project_id, provider, tool_name)        ← Wave 5    │
│  command_mart      (day, project_id, command_name)               ← Wave 5    │
│  mart_watermark    (mart_name → last_event_id, last_refresh_ts)              │
└─────────────────────────────────────────────────────────────────────────────┘
                          │
                          ▼
            REST routes — plain SELECTs from marts only
```

The watcher (`stackunderflow/etl/watcher.py`) ties Layers together: filesystem change → adapter.read() → writer inserts messages → normalizer inserts events → refresh_all_marts() advances watermarks. End-to-end ~400 ms.

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
    _streaming.py    # 128 MB cap + 8 MB stream threshold for JSONL
  api/               # Public Python API surface (list_projects/process/list_sessions)
  etl/                                                            ← NEW (v0.7)
    normalize/       # Per-provider transforms messages → usage_events
      base.py        # Normalizer ABC + cost_source constants + _build_event helper
      __init__.py    # last-wins registry: register/get/all + 16 normalizers wire here
      claude.py codex.py cursor.py cline.py                     # default-on
      <12 beta normalizers>
    marts/           # MartBuilder ABC + 7 builders (daily, session, project, provider_day, model_day, tool, command)
      base.py        # ABC; concrete rebuild_from_scratch default
      __init__.py    # last-wins registry; 7 builders wire here
      daily.py session.py project.py provider_day.py model_day.py
      tool.py command.py                                                      # Wave 5
    backfill.py      # Streams messages → events → marts; idempotent; --force rebuild
    backfill_jobs.py # Process-local lock + single-slot job state for POST /api/etl/backfill   # Wave 5
    lock.py          # fcntl/msvcrt watcher single-instance lock + stale-PID detection         # Wave 5
    watcher.py       # watchfiles daemon; debounced 200 ms; per-adapter dispatch
    watermark.py     # get/set/refresh_all_marts; persists last_event_id + last_refresh_ts
    status.py        # Shared assembler for /api/etl/status + `stackunderflow etl status`
  ingest/
    writer.py        # INSERT INTO messages + normalize+insert hook (Wave 4B)
    enumerate.py     # Discovery wrapper around all registered adapters
    __init__.py      # run_ingest(conn, adapters)
  infra/
    costs.py         # compute_cost(tokens, model, provider, *, speed) → dict
    currency.py      # Frankfurter live + 24h cache + ECB snapshot fallback
    cursor_cache.py  # Fingerprint cache for vscdb (3-8× cold-start speedup)
    discovery.py     # Filesystem scan helpers (legacy file-scan path)
    providers/       # Per-provider Pricers (anthropic, openai, cursor, etc.)
  mcp/
    server.py        # FastMCP server; reads from store; 3 tools (session_query, list_sessions, list_projects)
    store_reader.py  # Read-only store helpers shared with the MCP server
  reports/           # CLI report renderers (text/json/csv) + optimize patterns
  routes/            # FastAPI routes (one file per concern, 14 of them)
    cfg.py compare.py context_budget.py cost.py data.py etl.py
    export.py optimize.py plan.py projects.py sessions.py yield_route.py
    bookmarks.py commands.py misc.py qa.py search.py tags.py
  services/          # compare, plans, yield_tracker, pricing, search, qa, tags, bookmarks
  store/
    schema.py        # CURRENT_VERSION = 8; applies SQL + .py migrations idempotently
    queries.py       # Typed query helpers (one place for all SQL)
    mart_queries.py  # Read helpers used by route migrations (Wave 3A/4A/5A)
    db.py types.py
    migrations/      # v001 → v008 (v005 + v008 are .py, rest are .sql)
  cli.py server.py deps.py settings.py __version__.py

stackunderflow-ui/    # React dashboard (Vite)
  src/
    pages/           # Overview, ProjectDashboard, Settings
    components/
      common/         FilterBar, EtlStatusBadge, ExportButton, ...
      dashboard/      one Tab per top-level view (Overview/Sessions/Cost/Compare/Yield/...)
      cost/           # Cost-tab widgets including CostByProviderCard
      analytics/, charts/, layout/, qa/
    services/        # API client + format/currency/filters/providerStyle helpers
    types/api.ts     # Backend response shapes mirrored as TypeScript

tests/                # 1598 backend tests; integration/ has the slow-marker e2e + perf
docs/
  HANDOFF.md         # This file
  specs/             # Architecture specs (multi-provider, etl, etc.)
  cli-reference.md  api-reference.md  multi-provider.md  mcp.md  ...
```

---

## Recent history (v0.5 → v0.7)

| Tag | Date | Highlights |
|---|---|---|
| v0.5.0 | 2026-04-30 | All 16 codeburn-catalog providers as adapters; 4 default-on (claude/codex/cursor/cline) + 12 beta-flag-gated |
| v0.6.0 | 2026-05-01 | Currency, export, model aliases, plan budgets, compare, yield, optimize patterns, context budget, fast-mode SQLite, streaming reader, cursor cache. Multi-provider Python API + MCP. Cursor v3 conversationId fix. UI surfaces wired |
| v0.6.1 | 2026-05-01 | Currency snapshot fallback, cursor pricing for `composer-*`, per-workspace cursor slugs, `<synthetic>` cleanup, defensive adapter coverage |
| v0.6.x patches | 2026-05-04 to 2026-05-05 | Provider/model FilterBar URL-synced, `formatModelName` normalizer, `Annotated[..., Query()]` filter binding fix, non-blocking startup ingest, `bulk_*` SQL helpers replacing N+1 in `/api/projects` |
| v0.7.0 | 2026-05-06 | ETL pipeline (Waves 1–4): usage_events + 5 marts + watermarked refresh + filesystem watcher + every dashboard route migrated to mart reads + status surface + UI badge |
| **Wave 5 (unreleased)** | **2026-05-06** | **tool_mart + command_mart (v007) + POST /api/etl/backfill route + watcher single-instance lock + messages_YYYYMM partitioning behind UNION view (v008, NOT auto-applied) + 13 beta normalizers validated against real-shape fixtures + 1 copilot drift fix** |

---

## What changed in Wave 5 follow-ups (merged on `main`, unreleased)

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
- `/api/optimize` detectors `bash_output_limits`, `junk_reads`, `low_read_edit_ratio`, `ghost_agents` gain a `tool_mart` fast-path filter — instant return on project-scoped windows that didn't use the implicated tool

### Beta normalizer validation
All 13 beta normalizers (registry has 13, not 12 — `omp` aliases `pi`) graded against real-shape fixtures matching the codeburn catalog spec. Result: 12 ✅ matching, 1 ⚠️ drift fixed (copilot model-priority bug — see drift report), 0 ❌ broken. Drift report at `docs/beta-normalizer-drift.md`.

### v008 NOT auto-applied
The maintainer's real `~/.stackunderflow/store.db` (1.9 GB, 150K+ events) is still on `user_version = 6`. Apply v008 manually per the documented rollout in `docs/specs/messages-partitioning.md`: backup → apply on `/tmp/store.test.db` copy → verify counts (`view total == sum(partition counts)`) → spot-check dashboard against the copy → swap.

---

## What changed in v0.7.0 (the ETL push)

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
Spec called the ETL migration v004; v004 + v005 were already taken (synthetic-models cleanup + cursor-workspace redistribute). Final file is `v006_etl_layer.sql`. `schema.CURRENT_VERSION = 6`. Migration is **additive** — no existing tables touched.

### Empty-mart fallback
Every migrated route checks if its mart is populated. If yes → mart read. If no → original aggregator path. So the dashboard works even on a fresh install before backfill runs. After Wave 4B's backfill or a single watcher cycle, marts populate and the fast path takes over automatically.

### Cost is computed once
`cost_usd` lives on every `usage_events` row. Marts SUM it, never re-apply rate cards. Currency conversion stays at the API boundary (already correct from v0.6.0). When pricing changes, re-normalize from raw messages — one code path.

### `session_count` correctness across windows
Additive marts (daily, provider_day, model_day) can't simply SUM `COUNT(DISTINCT session_id)` across refresh windows (the same session can appear in two windows). Solution: after the additive INSERT...ON CONFLICT, a follow-up UPDATE recomputes `session_count` from `usage_events` for affected keys. Bounded by number of distinct keys in the window — typically O(1)..O(few dozen). Tests lock this in.

### Per-entity vs additive marts
- `session_mart` and `project_mart` use INSERT OR REPLACE over a re-aggregated subquery for affected entities (totals stay correct when new events arrive for an existing session).
- `daily_mart`, `provider_day_mart`, `model_day_mart` use INSERT...ON CONFLICT DO UPDATE additively (because the same `(day, …)` key never appears in two refresh windows once the watermark moves forward).

### Normalizer registry is in `__init__.py`
Per spec — Wave 1 puts the registry in `stackunderflow/etl/normalize/__init__.py`, NOT in `base.py`. Last-wins (re-registering overwrites). `_clear()` for tests. The 16 default registrations happen at package-import time via top-level `register("name", Cls)` calls.

### Watcher
Uses `watchfiles` (Rust-backed). Daemon thread spawned in lifespan. Catches every exception so a bad event never poisons the loop. `--no-watcher` / `STACKUNDERFLOW_DISABLE_WATCHER=1` for headless mode.

---

## How to run / what to know

```bash
# Run the dashboard
stackunderflow start                    # binds 127.0.0.1:8095
                                          # ingest + watcher run in background

# ETL ops
stackunderflow etl status                # health + watermarks
stackunderflow etl status --format json
stackunderflow etl backfill              # incremental (skips already-converted msgs)
stackunderflow etl backfill --force      # drops events + marts, rebuilds

# Tests
pytest tests/ -q                         # 1598 fast tests (default)
pytest -m slow tests/stackunderflow/integration -q   # the 11 slow tests
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
~/.stackunderflow/store.db (1.9 GB):
  user_version: 6 (v007 + v008 ship on `main`; both unapplied here pending manual rollout)
  150,337 usage_events
  Marts: daily=940, session=841, project=151, provider_day=146, model_day=184
         tool=0, command=0  ← Wave 5 marts populate after v007 applies + backfill
  Watermarks all at 150,337 (in sync) for the v0.7.0 marts
  Per-provider events: claude 150,014, cursor 220, cline 103
```

---

## What's left / known follow-ups

Items #1-#8 from the v0.7.0 HANDOFF mostly closed by the Wave 5 follow-ups now on `main`. The remaining live items:

| # | Item | Severity |
|---|---|---|
| 1 | **Apply v008 to the real `~/.stackunderflow/store.db`.** The migration ships on `main` but is not auto-applied (1.9 GB store, manual review preferred). Follow the rollout in `docs/specs/messages-partitioning.md`: backup → apply on `/tmp/store.test.db` copy → verify counts → spot-check dashboard → swap. Until then, the maintainer's store stays on `user_version = 6` and the partition path is dormant. | medium (not blocking; do when comfortable) |
| 2 | `optimize` per-message detectors still need raw `messages` for the per-file/per-byte signals their core logic depends on. The Wave 5 `tool_mart` fast-path filter short-circuits on project-scoped windows that didn't use the implicated tool, but on populated stores the full scan still happens. To fully migrate would need per-message-grain marts (`message_tool_mart`?) — significant new design work, low payoff today. | low (fast enough) |
| 3 | Beta normalizer pricing-table coverage gap: `qwen-coder-plus` and `gemini-1.5-pro` (and probably others in the long tail) aren't in the canonical RATE_CARD, so they emit `cost_source=unknown`. Adapter/normalizer code is correct — it's a pricing-table population question. Track the missing models per provider and fold them in. | low (correctness for enabled betas) |
| 4 | Watcher cross-platform coverage. POSIX `fcntl.flock` is smoke-tested on macOS. Windows `msvcrt.locking()` code path is written but **not verified on a real Windows box**. Linux watcher (watchfiles cross-platform) also untested on real data. Most users are on macOS so low urgency, but worth a CI matrix run before claiming Windows support. | low |
| 5 | `current_job` slot on `/api/etl/status` clears on completion but doesn't retain `last_job_status` history — if the orchestrator raises, the slot just goes empty. The `complete_job(status, error)` signature already accepts the failure context; consumers don't read it yet. Small extension if you want failed-backfill visibility in the UI. | low (UX nicety) |
| 6 | `tool_mart.event_count` semantics: distinct `(event, tool)` pair, not the aggregator's non-distinct call count. The reshim returns the distinct-pair count under the `calls` key. If a chart consumer needs total-call count (e.g., Read called 3× = 3, not 1), the mart needs a `calls_total` column alongside `event_count`. | low (no consumer broken today) |
| 7 | `command_costs` per-Interaction block in `/api/cost-data` stays on the aggregator path — its shape doesn't fit `(day, project, command_name)` aggregation. `command_mart_for_project` is wired and ready; if you want a per-command-NAME rollup surfaced, route it. | low (current path works) |
| 8 | Beta normalizer fixtures (`tests/fixtures/beta_normalizers/`) are synthetic-but-spec-accurate per the codeburn catalog. They don't replace real-world parity — that requires actual session data per provider on the maintainer's machine, which most beta providers don't have. The defensive empty/malformed coverage from v0.6.1 + the new spec-shape coverage from Wave 5 catch the structural failure modes; the next "Cursor v3 conversationId-in-the-key" still needs real local data. | low (no concrete bug today) |

---

## Files an incoming agent should read first

1. `docs/specs/etl-architecture.md` — design contract for the pipeline
2. `docs/specs/messages-partitioning.md` — v008 design + rollback + ops rollout (Wave 5)
3. `docs/beta-normalizer-drift.md` — per-provider verdicts from the Wave 5 audit (Wave 5)
4. `stackunderflow/etl/normalize/base.py` — `Normalizer` ABC + helpers
5. `stackunderflow/etl/marts/base.py` — `MartBuilder` ABC
6. `stackunderflow/etl/backfill.py` — orchestrator + writer hook
7. `stackunderflow/etl/backfill_jobs.py` — process-local backfill job slot (Wave 5)
8. `stackunderflow/etl/lock.py` — watcher single-instance lock (Wave 5)
9. `stackunderflow/etl/watcher.py` — watchfiles + per-adapter dispatch
10. `stackunderflow/store/migrations/v006_etl_layer.sql` + `v007_lower_grain_marts.sql` + `v008_messages_partitioning.py` — schema progression
11. `stackunderflow/store/mart_queries.py` — every read helper used by routes
12. `stackunderflow/ingest/writer.py` — partition routing helpers (Wave 5)
13. Any `routes/*.py` for the JSON contracts the dashboard depends on
14. `tests/stackunderflow/integration/` — e2e + perf regression — most useful single file to understand the whole pipeline at once

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
| Marts empty after backfill | Normalizer for that provider not registered | `stackunderflow.etl.normalize.all()` should list 16 keys |
| Dashboard cost = $0 for a provider | Pricer for the model returns `None` | `infra/providers/<provider>.py` `rates_for()` |
| Watcher spammed log: "Adapter raised in cycle" | Provider adapter has a parse bug | Look at the provider's adapter, run `adapter.read()` directly on the file |
| `health: error` in status | Mart watermark stuck + watcher dead | Restart server; `stackunderflow etl backfill --force` if persistent |
| New install, dashboard slow | Marts empty, fallback to aggregator. Run `stackunderflow etl backfill` to populate | One-time, then watcher keeps it fresh |
| `pytest -m slow` failing on `/api/etl/status` | Route may not be in main yet (Wave 4C pre-merge state); skip is acceptable | `tests/stackunderflow/integration/test_route_perf_regression.py` |

---

## What I'd do next if I had a week

1. **Apply v008 to the maintainer's real store + verify on the dashboard.** Walk the documented rollout end-to-end against the 1.9 GB store, measure read fanout cost on the live dashboard.
2. **Pricing-table population sweep for the long-tail betas.** Audit which models the 12 beta providers emit in real-world fixtures and add the missing entries to the canonical RATE_CARD. Closes the `cost_source=unknown` cosmetic gap on `qwen-coder-plus`, `gemini-1.5-pro`, and friends. Probably 30-50 model entries across the 12 providers.
3. **Cross-platform CI matrix for the watcher + lock.** Linux + Windows runners. Smoke-test `watchfiles` source-file detection latency, smoke-test `msvcrt.locking` lock acquisition, document the gaps. Closes follow-up #4.
4. **Failed-backfill UX.** Extend the `current_job` slot on `/api/etl/status` to retain `last_job_status` + `error` for N seconds after completion. Surface in the Settings backfill banner (red instead of green on failure). Small UX win that closes a real visibility gap (today, the slot just empties on orchestrator failure).
5. **`tool_mart.calls_total` column.** Add the non-distinct call count alongside the distinct-pair `event_count`. Lets the `tool_costs` chart consumer choose either semantics. Migration v009.

---

That's the picture. Files referenced are absolute paths under `/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow/`. Welcome.
