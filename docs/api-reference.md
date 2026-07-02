# StackUnderflow API Reference

The REST API is served by `stackunderflow start` (or its alias `stackunderflow init`) on
`http://localhost:8081` by default — change the port with `--port` or `stackunderflow cfg set port <N>`.
FastAPI's auto-generated Swagger UI is at `/docs`. This file is the human-written companion: it explains
intent, payload shapes, and status codes. Most endpoints return JSON; the export, meta-agent, and live
routes stream instead.

**Error shapes.** Two forms appear, depending on where the failure is raised:

- Routes that raise `HTTPException` (most `400` / `404` / `409` / `503` cases) return FastAPI's
  `{"detail": "<message>"}`.
- Service-level failures and most `500` cases return `{"error": "<message>"}`.
- A missing or mistyped query/path parameter is caught by FastAPI validation and returns `422` with a
  `{"detail": [ ... ]}` list.

**Currency.** Most endpoints that return dollar figures — `/api/dashboard-data`, `/api/stats`,
`/api/projects`, `/api/cost-data`, `/api/cost-data/by-provider`, `/api/commands`, `/api/jsonl-files`,
`/api/sessions/compare`, `/api/yield`, `/api/plan` — carry a top-level
`currency: {code, symbol, rate_from_usd}` block, and their cost amounts are pre-converted from USD into the
active currency (set via `stackunderflow cfg set currency GBP` or `STACKUNDERFLOW_CURRENCY=GBP`).
`/api/global-stats` and `/api/compare` are the exceptions: both return raw USD with no `currency` block.
FX rates are fetched from Frankfurter and cached for 24h; if a fetch fails the API falls back to USD with
`rate_from_usd=1.0`.

---

## Endpoint Overview

| Method | Path | Group |
|--------|------|-------|
| GET | `/api/project` | Projects |
| POST | `/api/project` | Projects |
| POST | `/api/project-by-dir` | Projects |
| GET | `/api/projects` | Projects |
| GET | `/api/recent-projects` | Projects |
| GET | `/api/providers` | Projects |
| GET | `/api/global-stats` | Projects |
| GET | `/api/stats` | Dashboard data |
| GET | `/api/dashboard-data` | Dashboard data |
| GET | `/api/messages` | Dashboard data |
| GET | `/api/messages/summary` | Dashboard data |
| POST | `/api/refresh` | Dashboard data |
| GET | `/api/cost-data` | Cost analytics |
| GET | `/api/cost-data/by-provider` | Cost analytics |
| GET | `/api/commands` | Cost analytics |
| GET | `/api/tool-distribution` | Cost analytics |
| GET | `/api/interaction/{id}` | Cost analytics |
| GET | `/api/jsonl-files` | Sessions |
| GET | `/api/jsonl-content` | Sessions |
| GET | `/api/sessions/compare` | Sessions |
| GET | `/api/search` | Search |
| POST | `/api/search/reindex` | Search |
| GET | `/api/search/stats` | Search |
| GET | `/api/qa` | Q&A |
| GET | `/api/qa/{id}` | Q&A |
| POST | `/api/qa/reindex` | Q&A |
| GET | `/api/qa/stats` | Q&A |
| GET | `/api/tags` | Tags |
| GET | `/api/tags/browse/{tag}` | Tags |
| GET | `/api/tags/session/{id}` | Tags |
| POST | `/api/tags/session/{id}` | Tags |
| DELETE | `/api/tags/session/{id}/{tag}` | Tags |
| POST | `/api/tags/reindex` | Tags |
| GET | `/api/bookmarks` | Bookmarks |
| POST | `/api/bookmarks` | Bookmarks |
| PUT | `/api/bookmarks/{id}` | Bookmarks |
| DELETE | `/api/bookmarks/{id}` | Bookmarks |
| POST | `/api/bookmarks/toggle` | Bookmarks |
| GET | `/api/bookmarks/session/{id}` | Bookmarks |
| GET | `/api/cfg` | Settings |
| GET | `/api/cfg/currencies` | Settings |
| POST | `/api/cfg/currency` | Settings |
| GET | `/api/cfg/model-aliases` | Settings |
| POST | `/api/cfg/model-aliases` | Settings |
| DELETE | `/api/cfg/model-aliases` | Settings |
| GET | `/api/export` | Export |
| GET | `/api/compare` | Compare |
| GET | `/api/yield` | Yield |
| GET | `/api/plan` | Plan |
| GET | `/api/forks` | Forks |
| GET | `/api/whatif` | What-if |
| GET | `/api/budgets` | Budgets |
| PUT | `/api/budgets` | Budgets |
| DELETE | `/api/budgets` | Budgets |
| GET | `/api/optimize` | Optimize |
| GET | `/api/context-budget` | Context Budget |
| GET | `/api/etl/status` | ETL pipeline |
| POST | `/api/etl/backfill` | ETL pipeline |
| GET | `/api/agent-teams` | Agent teams |
| GET | `/api/agent-teams/{session_id}` | Agent teams |
| GET | `/api/agent-teams/{session_id}/agent/{agent_session_id}` | Agent teams |
| GET | `/api/playback/{session_id}` | Playback |
| GET | `/api/playback/project/{slug}` | Playback |
| GET | `/api/playback/{session_id}/fs` | Playback |
| GET | `/api/static-analysis/session/{session_id}` | Static analysis |
| GET | `/api/static-analysis/session/{session_id}/quality` | Static analysis |
| POST | `/api/static-analysis/session/{session_id}/grade` | Static analysis |
| GET | `/api/meta-agent/tools` | Meta-agent |
| POST | `/api/meta-agent/chat` | Meta-agent |
| GET | `/api/live/stats` | Live |
| GET | `/api/live/stream` | Live |
| POST | `/api/webhooks/github` | Webhooks |
| POST | `/api/webhooks/gitlab` | Webhooks |
| POST | `/api/webhooks/ci` | Webhooks |
| GET | `/api/health` | Misc |
| GET | `/api/pricing` | Misc |
| POST | `/api/pricing/refresh` | Misc |
| GET | `/api/pricing/doctor` | Misc |
| GET, POST, PUT, DELETE | `/ollama-api/{path}` | Misc |

---

## Projects / Lifecycle

### GET /api/project

Returns the currently selected project. When no project has been set, `status` is `"no_project"`.

**Response (no project selected)**

```json
{"status": "no_project", "message": "No project selected"}
```

**Response (project active)**

```json
{
  "status": "active",
  "project_path": "Users/yadkonrad/dev/myproject",
  "log_path": "/Users/yadkonrad/.claude/projects/-Users-yadkonrad-dev-myproject",
  "log_dir_name": "-Users-yadkonrad-dev-myproject"
}
```

**Status codes:** `200` always.

---

### POST /api/project

Set the active project by filesystem path. The server locates the Claude log directory automatically.

**Request body**

```json
{"project_path": "/Users/yadkonrad/dev/myproject"}
```

**Response**

```json
{
  "status": "success",
  "project_path": "/Users/yadkonrad/dev/myproject",
  "log_path": "/Users/yadkonrad/.claude/projects/-Users-yadkonrad-dev-myproject",
  "message": "Project set successfully. You can now view the dashboard."
}
```

**Status codes:** `200` success; `400` missing or non-existent `project_path`;
`404` project path exists but no Claude logs were found there.

---

### POST /api/project-by-dir

Set the active project using the Claude log directory slug (the `~/.claude/projects/<slug>` folder name).
This is the endpoint the React UI calls when the user picks a project from the sidebar.

**Request body**

```json
{"dir_name": "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow"}
```

**Response**

```json
{
  "status": "success",
  "project_path": "Users/yadkonrad/dev/dev/year26/jan26/StackUnderflow",
  "log_path": "/Users/yadkonrad/.claude/projects/-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
  "log_dir_name": "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
  "message": "Now analyzing logs from: -Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow"
}
```

**Status codes:** `200` success; `400` missing `dir_name` or path traversal attempt;
`404` directory not found or contains no `.jsonl` files.

---

### GET /api/projects

List all known projects from the session store with metadata. Supports sorting and pagination.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `include_stats` | bool | `false` | Include per-project statistics (slower) |
| `sort_by` | string | `last_modified` | One of `last_modified`, `first_seen`, `size`, `name` |
| `limit` | int | none | Max results to return |
| `offset` | int | `0` | Skip this many results (for pagination) |
| `provider` | repeated string | (none) | Repeated query param (`?provider=cursor&provider=cline`) scoping the list to those providers. Case-insensitive; empty means all. |

**Response**

