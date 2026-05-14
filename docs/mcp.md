# MCP Server

StackUnderflow ships an [MCP](https://modelcontextprotocol.io/) server that exposes your local AI coding-agent session logs as tools any MCP client can call. With it wired up, your AI assistant can answer questions like *"what tools did I run in the last hour"*, *"find the last error I hit"*, or *"what have I been working on this week"* by reading your real session history — across **every coding agent you've ingested**, not just Claude. It can also do **self-referential discovery** — before starting non-trivial work, surface prior sessions in the same project / file / about the same decision so it reuses context instead of re-deriving it.

## What it does

Eight tools, all backed by the unified StackUnderflow store at `~/.stackunderflow/store.db`:

| Tool | What it answers |
|---|---|
| `session_query` | Recent events from a session, or recent events across all sessions. |
| `list_sessions` | "What sessions have I been running lately?" — across providers. |
| `list_projects` | "What projects have I touched?" — across providers. |
| `find_sessions_in_path` | "What's happened in the project rooted at this path?" — for context before starting work in a directory. |
| `find_sessions_touching_file` | "Which prior sessions read / wrote this file?" — for the rationale a previous session left behind. |
| `search_past_decisions` | "Which session was that decision / design discussion in?" — free-text search across transcripts. |
| `find_sessions_where_action_worked` | "Show me a prior session where this was done *and it worked*" — proven recipe with the confirming message as evidence. |
| `find_failure_modes_for_file` | "What went wrong last time someone edited this file?" — past edits that led to a revert or complaint. |

The store covers every adapter that's been ingested: `claude`, `codex`, `cursor`, `cline`, plus any beta-enabled providers (`droid`, `kiro`, `openclaw`, `pi`/`omp`, `copilot`, `kilocode`, `roocode`, `opencode`, `cursor-agent`, `gemini`, `qwen`, …). One MCP query sees them all.

The first three tools (`session_query` / `list_sessions` / `list_projects`) are unchanged from the v0.6 server — existing MCP clients keep working with no reconfiguration. The five discovery / outcome tools (`find_sessions_in_path`, `find_sessions_touching_file`, `search_past_decisions`, `find_sessions_where_action_worked`, `find_failure_modes_for_file`) are the same surface exposed by the matching `stackunderflow find-sessions-*` / `search-past-decisions` CLI commands — see [`docs/cli-reference.md`](cli-reference.md).

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

The shape is unchanged from the v0.6 server — existing MCP clients keep working with no reconfiguration.

**Fallback behaviour.** If `session_id` is given and the id is *not* in the store (e.g. a fresh install, or you haven't re-ingested yet), the server falls back to walking `~/.claude*` JSONL files directly via the same legacy code path. This means cold-start users still get useful results before they've run `stackunderflow init`.

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

The unified project list from the store, ordered by last-modified descending. Same project active in multiple providers (e.g. claude + codex on the same repo) returns one row per provider so you can see the full coverage.

Each result dict: `slug`, `provider`, `display_name`, `first_seen`, `last_modified`, `path`.

## Discovery tools

These three return ranked, **token-budgeted** results so an agent calling them inside a tight context window gets the most relevant sessions first, not an unprioritised dump. Within the `limit` hard cap, results are ranked (recency + cost + relevance — weights tunable via `STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS`) and packed greedily until ~`context_budget` estimated tokens (chars/4 heuristic) are used.

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

Sessions whose project root is `path` or any ancestor of `path` (ancestor-only — projects rooted *below* `path` don't match). Use **before** starting non-trivial work in a directory so you can avoid re-deriving context or duplicating a sibling agent's work.

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

Free-text (substring) search across past session transcripts and tool-call arguments. Use when you remember a decision / design discussion / bug diagnosis happened but not which session. Each result's `snippet` carries an excerpt around the match. Don't use it for structured questions answerable from session metadata (use `list_sessions` / `find_sessions_in_path`) or for which-sessions-touched-a-file (use `find_sessions_touching_file` — its tool-call match is more precise).

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

These two go beyond "which sessions touched X" to "which sessions touched X *and it worked* / *and it broke*". They are **not** token-budgeted — `limit` is the only cap. Each result carries the keys above plus `outcome`, `outcome_evidence` (a short justification — the message that established the outcome), and `outcome_msg_id`. When the optional spec-05 `captured_events` table is populated (you ran `stackunderflow hooks install`) it's used for a deterministic success/failure flag; otherwise the outcome is inferred from the following user turns.

### `find_sessions_where_action_worked`

```python
find_sessions_where_action_worked(
    action: str,
    project: str | None = None,
    file_path: str | None = None,
    since: str | None = None,
    limit: int = 20,
) -> dict
```

Sessions where `action` was performed and the next user turn confirmed it worked (an explicit "thanks"/"that worked", or no revert and no complaint before the session ended). `action` is matched as a case-insensitive substring against tool calls and message text — a tool name (`"Edit"`), a file fragment (`"cost.py"`), or a phrase (`"add caching"`). The positive-signal counterpart to `find_failure_modes_for_file`. `outcome` is always `"worked"` here.

| Arg | Default | Meaning |
|---|---|---|
| `action` | — | Free-text descriptor, case-insensitive substring. Must be non-empty. |
| `project` | `None` | Restrict to one project slug. `None` = all. |
| `file_path` | `None` | Optionally narrow to sessions that *also* touched this file (`~`-expanded). `None` = don't narrow. |
| `since` | `None` | Only sessions whose matching activity is newer than this. `None` = all time. |
| `limit` | `20` | Max sessions returned, sorted by `last_ts` DESC. Must be positive. |

### `find_failure_modes_for_file`

```python
find_failure_modes_for_file(
    file_path: str,
    since: str | None = None,
    limit: int = 20,
) -> dict
```

Sessions where editing `file_path` led to a follow-up correction — the user reporting it broke, the agent reverting it (`git revert` / `git reset --hard` / `git checkout --`), or a complaint — each with the triggering message as evidence. The negative-signal counterpart to `find_sessions_where_action_worked`. `outcome` is `"failed"` or `"reverted"`.

| Arg | Default | Meaning |
|---|---|---|
| `file_path` | — | Absolute or working-dir-relative file path; `~`-expanded and resolved. Must be non-empty. |
| `since` | `None` | Only sessions whose edit is newer than this. `None` = all time. |
| `limit` | `20` | Max sessions returned, sorted by `last_ts` DESC. Must be positive. |

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

Restart Claude Desktop. The hammer icon should now show all eight tools (`session_query`, `list_sessions`, `list_projects`, `find_sessions_in_path`, `find_sessions_touching_file`, `search_past_decisions`, `find_sessions_where_action_worked`, `find_failure_modes_for_file`) as available.

### Claude Code

```bash
claude mcp add stackunderflow stackunderflow-mcp
```

### Cursor

In Cursor's settings → MCP, add a new server:

- **Name:** `stackunderflow`
- **Command:** `stackunderflow-mcp`

## How it works

The server is **store-backed by default** and **stateless per call**:

- Each tool opens a read-only SQLite connection to `~/.stackunderflow/store.db`, runs one or two queries, closes the connection, and returns plain dicts.
- The store is fed by StackUnderflow's normal ingest path (`stackunderflow init`, `start`, or `reindex`), so as long as you run the dashboard occasionally the MCP results stay current.
- The store schema covers every provider via the `(provider, slug)` unique constraint on `projects` — same project ingested through claude + codex shows as two rows the MCP can surface separately, and `list_sessions` orders the cross-provider feed by `last_ts` so the MCP client sees the actual most-recent activity regardless of agent.

For backward compatibility, `session_query` falls through to the legacy JSONL-walk path if you ask for a `session_id` that isn't in the store yet. The fallback only ever scans these directories:

```
~/.claude
~/.claude-opus
~/.claude-sonnet
~/.claude-haiku
~/.claude-glm
```

and uses the same `ClaudeAdapter` parser the dashboard does. Other providers (codex, cursor, cline, …) are *not* covered by the fallback — once you ingest them, they appear in the store and the store-backed path handles them.

## Cost surfacing

`list_sessions`, `find_session` (internal), and the per-session rows the discovery / outcome tools return all compute USD cost via the same `compute_cost()` pricer the dashboard uses, so an MCP client can sort by spend or budget alerts without re-implementing pricing. Pricing failures (unknown model id, missing rate card) degrade silently to `cost_usd: 0.0`.

## Citation-feedback telemetry

When a discovery tool surfaces a session it passively records that (`loaded_count`); when `session_query` is later called on a specific `session_id` it records a cite for it (`cited_count`). `cite_rate = cited_count / loaded_count` feeds the discovery ranking so sessions agents actually use climb and uncited noise sinks. This is local-only — session ids + counters in `~/.stackunderflow/store.db`, no transcript content leaves the box — and the recording is gated behind `STACKUNDERFLOW_DISCOVERY_TELEMETRY` (default on; set to `0` to disable). Inspect / maintain it with `stackunderflow discovery telemetry` and `stackunderflow discovery demote-uncited` (see [`docs/cli-reference.md`](cli-reference.md)).

## Known limitations

- **`tool_calls` shape is Claude-format.** The `tool_calls` field on each `session_query` row decodes Anthropic's `tool_use` blocks (`{name, args}`). Non-claude providers (codex, cursor, …) have different raw shapes; the `tools` list (just names) is populated correctly for every provider, but the per-call `args` summary is empty for non-claude rows. This is unchanged from v0.6 — fixing it requires per-adapter raw-payload extraction.
- **`kind="errors"` records have empty `content_preview`.** The error-detection heuristic correctly finds `tool_result` blocks flagged `is_error` (or with error-like text), but `content_preview` is sourced from `messages.content_text`, which doesn't include nested tool-result text. Future polish: surface the matched error string into the preview.
- **Discovery search is substring by default.** `search_past_decisions` (and `find_sessions_where_action_worked`'s `action`) match plain `LIKE` substrings against message text. A phrase that's worded differently in the transcript won't match — pick distinctive keywords. For semantic re-ranking, `search_past_decisions` accepts `use_embeddings=True`; that path requires the optional `pip install stackunderflow[embeddings]` extra (sentence-transformers) and re-ranks the substring-matched candidate set by cosine similarity. It still won't widen the set, so a query with zero substring hits returns zero rows whether or not embeddings are on.
- **Outcome inference is heuristic without hooks.** `find_sessions_where_action_worked` / `find_failure_modes_for_file` read the deterministic `captured_events` table when it's populated (run `stackunderflow hooks install`), but on hook-less installs they fall back to inferring the outcome from the following user turns — "no complaint before the session ended" can be a false positive.
- **No streaming.** Each tool returns a fully-materialised list. Fine for sane `limit` values; not appropriate for "scan everything I've ever done."
- **No auth.** Anyone with stdio access has full read of your local store. Tools live in the same trust boundary as your shell.

## Source

- [`stackunderflow/mcp/server.py`](../stackunderflow/mcp/server.py) — all eight tool definitions + JSONL fallback.
- [`stackunderflow/mcp/store_reader.py`](../stackunderflow/mcp/store_reader.py) — read-only store accessors used by the first three tools.
- [`stackunderflow/services/discovery.py`](../stackunderflow/services/discovery.py) — the discovery / outcome query implementations (shared with the CLI), including `pack_within_budget` for token budgeting.
- [`stackunderflow/services/discovery_telemetry.py`](../stackunderflow/services/discovery_telemetry.py) — `loaded_count` / `cited_count` recording + the demote-uncited sweep.
- [`tests/stackunderflow/mcp/test_store_reader.py`](../tests/stackunderflow/mcp/test_store_reader.py) — store-reader unit tests.
- [`tests/stackunderflow/mcp/test_server.py`](../tests/stackunderflow/mcp/test_server.py) — store-backed tool tests + JSONL fallback tests.
- [`tests/stackunderflow/test_mcp.py`](../tests/stackunderflow/test_mcp.py) — legacy JSONL-walk tests (still passing — those code paths are the fallback).
