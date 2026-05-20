# MCP Server

StackUnderflow ships an [MCP](https://modelcontextprotocol.io/) server that exposes your local AI coding-agent session logs as tools any MCP client can call. With it wired up, your AI assistant can answer questions like *"what tools did I run in the last hour"*, *"find the last error I hit"*, or *"what have I been working on this week"* by reading your real session history — across every coding agent you've ingested, not just Claude.

It also supports self-referential discovery: before starting non-trivial work, an agent can surface prior sessions in the same project, on the same file, or about the same decision, and reuse that context instead of re-deriving it.

## What it does

Twelve tools, all backed by the unified StackUnderflow store at `~/.stackunderflow/store.db`:

| Tool | What it answers |
|---|---|
| `session_query` | Recent events from a session, or recent events across all sessions. |
| `list_sessions` | "What sessions have I been running lately?" — across providers. |
| `list_projects` | "What projects have I touched?" — across providers. |
| `find_sessions_in_path` | "What's happened in the project rooted at this path?" — context before starting work in a directory. |
| `find_sessions_touching_file` | "Which prior sessions read / wrote this file?" — the rationale a previous session left behind. |
| `search_past_decisions` | "Which session was that decision / design discussion in?" — free-text search across transcripts. |
| `find_sessions_where_action_worked` | "Show me a prior session where this was done *and it worked*" — a proven recipe with the confirming message as evidence. |
| `find_failure_modes_for_file` | "What went wrong last time someone edited this file?" — past edits that led to a revert or complaint. |
| `file_risk` | "How risky is this file?" — revert / failure / success counts plus recent failure-mode session ids. |
| `recommend_skills` | "What should I automate?" — repeated workflow patterns worth turning into a project skill. |
| `recommend_mode` | "Could a cheaper model handle this task?" — a model-routing nudge from your own past sessions. |
| `get_burn_projection` | "Will I overrun my plan this month?" — month-end forecast against the configured plan budget. |

The store covers every adapter that's been ingested — `claude`, `codex`, `cursor` and `cline` by default, plus any beta providers you've enabled. One MCP query sees them all.

`session_query`, `list_sessions` and `list_projects` are the original three tools and keep a stable shape, so existing MCP clients work with no reconfiguration. The discovery and outcome tools (`find_sessions_in_path`, `find_sessions_touching_file`, `search_past_decisions`, `find_sessions_where_action_worked`, `find_failure_modes_for_file`, `file_risk`) share their implementation with the matching `stackunderflow find-sessions-*` / `search-past-decisions` CLI commands — see [`docs/cli-reference.md`](cli-reference.md).

### `session_query`

```python
session_query(
    session_id: str | None = None,
    limit: int = 20,
    kind: Literal["tool_calls", "errors", "all"] = "all",
) -> list[dict]
```

| Arg | Default | Meaning |
|---|---|---|
| `session_id` | `None` | If set, only events from this session. If omitted, returns recent events across all sessions. |
| `limit` | `20` | Maximum events. |
| `kind` | `"all"` | `"tool_calls"` keeps only assistant records that invoked at least one tool. `"errors"` keeps records whose `tool_result` blocks look like errors. `"all"` returns everything. |

Each result dict has: `agent`, `project_slug`, `session_id`, `timestamp`, `role`, `model`, `tools`, `tool_calls` (each with `name` + summarised `args`), `content_preview`, `is_sidechain`, `uuid`.

**Fallback behaviour.** If `session_id` is given and the id is *not* in the store (e.g. a fresh install, or you haven't re-ingested yet), the server falls back to walking `~/.claude*` JSONL files directly via the legacy code path. Cold-start users still get useful results before they've run `stackunderflow init`.

### `list_sessions`

```python
list_sessions(
    provider: str | None = None,
    limit: int = 50,
    since: str | None = None,
) -> list[dict]
```

Recent session metadata across providers. Useful for "what have I been working on?" without needing to know a specific session id.

| Arg | Default | Meaning |
|---|---|---|
| `provider` | `None` | If set, restrict to one provider (`"claude"`, `"codex"`, `"cursor"`, `"cline"`, …). |
| `limit` | `50` | Max sessions. |
| `since` | `None` | ISO-8601 lower bound on session `last_ts` (inclusive). |

Each result dict: `session_id`, `provider`, `project_slug`, `project_display_name`, `started_at`, `last_ts`, `message_count`, `cost_usd`.

### `list_projects`

```python
list_projects(provider: str | None = None) -> list[dict]
```

The unified project list from the store, ordered by last-modified descending. A project active in multiple providers (e.g. claude + codex on the same repo) returns one row per provider so you can see the full coverage.

Each result dict: `slug`, `provider`, `display_name`, `first_seen`, `last_modified`, `path`.

## Discovery tools

These three return ranked, token-budgeted results so an agent calling them inside a tight context window gets the most relevant sessions first, not an unprioritised dump. Within the `limit` hard cap, results are ranked (recency + cost + relevance — weights tunable via `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS`) and packed greedily until ~`context_budget` estimated tokens are used (a chars/4 heuristic).

All three accept `since` as a relative spec (`"7d"`, `"1w"`, `"1m"`, `"24h"`) or an ISO-8601 instant; `None` (default) = all time. Paths are `~`-expanded and resolved to absolute form before matching.

**Return shape (all three):**

```json
{
  "sessions": [
    {"session_id": "...", "project_slug": "...", "project_path": "...",
     "provider": "...", "first_ts": "...", "last_ts": "...",
     "message_count": 0, "cost_usd": 0.0, "snippet": "..."}
  ],
  "_budget_used_tokens": 1840,
  "_budget_max_tokens": 2000
}
```

`_budget_used_tokens` / `_budget_max_tokens` are always present. When the budget dropped rows, `"_truncated": true` and `"_more_available": <count>` are added. An empty `sessions` list means the store is missing or nothing matched (never an error).

### `find_sessions_in_path`

```python
find_sessions_in_path(
    path: str,
    since: str | None = None,
    limit: int = 20,
    provider: str | None = None,
    context_budget: int | None = None,
) -> dict
```

Sessions whose project root is `path` or any ancestor of `path` (ancestor-only — projects rooted *below* `path` don't match). Use **before** starting non-trivial work in a directory so you avoid re-deriving context or duplicating a sibling agent's work.

| Arg | Default | Meaning |
|---|---|---|
| `path` | — | Absolute or working-dir-relative path; `~`-expanded and resolved. |
| `since` | `None` | Only sessions newer than this. `None` = all time. |
| `limit` | `20` | Hard cap on sessions returned. Must be positive. |
| `provider` | `None` | Restrict to one provider. `None` = all. |
| `context_budget` | `None` | Token budget. `None` = server default (env `STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS` or `2000`); `0` disables it so `limit` is the only cap. |

### `find_sessions_touching_file`

```python
find_sessions_touching_file(
    file_path: str,
    limit: int = 20,
    mode: str = "any",
    context_budget: int | None = None,
) -> dict
```

Sessions whose tool calls referenced a specific file (Read / Edit / Write / MultiEdit / NotebookEdit, Bash redirects, …). Use **before** editing or refactoring a file with non-obvious history.

| Arg | Default | Meaning |
|---|---|---|
| `file_path` | — | Absolute or working-dir-relative file path; `~`-expanded and resolved. |
| `limit` | `20` | Hard cap on sessions returned. Must be positive. |
| `mode` | `"any"` | `"any"` — any tool call referencing the file; `"write"` — only Edit/Write/MultiEdit/NotebookEdit mutations; `"read"` — only Read-style accesses. |
| `context_budget` | `None` | As above. |

### `search_past_decisions`

```python
search_past_decisions(
    query: str,
    project: str | None = None,
    since: str | None = None,
    limit: int = 20,
    context_budget: int | None = None,
    use_embeddings: bool = False,
    embed_model: str | None = None,
) -> dict
```

Free-text (substring) search across past session transcripts and tool-call arguments. Use when you remember a decision, design discussion, or bug diagnosis happened but not which session. Each result's `snippet` carries an excerpt around the match. Don't use it for structured questions answerable from session metadata (use `list_sessions` / `find_sessions_in_path`) or for which-sessions-touched-a-file (use `find_sessions_touching_file` — its tool-call match is more precise).

| Arg | Default | Meaning |
|---|---|---|
| `query` | — | Free-text search string. Must be non-empty. |
| `project` | `None` | Restrict to one project slug (e.g. `"-Users-x-app"`). `None` = all. |
| `since` | `None` | Only messages newer than this. `None` = all time. |
| `limit` | `20` | Hard cap on sessions returned. Must be positive. |
| `context_budget` | `None` | As above. |
| `use_embeddings` | `False` | Re-rank substring-matched candidates by sentence-transformers cosine similarity. Requires `pip install stackunderflow[embeddings]`. Each result gains an `embedding_score` in `[0, 1]`. |
| `embed_model` | `None` | Override the sentence-transformers model id. `None` = `STACKUNDERFLOW_EMBED_MODEL` env or `sentence-transformers/all-MiniLM-L6-v2`. Ignored when `use_embeddings=False`. |

## Outcome tools

`find_sessions_where_action_worked` and `find_failure_modes_for_file` go beyond "which sessions touched X" to "which sessions touched X *and it worked* / *and it broke*". They are **not** token-budgeted — `limit` is the only cap. Each result carries the discovery keys above plus `outcome`, `outcome_evidence` (a short justification — the message that established the outcome), `outcome_msg_id`, and `outcome_confidence` (a float in `[0.0, 1.0]`). Rows below `min_confidence` are filtered out, so silence isn't mistaken for success.

When the `captured_events` table is populated (you ran `stackunderflow hooks install` — see [`docs/hooks.md`](hooks.md)) it provides a deterministic success/failure signal; otherwise the outcome is inferred from the following user turns.

### `find_sessions_where_action_worked`

```python
find_sessions_where_action_worked(
    action: str,
    project: str | None = None,
    file_path: str | None = None,
    since: str | None = None,
    limit: int = 20,
    min_confidence: float | None = None,
) -> dict
```

Sessions where `action` was performed and the next user turn confirmed it worked — an explicit "thanks"/"that worked", or no revert and no complaint before the session ended. `action` is matched as a case-insensitive substring against tool calls and message text: a tool name (`"Edit"`), a file fragment (`"cost.py"`), or a phrase (`"add caching"`). The positive-signal counterpart to `find_failure_modes_for_file`. `outcome` is always `"worked"` here.

| Arg | Default | Meaning |
|---|---|---|
| `action` | — | Free-text descriptor, case-insensitive substring. Must be non-empty. |
| `project` | `None` | Restrict to one project slug. `None` = all. |
| `file_path` | `None` | Optionally narrow to sessions that *also* touched this file (`~`-expanded). `None` = don't narrow. |
| `since` | `None` | Only sessions whose matching activity is newer than this. `None` = all time. |
| `limit` | `20` | Max sessions returned, sorted by `last_ts` DESC. Must be positive. |
| `min_confidence` | `None` | Minimum `outcome_confidence` for a row to be returned. `None` → `0.5`. Pass `0.0` to include low-confidence inferences ("no complaint before session ended"). Clamped to `[0.0, 1.0]`. |

### `find_failure_modes_for_file`

```python
find_failure_modes_for_file(
    file_path: str,
    since: str | None = None,
    limit: int = 20,
    min_confidence: float | None = None,
) -> dict
```

Sessions where editing `file_path` led to a follow-up correction — the user reporting it broke, the agent reverting it (`git revert` / `git reset --hard` / `git checkout --`), or a complaint — each with the triggering message as evidence. The negative-signal counterpart to `find_sessions_where_action_worked`. `outcome` is `"failed"` or `"reverted"`.

| Arg | Default | Meaning |
|---|---|---|
| `file_path` | — | Absolute or working-dir-relative file path; `~`-expanded and resolved. Must be non-empty. |
| `since` | `None` | Only sessions whose edit is newer than this. `None` = all time. |
| `limit` | `20` | Max sessions returned, sorted by `last_ts` DESC. Must be positive. |
| `min_confidence` | `None` | Minimum `outcome_confidence` for a row to be returned. `None` → `0.5`. Pass `0.0` to include low-confidence inferences. Clamped to `[0.0, 1.0]`. |

### `file_risk`

```python
file_risk(path: str, since: str | None = None) -> dict
```

A risk summary for one file: how many past sessions reverted, failed, or worked when they edited it. Use **before** editing a file with a rocky history. Unlike the two tools above it returns aggregate counts rather than a session list, plus up to five recent failure-mode session ids — read those with `session_query` to learn the trap before falling into it.

| Arg | Default | Meaning |
|---|---|---|
| `path` | — | Absolute or working-dir-relative file path; `~`-expanded and resolved. Must be non-empty. |
| `since` | `None` | Only activity newer than this (`"7d"`, `"1w"`, `"1m"`, `"24h"`, or ISO-8601). `None` = all time. |

Returns `{path, since, total_sessions, reverted, failed, worked, recent_session_ids}`. `recent_session_ids` is capped at 5 ids.

## Recommendation tools

These two mine your own history for actionable nudges. Both are read-only — they never write a file or change a setting.

### `recommend_skills`

```python
recommend_skills(
    project: str,
    threshold: int = 5,
    window_days: int = 30,
) -> dict
```

Repeated workflow patterns in one project that you could turn into a Claude Code skill — a canonical test command, a command that reliably follows edits, a flag combo the project favours, a path the user keeps steering edits away from. Mines the local store for patterns appearing in at least `threshold` distinct sessions within the last `window_days` days, dropping anything you already have an auto-generated skill for. Each row carries an `accept_command`; pasting that command (or asking the user to) is the only thing that ever writes a skill. See [`docs/skills.md`](skills.md) for the skill format.

| Arg | Default | Meaning |
|---|---|---|
| `project` | — | Project slug to scope to (e.g. `"-Users-x-myproj"`). Required — there is no implicit "all projects" mode. |
| `threshold` | `5` | Minimum distinct-session count a pattern must clear. Must be ≥ 1. |
| `window_days` | `30` | Lookback window in days. Must be ≥ 1. |

Returns `{recommendations, project, threshold, window_days, generated_at, cache_status, filtered_already_installed}`. Each recommendation has `pattern_id`, `pattern_kind`, `suggested_skill_name`, `description`, `occurrences`, `sessions` (top-3 example session ids), `last_seen_ts`, `project_slug`, `suggested_skill_template` (a pre-rendered `SKILL.md` body), and `accept_command`. The `recommendations` list is empty when nothing clears the threshold or the store is missing.

### `recommend_mode`

```python
recommend_mode(
    prompt: str,
    current_model: str | None = None,
) -> dict
```

The cheapest model that has historically fit a task like `prompt`. A heuristic: it pattern-matches the prompt (intent + token band + language hints) against your own past sessions and returns the model whose similar past sessions had the lowest median cost. Use it for "this task fits a Sonnet, you used Opus" routing nudges, not for hard model selection.

| Arg | Default | Meaning |
|---|---|---|
| `prompt` | — | The task prompt to score. Must be non-empty. |
| `current_model` | `None` | The model the caller would otherwise route to. Drives `cost_delta_usd` (positive = switching saves that much per session). |

Returns `{recommended_model, current_model, confidence, cost_delta_usd, similar_session_count, evidence_session_ids, features, task_pattern_hash, rationale, cache_hit}`. `confidence` is `0.0` when there's no historical data to base an opinion on; `cache_hit` is `true` when the result came from the 24-hour cache.

## Budget projection

### `get_burn_projection`

```python
get_burn_projection() -> dict
```

Project month-end spend against your plan budget — the MCP-side answer to "will I overrun this month?". Mirrors the `projection` block on `GET /api/plan` and the JSON output of `stackunderflow plan show --format json`.

When no plan is configured the call returns `{"plan_set": False, "hint": "...stackunderflow plan set..."}` so a client can suggest the right command without parsing nested fields. With a plan set, the response carries `plan_set: True`, the active `plan`, the current period's `used_usd` / `remaining_usd` / `pct_used` / `status`, the `projected_month_end_usd` total, the `daily_burn_usd` rate the projection used, the `projection_method` (`"linear"` or `"weighted-7d"`), the `days_to_limit` at the current burn (or `null`), the configured `thresholds` (default `[50, 75, 90]`), the highest one crossed (or `null`), and a human-readable `alert` string (or `null`).

The projection auto-picks `"weighted-7d"` once the period has at least 3 non-zero daily samples (decay 0.85/day, so recent activity dominates and weekends fade); otherwise — or when the recent 7-day window is all zero against an otherwise non-empty period (the stale-store case) — it falls back to `"linear"` and reports `projection_method: "linear"` so the cause is visible.

This tool takes no arguments: it reads the active plan from settings and resolves the period window automatically.

## Install

```bash
pip install stackunderflow
```

The MCP server is bundled with the main package — no separate install. Two equivalent invocations:

```bash
stackunderflow-mcp     # console script
stackunderflow mcp     # CLI subcommand (same thing)
```

Both run a FastMCP server over stdio.

## Wire up to a client

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "stackunderflow": {
      "command": "stackunderflow-mcp"
    }
  }
}
```

Restart Claude Desktop. The hammer icon should now list all twelve StackUnderflow tools (see the table above).

### Claude Code

```bash
claude mcp add stackunderflow stackunderflow-mcp
```

### Cursor

In Cursor's settings → MCP, add a new server:

- **Name:** `stackunderflow`
- **Command:** `stackunderflow-mcp`

## How it works

The server is store-backed by default and stateless per call:

- Each tool opens a read-only SQLite connection to `~/.stackunderflow/store.db`, runs one or two queries, closes the connection, and returns plain dicts.
- The store is fed by StackUnderflow's normal ingest path (`stackunderflow init`, `start`, or `reindex`), so as long as you run the dashboard occasionally the MCP results stay current.
- The schema covers every provider via the `(provider, slug)` unique constraint on `projects` — the same project ingested through claude + codex shows as two rows the MCP can surface separately, and `list_sessions` orders the cross-provider feed by `last_ts` so the client sees the actual most-recent activity regardless of agent.

For backward compatibility, `session_query` falls through to a legacy JSONL-walk path if you ask for a `session_id` that isn't in the store yet. The fallback only ever scans these directories:

```
~/.claude
~/.claude-opus
~/.claude-sonnet
~/.claude-haiku
~/.claude-glm
```

and uses the same `ClaudeAdapter` parser the dashboard does. Other providers (codex, cursor, cline, …) are *not* covered by the fallback — once you ingest them they appear in the store and the store-backed path handles them.

## Cost surfacing

`list_sessions`, `find_session` (internal), and the per-session rows the discovery / outcome tools return all compute USD cost via the same `compute_cost()` pricer the dashboard uses, so a client can sort by spend or budget alerts without re-implementing pricing. Pricing failures (unknown model id, missing rate card) degrade silently to `cost_usd: 0.0`.

## Citation-feedback telemetry

When a discovery tool surfaces a session it records that (`loaded_count`); when `session_query` is later called on a specific `session_id` it records a cite for it (`cited_count`). `cite_rate = cited_count / loaded_count` feeds the discovery ranking, so sessions agents actually use climb and uncited noise sinks. This is local-only — session ids and counters in `~/.stackunderflow/store.db`, no transcript content leaves the box — and the recording is gated behind `STACKUNDERFLOW_DISCOVERY_TELEMETRY` (default on; set to `0` to disable). Inspect or maintain it with `stackunderflow discovery telemetry` and `stackunderflow discovery demote-uncited` (see [`docs/cli-reference.md`](cli-reference.md)).

## Known limitations

- **`tool_calls` shape is Claude-format.** The `tool_calls` field on each `session_query` row decodes Anthropic's `tool_use` blocks (`{name, args}`). Non-claude providers (codex, cursor, …) have different raw shapes; the `tools` list (just names) is populated correctly for every provider, but the per-call `args` summary is empty for non-claude rows. Fixing it requires per-adapter raw-payload extraction.
- **`kind="errors"` records have empty `content_preview`.** The error-detection heuristic correctly finds `tool_result` blocks flagged `is_error` (or with error-like text), but `content_preview` is sourced from `messages.content_text`, which doesn't include nested tool-result text.
- **Discovery search is substring by default.** `search_past_decisions` (and `find_sessions_where_action_worked`'s `action`) match plain `LIKE` substrings against message text. A phrase worded differently in the transcript won't match — pick distinctive keywords. `search_past_decisions` accepts `use_embeddings=True` for semantic re-ranking; that path needs the optional `pip install stackunderflow[embeddings]` extra (sentence-transformers) and re-ranks the substring-matched candidate set by cosine similarity. It won't widen the set, so a query with zero substring hits returns zero rows whether or not embeddings are on.
- **Outcome inference is heuristic without hooks.** `find_sessions_where_action_worked`, `find_failure_modes_for_file` and `file_risk` read the deterministic `captured_events` table when it's populated (run `stackunderflow hooks install`), but on hook-less installs they fall back to inferring the outcome from the following user turns. The `min_confidence` filter exists to keep those inferences honest.
- **No streaming.** Each tool returns a fully-materialised list. Fine for sane `limit` values; not appropriate for "scan everything I've ever done."
- **No auth.** Anyone with stdio access has full read of your local store. Tools live in the same trust boundary as your shell.

## Source

- [`stackunderflow/mcp/server.py`](../stackunderflow/mcp/server.py) — all twelve tool definitions (count the `@mcp.tool()` decorators) + the JSONL fallback.
- [`stackunderflow/mcp/store_reader.py`](../stackunderflow/mcp/store_reader.py) — read-only store accessors used by `session_query` / `list_sessions` / `list_projects`.
- [`stackunderflow/services/discovery.py`](../stackunderflow/services/discovery.py) — the discovery / outcome query implementations (shared with the CLI), including the token-budget packer.
- [`stackunderflow/services/discovery_telemetry.py`](../stackunderflow/services/discovery_telemetry.py) — `loaded_count` / `cited_count` recording + the demote-uncited sweep.
- [`tests/stackunderflow/mcp/`](../tests/stackunderflow/mcp/) — store-reader, store-backed tool, discovery-tool, file-risk and recommend-skills tests.
- [`tests/stackunderflow/test_mcp.py`](../tests/stackunderflow/test_mcp.py) — legacy JSONL-walk tests (those code paths are the fallback).
