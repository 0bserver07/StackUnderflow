# Analytics Expansion — Token Burn + Waste Attribution

**Goal:** answer "where did my tokens go?" and "where was time/money wasted?" by extending the existing stats pipeline and dashboard.

All new computation lives in `stackunderflow/stats/aggregator.py`. All new UI lives as a new **Cost** tab plus targeted upgrades to `OverviewTab` and `SessionsTab`. No new data extraction from log files is required for Phases 1–3 — everything derives from the existing `Record` / `Interaction` objects in `EnrichedDataset`.

## 1. API contract

These fields are appended to the dict returned by `summarise()` in `stackunderflow/stats/aggregator.py`. They live alongside the existing keys (`overview`, `tools`, `sessions`, `daily_stats`, `hourly_pattern`, `errors`, `models`, `user_interactions`, `cache`) and ship on every call to `GET /api/dashboard-data`.

### 1.1 `session_costs: list[SessionCost]` — ranked desc by cost

```python
class SessionCost(TypedDict):
    session_id: str
    started_at: str           # ISO-8601
    ended_at: str             # ISO-8601
    duration_s: float
    cost: float               # total $ for the session
    tokens: dict[str, int]    # {input, output, cache_read, cache_creation}
    messages: int             # total record count
    commands: int             # user_interactions count
    errors: int
    first_prompt_preview: str # first 140 chars of first user prompt, no newlines
    models_used: list[str]
```

All sessions returned (chimera has 18 — pagination is out-of-scope).

### 1.2 `command_costs: list[CommandCost]` — top 50 desc by cost

One entry per real user prompt (an `Interaction`, not a `Record`). The cost for a command = the cost of every assistant + tool_result record that followed until the next prompt.

```python
class CommandCost(TypedDict):
    interaction_id: str
    session_id: str
    timestamp: str             # ISO-8601 of the user prompt
    prompt_preview: str        # first 200 chars, no newlines
    cost: float
    tokens: dict[str, int]
    tools_used: int            # Interaction.tool_count
    steps: int                 # Interaction.assistant_steps
    models_used: list[str]
    had_error: bool
```

### 1.3 `tool_costs: dict[str, ToolCost]`

```python
class ToolCost(TypedDict):
    calls: int
    input_tokens: int         # input attributable to assistant messages that invoked this tool
    output_tokens: int
    cache_read_tokens: int
    cache_creation_tokens: int
    cost: float               # assistant message cost, weighted by 1/(num distinct tools in that message)
```

**Attribution rule:** when an assistant message invokes N distinct tools, 1/N of its cost is attributed to each. (Good-enough approximation — exact attribution is impossible without per-tool token accounting.)

### 1.4 `token_composition`

```python
class TokenComposition(TypedDict):
    daily: dict[str, dict[str, int]]    # {"2026-02-23": {input, output, cache_read, cache_creation}}
    totals: dict[str, int]              # same 4 keys across the whole dataset
    per_session: dict[str, dict[str, int]]  # {session_id: same 4 keys}
```

### 1.5 `outliers`

```python
class OutlierCommand(TypedDict):
    interaction_id: str
    session_id: str
    timestamp: str
    prompt_preview: str
    tool_count: int
    step_count: int
    cost: float

class Outliers(TypedDict):
    high_tool_commands: list[OutlierCommand]    # tool_count > 20, ranked desc
    high_step_commands: list[OutlierCommand]    # step_count > 15, ranked desc
```

### 1.6 `retry_signals: list[RetrySignal]`

A *retry signal* = within one Interaction, the same tool name is invoked ≥2 times in sequence with at least one is_error between them.

```python
class RetrySignal(TypedDict):
    interaction_id: str
    session_id: str
    timestamp: str
    tool: str
    consecutive_failures: int     # number of is_error records of that tool in a row
    total_invocations: int        # total times that tool was called in the interaction
    estimated_wasted_tokens: int  # sum of output tokens on the failed assistant turns
    estimated_wasted_cost: float
```

### 1.7 `session_efficiency: list[SessionEfficiency]`

```python
class SessionEfficiency(TypedDict):
    session_id: str
    search_ratio: float         # search_tool invocations / total tool invocations
    edit_ratio: float           # Edit+Write / total tool invocations
    read_ratio: float           # Read / total tool invocations
    bash_ratio: float
    idle_gap_total_s: float     # sum of gaps >= 30s between consecutive records
    idle_gap_max_s: float
    classification: str         # "edit-heavy" | "research-heavy" | "balanced" | "idle-heavy"
```

Classification rule:
- `edit_ratio ≥ 0.25` → `edit-heavy`
- `search_ratio + read_ratio ≥ 0.6` and `edit_ratio < 0.1` → `research-heavy`
- `idle_gap_total_s > duration_s * 0.4` → `idle-heavy`
- else → `balanced`

Search tools: `Grep`, `Glob`, plus any tool name containing "search" (case-insensitive).

### 1.8 `error_cost`

```python
class ErrorCost(TypedDict):
    total_errors: int
    estimated_retry_tokens: int       # avg_output_tokens_per_error_retry × total_errors
    estimated_retry_cost: float
    errors_by_tool: dict[str, int]
    top_error_commands: list[OutlierCommand]   # top 10 interactions by error count
```

### 1.9 `trends`

Compares the **last 7 days** to the **prior 7 days** of activity (by `overview.date_range.end`, not wall clock).

```python
class TrendMetrics(TypedDict):
    cost_per_command: float
    errors_per_command: float
    tools_per_command: float
    tokens_per_command: float
    commands: int
    cost: float

class Trends(TypedDict):
    current_week: TrendMetrics
    prior_week: TrendMetrics
    delta_pct: TrendMetrics       # (current - prior) / prior * 100, 0.0 if prior is 0
```

