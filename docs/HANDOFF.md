# StackUnderflow — Handoff doc

**Date:** 2026-05-06 (post v0.7.0 release)
**Maintainer:** 0bserver07 / 0bserver07
**Branch:** `main` (clean), tag `v0.7.0`
**Tests:** 1598 passing, 2 skipped, 11 deselected (`pytest -m slow` runs the 11)
**Frontend:** typecheck + build clean, `stackunderflow-ui@0.6.1`

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
    marts/           # MartBuilder ABC + 5 builders (daily, session, project, provider_day, model_day)
      base.py        # ABC; concrete rebuild_from_scratch default
      __init__.py    # last-wins registry; 5 builders wire here
      daily.py session.py project.py provider_day.py model_day.py
    backfill.py      # Streams messages → events → marts; idempotent; --force rebuild
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
    schema.py        # CURRENT_VERSION = 6; applies SQL + .py migrations idempotently
    queries.py       # Typed query helpers (one place for all SQL)
    mart_queries.py  # Read helpers used by route migrations (Wave 3A/4A)
    db.py types.py
    migrations/      # v001 → v006 (v005 is .py, rest are .sql)
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
| **v0.7.0** | **2026-05-06** | **ETL pipeline (Waves 1–4): usage_events + 5 marts + watermarked refresh + filesystem watcher + every dashboard route migrated to mart reads + status surface + UI badge** |

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
  150,337 usage_events
  Marts: daily=940, session=841, project=151, provider_day=146, model_day=184
  Watermarks all at 150,337 (in sync)
  Per-provider events: claude 150,014, cursor 220, cline 103
```

---

## What's left / known follow-ups

| # | Item | Severity |
|---|---|---|
| 1 | `optimize` patterns that stay on aggregator path (bash_output_limits, junk_reads, low_read_edit_ratio, ghost_agents, etc.) need lower-grain marts (`tool_mart`, `command_mart`) — those marts are not built. They run against `messages` table directly, but only on the optimize endpoint, so it's slow but bounded. | low (fast enough) |
| 2 | The `/api/etl/backfill` POST route doesn't exist yet — Wave 4F's backfill button shows the equivalent CLI command on 404. Ship a thin route that wraps `etl.backfill.backfill(conn)` in a background task. | medium (UX) |
| 3 | Beta normalizers (12 of them) are wired but most haven't been validated against real local data on the maintainer's machine — only claude / codex / cursor / cline / gemini / droid / qwen have actual data. The Cursor v3 bug from v0.6.0 (`#52`) is the kind of latent failure to expect. The defensive empty-source/malformed-data tests added in v0.6.1 cover the failure modes but not full real-data parity. | medium (correctness on enabled betas) |
| 4 | Per-route latency target on `/api/optimize` is 100 ms warm, 200 ms budget. Currently passes but tight. As the 7 patterns grow, this will need either lower-grain marts (#1 above) or pattern-specific caching. | medium |
| 5 | `messages_YYYYMM` partitioning was designed for in the spec but not implemented — `messages` table stays unpartitioned. On long-lived stores (years of data) this will eventually need to ship. | low (future) |
| 6 | The `tool_mart` / `command_mart` lower-grain marts (deferred from Wave 3A/4A) — needed to migrate the per-session/per-command/per-tool detail blocks of `/api/cost-data`. Currently those blocks read raw messages. | low (current path works, just slower) |
| 7 | Wave 2C watcher is macOS-only verified. `watchfiles` claims cross-platform parity. Linux/Windows haven't been smoke-tested on real data. | low (most users on macOS) |
| 8 | The watcher restarts on every `stackunderflow start` — there's no cross-process coordination. If two `start` invocations run, both will spin up watchers. The lifespan binds to a single process so this is theoretical, but worth a lock file someday. | low |

---

## Files an incoming agent should read first

1. `docs/specs/etl-architecture.md` — design contract for the pipeline
2. `stackunderflow/etl/normalize/base.py` — `Normalizer` ABC + helpers
3. `stackunderflow/etl/marts/base.py` — `MartBuilder` ABC
4. `stackunderflow/etl/backfill.py` — orchestrator + writer hook
5. `stackunderflow/etl/watcher.py` — watchfiles + per-adapter dispatch
6. `stackunderflow/store/migrations/v006_etl_layer.sql` — schema
7. `stackunderflow/store/mart_queries.py` — every read helper used by routes
8. Any `routes/*.py` for the JSON contracts the dashboard depends on
9. `tests/stackunderflow/integration/` — e2e + perf regression — most useful single file to understand the whole pipeline at once

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

1. **Wave 5: lower-grain marts.** `tool_mart` (per-tool aggregates) + `command_mart` (per-command). Unblocks the deferred `/api/cost-data` per-session/per-tool blocks and lets `optimize.py`'s remaining 6 detectors move off the aggregator path.
2. **Real-data validation of the 12 beta normalizers.** For each, generate a synthetic but spec-accurate fixture; assert event shape matches the codeburn catalog spec; flag any drift. Most useful: catch the next "Cursor v3 conversationId-in-the-key" before it ships.
3. **Real `/api/etl/backfill` route.** Wraps the existing CLI orchestrator in a FastAPI BackgroundTask. Wave 4F's UI button hits 404 today.
4. **Lock file / single-watcher invariant.** Prevent two `stackunderflow start` instances from racing. `flock` on `~/.stackunderflow/server.lock`.
5. **Streaming-safe `messages` partitioning.** `messages_YYYYMM` partitions, `litestream`-friendly. Future-proofs the store at multi-year scale.

---

That's the picture. Files referenced are absolute paths under `/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow/`. Welcome.
