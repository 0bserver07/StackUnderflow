# Analytics Polish — Make the Cost Tab Actually Analytical

Fixes to the v1 analytics build (merged at `8e5df57`). Base: `main`. 25 owners, tight file ownership — each agent has exactly one primary file.

Known gaps from the v1 ship:
- No sort, no pagination, no click-through
- 2.6 MB payload on `/api/dashboard-data` — 1.5s warm load
- `retry_signals: 0` and `error_cost.estimated_retry_cost: $0.00` despite 222 errors (buggy collectors)
- Tables render all rows (163 high-step outliers on chimera) without virtualization
- No filter controls, no aggregates, no row expansion

Acceptance for **every** owner: type-check passes, build passes, no new lint warnings, commit on the worktree branch with a clear message referencing this spec section.

---

## Wave A — backend + primitives (8 owners, parallel)

### A1 `backend-retry` — Fix `_RetryCollector`
**Files:** `stackunderflow/stats/aggregator.py`, `tests/stackunderflow/stats/` (add tests).
**Problem:** chimera has 222 errors but `retry_signals: []`. The detection rule ("same tool ≥2× with is_error between") is too strict — most retries don't flag `is_error` on the tool record itself; the error lives in the assistant's follow-up or tool_result.
**Scope:**
- Debug against chimera: load one Interaction with a known error and inspect whether `is_error` appears on the tool record, the assistant record, or the tool_result.
- Rewrite detection so a retry signal fires when: within one `Interaction`, the same tool is invoked ≥2 times AND at least one preceding invocation was followed by an error (is_error on the next record, OR a tool_result whose content starts with "Error" / "failed", OR an assistant message with content matching the existing `INTERRUPT_API` / `INTERRUPT_PREFIX` patterns).
- Compute `estimated_wasted_tokens` as sum of output tokens on failed assistant turns of that tool.
- Add 3+ tests covering: legit retry chain, single success (no signal), retry-after-success (no signal).
**Done when:** chimera surfaces ≥ 1 retry_signal.

### A2 `backend-errcost` — Fix `_ErrorCostCollector`
**Files:** `stackunderflow/stats/aggregator.py`, tests.
**Problem:** `estimated_retry_cost: 0.0` despite 222 errors. Collector likely zeroes everything when retry_signals is empty — wrong linkage.
**Scope:**
- Decouple from retry_signals. Estimate cost as: for each error record, sum the output_tokens of the next assistant message (or the current record's own output_tokens if it's the assistant) and apply `compute_cost` using the record's model.
- Populate `top_error_commands: list[OutlierCommand]` — top 10 Interactions ranked by count of error records inside them.
- Populate `errors_by_tool` properly from tool invocation errors.
- Add 2+ tests.
**Done when:** chimera shows non-zero `estimated_retry_cost` and a populated `top_error_commands`.

### A3 `backend-splitapi` — Lazy endpoints
**Files:** `stackunderflow/routes/data.py` (split), new `stackunderflow/routes/cost.py`.
**Problem:** `/api/dashboard-data` ships 2.6 MB because analytics is bundled.
**Scope:**
- Move the 9 new analytics keys (`session_costs`, `command_costs`, `tool_costs`, `token_composition`, `outliers`, `retry_signals`, `session_efficiency`, `error_cost`, `trends`) out of `/api/dashboard-data` and into a new `GET /api/cost-data?log_path=`.
- Existing `/api/dashboard-data` keeps only: `overview`, `tools`, `sessions`, `daily_stats`, `hourly_pattern`, `errors`, `models`, `user_interactions`, `cache`. Target: <1 MB for chimera.
- Add `GET /api/interaction/{interaction_id}?log_path=` returning one enriched Interaction (command + responses + tool_results).
- Register the new router in `stackunderflow/server.py` or wherever `routes/` is wired.
- Add tests.
**Done when:** `/api/dashboard-data` size drops below 1 MB on chimera; new endpoints respond 200 with the expected shapes.

