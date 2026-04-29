# MCP Server

StackUnderflow ships an [MCP](https://modelcontextprotocol.io/) server that exposes your local Claude Code session logs as a tool any MCP client can call. With it wired up, your AI assistant can answer questions like *"what tools did I run in the last hour"* or *"find the last error I hit"* by reading your real on-disk session files — not a cached summary.

## What it does

One tool, `session_query`, that returns timestamp-sorted events from local Claude-Code-format JSONL logs.

```python
session_query(
    session_id: str | None = None,
    limit: int = 20,
    kind: Literal["tool_calls", "errors", "all"] = "all",
) -> list[dict]
```

| Arg | Default | Meaning |
|---|---|---|
| `session_id` | `None` | If set, only events from this session are returned. |
| `limit` | `20` | Maximum events. |
| `kind` | `"all"` | `"tool_calls"` keeps only assistant records that invoked at least one tool. `"errors"` keeps records whose `tool_result` blocks look like errors. `"all"` returns everything. |

Each result is a dict with: `agent`, `project_slug`, `session_id`, `timestamp`, `role`, `model`, `tools`, `tool_calls` (each with `name` + summarised `args`), `content_preview`, `is_sidechain`, `uuid`.

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

Restart Claude Desktop. The hammer icon should now show `session_query` as an available tool.

### Claude Code

```bash
claude mcp add stackunderflow stackunderflow-mcp
```

### Cursor

In Cursor's settings → MCP, add a new server:

- **Name:** `stackunderflow`
- **Command:** `stackunderflow-mcp`

## What gets scanned

By default the server walks these directories looking for `projects/<slug>/<session>.jsonl`:

```
~/.claude
~/.claude-opus
~/.claude-sonnet
~/.claude-haiku
~/.claude-glm
```

Non-existent roots are silently skipped. The agent label on each result (`agent` field) comes from the directory name (e.g. `~/.claude-opus` → `claude-opus`).

OpenAI Codex (`~/.codex/sessions/...`) uses a different layout and is not yet picked up. See the roadmap below.

## How it works

The server is **stateless** and **adapter-imported**:

- It does **not** touch StackUnderflow's SQLite store (`~/.stackunderflow/store.db`).
- It does **not** require you to have run `stackunderflow init` or `reindex`.
- It imports `stackunderflow.adapters.claude.ClaudeAdapter` and parses JSONL directly.

This was a deliberate design call. The store schema can change between releases; the adapter contract (`SessionRef` + `Record` frozen dataclasses) is the most stable surface in the codebase. Building the MCP on top of it means schema-evolution insulation, no ingest dependency, and no DB lock contention.

For performance on the common "show me the last N events" query, the server sorts session files by mtime descending and stops scanning after gathering ~`4 × limit` candidates, then sorts the final list by record timestamp. Per-file `try/except` means a single corrupt JSONL won't break the query.

## Known limitations

- **`kind="errors"` records have empty `content_preview`.** The error-detection heuristic correctly finds `tool_result` blocks flagged `is_error` (or containing error-like text), but `content_preview` is sourced from `Record.content_text`, which doesn't include nested tool-result text. Future polish: surface the matched error string into the preview.
- **Codex / opencode not supported.** `DEFAULT_AGENT_ROOTS` is hardcoded to Claude-format dirs. Adding Codex support requires the existing `CodexAdapter` and a few more lines to discover its different layout.
- **No streaming.** `session_query` returns a fully-materialised list. Fine for sane `limit` values; not appropriate for "scan everything I've ever done."

## Roadmap

- Codex / opencode root support
- More tools beyond `session_query` — full-text search across sessions, project-level summaries, "what did I work on in this date range"
- Optional auth (currently anyone with stdio access has full read)

## Source

[`stackunderflow/mcp/server.py`](../stackunderflow/mcp/server.py) — ~250 lines, the whole thing.
[`tests/stackunderflow/test_mcp.py`](../tests/stackunderflow/test_mcp.py) — 10 tests covering registration, all filter modes, and edge cases.