### 1.10 New endpoint: `/api/sessions/compare`

```
GET /api/sessions/compare?log_path=<path>&a=<session_id>&b=<session_id>

Response:
{
  "a": SessionCost,
  "b": SessionCost,
  "diff": {
    "cost": float,                 # b - a
    "tokens": dict[str, int],      # b - a per key
    "commands": int,
    "errors": int,
    "duration_s": float
  }
}
```

Lives in `stackunderflow/routes/sessions.py`.

## 2. Frontend — new files

### 2.1 Type definitions

Append to `stackunderflow-ui/src/types/api.ts` (or put in a new `stackunderflow-ui/src/types/analytics.ts` imported by `api.ts`). All interfaces above, TypeScript-ified — use `string` for ISO timestamps, `Record<string, ...>` for dicts, etc.

Also extend `DashboardStats` interface with the new optional fields (all optional for backwards compat during rollout).

### 2.2 New chart / panel components

Each in its own file. All are pure presentational components taking a single prop typed from the API schema.

```
stackunderflow-ui/src/components/cost/
├── SessionCostBarChart.tsx        // props: { data: SessionCost[] }
├── CommandCostList.tsx            // props: { data: CommandCost[], onOpen?: (id) => void }
├── ToolCostBarChart.tsx           // props: { data: Record<string, ToolCost> }
├── TokenCompositionStack.tsx      // props: { daily: Record<string, ...> }
├── TokenCompositionDonut.tsx      // props: { totals: Record<string, number> }
├── CacheRoiCard.tsx               // props: { cache: CacheStats }
├── OutlierCommandsTable.tsx       // props: { outliers: Outliers }
├── RetryAlertsPanel.tsx           // props: { signals: RetrySignal[] }
├── SessionEfficiencyTable.tsx     // props: { data: SessionEfficiency[] }
├── ErrorCostCard.tsx              // props: { errorCost: ErrorCost }
└── TrendDeltaStrip.tsx            // props: { trends: Trends }
```

Chart library convention: follow existing `stackunderflow-ui/src/components/charts/*.tsx` — use the same chart library (Recharts) and same Tailwind dark styling.

### 2.3 New Cost tab

```
stackunderflow-ui/src/components/dashboard/CostTab.tsx
```

Layout:
1. `TrendDeltaStrip` full-width at top
2. Grid row: `CacheRoiCard` · `ErrorCostCard`
3. `SessionCostBarChart` full-width
4. `CommandCostList` full-width
5. `ToolCostBarChart` · `TokenCompositionDonut`
6. `TokenCompositionStack` full-width
7. `OutlierCommandsTable` full-width
8. `RetryAlertsPanel` full-width

### 2.4 Overview tab upgrades

Replace the 4 cache/token mini stat cards at `OverviewTab.tsx:100-117` with a single `TokenCompositionDonut`. Add `CacheRoiCard` above the mini-card grid. Add `TrendDeltaStrip` at the very top of the page, above `StatsCards`.

### 2.5 Sessions tab upgrades

Embed `SessionEfficiencyTable` above the existing session list.
Add a "Compare" button/mode — user picks two sessions, clicks Compare, we hit `/api/sessions/compare` and render a two-column diff view. Out-of-scope for Phase 1 if time-constrained; can ship with just the efficiency table.

### 2.6 ProjectDashboard wiring

Register the new Cost tab in whichever tab-registry pattern `ProjectDashboard.tsx` uses. Tab label: `Cost`. Tab icon: `IconCurrencyDollar` from `@tabler/icons-react`.

## 3. Implementation split

Three agents, worktree-isolated:

### 3.1 `backend` — all Python
Files owned:
- `stackunderflow/stats/aggregator.py` — add 7 new collector classes + 1 trends function, wire into `summarise()`
- `stackunderflow/routes/sessions.py` — add `/api/sessions/compare` endpoint
- `tests/stackunderflow/stats/` — add unit tests for each new collector against mock data
- Ensure existing 138 tests still pass

### 3.2 `charts` — new React components, zero modification of existing files
Files owned:
- Everything under `stackunderflow-ui/src/components/cost/` (new directory)
- `stackunderflow-ui/src/types/analytics.ts` (new file, imported by api.ts later)
- Add to `stackunderflow-ui/src/types/api.ts` ONLY the single export re-statement + optional-field additions to `DashboardStats`. Everything else in `analytics.ts`.

### 3.3 `integrator` — wires everything, runs after `backend` and `charts` are merged
Files owned:
- `stackunderflow-ui/src/components/dashboard/CostTab.tsx` (new)
- `stackunderflow-ui/src/components/dashboard/OverviewTab.tsx` (edit)
- `stackunderflow-ui/src/components/dashboard/SessionsTab.tsx` (edit)
- `stackunderflow-ui/src/pages/ProjectDashboard.tsx` (edit — tab registration)
- End-to-end smoke test: run `curl /api/dashboard-data` against chimera, verify new fields; load Cost tab in browser, verify no console errors.

## 4. Constraints

- No breaking changes to existing API fields.
- No deletion of existing chart components even if their data is superseded — the Cost tab *adds*, doesn't replace. Overview tab upgrades are additive except for the cache/token mini-cards which collapse into the donut.
- All new TypedDicts in Python should degrade gracefully: if the collector fails, return an empty structure (`[]` or `{}`), never raise. The dashboard must still render if one section is empty.
- All new UI components must render gracefully with empty data (show "No data yet" or equivalent, not blank space or errors).