### A4 `backend-perf` — Optimize summarise()
**Files:** `stackunderflow/stats/aggregator.py`.
**Problem:** chimera full dashboard cold: 1.76s, warm: 1.55s. Target: warm < 500ms.
**Scope:**
- Profile `summarise()` against chimera (cProfile or timing prints per collector). Identify top 2 hot spots.
- Optimize those specifically. Likely candidates: `_command_analysis` already iterates interactions twice; the new `_SessionCostCollector.result()` may be re-computing per-session totals from scratch; `_trends` iterates records twice.
- Do NOT restructure the collector pattern. Small targeted wins only.
- Benchmark before/after and include numbers in commit message.
**Done when:** chimera warm `/api/dashboard-data` < 500ms AND `/api/cost-data` (from A3) < 800ms.

### A5 `prim-sort` — Sortable table hook
**Files:** NEW `stackunderflow-ui/src/hooks/useSortableTable.ts`.
**Scope:**
- Export `useSortableTable<T>(rows: T[], initialSort: {key: keyof T, dir: 'asc' | 'desc'})` returning `{sorted, sortKey, sortDir, setSort}`.
- Export `<SortHeader>` component (clickable `<th>` with arrow indicator).
- Multi-type safe: number, string, boolean. Nullish sorts to the end.
- One simple usage example in a JSDoc block.
**Done when:** hook + component type-check clean and can be imported.

### A6 `prim-expand` — Expandable row
**Files:** NEW `stackunderflow-ui/src/components/common/ExpandableRow.tsx`.
**Scope:**
- Render a `<tr>` pair: header row + hidden detail row. `expanded` prop toggles. Tabler chevron rotates.
- Accepts `detail` as React children rendered inside a full-width `<td colSpan>` cell.
- Keyboard: Enter/Space on header row toggles.
**Done when:** clean component, works with any table column layout.

### A7 `prim-aggr` — Table footer aggregates
**Files:** NEW `stackunderflow-ui/src/components/common/TableFooterAggregates.tsx`.
**Scope:**
- Props: `{columns: Array<{label: string, value: number, format?: (n) => string}>}`.
- Renders a footer `<tr>` in the same column layout showing sum/median/p95 of each column where applicable. For presentational use, caller passes pre-computed values.
- Styling matches existing cost table borders.
**Done when:** can be dropped into any `<table>`.

### A8 `prim-nav` — Cross-tab navigation
**Files:** NEW `stackunderflow-ui/src/services/navigation.ts`.
**Scope:**
- Export `openInteraction(id: string): void` — updates URL to current-project-path with `?tab=messages&interaction={id}`.
- Export `openSession(id: string): void` — `?tab=sessions&session={id}`.
- Export `getTabFromURL(): string | null` and `getParam(name): string | null`.
- Uses `URLSearchParams` + `window.history.pushState`. Dispatches a `stackunderflow:nav` custom event so `ProjectDashboard` can listen.
**Done when:** functions work; a simple unit test (Vitest if configured, else a manual assertion) confirms URL updates.

---

## Wave B — components (12 owners, one per file in `cost/`)

**Shared rules for every B-wave owner:**
- Import `useSortableTable` from `../../hooks/useSortableTable` (primitive may land concurrently; if missing at start-time, stub locally with a TODO comment and it will merge).
- Import `openInteraction` / `openSession` from `../../services/navigation` for click-through wiring.
- Add all number columns to sort. Sort defaults to existing hard-coded order.
- Add graceful empty state if missing (most components already have one).
- Keep props API additive — existing callers must still work with just `data={...}`.
- Add `data-testid` attributes on root + interactive elements for future e2e.

### B9 `c-cmdlist` — `cost/CommandCostList.tsx`
Add: sortable columns (cost, tokens, tools, steps, when), click row → `openInteraction(r.interaction_id)`, expand row → show full `prompt_preview` (no truncation) + `models_used` + `had_error` badge, footer aggregates (sum cost, median cost, p95 cost), `%-of-total` column, caption showing "showing N of M, sorted by X".

### B10 `c-outlier` — `cost/OutlierCommandsTable.tsx`
Add: `onOpen?` prop, make rows clickable (`openInteraction`), sortable columns (count, cost, when), expandable row with full prompt, virtualize if rows > 50 (use `react-window` if already a dep; else cap UI at 50 with "show more" toggle). Footer: count + median per section.