```json
{
  "projects": [
    {
      "dir_name": "-Users-yadkonrad-dev-myproject",
      "log_path": "/Users/yadkonrad/.claude/projects/-Users-yadkonrad-dev-myproject",
      "file_count": 0,
      "total_size_mb": 0.0,
      "last_modified": 1776649168.04,
      "first_seen": 1772585609.12,
      "display_name": "-Users-yadkonrad-dev-myproject",
      "in_cache": false,
      "url_slug": "-Users-yadkonrad-dev-myproject",
      "provider": "claude",
      "providers": ["claude"],
      "stats": null
    }
  ],
  "total_count": 143,
  "has_more": true,
  "cache_status": {"cached_count": 0, "total_projects": 143},
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

`provider` is the most-recent provider that ingested the slug; `providers` is the sorted list of every
provider with a row for that slug (the schema has `UNIQUE(provider, slug)`, so one project used through both
Claude and Cursor yields two rows that the route collapses into one card). `file_count` is the project's
session count, summed across those provider rows. When `include_stats=true` and a non-USD currency is
active, each `stats.total_cost` is pre-converted; other `stats` figures stay raw.

**Status codes:** `200` always (errors return `500` with `{"error": "..."}`).

---

### GET /api/recent-projects

The 20 most recently modified projects, in a lighter shape than `/api/projects`. No query parameters.
Unlike `/api/projects`, rows are not collapsed by slug, so a project used through two providers can appear
twice. `file_count` is always `0` here (the store does not track a per-project file count for this view).

**Response**

```json
{
  "projects": [
    {
      "dir_name": "-Users-yadkonrad-dev-myproject",
      "log_path": "",
      "last_modified": 1776649168.04,
      "file_count": 0
    }
  ]
}
```

**Status codes:** `200` always. On a database error the body is `{"projects": [], "error": "..."}` with
status `200`.

---

### GET /api/providers

Every distinct provider in the store, with project and session counts. Powers the dashboard's provider
filter chips. No query parameters.

**Response**

```json
{
  "providers": [
    {"provider": "claude", "project_count": 180, "session_count": 1042},
    {"provider": "cursor", "project_count": 6,   "session_count": 23}
  ]
}
```

Providers sort by `project_count` descending; a null provider column reports as `"unknown"`.

**Status codes:** `200` success. On error the body is `{"providers": [], "error": "..."}` with status `500`.

---

### GET /api/global-stats

Aggregated statistics across **all** projects in the store. This is the only dashboard endpoint that does not
require a project to be selected. The Overview page calls this endpoint exclusively.

See the [Data Shapes Appendix](#data-shapes-appendix) for the full field reference.

**Response (abbreviated)**

```json
{
  "first_use_date": "2025-11-29",
  "last_use_date": "2026-04-20",
  "daily_token_usage": [{"date": "2025-11-29", "input": 0, "output": 0}],
  "daily_costs": [{"date": "2025-11-29", "cost": 0.0, "by_model": {}}],
  "models": {
    "claude-opus-4-6": {"count": 57584, "cost": 29402.30}
  },
  "total_cache_read_tokens": 16328970052,
  "total_cache_write_tokens": 1126449344,
  "config": {"max_date_range_days": 30}
}
```

**Status codes:** `200` success; `500` on database error.

---

## Dashboard Data

> **Project scoping:** `/api/stats`, `/api/dashboard-data`, `/api/messages`, and `/api/messages/summary` all
> act on the **current project** — the one most recently set via `POST /api/project-by-dir` or
> `POST /api/project`. If no project has been selected they return `400 {"error": "No project selected"}`.
> `/api/global-stats` (above) is the only aggregate endpoint that does not require a project.
> `POST /api/refresh` refreshes the current project when one is selected; if no project is set it refreshes
> **all** projects instead.

---

### GET /api/stats

Full statistics object for the current project, computed via the pipeline
(`classifier → enricher → aggregator`).

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `timezone_offset` | int | `0` | Minutes offset from UTC for daily bucketing |
| `days` | int | `90` | Cap `daily_stats` to the most recent N calendar days. Pass `0` to disable the cap. |
| `include` | repeated string | (none) | Repeated query param — return only the named top-level blocks (e.g. `?include=overview&include=models`). `currency` always passes through. |
| `details` | bool | `false` | When `false`, strip the heaviest per-row lists (`user_interactions.command_details`, `user_interactions.tool_count_distribution`, `errors.assistant_details`, `errors.error_details`, and the top-level `session_costs` / `command_costs` / `session_efficiency` / `retry_signals` lists; `outliers` lists are capped at 10 entries). On large stores this drops the payload from ~4 MB to ~150 KB while keeping every key present (empty list/dict where stripped). Set `true` to opt back into the legacy full-body response. |

**Response** — the top-level statistics dict (same as the `statistics` key in `/api/dashboard-data`).
Key nested sections include `overview`, `tools`, `sessions`, `daily_usage`, `models`, `costs`.

```json
{
  "overview": {
    "project_name": "StackUnderflow",
    "log_dir_name": "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
    "total_messages": 11341,
    "date_range": {"start": "2026-01-30T20:58:11.193Z", "end": "2026-04-20T01:39:11.887Z"},
    "sessions": 20,
    "message_types": {"user": 4381, "assistant": 6960},
    "total_tokens": {
      "input": 600390, "output": 3030862,
      "cache_creation": 86450124, "cache_read": 1466551362
    },
    "total_cost": 3767.61
  },
  "tools": {
    "usage_counts": {"Bash": 1296, "Read": 700, "Edit": 531},
    "error_counts": {},
    "error_rates": {"Bash": 0.0}
  }
}
```

**Status codes:** `200` success; `400` no project selected; `404` project not in store (run `/api/refresh`).

---

### GET /api/dashboard-data

Single call for the initial dashboard load — statistics plus the first 50 messages in one round-trip. The
heavy analytics sections live behind `/api/cost-data`, `/api/commands`, and `/api/tool-distribution`; this
payload keeps only the summary so initial paint stays fast.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `timezone_offset` | int | `0` | Minutes offset from UTC for daily bucketing |
| `provider` | repeated string | (none) | Scope to those providers. The current project belongs to one provider; an active filter that excludes it returns an empty payload with `filtered: true`. |
| `model` | repeated string | (none) | Scope the top-level `models` map to these model ids |

**Response**

```json
{
  "statistics": {"overview": {"...": "..."}, "tools": {"...": "..."}},
  "messages_page": {
    "messages": [{"...": "..."}],
    "total": 11341,
    "page": 1,
    "per_page": 50,
    "total_pages": 227,
    "start_index": 0,
    "end_index": 50
  },
  "message_count": 11341,
  "is_reindexing": false,
  "config": {
    "messages_initial_load": 50,
    "max_date_range_days": 30
  },
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

When a `provider` filter excludes the active project, the response is an empty-but-shape-stable body with
`"filtered": true` and a zeroed `messages_page`.

**Status codes:** `200` success; `400` no project; `404` project not in store.

---

### GET /api/messages

A page of pipeline-formatted messages for the current project. Returns a paginated envelope, not a bare
array. Default page size is 100; the maximum is 500. Earlier releases returned the full unbounded list,
which ballooned to ~37 MB on a 26K-message project — pagination caps the worst case at ~750 KB.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `page` | int | `1` | 1-indexed page number. Out-of-range values are clamped. |
| `per_page` | int | `100` | Items per page. Clamped to `[1, 500]`. |
| `limit` | int | none | Legacy alias — if set and `per_page` was left at its default, it caps `per_page` (also clamped to `[1, 500]`). New callers should use `page`/`per_page`. |
| `timezone_offset` | int | `0` | UTC offset in minutes |
| `provider` | repeated string | (none) | Scope to those providers. An active filter that excludes the project returns a shape-stable empty envelope. |
| `model` | repeated string | (none) | Scope by model id (filtered after the store read) |

**Response** — a paginated envelope.

```json
{
  "messages": [
    {
      "session_id": "020a13e5-ad60-41e5-9313-7cdf03cecf26",
      "type": "user",
      "timestamp": "2026-01-30T20:58:11.193Z",
      "model": null,
      "content": "What does this function do?",
      "tools": [],
      "tokens": {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
      "cwd": "/Users/yadkonrad/dev/myproject",
      "uuid": "…",
      "parent_uuid": "…",
      "is_sidechain": false,
      "has_tool_result": false,
      "error": false,
      "is_interruption": false,
      "message_id": null
    }
  ],
  "total": 11341,
  "page": 1,
  "per_page": 100,
  "total_pages": 114,
  "start_index": 0,
  "end_index": 100
}
```

`type` is the enricher's record kind — `user`, `assistant`, `tool_result`, `summary`, or `compact_summary`
(there is no `role` field). `tokens` is a nested dict. User-command messages also carry
`interaction_tool_count`, `interaction_model`, and `interaction_assistant_steps`.

**Status codes:** `200` success; `400` no project; `404` project not in store.

---

### GET /api/messages/summary

Counts and a per-type / per-model breakdown for the current project. No query parameters.

**Response**

```json
{
  "total": 11341,
  "total_sessions": 20,
  "by_type": {"user": 4381, "assistant": 6960},
  "by_model": {
    "claude-opus-4-6": 3566,
    "claude-opus-4-7": 1455,
    "null": 4381
  },
  "total_tokens": 3631252
}
```

`by_type` keys are record kinds (`user`, `assistant`, `tool_result`, …); messages with no model land under
a `null` key. `total_sessions` is only present when the project has a `project_mart` row — that mart row
also supplies the `total` count; otherwise `total` is the in-memory message count and `total_sessions` is
absent. `total_tokens` sums input + output across every message.

**Status codes:** `200` success; `400` no project; `404` project not in store.

---

### POST /api/refresh

Runs an incremental ingest pass and updates the store. If a project is active, only that project is
re-ingested. If no project is selected, all projects are refreshed.

**Request body** — a JSON object is required by the route signature (send `{}`); its contents are ignored.
Omitting the body entirely fails FastAPI validation with `422`.

**Response (project active)**

```json
{
  "status": "success",
  "message": "Files changed - data refreshed successfully",
  "files_changed": true,
  "message_count": 12,
  "refresh_time_ms": 340
}
```

**Response (no project — all projects)**

```json
{
  "status": "success",
  "message": "Ingested 42 new records",
  "files_changed": true,
  "refresh_time_ms": 1820,
  "projects_refreshed": 42,
  "total_projects": 42
}
```

**Status codes:** `200` always (errors are surfaced inside the JSON body or as `500`).

---

## Cost Analytics

The Cost tab loads its heavy attribution sections from a separate endpoint so the
initial dashboard payload stays small. All three endpoints accept an optional
`log_path` query parameter; when omitted they fall back to the project most recently
set via `POST /api/project-by-dir`.

### GET /api/cost-data

Return only the analytics sections that were split off `/api/dashboard-data`.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `log_path` | string | current project | Override the active project |
| `timezone_offset` | int | `0` | UTC offset in minutes for daily bucketing |

**Response** — an object keyed by the nine analytics sections. Missing sections
default to `[]` or `{}` (never `null`).

```json
{
  "session_costs":      [{"session_id": "...", "cost": 4.21, "tokens": {}, "commands": 12, "errors": 0, "duration_s": 940}],
  "command_costs":      [{"interaction_id": "...", "session_id": "...", "timestamp": "...", "prompt_preview": "...", "cost": 0.18, "tokens": {}, "tools_used": 3, "steps": 2, "models_used": ["..."], "had_error": false}],
  "tool_costs":         {"Bash": {"calls": 120, "cost": 1.40}},
  "token_composition":  {"input": 24000, "output": 96000, "cache_read": 1200, "cache_write": 800},
  "outliers":           {"...": "..."},
  "retry_signals":      [{"...": "..."}],
  "session_efficiency": [{"...": "..."}],
  "error_cost":         {"total": 0.42, "by_category": {}},
  "trends":             {"week_over_week": {}},
  "currency":           {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

**Status codes:** `200` success; `400` no project selected and no `log_path`;
`404` project not in store (run `/api/refresh` first).

---

### GET /api/cost-data/by-provider

Total cost, message count, and session count grouped by provider. Powers the Cost tab's
"Cost by provider" card.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `period` | string | `month` | One of `today`, `week`, `month`, `all`. Unknown values return `400`. |
| `provider` | repeated string | (none) | Narrow the rows to these providers (case-insensitive; empty means all) |

**Response**

```json
{
  "period": "month",
  "rows": [
    {"provider": "claude", "cost_usd": 412.88, "message_count": 22310, "session_count": 184},
    {"provider": "cursor", "cost_usd": 3.07,   "message_count": 1035,  "session_count": 23}
  ],
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

`rows` sort by cost descending. `cost_usd` is pre-converted to the active currency despite the field name
(it keeps the `_usd` suffix for backward compatibility with existing consumers). Unlike `/api/cost-data`
this route does not require a selected project — it rolls up the whole store.

**Status codes:** `200` success; `400` unknown `period`.

---

### GET /api/commands

Paginated, sorted slice of the project's command list. Used by the Cost tab's
"Most expensive commands" panel; clicking a row deep-links to the corresponding
interaction in the Messages tab.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `log_path` | string | current project | Override the active project |
| `offset` | int | `0` | Pagination offset (clamped to ≥ 0) |
| `limit` | int | `50` | Page size (clamped to `[1, 500]`) |
| `sort` | string | `cost` | One of `cost`, `tokens`, `tools`, `steps`, `time`. Unknown values fall back to `cost`. |
| `order` | string | `desc` | `desc` or `asc` |

**Response**

```json
{
  "commands": [
    {
      "interaction_id": "abc123...",
      "session_id": "020a13e5-...",
      "timestamp": "2026-04-19T12:34:56Z",
      "prompt_preview": "How do I refactor this function?",
      "cost": 0.42,
      "tokens": {"input": 1200, "output": 800, "cache_read": 0, "cache_write": 0},
      "tools_used": 3,
      "steps": 2,
      "models_used": ["claude-opus-4-6"],
      "had_error": false
    }
  ],
  "total": 482,
  "offset": 0,
  "limit": 50,
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

When the project has no enriched dataset the body is `{"commands": [], "total": 0, "offset", "limit"}`
(no `currency` key on that empty branch).

**Status codes:** `200` success; `400` no project selected and no `log_path`;
`404` project not in store.

---

### GET /api/interaction/{interaction_id}

Return one enriched interaction (the originating user command + every assistant
response + every tool result between them). Lets the Messages tab deep-link to a
specific prompt without paging through the full message list.

**Path parameter:** `interaction_id` — the `interaction_id` from `/api/commands`.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `log_path` | string | current project | Override the active project |

**Response** — a JSON object describing the interaction:

```json
{
  "interaction_id": "abc123...",
  "session_id": "020a13e5-...",
  "start_time": "2026-04-19T12:34:56Z",
  "end_time":   "2026-04-19T12:35:48Z",
  "model": "claude-opus-4-6",
  "tool_count": 3,
  "assistant_steps": 2,
  "is_continuation": false,
  "tools_used": ["Bash", "Read"],
  "has_task_tool": false,
  "command":      {"kind": "user", "content": "...", "tokens": {}, "tools": [], "...": "..."},
  "responses":    [{"kind": "assistant", "content": "...", "tokens": {}, "...": "..."}],
  "tool_results": [{"kind": "tool_result", "content": "...", "...": "..."}]
}
```

**Status codes:** `200` success; `400` no project selected and no `log_path`;
`404` project (or interaction) not found.

---

## Sessions

### GET /api/jsonl-files

Every session (JSONL file) for a project, with per-session metadata and cost estimates.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `project` | string | current project | Log directory slug to query |
| `provider` | repeated string | (none) | Scope to those providers. If the project's provider is not in the set the response is `{"files": [], "currency": {...}}`. |

**Response** — an object with a `files` array (sorted by `created` ascending) and a `currency` block.

```json
{
  "files": [
    {
      "name": "020a13e5-ad60-41e5-9313-7cdf03cecf26.jsonl",
      "path": "020a13e5-ad60-41e5-9313-7cdf03cecf26.jsonl",
      "is_subagent": false,
      "created": 1738274291.193,
      "modified": 1738274291.193,
      "size": 0,
      "messages": 120,
      "user_messages": 40,
      "assistant_messages": 80,
      "input_tokens": 24000,
      "output_tokens": 96000,
      "model": "claude-opus-4-6",
      "title": "What does this function do?",
      "tool_calls": 15,
      "estimated_cost": 0.4812,
      "provider": "claude"
    }
  ],
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

`is_subagent` is `true` when the session id starts with `agent-`. `provider` carries the parent project's
provider name (`claude`, `codex`, `cursor`, `cline`, …); the React UI uses it to colour the per-session
chip. When the slug is not found in the store the route returns a bare empty array `[]` instead of the
object form.

**Status codes:** `200` success; `400` no project; `500` internal error.

---

### GET /api/jsonl-content

Return the raw parsed messages for a single session, identified by filename.

**Query parameters**

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `file` | string | yes | Filename, e.g. `020a13e5-....jsonl` |
| `project` | string | no | Log dir slug (defaults to current project) |

**Response**

```json
{
  "lines": [{"type": "user", "message": {"role": "user", "content": "..."}}],
  "total_lines": 120,
  "user_count": 40,
  "assistant_count": 80,
  "metadata": {
    "session_id": "020a13e5-ad60-41e5-9313-7cdf03cecf26",
    "file_size": 0,
    "created": 1738274291.193,
    "modified": 1745116760.887,
    "first_timestamp": "2026-01-30T20:58:11.193Z",
    "last_timestamp": "2026-04-20T01:39:20.887Z",
    "duration_minutes": 117.6,
    "cwd": "/Users/yadkonrad/dev/myproject"
  }
}
```

**Status codes:** `200` success; `400` no project or invalid `file` param;
`404` project or session not found in store; `500` internal error.

---

### GET /api/sessions/compare

Side-by-side diff of two sessions, reusing the same `session_costs` rows the Cost
tab consumes so cost attribution stays consistent.

**Query parameters**

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `a` | string | yes | Baseline `session_id` |
| `b` | string | yes | Comparison `session_id` |
| `log_path` | string | no | Override the active project |

**Response**

```json
{
  "a":    {"session_id": "020a13e5-...", "cost": 4.21, "tokens": {}, "commands": 12, "errors": 0, "duration_s": 940},
  "b":    {"session_id": "7f72e05c-...", "cost": 5.83, "tokens": {}, "commands": 14, "errors": 1, "duration_s": 1120},
  "diff": {"cost": 1.62, "tokens": {}, "commands": 2, "errors": 1, "duration_s": 180},
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

`diff` values are `b - a`. Token diffs are computed for the union of token keys
present in either side.

**Status codes:** `200` success; `400` no project selected and no `log_path`;
`404` project or one/both `session_id`s not found; `500` internal error.

---

## Search

### GET /api/search

Full-text search across all indexed Claude Code sessions using SQLite FTS5.
Returns `503` if the search service failed to initialise on startup.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `q` | string | `""` | Search query |
| `project` | string | none | Filter to a specific project slug |
| `date_from` | string | none | ISO date `YYYY-MM-DD` (inclusive lower bound) |
| `date_to` | string | none | ISO date `YYYY-MM-DD` (inclusive upper bound) |
| `model` | string | none | Filter by model name |
| `role` | string | none | Filter by role (`user` or `assistant`) |
| `page` | int | `1` | Result page (1-indexed) |
| `per_page` | int | `20` | Results per page (max 100) |

**Response**

```json
{
  "results": [
    {
      "id": 238980,
      "session_id": "7f72e05c-93a2-4221-9a3b-ce5648e0b433",
      "project": "-Users-yadkonrad-dev-myproject",
      "role": "assistant",
      "content": "Here is the refactored version...",
      "timestamp": "2026-03-23T20:58:44.155Z",
      "model": "claude-opus-4-6",
      "tokens_input": 1,
      "tokens_output": 176,
      "snippet": "...Here is the <mark>refactored</mark> version..."
    }
  ],
  "total": 233,
  "page": 1,
  "per_page": 20,
  "total_pages": 12,
  "query": "refactor"
}
```

**Status codes:** `200` success; `503` search service unavailable; `500` query error.

---

### POST /api/search/reindex

Rebuild the full-text search index from all project data in the store. This is a blocking operation.

**Request body** — empty object `{}`.

**Response**

```json
{
  "projects_indexed": 98,
  "total_messages_indexed": 82759,
  "elapsed_ms": 4230.5
}
```

**Status codes:** `200` success; `503` search service unavailable; `500` reindex error.

---

### GET /api/search/stats

Return metadata about the search index — total message count, known models, and a per-project index log.

**Response**

```json
{
  "total_messages": 82759,
  "total_projects": 98,
  "models": ["claude-opus-4-6", "claude-sonnet-4-6", "claude-opus-4-7"],
  "indexed_projects": [
    {
      "project": "-Users-yadkonrad-dev-myproject",
      "indexed_at": "2026-04-02T02:48:28.332578+00:00",
      "message_count": 13525
    }
  ]
}
```

**Status codes:** `200` success; `503` search service unavailable; `500` error.

---

## Q&A

### GET /api/qa

List extracted Q&A pairs with filtering and pagination. A Q&A pair is a (user question, assistant answer)
tuple extracted from sessions. Returns `503` if the Q&A service failed to initialise.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `project` | string | none | Filter to a specific project slug |
| `date_from` | string | none | ISO date `YYYY-MM-DD` |
| `date_to` | string | none | ISO date `YYYY-MM-DD` |
| `search` | string | none | Text filter applied to question and answer text |
| `resolution_status` | string | none | One of `resolved`, `looped`, `open` |
| `page` | int | `1` | Page number (1-indexed) |
| `per_page` | int | `20` | Results per page (max 100) |

**Response**

```json
{
  "results": [
    {
      "id": "edaf6f427ec0a5de",
      "session_id": "14b76974-d5c3-4430-b7e6-db7513bb1499",
      "project": "-Users-yadkonrad-dev-myproject",
      "question_text": "help me find this chrome tab plugin...",
      "answer_text": "Let me search for the ObserverTab extension...",
      "code_snippets": [],
      "tools_used": [],
      "timestamp": "2026-04-01T00:07:45.083Z",
      "model": "claude-opus-4-6",
      "num_attempts": 1,
      "resolution_status": "open",
      "loop_count": 0
    }
  ],
  "total": 4986,
  "page": 1,
  "per_page": 20,
  "total_pages": 250
}
```

**Status codes:** `200` success; `503` Q&A service unavailable; `500` error.

---

### GET /api/qa/{qa_id}

Fetch a single Q&A pair by its hex ID.

**Path parameter:** `qa_id` — the hex string ID from the `id` field in `/api/qa` results.

**Response** — same shape as a single element from `/api/qa`'s `results` array.

**Status codes:** `200` success; `404` pair not found; `503` service unavailable; `500` error.

---

### POST /api/qa/reindex

Rebuild the Q&A index by re-extracting pairs from all sessions in the store.

**Request body** — empty object `{}`.

**Response**

```json
{
  "projects_indexed": 98,
  "total_qa_indexed": 4986,
  "elapsed_ms": 8120.3
}
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

### GET /api/qa/stats

Return aggregate statistics about the Q&A index, including a per-project breakdown.

**Response (abbreviated)**

```json
{
  "total_pairs": 4986,
  "by_project": [
    {"project": "-Users-yadkonrad-dev-myproject", "count": 1310}
  ]
}
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

## Tags

### GET /api/tags

Return the full tag cloud — all tags with their session counts, category, and display colour.
Returns `503` if the tag service failed to initialise.

**Response**

```json
{
  "tags": [
    {"name": "database", "count": 130, "category": "topic", "color": "#dd6b20"},
    {"name": "python",   "count": 78,  "category": "language", "color": "#3572A5"},
    {"name": "Bash",     "count": 101, "category": "tool", "color": "#718096"}
  ],
  "total_sessions": 144
}
```

Tag `category` values observed: `topic`, `language`, `framework`, `tool`.

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

### GET /api/tags/browse/{tag}

List every session that carries a given tag.

**Path parameter:** `tag` — the tag name (e.g. `python`, `debugging`).

**Response**

```json
{
  "tag": "python",
  "sessions": [
    {"session_id": "020a13e5-...", "project": "-Users-yadkonrad-dev-myproject"}
  ],
  "count": 78
}
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

### GET /api/tags/session/{session_id}

Return all tags attached to a specific session.

**Path parameter:** `session_id` — UUID of the session.

**Response** — the shape is whatever the tag service returns; typically a list of tag names.

```json
["python", "debugging", "fastapi"]
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

### POST /api/tags/session/{session_id}

Manually add a tag to a session.

**Path parameter:** `session_id` — UUID of the session.

**Request body**

```json
{"tag": "my-custom-tag"}
```

**Response** — the result object returned by the tag service.

```json
{"status": "added", "tag": "my-custom-tag", "session_id": "020a13e5-..."}
```

**Status codes:** `200` success; `400` missing `tag`; `503` service unavailable; `500` error.

---

### DELETE /api/tags/session/{session_id}/{tag}

Remove a manually added tag from a session.

**Path parameters:** `session_id` (UUID), `tag` (tag name).

**Response** — the result object returned by the tag service.

```json
{"status": "removed", "tag": "my-custom-tag", "session_id": "020a13e5-..."}
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

### POST /api/tags/reindex

Rebuild auto-tags for all sessions across all projects.

**Request body** — empty object `{}`.

**Response**

```json
{
  "projects_indexed": 98,
  "total_sessions_tagged": 144,
  "elapsed_ms": 3100.8
}
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

## Bookmarks

### GET /api/bookmarks

List all bookmarks, optionally filtered by tag and sorted.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `tag` | string | none | Filter to bookmarks carrying this tag |
| `sort_by` | string | `created_at` | Sort field |

**Response**

```json
{
  "bookmarks": [
    {
      "id": "bm_abc123",
      "session_id": "020a13e5-...",
      "title": "Great refactor session",
      "notes": "Shows the clean pipeline pattern",
      "tags": ["python", "refactoring"],
      "message_index": 42,
      "created_at": "2026-03-01T12:00:00Z",
      "session_first_ts": "2026-03-01T09:00:00Z",
      "session_last_ts": "2026-03-01T11:55:00Z",
      "session_message_count": 120
    }
  ]
}
```

The `session_first_ts`, `session_last_ts`, and `session_message_count` fields are enriched from the session
store and may be absent if the session is not in the store.

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

### POST /api/bookmarks

Create a new bookmark.

**Request body**

```json
{
  "session_id": "020a13e5-ad60-41e5-9313-7cdf03cecf26",
  "title": "Great refactor session",
  "message_index": 42,
  "notes": "Shows the clean pipeline pattern",
  "tags": ["python", "refactoring"]
}
```

`session_id` is required. All other fields are optional (`title` defaults to `"Untitled bookmark"`).

**Response** — the created bookmark object (same shape as one element in `GET /api/bookmarks`).

**Status codes:** `201` created; `400` missing `session_id`; `503` service unavailable; `500` error.

---

### PUT /api/bookmarks/{bookmark_id}

Update a bookmark's `title`, `notes`, and/or `tags`.

**Path parameter:** `bookmark_id` — the bookmark's ID string.

**Request body** — all fields optional; only provided fields are updated.

```json
{
  "title": "Updated title",
  "notes": "New notes",
  "tags": ["python"]
}
```

**Response** — the updated bookmark object.

**Status codes:** `200` success; `404` not found; `503` service unavailable; `500` error.

---

### DELETE /api/bookmarks/{bookmark_id}

Remove a bookmark by ID.

**Response**

```json
{"status": "success", "message": "Bookmark removed"}
```

**Status codes:** `200` success; `404` not found; `503` service unavailable; `500` error.

---

### POST /api/bookmarks/toggle

Add a bookmark if the session is not already bookmarked, or remove it if it is. Useful for a
single-click "star" button.

**Request body**

```json
{
  "session_id": "020a13e5-ad60-41e5-9313-7cdf03cecf26",
  "title": "Interesting session",
  "message_index": 0
}
```

`session_id` is required. `title` and `message_index` are optional.

**Response** — the result object returned by the bookmark service.

```json
{"action": "added", "bookmark": {"id": "bm_abc123", "session_id": "020a13e5-..."}}
```

or `{"action": "removed", "bookmark_id": "bm_abc123"}` when the bookmark existed.

**Status codes:** `200` success; `400` missing `session_id`; `503` service unavailable; `500` error.

---

### GET /api/bookmarks/session/{session_id}

List all bookmarks attached to a specific session.

**Path parameter:** `session_id` — UUID of the session.

**Response**

```json
{"bookmarks": [{"id": "bm_abc123", "title": "Great session", "...": "..."}]}
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

## Settings

These endpoints back the **Settings page** (`/settings` in the React UI) so the dashboard can
read and write the same persistent settings that the `stackunderflow cfg` CLI manipulates.
Writes go through the same `Settings.persist` machinery (validators run; the config file at
`~/.stackunderflow/config.json` is the single source of truth) and invalidate the dashboard
cache so the next `/api/dashboard-data` reflects the new values.

**UI surfaces:**

- `/settings` — Currency dropdown (24 common ISO codes plus an "Other" text-input branch),
  Model aliases table (add/remove `from → to` pairs).
- The dashboard tab bar — an **Export** button (top-right of every project tab) opens a
  popover with format (CSV/JSON) and period (Today/7d/30d/All) controls; submit issues a
  `GET /api/export?...` request and triggers a browser download.

### GET /api/cfg

Return all user-facing settings plus the currently active currency block.

**Response**

```json
{
  "settings": {
    "currency": "EUR",
    "model_aliases": {"openrouter/claude-opus": "claude-opus-4-6"},
    "port": 8081,
    "...": "..."
  },
  "currency": {"code": "EUR", "symbol": "€", "rate_from_usd": 0.93}
}
```

**Status codes:** `200` always.

---

### GET /api/cfg/currencies

Return the suggested + locally cached currency codes the UI can render.

**Response**

```json
{
  "common": ["USD", "EUR", "GBP", "JPY", "..."],
  "supported": ["USD", "AUD", "BGN", "..."],
  "current": {"code": "EUR", "symbol": "€", "rate_from_usd": 0.93}
}
```

`common` is the shortlist always shown in the dropdown. `supported` is whatever Frankfurter has
published locally (cached); the file may be empty until the first conversion fetch lands.

**Status codes:** `200` always.

---

### POST /api/cfg/currency

Set the active currency. Body: `{"code": "EUR"}` (or `{"currency": "EUR"}` — both are accepted).
Codes are normalised to uppercase. Invalidates the dashboard cache so the next data fetch
reflects the new currency.

**Response**

```json
{"currency": {"code": "EUR", "symbol": "€", "rate_from_usd": 0.93}}
```

**Status codes:** `200` success; `400` if the code is not a 3-letter ISO 4217 string.

---

### GET /api/cfg/model-aliases

Return the current proxy → canonical alias map.

**Response**

```json
{"aliases": {"openrouter/claude-opus": "claude-opus-4-6"}}
```

**Status codes:** `200` always.

---

### POST /api/cfg/model-aliases

Add or update one alias. Body: `{"from": "<proxy>", "to": "<canonical>"}`. Both fields must be
non-empty strings.

**Response**

```json
{"aliases": {"openrouter/claude-opus": "claude-opus-4-6", "...": "..."}}
```

**Status codes:** `200` success; `400` if either field is empty / missing.

---

### DELETE /api/cfg/model-aliases

Remove one alias. The proxy id is passed as `?from=…` (query parameter, not path) because
alias keys often contain slashes (`openrouter/...`) which would need double-encoding to round-trip
a path component.

**Response**

```json
{"aliases": {"...": "..."}}
```

**Status codes:** `200` success; `400` if `from` is empty / missing; `404` if the alias is not present.

---

## Misc

### GET /api/health

Liveness and readiness check. Reports whether each optional service initialised successfully.

**Response**

```json
{
  "status": "ok",
  "services": {
    "search": true,
    "tags": true,
    "qa": true,
    "bookmarks": true,
    "pricing": true
  }
}
```

`false` for a service means it failed to start (the corresponding `/api/<service>/*` endpoints will
return `503`).

**Status codes:** `200` always.

---

### GET /api/pricing

Return the current per-model pricing table. Data is fetched from LiteLLM and cached locally.

**Response**

```json
{
  "pricing": {
    "claude-opus-4-6": {
      "input_cost_per_token": 1.5e-05,
      "output_cost_per_token": 7.5e-05,
      "cache_creation_cost_per_token": 1.875e-05,
      "cache_read_cost_per_token": 1.5e-06
    }
  },
  "source": "litellm",
  "timestamp": "2026-04-20T01:39:47.194182+00:00",
  "is_stale": false
}
```

**Status codes:** `200` success; `503` service unavailable; `500` error.

---

### POST /api/pricing/refresh

Force a re-fetch of pricing data from LiteLLM, bypassing the local cache.

**Request body** — empty object `{}`.

**Response**

```json
{"status": "success", "message": "Pricing updated successfully"}
```

**Status codes:** `200` success; `500` fetch failed or service error; `503` service unavailable.

---

### GET /api/pricing/doctor

Read-only pricing-health report — the same payload `stackunderflow pricing doctor` renders, assembled by
the shared `assemble_pricing_health` function so the CLI and HTTP surfaces never disagree. Only `SELECT`
queries; no writes, no network.

**Query parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `stale_days` | int | no | Overlay-cache age (days) past which rates are reported stale. Default `7`. |
| `limit` | int | no | Max entries per model list; full counts stay in `summary`. Default `50`. |

**Response**

```json
{
  "stale_days": 7,
  "ok": true,
  "summary": {
    "total_events": 228311,
    "total_cost_usd": 7421.03,
    "unpriced_model_count": 2,
    "billable_unpriced_model_count": 0,
    "unknown_cost_source_model_count": 2,
    "unknown_nonzero_cost_rows": 0,
    "estimated_unpriced_exposure_usd": 12.41,
    "rate_cache_stale": false
  },
  "unpriced_models": [ { "provider": "...", "model": "...", "events": 0, "billable": false, "estimated_delta_usd": 0.0 } ],
  "unknown_cost_source": [ { "...": "per-model rollup of cost_source='unknown' rows" } ],
  "rate_freshness": { "age_days": 2, "stale_days_threshold": 7, "stale": false }
}
```

`ok` reflects hard defects only: a billable row referencing an unresolvable model, or an `unknown` row
carrying a nonzero cost. Unpriced exotic models correctly stamped `unknown` (⇒ $0) and a stale overlay are
warnings, not failures. Both model lists are sorted by estimated dollar exposure descending and truncated
to `limit`. A fresh-install store returns an empty, `ok`-true payload.

**Status codes:** `200` always (including the fresh-install case).

---

### /ollama-api/{path}

Pass-through proxy to a local Ollama instance at `http://localhost:11434/api/{path}`. Accepts `GET`, `POST`,
`PUT`, and `DELETE`; the request body and most headers are forwarded verbatim, and chunked responses are
streamed back. This is what the chat sidebar's model discovery and any in-app Ollama feature talk to so the
browser never has to reach `localhost:11434` directly. (The meta-agent chat route calls Ollama server-side
and separately honours `STACKUNDERFLOW_OLLAMA_URL`; this proxy is always local.)

**Status codes:** mirrors whatever Ollama returns; `502` with `{"error": "Ollama not available"}` when the
local instance is unreachable.

The static-asset routes `GET /favicon.ico` and `GET /assets/{path}` are also served by this module; they
return files, not JSON, and are not part of the data API.

---

## Data Shapes Appendix

### Global Stats shape (`GET /api/global-stats`)

This endpoint is the primary data source for the Overview page. The authoritative implementation is in
`stackunderflow/store/queries.py:get_global_stats()`. The `config` key is appended by the route handler.

| Field | Type | Description |
|-------|------|-------------|
| `first_use_date` | string | ISO date `YYYY-MM-DD` of the earliest message in the store |
| `last_use_date` | string | ISO date `YYYY-MM-DD` of the most recent message |
| `daily_token_usage` | array | Per-day `{date, input, output}` token totals across all projects |
| `daily_costs` | array | Per-day `{date, cost, by_model}` cost rollup; `by_model` maps model name to cost float |
| `models` | object | Map of model name to `{count, cost}` across all time |
| `total_cache_read_tokens` | int | Sum of `cache_read_tokens` across every message in the store |
| `total_cache_write_tokens` | int | Sum of `cache_create_tokens` across every message in the store |
| `config` | object | Server config values surfaced to the UI; currently `{max_date_range_days}` |

**`daily_token_usage` element**

```json
{"date": "2026-03-23", "input": 24000, "output": 96000}
```

**`daily_costs` element**

```json
{
  "date": "2026-03-23",
  "cost": 1.44,
  "by_model": {
    "claude-opus-4-6": 1.20,
    "claude-sonnet-4-6": 0.24
  }
}
```

**`models` entry**

```json
{
  "claude-opus-4-6": {"count": 57584, "cost": 29402.30}
}
```

`count` is the number of messages attributed to the model; `cost` is the cumulative USD cost computed
using the pricing table.


## Export

### GET /api/export

Stream a downloadable CSV or JSON file of cross-project usage data.
Same surface as the `stackunderflow export` CLI — both share a single
internal helper.

**Query parameters**

| Name       | Type                                        | Required | Description |
|------------|---------------------------------------------|----------|-------------|
| `format`   | `csv \| json`                               | yes      | Output format. |
| `period`   | `today \| week \| month \| all`             | no       | Single window. Omit for a multi-period rollup (today + last 7 days + last 30 days) in one document. |
| `provider` | string                                      | no       | Filter by provider (`claude`, `codex`, etc.). |
| `project`  | string (repeatable)                         | no       | Include only this project slug. |
| `exclude`  | string (repeatable)                         | no       | Exclude this project slug. |

**Response headers**

- `Content-Type`: `text/csv` or `application/json`
- `Content-Disposition`: `attachment; filename="stackunderflow-export-<period>-<YYYY-MM-DD>.<ext>"`
- `X-Suggested-Filename`: same filename, surfaced for fetch/blob clients
  that do not parse `Content-Disposition`.

**Body**

For `format=csv` the body is a multi-section CSV: one daily-rows block
plus one activity block per period. For `format=json` the body is a
period dict (with `--period`) or a `{today, last_7d, last_30d}` rollup
(without). See the CLI reference for the full schema.

---

## Compare

### GET /api/compare

Per-model side-by-side comparison over a window. Same surface as the `stackunderflow compare` CLI — both
call `stackunderflow.services.compare.build_compare_payload`.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `period` | string | `month` | Window: `today`, `week`, `month`, or `all`. Unknown values return `400`. |
| `project` | repeated string | (none) | Restrict to these project slugs |
| `provider` | string | (none) | Filter by provider id (`claude`, `codex`, `cursor`, …) |

**Response**

```json
{
  "period": "month",
  "models": [
    {
      "model": "claude-opus-4-6",
      "provider": "claude",
      "sessions": 12,
      "calls": 480,
      "one_shot_pct": 0.167,
      "retry_rate": 2.33,
      "cache_hit_rate": 0.924,
      "cost_per_call": 0.021,
      "cost_per_session": 0.84,
      "total_cost": 10.07,
      "total_tokens": 4120000
    }
  ],
  "generated": 1746125443.117
}
```

`models` sorts by `total_cost` descending; an empty store yields `"models": []`. `generated` is a Unix
epoch float. Per-row fields:

- `one_shot_pct` — fraction of sessions where the user prompted once, the assistant answered once, and that
  was it (range 0.0–1.0).
- `retry_rate` — `(assistant_messages / sessions) - 1`, the average number of extra assistant turns per
  session.
- `cache_hit_rate` — `cache_read / (cache_read + cache_create)` (range 0.0–1.0).
- `cost_per_call`, `cost_per_session`, `total_cost` are raw USD. This route does not pre-convert to the
  active currency and emits no `currency` block.

**Status codes:** `200` success; `400` unknown `period`.

---

## Yield

### GET /api/yield

Productive vs reverted vs abandoned breakdown for the sessions in a window. Same surface as
`stackunderflow yield`.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `period` | string | `month` | One of `today`, `week`, `month`, `all`, `7days`, `30days`. Unknown values return `400`. (`week` is an alias for `7days`.) |
| `project` | repeated string | (none) | Filter to one or more project slugs |

**Response**

```json
{
  "period": "month",
  "summary": {
    "productive": 42,
    "reverted": 3,
    "abandoned": 18,
    "no_repo": 5,
    "total": 68,
    "productive_cost": 124.55,
    "reverted_cost": 9.12,
    "abandoned_cost": 47.30,
    "no_repo_cost": 0.42,
    "total_cost": 181.39
  },
  "entries": [
    {
      "session_id": "ada0010e-f34f-4db5-9025-caa4a0db2b6a",
      "project_slug": "-Users-you-dev-myproj",
      "cwd": "/Users/you/dev/myproj",
      "started_at": "2026-04-25T14:02:11+00:00",
      "cost_usd": 12.43,
      "classification": "productive",
      "follow_commit_sha": "abc123def4567...",
      "follow_commit_msg": "feat: add yield route",
      "follow_commit_age_hours": 1.8
    }
  ],
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0},
  "warning": "Yield is correlated by time, not by content. ..."
}
```

`entries` sorts by `cost_usd` descending. `classification` is one of `productive`, `reverted`,
`abandoned`, `no_repo`. The `*_cost` fields in `summary` and each entry's `cost_usd` are pre-converted to
the active currency (USD by default). The `warning` field always carries the heuristic caveat — frontend
consumers should render it inline so users read the breakdown as a smoke signal, not an audit.

**Status codes:** `200` success; `400` unknown `period`.

**Heuristic warning.** Yield correlates sessions with commits by time, not by content. A commit landing
within 24h of a session start is credited to that session even if it was about something else. Multiple
sessions in the same repo on the same day share follow-up commit attribution. See the CLI reference for the
per-class definitions.

---

## Plan

### GET /api/plan

Active plan + current usage against budget + burn-projector v2 forecast.
Same payload as `stackunderflow plan show --format json`; see the CLI
reference's "Plan Budget Commands" section for the field-by-field
semantics. Status banding (`ok` / `warn` / `over`) is computed
identically to the CLI.

**Query parameters** — none.

**Response (no plan configured)**

```json
{
  "plan": null,
  "usage": null,
  "projection": null,
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

When no plan is set, `plan` / `usage` / `projection` are all `null` so
the frontend can render an "add a plan" CTA without parsing fields.

**Response (plan configured)**

```json
{
  "plan": {
    "name": "claude-pro",
    "monthly_usd": 20.0,
    "reset_day": 1
  },
  "usage": {
    "used": 12.50,
    "budget": 20.00,
    "remaining": 7.50,
    "pct": 62.5,
    "projected": 32.29,
    "status": "ok",
    "period_start": "2026-05-01",
    "period_end": "2026-05-31",
    "days_so_far": 12,
    "days_in_period": 31
  },
  "projection": {
    "projected_month_end_usd": 32.29,
    "projection_method": "weighted-7d",
    "daily_burn_usd": 1.04,
    "days_to_limit": 7,
    "thresholds": [50, 75, 90],
    "crossed_threshold": 50,
    "alert": "Crossed 50% of plan budget"
  },
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

**Field notes**

- `plan.monthly_usd` — canonical USD amount the user signed up for.
  Always in USD regardless of the active currency, because it's the
  user's contract amount.
- `usage.used`, `usage.budget`, `usage.remaining`, `usage.projected`
  are pre-converted to the active currency via `currency.rate_from_usd`
  (same convention as `/api/cost-data` and `/api/yield`).
- `usage.pct` is dimensionless and computed pre-conversion so the
  status banding (`pct < 80 → ok`, `80 ≤ pct ≤ 100 → warn`,
  `pct > 100 → over`) is identical across currencies.
- `usage.projected` is the legacy linear extrapolation kept for
  backward compat; the burn-projector v2 number lives under
  `projection.projected_month_end_usd` (which can use a smarter
  weighted-7d average — see below).
- `projection.projection_method` is `"weighted-7d"` once the period
  has accumulated at least 3 non-zero daily samples (the weighted
  average decays at 0.85/day so recent activity dominates), else
  `"linear"`. Auto-falls back to `"linear"` when the recent 7-day
  window collapses to $0 against an otherwise non-empty period
  (stale-store case) so the forecast doesn't silently zero out.
- `projection.daily_burn_usd` and `projection.projected_month_end_usd`
  are pre-converted to the active currency, same as the `usage.*`
  fields. `thresholds` / `crossed_threshold` / `days_to_limit` /
  `projection_method` are dimensionless.
- `projection.crossed_threshold` is the highest configured threshold
  the user has met or exceeded (or `null`). `projection.alert` is a
  human-readable banner string (or `null`); the "projected to overrun"
  message supersedes a threshold-crossed message when the forecast
  exceeds the budget before the period ends.
- `projection.thresholds` echoes the active alert ladder (default
  `[50, 75, 90]`; configurable via `stackunderflow plan thresholds set`).
- `currency` is always present on both branches.

**Status codes:** `200` always.

---

## Forks

### GET /api/forks

Fork / sidechain economics: the cost and token share that went to subagent (sidechain) messages, plus the
fork points where a conversation branched and one path was abandoned — priced from the message DAG that
`is_sidechain` + `parent_uuid` capture.

**Query parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `period` | string | no | `today` \| `week` \| `month` \| `30days` \| `all` (default `all`; `week` = `7days`) |
| `log_path` | string | no | Project log path; defaults to the active project, or whole-store when none is selected |

**Response** — `{period, scope, report, currency, warning}`. `report` carries the fork analysis with every
dollar figure pre-converted to the active currency (`sidechain_cost_usd`, `total_cost_usd`,
`abandoned_cost_usd`, and per-branch `cost_usd` on `abandoned_branches`). `warning` is a fixed caveat
string: branch abandonment is inferred from the DAG, so edits, retries, and tool re-runs can look like
abandoned branches — treat the list as a signal to review, not a verdict.

**Status codes:** `200` success; `400` invalid `period`.

---

## What-If Repricing

### GET /api/whatif

Reprice the project's (or store's) total token workload across candidate provider/model rate cards —
"what would this workload have cost on X?".

**Query parameters**

| Param | Type | Required | Description |
|-------|------|----------|-------------|
| `log_path` | string | no | Project log path; defaults to the active project, or whole-store when none is selected |

**Response**

```json
{
  "tokens": {"input": 0, "output": 0, "cache_read": 0, "cache_create": 0, "total": 0},
  "actual": {"cost_usd": 0.0, "models": ["..."]},
  "candidates": [
    {"provider": "...", "model": "...", "label": "...",
     "cost_usd": 0.0, "delta_usd": 0.0, "delta_pct": 0.0}
  ],
  "cheapest": {"...": "the first (lowest-cost) candidate row, or null"},
  "scope": "project",
  "project_slug": "-Users-you-dev-app",
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

`candidates` is sorted cheapest first. `delta_usd` is `cost_usd - actual.cost_usd` (negative = the
candidate would have been cheaper); `delta_pct` is `null` when there is no actual spend to compare
against. `scope` is `"project"` or `"all"`; `project_slug` is `null` for whole-store. Dollar figures are
pre-converted to the active currency. Read-only — `SELECT` queries only.

**Status codes:** `200` always.

---

## Budgets

Spend budgets (a monthly and/or daily USD ceiling) persisted in `~/.stackunderflow/config.json` — no DB
migration. Spend is summed across the **whole store** (a budget caps everything, not one project),
bucketed on the caller's local day via `timezone_offset` (minutes east of UTC) so the boundaries line up
with the Cost and Live tabs. All three endpoints return the same shape.

### GET /api/budgets

**Query parameters:** `timezone_offset` (int, optional, default `0`).

**Response**

```json
{
  "budget": {"monthly_usd": 500.0, "daily_usd": null},
  "status": {
    "monthly": {"budget": 500.0, "used": 311.9, "remaining": 188.1, "pct": 62.4, "status": "under"},
    "daily": null,
    "projected_month_end": 540.12,
    "projection_overruns": true,
    "models": ["claude-opus-4-6", "..."]
  },
  "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0}
}
```

A leg (`monthly` / `daily`) is `null` when its ceiling is unset; when **no** budget is set at all,
`status` is `null` and both `budget` legs are `null` (the UI renders an "add a budget" CTA). The
projection fields are `null` without a monthly ceiling. Dollar fields inside `status` are pre-converted
to the active currency.

### PUT /api/budgets

**Request body** — `{"monthly_usd": 500}`, `{"daily_usd": 25}`, or both. Each leg is independent: a
positive number sets that ceiling, an explicit `null` clears it, and an omitted field leaves it
untouched.

**Response** — the refreshed `GET` shape.

**Status codes:** `200` success; `422` non-positive amount (or body validation failure).

### DELETE /api/budgets

Clears both ceilings. **Response** — the now-empty `GET` shape. **Status codes:** `200` always.

---

## Optimize

### GET /api/optimize

Run waste-detection (legacy looped Q&A heuristic) and structural-pattern
findings (CLAUDE.md bloat, unused MCP, ghost agents, junk reads, cache
thrash, oversized bash output, exploration-only sessions) over a period.
Same surface as `stackunderflow optimize`.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `period` | string | `30days` | One of `today`, `7days`, `30days`, `month`, `all`. Unknown values return 400. |
| `project` | string (repeatable) | — | Narrow project scope to these slugs |
| `exclude` | string (repeatable) | — | Drop these project slugs |
| `force` | bool | `false` | Bypass the in-process response cache for this call. The result is still written back to the cache. |

**Response**

```json
{
  "scope": "this month (May 2026)",
  "waste": [
    {
      "project": "-Users-yadkonrad-dev-myproject",
      "looped_pairs": 7,
      "sample_questions": ["why did the test fail again", "..."]
    }
  ],
  "patterns": [
    {
      "pattern_id": "unused_mcp_servers",
      "severity": "high",
      "title": "6 unused MCP server(s)",
      "description": "6 MCP server(s) registered but no tool calls observed in the last 30 days.",
      "affected_count": 6,
      "suggested_fix": "Remove unused MCP server entries from ~/.claude.json — every server adds tool definitions to each request's context.",
      "estimated_waste_tokens": null,
      "details": {
        "unused_servers": ["apple-calendar", "tavily", "..."],
        "registered_total": 6,
        "lookback_days": 30
      }
    }
  ],
  "warnings": [
    {
      "code": "mart_empty",
      "level": "info",
      "message": "message_tool_mart is empty — optimize detectors are running on the raw messages table and will be slower. Backfill via the ETL pipeline for the fast path."
    }
  ],
  "cache": "miss"
}
```

**Field notes (new)**

- `warnings` — list of advisory hints. Currently the only emitted code
  is `mart_empty`, raised when `message_tool_mart` has no rows so the
  detectors are running the slow raw-`messages` fallback path.
- `cache` — `"hit"` when the response came from the in-process cache,
  `"miss"` on a fresh compute. The cache is keyed on
  `(period, project, exclude, store.db mtime)` and self-invalidates
  whenever an ingest run bumps the store mtime; `/api/refresh` also
  drops it eagerly.

**Field notes**

- `scope` — human-readable label for the resolved period (e.g.
  `"this month (May 2026)"`, `"last 7 days"`, `"all time"`).
- `waste` — legacy Q&A loop heuristic. List of
  `{project, looped_pairs, sample_questions}` with at most 3 sample
  question strings each. Empty when no looped pairs are present.
- `patterns` — list of `Finding` dicts emitted by the structural
  detectors in `stackunderflow/reports/optimize.py`. Each finding has:
  `pattern_id` (one of `bloated_claude_md`, `unused_mcp_servers`,
  `ghost_agents`, `low_read_edit_ratio`, `junk_reads`, `cache_overhead`,
  `bash_output_limits`), `severity` (`high` | `medium` | `low`),
  `title`, `description`, `affected_count`, `suggested_fix`,
  `estimated_waste_tokens` (int or null when not applicable), and a
  `details` dict whose keys vary by pattern. Sorted by severity desc,
  then by `estimated_waste_tokens` desc.

**Status codes:** `200` success; `400` unknown `period`
(message lists the valid values).

---

## Context Budget

### GET /api/context-budget

Per-session "context tax" estimator — system prompt, registered MCP
servers, available skills, agent definitions, memory files. Same payload
as `stackunderflow context-budget --format json`. The estimator walks
visible config files defensively: any missing file contributes a
zero-token slice rather than raising.

**Query parameters**

| Name | Type | Required | Description |
|------|------|----------|-------------|
| `project` | string | no | Project slug (the `url_slug` from `/api/projects`). When omitted, the **global** budget (`~/.claude` only — system prompt, global CLAUDE.md, registered MCP servers, every `~/.claude/skills/*/SKILL.md`) is returned. |

**Response**

```json
{
  "total_tokens": 19399,
  "slices": [
    {"name": "system_prompt", "tokens": 3000, "source_path": null},
    {"name": "memory:global_CLAUDE.md", "tokens": 401, "source_path": "/Users/you/.claude/CLAUDE.md"},
    {"name": "mcp:apple-calendar", "tokens": 400, "source_path": "/Users/you/.claude.json"},
    {"name": "skill:anti-slop-guide", "tokens": 5856, "source_path": "/Users/you/.claude/skills/anti-slop-guide/SKILL.md"}
  ],
  "cost_per_session_usd": 0.058197,
  "estimated_monthly_cost_usd": 5.8197,
  "heuristic": "len(text) // 4; per-MCP-server 200 + 50/tool"
}
```

**Field notes**

- `total_tokens` — sum of `tokens` across every slice.
- `slices` — list of `ContextSlice` dicts. `name` follows
  `<kind>:<id>` for kinds `mcp`, `skill`, `agent`, `memory`; the
  system prompt slice is named `system_prompt` with `source_path: null`.
  `tokens` is `len(text) // 4` (rounded down — see `heuristic`).
  `source_path` is the absolute path of the file that produced the
  slice, or `null` for synthetic slices.
- `cost_per_session_usd` — `total_tokens × current Anthropic Sonnet
  input rate`. USD; not pre-converted to the active currency.
- `estimated_monthly_cost_usd` — `cost_per_session_usd × 100`
  (a flat 100-sessions/mo assumption). USD.
- `heuristic` — the approximation string baked into every payload so
  consumers know the numbers are advisory.

When `project` points to a slug that exists in the store but whose
on-disk path is gone, the global-budget shape is returned as a
fallback (the project-CLAUDE.md slice silently contributes zero).

**Status codes:** `200` success; `404` unknown project slug
(`{"detail": "Unknown project slug: <slug>"}`).

---

## ETL Pipeline

### GET /api/etl/status

Live snapshot of the ETL pipeline — watcher state, mart watermarks vs
the max event id, per-provider event counts, and a coarse `health` enum
so the dashboard can render a status badge with one fetch.

Designed to be cheap (<50ms against a 200K-event store): every count
is a `SELECT COUNT(*)` on an indexed column, every per-mart watermark
is a primary-key lookup. Safe to poll from a UI status pill.

**Query parameters**

None.

**Response**

```json
{
  "watcher": {
    "enabled": true,
    "running": true,
    "last_refresh_ts": "2026-05-06T08:24:11+00:00",
    "seconds_since_refresh": 7,
    "events_in_last_cycle": 12,
    "lock_held_by": 48213
  },
  "marts": {
    "daily":        {"watermark": 228311, "row_count": 4521, "last_refresh_ts": "..."},
    "session":      {"watermark": 228311, "row_count": 1106, "last_refresh_ts": "..."},
    "project":      {"watermark": 228311, "row_count": 188,  "last_refresh_ts": "..."},
    "provider_day": {"watermark": 228311, "row_count": 312,  "last_refresh_ts": "..."},
    "model_day":    {"watermark": 228311, "row_count": 482,  "last_refresh_ts": "..."}
  },
  "events": {
    "total": 228311,
    "max_id": 228311,
    "by_provider": {"claude": 225245, "codex": 1171, "cursor": 1035, "cline": 860},
    "by_cost_source": {"rate_card": 226000, "estimated": 2311}
  },
  "lag_seconds": 0,
  "health": "live",
  "current_job": {
    "job_id": "3f9a1c2e8b7d4a6f9e0c1b2a3d4e5f60",
    "started_at": "2026-05-06T08:23:54+00:00",
    "force": false,
    "status": "running"
  },
  "last_job": null
}
```

**Field reference**

| Field | Type | Description |
|---|---|---|
| `watcher.enabled` | bool | False iff `STACKUNDERFLOW_DISABLE_WATCHER=1` is set in the environment |
| `watcher.running` | bool \| `"unknown"` | True iff the watcher's daemon thread is alive. `"unknown"` when the route is called outside the live FastAPI process (e.g. the CLI) — there's no way to introspect a thread that lives elsewhere |
| `watcher.last_refresh_ts` | string \| null | ISO 8601 UTC timestamp of the most recent watcher cycle; null until the watcher publishes a refresh stamp |
| `watcher.seconds_since_refresh` | int \| null | Seconds elapsed since `last_refresh_ts`. Drives the `health=syncing` rule |
| `watcher.events_in_last_cycle` | int \| null | Number of events written in the most recent watcher cycle |
| `watcher.lock_held_by` | int \| null | PID of the process currently holding the watcher lock; `null` when no lock is held or when the lock file is missing |
| `marts.<name>.watermark` | int | The `last_event_id` the mart has caught up to. `0` until the mart has refreshed once |
| `marts.<name>.row_count` | int | `SELECT COUNT(*)` against the underlying mart table |
| `marts.<name>.last_refresh_ts` | string \| null | ISO 8601 UTC timestamp of the most recent mart refresh; null until the mart has refreshed once |
| `events.total` | int | `COUNT(*) FROM usage_events` |
| `events.max_id` | int | `MAX(id) FROM usage_events`; `0` on an empty store |
| `events.by_provider` | object | `{provider: count}` over every row in `usage_events` |
| `events.by_cost_source` | object | `{cost_source: count}` over every row in `usage_events`. The two values today are `rate_card` (priced from a published rate table) and `estimated` (priced from a heuristic when source data lacks per-token counts) |
| `lag_seconds` | int | `max(0, max_event_id - min(mart watermarks))`. The spec key is `lag_seconds`; the unit is "events behind", not seconds — kept under the spec key to match the wave-4C contract |
| `health` | enum | `"live"` (zero lag or empty store) / `"syncing"` (lag > 0 and a refresh in the last 10s) / `"stale"` (any mart > 100 events behind, watcher alive) / `"error"` (any mart > 100 events behind **and** watcher `running=false`, **or** the most recent backfill failed within its retention window) |
| `current_job` | object \| null | The in-process backfill slot while a job runs: `{job_id, started_at, force, status}` (`status` is `"running"`); `null` when no backfill is running |
| `last_job` | object \| null | The most recently finished backfill: `{job_id, started_at, completed_at, force, status, error}` where `status` is `"complete"` or `"failed"` and `error` is set only on failure; `null` until a backfill has finished |

The `marts` block has five entries — `daily`, `session`, `project`, `provider_day`, `model_day`. The store
holds additional mart tables (`tool`, `command`, `message_tool`), but the status route reports only these
five. `current_job` and `last_job` are in-process slots — they can change between back-to-back calls with no
database activity.

**Status codes:** `200` success on every call (the route never 4xx/5xxs
on a missing watcher or empty store — the response degrades gracefully
to `running="unknown"` and zero counts so a status pill in the UI is
never blocked by an in-flight bring-up).

---

### POST /api/etl/backfill

Kick off an ETL backfill in the background. Equivalent to running
`stackunderflow etl backfill` from the CLI, but non-blocking — the route
returns immediately with a job identifier and the caller polls
`GET /api/etl/status` (`current_job` block) to track progress. Powers the
"Backfill now" button on the Settings page.

**Request body** — optional. `{"force": true}` requests a full rebuild; the default (`{}` or no body) is
an incremental backfill.

**Response (202 Accepted — backfill scheduled)**

```json
{
  "job_id": "3f9a1c2e8b7d4a6f9e0c1b2a3d4e5f60",
  "started_at": "2026-05-06T08:23:54+00:00"
}
```

**Response (409 Conflict — a backfill is already running)**

```json
{
  "error": "backfill_in_progress",
  "job_id": "3f9a1c2e8b7d4a6f9e0c1b2a3d4e5f60"
}
```

**Status codes:** `202` accepted; `409` a backfill is already in flight (the in-flight `job_id` is echoed in
the body so the caller can join the existing run instead of starting a new one).

---

### GET /api/tool-distribution

The `tool_count_distribution` map — number of commands keyed by how
many tool calls each command made. Split off `/api/dashboard-data`
(spec §D2) so the Overview chart can lazy-fetch this section without
blocking initial paint. Mirrors the `/api/cost-data` pattern: same
canonical `queries.get_project_stats` call, projected to a single key.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `log_path` | string | current project | Override the active project (full log path, e.g. `/Users/you/.claude/projects/<slug>`) |
| `timezone_offset` | int | `0` | Minutes offset from UTC for daily bucketing (forwarded to `get_project_stats`) |

**Response**

```json
{
  "tool_count_distribution": {
    "0": 273,
    "1": 111,
    "3": 41,
    "13": 7,
    "31": 1
  }
}
```

Keys are the per-command tool count as a string (`"0"` = commands that
made no tool calls, `"3"` = commands that made exactly 3, etc.); values
are the number of commands with that exact tool count. Missing data
resolves to `{"tool_count_distribution": {}}` so the chart renders its
empty state rather than 500ing.

**Status codes:** `200` success; `400` no project selected and no
`log_path` provided; `404` project not in store
(run `POST /api/refresh` first).


---

## Playback

The Playback tab on the dashboard scrubs through a session's tool calls
in order. Two routes back the event-stream view; a third reconstructs
the working filesystem at any timestamp.

### GET /api/playback/{session_id}

Ordered tool-call event stream for one session.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `tool_filter` | string | `null` | Comma-separated exact tool names (`Edit,Write`) to keep |
| `limit` | int | `1000` | Clamped to `[1, 10000]` |
| `include_payload` | bool | `true` | Include the ~200-char `payload_excerpt` on each event (empty string when `false`) |

**Response**

```json
{
  "session_id": "abc…",
  "events": [
    {
      "seq": 0,
      "ts": "2026-05-13T11:55:00Z",
      "message_id": 1234,
      "tool_name": "Edit",
      "summary": "Edit src/main.py",
      "target_path": "src/main.py",
      "byte_count": 1280,
      "success": true,
      "duration_ms": 42,
      "payload_excerpt": "…",
      "session_id": "abc…"
    }
  ],
  "total": 1,
  "truncated": false
}
```

`success` is `true` / `false` / `null` (null when no authoritative tool-result flag was found).
`truncated` is `true` when the event count hit `limit`.

Returns 404 when the session id isn't in the store. Returns 200 with `events: []` when the session exists
but issued no tool calls.

### GET /api/playback/project/{project_slug}

Cross-session timeline for one project. The top-level key is `project_slug` (not `session_id`); each event
carries its own `session_id`.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `since` | string | `null` | Lower bound — no bound when omitted. Relative (`7d`, `24h`, `90m`) or an ISO-8601 timestamp. |
| `tool_filter` | string | `null` | Comma-separated exact tool names |
| `limit` | int | `5000` | Cap at 20000 |
| `include_payload` | bool | `false` | Default off — project-wide streams are large |

Returns 404 when the slug isn't in the store.

### GET /api/playback/{session_id}/fs

**Virtual-filesystem reconstruction at a point in time.** Replays the
session's `Read` / `Write` / `Edit` / `MultiEdit` / `NotebookEdit` tool
calls up to `at` and returns the reconstructed content of every file
the session touched.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `at` | string | required | ISO-8601 timestamp; only operations with `ts <= at` are replayed |
| `paths` | string | `null` | Comma-separated relative paths; restrict the response to these files |
| `include_content` | bool | `true` | When `false`, omits the `content` field (file list + metadata only) |

**Response**

```json
{
  "session_id": "abc…",
  "snapshot_ts": "2026-05-13T12:00:00Z",
  "files": {
    "src/main.py": {
      "content": "def main():\n    …\n",
      "byte_count": 1234,
      "last_modified_ts": "2026-05-13T11:55:00Z",
      "operations_applied": ["Read#0", "Edit#0", "Edit#1"],
      "reconstruction_complete": true,
      "risk": {
        "reverted_count": 2,
        "failed_count": 0,
        "worked_count": 9,
        "total_sessions": 11
      }
    }
  },
  "warnings": [
    "src/main.py: Edit#3 old_string did not match — substitution skipped"
  ]
}
```

A `risk` block is attached to a file only when it has been reverted or failed at least once across the
store's history; files with a clean record have no `risk` key. `include_content=false` drops `content`
but keeps `byte_count`, the metadata, and `risk`.

**Reconstruction semantics:**
- **Read** seeds the initial content. The Claude Code `cat -n`-style line-number prefix is stripped so subsequent Edits match the raw bytes.
- **Write** replaces full content; `reconstruction_complete` becomes `true` from this point regardless of prior state.
- **Edit** / **MultiEdit** substitute `old_string → new_string`. An Edit without a prior Read records the `new_string` as best-effort content with `reconstruction_complete = false` and a warning. An Edit whose `old_string` doesn't match the current content is skipped (warning fired, prior state preserved).
- **NotebookEdit** accumulates a JSON `{cell_id: source}` map keyed by cell id. Never marks `reconstruction_complete = true` because the full notebook tree is unobservable from edits alone.
- `replace_all` on Edit / per-MultiEdit sub-edit is honoured.

**Status codes:** `200` success (including the "session exists but issued no FS-touching tool calls" case → `files: {}`); `404` unknown session id; `422` unparseable `at`.

---

## Static Analysis & Session Quality

Read-only surface over the per-session static-analysis findings (complexity / lint / type-completeness
deltas, written by `stackunderflow analyze session` / `analyze backfill`) and the LLM-graded session
quality metrics.

### GET /api/static-analysis/session/{session_id}

Persisted findings + aggregate summary for one session. Does **not** analyze on demand — analyzers fork
shell subprocesses, so analysis is a CLI / backfill operation; a never-analyzed session returns `200` with
empty `findings`.

**Response**

```json
{
  "session_id": "abc123…",
  "findings": [
    {"file_path": "src/foo.py", "language": "python", "ts": "…", "metric": "complexity",
     "pre_value": 12.0, "post_value": 9.0, "delta": -3.0, "details_json": "…"}
  ],
  "summary": {
    "files": 3,
    "languages": ["python"],
    "metrics": {
      "complexity": {"files": 3, "avg_delta": -0.7, "improved": 2, "regressed": 0, "neutral": 1}
    },
    "headline": "Reduced complexity by 0.7 on average across 3 files."
  }
}
```

**Status codes:** `200` always (empty `findings` + zero-count `summary` when not analyzed).

### GET /api/static-analysis/session/{session_id}/quality

The session's quality grade, grading it lazily via the local Ollama model when no cached grade exists.

**Response**

```json
{
  "session_id": "abc123…",
  "overall_score": 7.5,
  "grades": {"goal_clarity": 8.0, "execution_efficiency": 7.0, "success": 7.5},
  "rationale": "…",
  "suggestions": ["…"],
  "graded_at": "2026-06-30T12:00:00+00:00",
  "grade_source": "llm"
}
```

**Status codes:** `200` success; `404` unknown session id.

### POST /api/static-analysis/session/{session_id}/grade

Force a re-grade (ignores the cached grade) and return the fresh quality metrics — same shape as the
`quality` GET.

**Status codes:** `200` success; `404` unknown session id.

---

## Agent Teams

Read-only views over Claude Code parallel-agent topology — sessions that spawned sub-agents. Since store
migration v013, materialised `~/.claude/teams/` artefacts add `spawn_prompt`, `agent_role`, and team
`description`; on stores without those artefacts the service falls back to the `is_sidechain` heuristic and
those fields are `null`.

### GET /api/agent-teams

Recent sessions that spawned at least one sub-agent.

**Query parameters**

| Name | Type | Default | Description |
|------|------|---------|-------------|
| `limit` | int | `50` | Clamped to `[1, 500]` |
| `project` | string | (none) | Restrict to teams whose lead session belongs to this project slug |

**Response**

```json
{
  "teams": [
    {
      "session_id": "020a13e5-…",
      "project_slug": "-Users-you-dev-myproj",
      "project_display_name": "myproj",
      "team_name": "doc-overhaul",
      "first_ts": "2026-05-18T09:00:00Z",
      "last_ts": "2026-05-18T11:30:00Z",
      "agent_count": 14,
      "sub_agent_message_count": 5821,
      "lead_message_count": 240,
      "description": "Documentation overhaul"
    }
  ]
}
```

Empty stores return `{"teams": []}`.

**Status codes:** `200` always.

---

### GET /api/agent-teams/{session_id}

Dependency graph for one team, rooted at the lead `session_id`.

**Response**

```json
{
  "session_id": "020a13e5-…",
  "team_name": "doc-overhaul",
  "description": "Documentation overhaul",
  "project_slug": "-Users-you-dev-myproj",
  "project_display_name": "myproj",
  "lead": {
    "session_id": "020a13e5-…",
    "agent_id": null,
    "agent_name": null,
    "is_lead": true,
    "parent_session_id": null,
    "message_count": 240,
    "first_ts": "2026-05-18T09:00:00Z",
    "last_ts": "2026-05-18T11:30:00Z",
    "first_user_prompt": "Overhaul the docs…",
    "model": "claude-opus-4-7",
    "cost_usd": 12.40,
    "spawn_prompt": null,
    "agent_role": null
  },
  "agents": [
    {"session_id": "agent-7f…", "agent_id": "1", "agent_name": "api-reference",
     "is_lead": false, "parent_session_id": "020a13e5-…", "message_count": 410,
     "first_ts": "…", "last_ts": "…", "first_user_prompt": "…",
     "model": "claude-opus-4-7", "cost_usd": 3.18,
     "spawn_prompt": "Overhaul docs/api-reference.md", "agent_role": "claude"}
  ]
}
```

`agents` is empty when the session exists but spawned no sub-agents.

**Status codes:** `200` success; `404` lead session not found in store.

---

### GET /api/agent-teams/{session_id}/agent/{agent_session_id}

Full transcript for one agent within a team. `session_id` is the lead session; it acts as a same-project
fence so the URL cannot surface a cross-project session.

**Response**

```json
{
  "session_id": "020a13e5-…",
  "agent_session_id": "agent-7f…",
  "messages": [
    {
      "id": 91234, "seq": 0, "timestamp": "2026-05-18T09:05:00Z",
      "role": "user", "model": null,
      "input_tokens": 0, "output_tokens": 0,
      "cache_create_tokens": 0, "cache_read_tokens": 0,
      "content_text": "…", "tools_json": null, "raw_json": "{…}",
      "is_sidechain": true, "uuid": "…", "parent_uuid": "…", "speed": "standard"
    }
  ],
  "message_count": 410
}
```

**Status codes:** `200` success; `404` when the lead or agent session is missing, or the two sessions live
in different projects.

---

## Meta-Agent

Endpoints behind the right-side meta-agent sidebar (shipped v0.8.0). The agent answers questions about the
user's own sessions by calling backend tools that read the local SQLite store, driven by a local Ollama
model — no remote network call.

### GET /api/meta-agent/tools

The static tool catalogue the chat route hands the model on each turn.

**Response**

```json
{
  "tools": [{"type": "function", "function": {"name": "search_past_decisions", "...": "..."}}],
  "names": ["search_past_decisions", "get_cost_summary", "get_file_risk", "..."],
  "max_hops": 5
}
```

`tools` is the OpenAI-style schema array (14 tools); `names` is the flat name list; `max_hops` is the
tool-call loop cap.

**Status codes:** `200` always.

---

### POST /api/meta-agent/chat

Drive one user turn through the local model plus the tool catalogue. The response is **not** JSON — it is an
NDJSON stream (`application/x-ndjson`), one JSON object per line.

**Request body**

```json
{
  "messages": [{"role": "user", "content": "What did this project cost last week?"}],
  "model": "qwen2.5-coder:7b",
  "tools_enabled": true,
  "project_slug": "-Users-you-dev-myproj"
}
```

`messages` (non-empty list) and `model` (non-empty string) are required. `tools_enabled` defaults to
`true`; `project_slug` is optional and sets the default scope for "this project" questions.

**Stream events** — each line carries a `type` discriminator:

- `{"type": "token", "delta": "...", "ts": "..."}` — a chunk of the final assistant message.
- `{"type": "tool_call", "id": "...", "name": "...", "args": {...}, "ts": "..."}` — the model requested a
  tool; `id` pairs with the result.
- `{"type": "tool_result", "id": "...", "name": "...", "ok": bool, "data": {...}, "duration_ms": N, "ts": "..."}`
  — execution finished.
- `{"type": "error", "message": "...", "ts": "..."}` — terminal; the loop bailed (Ollama down, hop cap hit).
- `{"type": "done", "hops": N, "ts": "..."}` — terminal; the model produced a content-only final turn.

**Status codes:** `200` and the stream opens (when Ollama is down the first stream line is an `error`
event); `400` for invalid JSON, an empty `messages` list, or a missing `model`.

---

## Live

Live-observability surface (Spec 13) for the dashboard's Live tab.

### GET /api/live/stats

One-shot snapshot: rolling burn, per-tool latency percentiles, the current SSE watermarks, and watcher
state. No query parameters.

**Response**

```json
{
  "burn": {
    "window_minutes": 5,
    "window_cost": 0.84,
    "per_minute": 0.168,
    "per_hour": 10.08,
    "today_cost": 22.40,
    "month_to_date": 311.90,
    "projected_month_end": 540.12,
    "ts": "2026-05-19T17:00:00+00:00"
  },
  "tool_latency": [
    {"tool_name": "Bash", "samples": 412, "p50": 0.8, "p95": 4.2, "p99": 9.1}
  ],
  "watermarks": {"event_id": 228311, "tool_call_id": 41020},
  "watcher": {"running": true}
}
```

`tool_latency` percentiles are in seconds, sorted by sample count, capped at the top 6 tools.
`watcher.running` is `true` / `false` / `"unknown"`.

**Status codes:** `200` always.

---

### GET /api/live/stream

Server-Sent Events stream for the Live tab. Returns `text/event-stream`; each message is a
`event: <name>\ndata: <json>\n\n` block.

**Event names and payloads**

- `ready` — emitted once on connect: `{type, ts, payload: {watermarks, watcher, burn_interval_seconds}}`.
- `event` — a new `usage_events` row: `{type, ts, payload: {…row…}}`.
- `tool_call` — a new tool-call row: `{type, ts, payload: {…row…}}`.
- `burn_tick` — emitted every 5s: `{type, ts, payload: {…rolling burn…}}`.

The store is polled every 2s; each cycle emits at most 100 rows per stream. The loop exits within ~100ms of
a client disconnect.

**Status codes:** `200`, then the stream stays open until the client disconnects.

---

## Webhooks

Opt-in inbound receivers for PR / CI events. Each endpoint validates a signature header **before** parsing
the body, and reads its signing secret from an environment variable — never the database. If the secret env
var is unset the receiver returns `503` (opt-in by design; anonymous payloads are never accepted).

| Endpoint | Signature header | Secret env var |
|----------|------------------|----------------|
| `POST /api/webhooks/github` | `X-Hub-Signature-256` (HMAC-SHA256) | `STACKUNDERFLOW_GITHUB_WEBHOOK_SECRET` |
| `POST /api/webhooks/gitlab` | `X-Gitlab-Token` (static-token compare) | `STACKUNDERFLOW_GITLAB_WEBHOOK_SECRET` |
| `POST /api/webhooks/ci` | `X-Webhook-Signature-256` (HMAC-SHA256) | `STACKUNDERFLOW_CI_WEBHOOK_SECRET` |

All comparisons use `hmac.compare_digest`. `github` handles `pull_request` and `workflow_run` events (and
replies `{"status": "pong"}` to a `ping`); `gitlab` handles `merge_request` and `pipeline`; `ci` accepts a
generic workflow-run-shaped body.

**Response** — a small status object echoed back to the sender, for example:

```json
{"status": "ok", "kind": "pr", "verb": "inserted", "pr_number": 412}
```

An unrecognised event type is accepted with `{"status": "ignored", ...}` rather than failing.

**Status codes:** `200` accepted (including the `ignored` case); `400` invalid JSON or a payload missing
required fields; `403` missing or mismatched signature; `503` the signing secret env var is unset.
