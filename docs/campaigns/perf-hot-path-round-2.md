# StackUnderflow — Perf spec: hot-path round 2 (startup + slow pages)

> Branch: `perf/hot-path-round-2`. Working spec for the second performance pass.
> The June-25 campaign (`docs/campaigns/ui-perf-audit.md`) fixed the Overview /
> dashboard / messages / sessions hot paths and the frontend first-paint. This
> round targets what that pass did **not** reach: the raw-message-grain routes
> and the background ingest. Every number below is measured on the maintainer's
> real store (204,938 events; the `chimera` project = 41,814 messages across 3
> provider ids), 2026-07-02.

## Measured baseline (this is the evidence, not memory)

| Surface | Cold | Warm | Verdict |
|---|---|---|---|
| Server time-to-bind | **2.8s** | — | GOOD (yesterday's fix moved price-book activation off the lifespan thread) |
| `GET /api/global-stats` | 0.05s | 0.01s | GOOD (mart-backed) |
| `GET /api/worktrees` | 0.08s | — | GOOD (bounded git scan) |
| `GET /api/cost-data` (chimera) | **8.4s** | 0.25s | cold slow; warm cached |
| `GET /api/patterns` (chimera) | **5.4s** | — | slow, no memo |
| `GET /api/forks` (chimera) | **13.6s** | **6.2s** | WORST — no memoization at all |
| Background ingest (per boot) | pegs 1 core for minutes | — | doesn't block bind, but wasteful |

Frontend first-paint is already optimized (June campaign): entry `index` 62KB +
`react-vendor` 203KB + css 93KB eager; `recharts` (433KB) lazy per chart-route;
`markdown` (164KB) + `syntax-highlighter` (78KB) lazy behind the sidebar's
`MetaAgentMessageList`. **No large low-hanging frontend win remains** — the CSS
(93KB Tailwind) and react-vendor are the floor. This round is backend.

## Root cause (one theme, three symptoms)

`forks`, `cost-data` (cold), and `patterns` all do the **same** thing: a
**per-message-grain Python computation over tens of thousands of raw
`messages` rows**, with no mart carrying the derived shape and thin-or-absent
memoization.

- **forks** (`reports/forks.py::analyze_forks` → `_load_messages`): loads ALL
  41,814 messages for the project (JOIN messages+sessions, ORDER BY
  session,seq), builds the conversation DAG and walks it in Python — on
  **every request**, no cache. The 6.2s "warm" is just SQLite's page cache
  on the read; the Python DAG walk re-runs every time.
- **cost-data** (`routes/cost.py`): the base aggregator (`get_project_stats`
  over all provider-ids at once) is the 8.4s cold cost. It IS memoized
  (`_project_stats_cached`, signature-keyed on the store + slug + tz + ids),
  so warm is 0.25s — but the cache is **process-local** and invalidated on the
  next ingest signature change, so the first hit after any watcher cycle pays
  full price again.
- **patterns** (`reports/patterns.py`): window-bounded (good) but still a raw
  `messages` scan + per-row `tools_json` parse; 5.4s on a 90d window.

The marts layer that makes `global-stats`/dashboard fast doesn't help these:
marts are aggregate-grain (day/session/tool/…), and none carries a conversation
DAG, a cross-session error-recurrence index, or the full analytics-block shape.

## Plan — tiered by value ÷ risk

### Tier 1 — memoize forks ✅ DONE (this branch)
Mirrored the `_project_stats_cached` pattern: `routes/forks.py` now has
`_analyze_forks_cached` — a process-local read-through cache keyed on
`(store_path, scope.label, sorted(project_ids) | None)` with a signature =
`(MAX(sessions.last_ts), SUM(sessions.message_count))` over the scoped
sessions (whole-store when `project_ids is None`). Currency conversion stays
outside the cache (applied to a deep copy) so an FX change needs no recompute.
**Measured on the real store (chimera):** cold 6.19s (one compute) → **warm
0.005s / 0.002s** (~1300×). 3 new tests (hit, signature-invalidation on new
messages, period-keying) + an autouse cache-clear fixture; suite 3,565 green;
ruff at 54 baseline.

### Tier 2 — kill the cold recompute for forks + cost-data
The DAG walk and the analytics blocks are derivable from columns the marts
*could* carry. Two sub-options, decide during Tier 1:
- (a) **`fork_mart`** at (project_id, session_id) grain holding the
  per-session sidechain/branch/abandoned rollup + fork points, watermarked on
  `usage_events.id` like every other mart, refreshed by the watcher. Then
  `analyze_forks` reads the mart (aggregate-grain, <100ms) and only touches raw
  messages for the top-N abandoned-branch previews (bounded). Cold becomes fast
  for everyone, not just warm.
- (b) Cheaper interim: push the DAG walk into SQL (recursive CTE over
  `uuid`/`parent_uuid`) so Python isn't iterating 41K rows. Less complete than a
  mart but no schema change.

### Tier 3 — persist the analytics cache across restarts
`_project_stats_cached` and the Tier-1 fork cache are process-local, so every
server restart / first-hit-after-ingest pays cold. Option: a small on-disk
`analytics_cache` (TieredCache cold tier already exists in `infra/cache.py`)
keyed on the same signature, so a restart warm-starts. Risk: medium (cache
invalidation correctness — must key on the event-id signature, never time).

### Tier 4 — ingest: stop pegging a core every boot
Bind is already fast (2.8s), but the background ingest stats **6,566 source
files** every boot (`ingest/enumerate.py::iter_refs`) and there's a genuine
backlog (3,223 `ingest_log` rows vs 6,566 files on disk). Two moves:
- **Enumeration budget / batching**: `iter_refs` should `stat()` in a bounded
  batch and yield, so a cold boot with thousands of files doesn't monopolize a
  core in one burst; the watcher already handles steady-state incrementally.
- **WAL checkpointing for bulk writers**: the `--force` backfill / chunked
  rebuilds left a **1.5GB WAL** after an interrupted run that degraded every
  reader by orders of magnitude (each late chunk took ~3 min purely from WAL
  scan). Bulk writers should issue `PRAGMA wal_checkpoint(PASSIVE)` every N
  commits. Low risk, high insurance.

### Tier 5 — patterns
After Tier 1–2, re-measure. Likely bound the window default tighter or reuse the
`message_tool_mart` (already per-(message,tool) grain, 105K rows) for the
file-touch half instead of re-parsing `tools_json`.

## Invariants to preserve
- Cost totals: `tests/stackunderflow/infra/test_pricing_invariants.py` must stay
  green (marts==events, nothing silently unpriced).
- Mart fast-path <100ms perf tests must stay green.
- Any new cache keys on an **event-id signature**, never wall-clock — the
  fabricated-grade / stale-data class of bug is not acceptable.
- No package-version changes (exact-pin CI guard). Schema bumps (a `fork_mart`
  migration) are engineering state and fine — next free slot is **v028**.

## Order of work in this branch
1. Tier 1 (forks memoization) + tests + re-measure. ← start here
2. Tier 4 WAL checkpointing (cheap, independent, high insurance).
3. Decide Tier 2 (mart vs CTE) from the Tier-1 numbers; spec the migration.
4. Tier 3 / Tier 5 as follow-ups; re-measure and record deltas in this doc.
