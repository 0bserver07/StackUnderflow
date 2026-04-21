---
title: Claude log format
description: How StackUnderflow reads ~/.claude/projects/ and ~/.claude/history.jsonl.
---

# Claude Logs Structure and Processing Documentation

This document describes Claude log files (JSONL format), their structure, and how StackUnderflow processes them to generate analytics while handling complex edge cases.

## Table of Contents
1. [Log File Structure](#log-file-structure)
2. [Entry Types and Fields](#entry-types-and-fields)
3. [Message Formats](#message-formats)
4. [Tool Usage](#tool-usage)
5. [Special Cases](#special-cases)
6. [Processing Pipeline](#processing-pipeline)
7. [Deduplication and Tool Counting](#deduplication-and-tool-counting)
8. [Storage](#storage)
9. [Legacy Format](#legacy-format)
10. [Known Issues and Solutions](#known-issues-and-solutions)

## Log File Structure

### File Location

Modern Claude Code (January 2026 and later) writes one JSONL file per session, organised by project:

```
~/.claude/projects/{project-path-slug}/{session-id}.jsonl
```

The slug is the absolute project path with path separators replaced by hyphens:

```
/Users/example/.claude/projects/-Users-example-dev-myproject/08fce8c2-8453-42da-a52c-e03472c24e0f.jsonl
```

`ClaudeAdapter.enumerate()` walks `~/.claude/projects/`, yields a `SessionRef` for every `.jsonl` file it finds, and falls back to `~/.claude/history.jsonl` for project directories that predate the per-project format (see [Legacy Format](#legacy-format)).

### Important: Multiple Sessions Per File

While JSONL files are named after a primary session ID, they can contain log entries from multiple sessions:

1. **Conversation Continuation**: When a conversation is continued after compaction or restart
2. **Cross-Session References**: When Claude references work from another session
3. **Session Merging**: When multiple related sessions are logged together

**Best Practice**: The adapter reads the `sessionId` field from each JSONL line and stores it on the `Record`. Filename stems are used as a fallback only when `sessionId` is absent.

## Entry Types and Fields

### Entry Types
- `summary` — Session or conversation summary
- `user` — User messages (includes tool results)
- `assistant` — Claude's responses

**Important:** The root `type` field indicates the log entry type, NOT necessarily the message role.

### Common Fields

#### All Entries
- `type` (string): Type of the entry
- `timestamp` (ISO 8601): When the entry was created
- `uuid` (string): Unique identifier for this entry

#### User/Assistant Entries
- `sessionId` (string): Session identifier
- `parentUuid` (string|null): UUID of the parent message
- `isSidechain` (boolean): Whether this is a side conversation (e.g., Task tool)
- `userType` (string): Type of user (e.g., "external")
- `cwd` (string): Current working directory
- `version` (string): Claude version
- `message` (object): The actual message content

#### Assistant-Specific Fields
- `requestId` (string): API request identifier
- `message.id` (string): Unique message ID (important for streaming)

#### User-Specific Fields
- `toolUseResult` (object|string): Detailed tool execution results
- `isCompactSummary` (boolean): True for conversation summaries

## Message Formats

### User Messages
```json
{
  "type": "user",
  "message": {
    "role": "user",
    "content": [
      {
        "type": "text",
        "text": "User's message text"
      },
      {
        "type": "tool_result",
        "tool_use_id": "tool_id",
        "content": "Tool execution result"
      }
    ]
  }
}
```

### Assistant Messages
```json
{
  "type": "assistant",
  "message": {
    "id": "msg_id",
    "type": "message",
    "role": "assistant",
    "model": "claude-opus-4-20250514",
    "content": [
      {
        "type": "text",
        "text": "Claude's response text"
      },
      {
        "type": "tool_use",
        "id": "toolu_xxxxx",
        "name": "ToolName",
        "input": {
          "parameter": "value"
        }
      }
    ],
    "stop_reason": "tool_use",
    "usage": {
      "input_tokens": 1234,
      "output_tokens": 567,
      "cache_creation_input_tokens": 890,
      "cache_read_input_tokens": 123
    }
  }
}
```

### Summary Entries
```json
{
  "type": "summary",
  "summary": "Brief description of the conversation",
  "leafUuid": "uuid-of-last-message"
}
```

`summary` and `compact_summary` entries are skipped by the adapter (`_role_from()` returns `None` for them) — they are not inserted into the messages table.

## Tool Usage

### Common Tools
- File Operations: `Read`, `Write`, `Edit`, `MultiEdit`
- System: `Bash`, `Grep`, `Glob`, `LS`
- Task Management: `TodoWrite`, `TodoRead`
- Special: `Task` (launches sub-agents), `WebFetch`, `WebSearch`
- Jupyter: `NotebookRead`, `NotebookEdit`

### Tool Results
Tool results appear in subsequent user messages:
```json
{
  "type": "tool_result",
  "tool_use_id": "toolu_xxxxx",
  "content": "Result of tool execution",
  "is_error": true
}
```

### Tool Names on Records
`ClaudeAdapter._tools_from()` walks the `message.content` array and collects every block whose `type` is `"tool_use"`. The resulting tuple of names is stored in the `Record.tools` field and serialised as `tools_json` in the messages table.

### Task Tool Limitations
**Critical**: Task tool operations are NOT individually logged:
- Only the Task invocation and final result appear in logs
- Internal tool operations by sub-agents are invisible
- Token usage by sub-agents is NOT tracked
- This causes apparent "missing" tool counts in analytics

## Special Cases

### Streaming Responses
Claude logs streaming responses as multiple entries with the same message ID:

```json
// Entry 1: Text response
{
  "type": "assistant",
  "message": {
    "id": "msg_01Y9yWFraRY5ptb3Bqbvpmqx",
    "content": [{"type": "text", "text": "I'll implement..."}]
  }
}

// Entry 2: Tool use (same message ID)
{
  "type": "assistant",
  "message": {
    "id": "msg_01Y9yWFraRY5ptb3Bqbvpmqx",
    "content": [{"type": "tool_use", "name": "Write", ...}]
  }
}
```

### Conversation Compaction
When conversations approach context limits, Claude Code creates comprehensive summaries:

```json
{
  "type": "user",
  "isCompactSummary": true,
  "message": {
    "role": "user",
    "content": [{
      "type": "text",
      "text": "This session is being continued from a previous conversation..."
    }]
  }
}
```

### Error Types

#### User Rejection (Before Execution)
```json
{
  "type": "tool_result",
  "content": "The user doesn't want to proceed with this tool use...",
  "is_error": true
}
```

#### User Interruption (During Execution)
Appears as both error AND user message:
```json
// As error
{
  "type": "tool_result",
  "content": "[Request interrupted by user for tool use]",
  "is_error": true
}
// As user message
{
  "type": "user",
  "message": {
    "content": [{"text": "[Request interrupted by user for tool use]no, don't..."}]
  }
}
```

## Processing Pipeline

### Overview

```
~/.claude/
    |
    v
ClaudeAdapter          (stackunderflow/adapters/claude.py)
    enumerate() -> SessionRef[]
    read(ref)   -> Record[]
    |
    v
ingest/writer          (stackunderflow/ingest/writer.py)
    ingest_file()  -- one transaction per file,
                      mtime + byte-offset tracking via ingest_log table
    |
    v
SQLite store           (~/.stackunderflow/store.db)
    projects / sessions / messages / ingest_log tables
    |
    v
store/queries          (stackunderflow/store/queries.py)
    get_project_stats() -- reconstructs RawEntry objects from raw_json,
                           feeds the stats chain below
    |
    v
stats chain            (stackunderflow/stats/)
    classifier  -> enricher -> aggregator -> formatter
    |
    v
API routes             (stackunderflow/routes/)
```

### Incremental Ingest

`ingest/writer.run_ingest()` compares each `SessionRef`'s `(mtime, size)` against the `ingest_log` table:

- **Unchanged** (same mtime and size): skip entirely — no read, no transaction.
- **Appended** (larger size, same or newer mtime): seek to `processed_offset` and read only the new bytes.
- **Truncated / rotated** (size shrank): delete the `ingest_log` row and reparse from byte 0.

This means large projects pay for a filesystem stat check only, not a full reparse, on every poll.

### Record Normalisation

`ClaudeAdapter._parse_line()` converts a raw JSONL object into a `Record` dataclass:

```python
# Role assignment
base_type = obj['type']      # 'user' | 'assistant' | 'summary' | ...
if base_type == 'user':
    role = 'user'
elif base_type == 'assistant':
    role = 'assistant'
elif base_type in ('summary', 'compact_summary'):
    return None              # skip — not a conversational record
```

Token counts come from `message.usage`; tool names from every `"tool_use"` block in `message.content`; the entire raw dict is preserved in `Record.raw` and written to `messages.raw_json`.

### Timezone Handling
All timestamps are stored in UTC but displayed in the user's local timezone:

1. Frontend detects timezone offset: `new Date().getTimezoneOffset()`
2. Backend converts UTC to local time for grouping
3. Charts display dates in the user's local timezone

## Deduplication and Tool Counting

### The Problem
When Claude Code crashes and restarts with `--continue`:
- Duplicate messages appear in multiple files
- Same message shows inconsistent tool counts
- Incomplete assistant responses
- Missing tool execution logs

### Solution: stats/classifier Deduplication

The `stackunderflow/stats/classifier.py` module receives a list of `RawEntry` objects (reconstructed from `messages.raw_json`) and performs two-phase deduplication:

1. **Phase 1 — ID-based merge**: Merges entries sharing the same `message.id` (keeping the longer content variant). This handles streaming responses where Claude emits multiple entries for the same message.

2. **Phase 2 — Exact duplicate drop**: Drops exact duplicates by hashing timestamp + content + UUID. This handles entries duplicated across files after crash/continue scenarios.

The deduplication logic that was previously in `pipeline/dedup.py` now lives inside the stats chain at `stackunderflow/stats/classifier.py`. The on-disk records themselves are stored with duplicates intact — dedup is a query-time operation so the raw JSONL is always faithfully preserved.

### Edge Cases Handled

1. **Split Interactions**: User message in file A, assistant response in file B
2. **Incomplete Tool Executions**: Crash during tool execution
3. **Compact Summary Continuations**: Sessions starting with summaries
4. **Missing Tool Logs**: Tools used but not logged
5. **Streaming Response Merging**: Multiple entries with same message ID
6. **Task Tool Sidechains**: Sub-agent operations not logged

## Storage

### Database Location
```
~/.stackunderflow/store.db
```

### Schema

The authoritative schema lives in `stackunderflow/store/migrations/v001_initial.sql`. Key tables:

| Table | Purpose |
|---|---|
| `projects` | One row per `(provider, slug)` pair |
| `sessions` | One row per session UUID, FK to `projects` |
| `messages` | One row per parsed line, FK to `sessions` |
| `ingest_log` | One row per source file; tracks `mtime`, `size`, `processed_offset` |

**messages** is the central table. Rows are keyed on `(session_fk, seq)` where `seq` is the byte offset of the line within its source file. Every row carries a `raw_json` column containing the full original JSONL object, so nothing is ever discarded during ingest — downstream consumers reconstruct whatever they need from the raw payload.

Selected `messages` columns:
- `seq` (INTEGER) — byte offset used as a stable, monotonically increasing sequence number
- `role` (TEXT) — `"user"` or `"assistant"`
- `model` (TEXT) — model identifier when present in the source line
- `input_tokens`, `output_tokens`, `cache_create_tokens`, `cache_read_tokens` (INTEGER)
- `tools_json` (TEXT) — JSON array of tool names called in this message
- `raw_json` (TEXT) — the complete original JSONL object
- `is_sidechain` (INTEGER 0/1) — set when `isSidechain` is true in the source
- `uuid`, `parent_uuid` (TEXT) — message threading fields from the JSONL

All typed query helpers that read from the store live in `stackunderflow/store/queries.py`. Application code imports helpers from there rather than writing raw SQL.

## Legacy Format

Before January 2026, Claude Code did not write per-project JSONL files. Instead, all prompts were appended to a single centralised file:

```
~/.claude/history.jsonl
```

Each line in that file has a different shape from modern per-project JSONL — notably it uses `"project"` (an absolute path string) and `"timestamp"` (milliseconds since epoch) rather than the nested `"message"` object modern sessions use.

`ClaudeAdapter` handles both formats transparently:

- `enumerate()` checks each project directory for `.jsonl` files. If none are found but a `.continuation_cache.json` exists, it treats the project as legacy and yields a single synthetic `SessionRef` whose `session_id` starts with `"legacy-"` and whose `file_path` points at `~/.claude/history.jsonl`.
- `read()` detects the `"legacy-"` prefix and calls `_read_history()` instead of `_read_jsonl()`.
- `_read_history()` filters lines by `_slug_for(obj["project"])`, converts the millisecond timestamp to ISO 8601, and yields minimal `Record` objects (role `"user"`, no token counts, no tools) — one per matching history line.

This means analytics for pre-January-2026 projects will show user prompts but no token counts or model information, since the legacy format does not record those fields.

## Known Issues and Solutions

### Issue 1: Duplicate Commands in Table
**Cause**: Same user message in multiple files after crash/continue
**Solution**: Two-phase deduplication in `stats/classifier.py` at query time; raw records are preserved intact in the store

### Issue 2: Wrong Tool Counts
**Cause**: Incomplete logging, Task tool limitations, streaming issues
**Solution**: Tool count reconciliation across all interaction versions during the classify → enrich chain

### Issue 3: Missing Model Names
**Cause**: Incomplete assistant messages from crashes
**Solution**: Preserve model info during interaction merging in the stats chain; `MAX(CASE WHEN model IS NOT NULL …)` aggregation in `get_session_stats()`

### Issue 4: Overview Refresh Intermittent
**Status**: Documented in TODO
**Workaround**: Refresh individual project dashboards first

## Success Metrics

1. **Accuracy**: No duplicate messages, correct type classification
2. **Performance**: Incremental ingest — only new bytes read per poll cycle
3. **Completeness**: All tools counted accurately; raw JSONL always preserved
4. **Timezone Support**: Correct local time display
5. **Reliability**: Graceful handling of crashes and continuations
