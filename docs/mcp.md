# MCP Server

StackUnderflow ships an [MCP](https://modelcontextprotocol.io/) server that exposes your local AI coding-agent session logs as tools any MCP client can call. With it wired up, your AI assistant can answer questions like *"what tools did I run in the last hour"*, *"find the last error I hit"*, or *"what have I been working on this week"* by reading your real session history — across **every coding agent you've ingested**, not just Claude.

## What it does

Three tools, all backed by the unified StackUnderflow store at `~/.stackunderflow/store.db`:

| Tool | What it answers |
|---|---|
| `session_query` | Recent events from a session, or recent events across all sessions. |
| `list_sessions` | "What sessions have I been running lately?" — across providers. |
| `list_projects` | "What projects have I touched?" — across providers. |

The store covers every adapter that's been ingested: `claude`, `codex`, `cursor`, `cline`, plus any beta-enabled providers (`droid`, `kiro`, `openclaw`, `pi`/`omp`, `copilot`, `kilocode`, `roocode`, `opencode`, `cursor-agent`, `gemini`, `qwen`, …). One MCP query sees them all.

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

Restart Claude Desktop. The hammer icon should now show `session_query`, `list_sessions`, and `list_projects` as available tools.

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

`list_sessions` and `find_session` (internal) compute per-session USD cost using the same `compute_cost()` pricer the dashboard does, so an MCP client can sort by spend or budget alerts without re-implementing pricing. Pricing failures (unknown model id, missing rate card) degrade silently to `cost_usd: 0.0`.

## Known limitations

- **`tool_calls` shape is Claude-format.** The `tool_calls` field on each `session_query` row decodes Anthropic's `tool_use` blocks (`{name, args}`). Non-claude providers (codex, cursor, …) have different raw shapes; the `tools` list (just names) is populated correctly for every provider, but the per-call `args` summary is empty for non-claude rows. This is unchanged from v0.6 — fixing it requires per-adapter raw-payload extraction.
- **`kind="errors"` records have empty `content_preview`.** The error-detection heuristic correctly finds `tool_result` blocks flagged `is_error` (or with error-like text), but `content_preview` is sourced from `messages.content_text`, which doesn't include nested tool-result text. Future polish: surface the matched error string into the preview.
- **No streaming.** Each tool returns a fully-materialised list. Fine for sane `limit` values; not appropriate for "scan everything I've ever done."
- **No auth.** Anyone with stdio access has full read of your local store. Tools live in the same trust boundary as your shell.

## Source

- [`stackunderflow/mcp/server.py`](../stackunderflow/mcp/server.py) — tool definitions + JSONL fallback.
- [`stackunderflow/mcp/store_reader.py`](../stackunderflow/mcp/store_reader.py) — read-only store accessors used by every tool.
- [`tests/stackunderflow/mcp/test_store_reader.py`](../tests/stackunderflow/mcp/test_store_reader.py) — store-reader unit tests.
- [`tests/stackunderflow/mcp/test_server.py`](../tests/stackunderflow/mcp/test_server.py) — store-backed tool tests + JSONL fallback tests.
- [`tests/stackunderflow/test_mcp.py`](../tests/stackunderflow/test_mcp.py) — legacy JSONL-walk tests (still passing — those code paths are the fallback).