### B11 `c-sesscost` — `cost/SessionCostBarChart.tsx`
Add: `onSelect` wiring (pass `openSession`), richer tooltip (cost breakdown, duration, models), label bars with $ amount when >10% of total, click bar → open session.

### B12 `c-sesseff` — `cost/SessionEfficiencyTable.tsx`
Add: sortable columns, classification filter (chip row above table), click row → `openSession`, footer with count per classification.

### B13 `c-toolcost` — `cost/ToolCostBarChart.tsx`
Add: sort toggle (by cost / by calls / by tokens), %-of-total annotation on each bar, click bar → filter Commands list by that tool (emits a custom event or query param; if out of scope, add TODO).

### B14 `c-tokstack` — `cost/TokenCompositionStack.tsx`
Add: date range filter (7d / 30d / all), rich tooltip (absolute + %), legend with click-to-isolate a series.

### B15 `c-tokdonut` — `cost/TokenCompositionDonut.tsx`
Add: center label (total tokens), % labels on slices > 3%, legend with absolute values, hover highlight.

### B16 `c-roi` — `cost/CacheRoiCard.tsx`
Add: expandable section → breakdown by session (top 5 cache savers), ROI trend sparkline if feasible from `daily_stats`, fail-gracefully empty state.

### B17 `c-errcost` — `cost/ErrorCostCard.tsx`
Add: expand → full `errors_by_tool` bar list (scrollable), `top_error_commands` link list (rows use `openInteraction`), clearer hero number.

### B18 `c-trend` — `cost/TrendDeltaStrip.tsx`
Add: tooltip on each tile with current / prior raw values + window dates, click tile → filter dashboard to current-week window (emit custom event for now; filter wiring in Wave C).

### B19 `c-retry` — `cost/RetryAlertsPanel.tsx`
Add: click signal → `openInteraction(signal.interaction_id)`, severity filter chips (≥3 failures / ≥2 / all), summary header "N retries wasted $X.XX".

### B20 `c-compare` — NEW `cost/SessionCompareView.tsx`
Two-column side-by-side diff using `GET /api/sessions/compare?a=&b=`. Columns: cost, tokens (4 types), commands, messages, errors, duration. Highlight diff in green/red. Fetches via existing API service layer. Self-contained — exports the component; `SessionsTab` integration is Wave C.

---

## Wave C — integration (5 owners, after Waves A & B merge)

### C21 `integ-costtab` — `dashboard/CostTab.tsx`
- Switch to lazy fetch: on tab-mount, call `/api/cost-data` (from A3) instead of reading from `stats`. Show skeleton while loading.
- Insert `FilterBar` at top (date range + session filter — listen to global filter state if one exists; otherwise local component state).
- Wire all callbacks exposed by Wave B components (`onOpen`, `onSelect`).
- Listen for `stackunderflow:nav` event for cross-tab coordination.

### C22 `integ-overview` — `dashboard/OverviewTab.tsx`
- Wire `TrendDeltaStrip` click → switches to Cost tab with date filter applied.
- Ensure the `TokenCompositionDonut` pulls from new split endpoint when available, falls back to `overview.total_tokens`.

### C23 `integ-sessions` — `dashboard/SessionsTab.tsx`
- Accept `?session=ID` query param → scroll to + highlight that session.
- Add "Compare" mode: toggle button, two checkboxes on rows, then renders `<SessionCompareView a={...} b={...} />` inline (from B20).

### C24 `integ-messages` — `dashboard/MessagesTab.tsx`
- Accept `?interaction=ID` query param → scroll to that interaction, highlight its prompt.
- If the ID isn't in the current page, fetch the specific message via A3's `/api/interaction/{id}`.

### C25 `integ-router` — `pages/ProjectDashboard.tsx`
- Parse `?tab=` on mount, switch to that tab.
- Listen to `stackunderflow:nav` and `popstate` to respond to URL changes without full reload.
- Ensure tab switches push a clean URL back via `history.replaceState`.

---

## Merge strategy
- Every worktree commits to its own branch; team-lead (me) merges into `main` in order: Wave A → Wave B → Wave C.
- Wave B may find primitives missing at start — they should add TODO stubs and import optimistically; I'll reconcile during merge if needed.
