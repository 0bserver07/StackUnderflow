# Claude Code Logs: Structure and Processing

This document describes Claude Code log files (JSONL format), their structure, and how StackUnderflow ingests and processes them.

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

### Multiple Sessions Per File

Each JSONL file is named after a primary session id, but its lines can carry several different `sessionId` values — for example when a conversation is continued after compaction or a restart. The adapter reads `sessionId` from each line and stores it on the `Record`; the filename stem is used only as a fallback when a line has no `sessionId`.

## Entry Types and Fields

### Entry Types
- `summary` — Session or conversation summary
- `user` — User messages (includes tool results)
- `assistant` — Claude's responses

The root `type` field is the log entry type, which is not always the message role — when `type` is neither `user` nor `assistant`, the adapter falls back to `message.role`.

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
The module-level helper `_tools_from()` walks the `message.content` array and collects every block whose `type` is `"tool_use"`. The resulting tuple of names is stored in `Record.tools` and serialised as `tools_json` in the messages table.

### Task Tool Limitations
Task tool operations are not individually logged:
- Only the Task invocation and its final result appear in the logs
- Internal tool calls by sub-agents are invisible
- Sub-agent token usage is not recorded
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
When a conversation approaches the context limit, Claude Code compacts it and writes a summary entry:

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
~/.claude/projects/<slug>/*.jsonl
    |
    v
ClaudeAdapter          (python-legacy: adapters/claude.py)
    enumerate() -> SessionRef[]
    read(ref)   -> Record[]
    |
    v
ingest                 (stackunderflow/ingest/)
    run_ingest()   -- per file: skip / tail-read / reparse
    ingest_file()  -- one transaction per file; updates the ingest_log row
    |
    v
SQLite store           (~/.stackunderflow/store.db)
    projects / sessions / ingest_log tables;
    messages -- a view over monthly messages_YYYYMM partitions (since v008)
    |
    v
store/queries          (python-legacy: store/queries.py)
    get_project_stats() -- rebuilds RawEntry objects from raw_json,
                           then runs the stats chain below
    |
    v
stats chain            (stackunderflow/stats/)
    classifier.tag -> enricher.build -> aggregator.summarise
                                     \-> formatter.to_dicts
    |
    v
API routes             (stackunderflow/routes/)
```

### Incremental Ingest

`run_ingest()` (in `python-legacy: ingest/__init__.py`) compares each `SessionRef`'s `(mtime, size)` against the matching `ingest_log` row before reading anything:

- **Unchanged** (mtime and size both match the stored row): skip entirely — no read, no transaction.
- **Truncated or rotated** (file smaller than the stored size): delete the `ingest_log` row and reparse from byte 0.
- **Changed otherwise** (grown, or no stored row yet): resume from the stored `processed_offset` and read only the bytes past it.

`run_ingest()` makes the skip/resume decision; `ingest_file()` (in `writer.py`) runs the per-file transaction and writes the updated `ingest_log` row. Large projects pay for a filesystem stat check only, not a full reparse, on every poll.

### Record Normalisation

`ClaudeAdapter._parse_line()` converts a raw JSONL object into a `Record` dataclass. Role assignment is delegated to the module-level `_role_from(obj, msg)`:

```python
raw_type = obj.get("type", "")
if raw_type == "user":
    return "user"
if raw_type == "assistant":
    return "assistant"
if raw_type in ("summary", "compact_summary"):
    return None                       # skip — not a conversational record
role = msg.get("role")                # fall back to the nested message role
return role if role in ("user", "assistant") else None
```

A `None` role drops the line — it is not inserted into the store. Token counts come from `message.usage`; tool names from every `"tool_use"` block in `message.content`; `message.usage.service_tier == "priority"` sets `Record.speed` to `"fast"` (Anthropic's priority tier, billed higher for Opus). The entire raw dict is preserved in `Record.raw` and written to `messages.raw_json`.

### Timezone Handling
Timestamps are stored in UTC. The frontend sends its offset (`new Date().getTimezoneOffset()`); `get_project_stats()` passes a `tz_offset` into `aggregator.summarise()`, which groups daily buckets in the user's local time.

## Deduplication and Tool Counting

### The Problem
When Claude Code crashes and restarts with `--continue`:
- Duplicate messages appear in multiple files
- The same interaction shows inconsistent tool counts
- Assistant responses arrive split across several entries

### Solution: interaction-level dedup in the enricher

Dedup runs at query time inside the stats chain, not at ingest. The on-disk records keep their duplicates — the raw JSONL is preserved faithfully — and the duplicates are collapsed each time the stats chain runs.

`stats/classifier.py` only tags entries (message kind, error status, interruption flag). The dedup itself lives in `stats/enricher.py`, whose `build()` constructs the `EnrichedDataset` in five steps:

1. **Extract** every `TaggedEntry` into a `Record`.
2. **Group** time-sorted records into `Interaction` chains: a user message carrying no `tool_result` opens an interaction; the assistant responses and tool results that follow attach to it.
3. **Deduplicate interactions** — interactions sharing an `interaction_id` collapse to one. The id is `sha256(f"{timestamp}|{content[:64]}")[:16]`, so a prompt duplicated across files after a crash/continue produces the same id. The survivor is the interaction with more assistant responses; the loser's tool-use blocks are absorbed into it.
4. **Finalise tools** — within each interaction, tool-use blocks are deduplicated by their tool-call `id`; `tool_count` is the number of unique calls.
5. **Scan sessions** for per-session start/end timestamps and message counts.

There is no `message.id`-based merge: streaming responses (several assistant entries sharing one `message.id`) simply become several `responses` on the same interaction, and their tool calls are merged and deduped by id in step 4.

### Edge Cases Handled

1. **Duplicate prompts after crash/continue** — identical `interaction_id`, collapsed in step 3.
2. **Split interactions** — assistant responses spread across entries all attach to the open interaction.
3. **Streaming responses** — multiple assistant entries with one `message.id` become multiple responses; their tools are merged and deduped by id.
4. **Compact-summary entries** — `summary` / `compact_summary` records never reach the store, and the grouping step skips them anyway.
5. **Task tool sidechains** — sub-agent operations are not logged, so they cannot be counted.

## Storage

### Database Location
```
~/.stackunderflow/store.db
```

### Schema

The schema is built by the migration chain under `stackunderflow/store/migrations/` (`v001` through `v017`, applied in order by `store/schema.py`). `v001_initial.sql` creates the original tables; later migrations evolve them. Core ingest relations:

| Relation | Purpose |
|---|---|
| `projects` | One row per `(provider, slug)` pair |
| `sessions` | One row per session, FK to `projects` |
| `messages` | One parsed line per row, FK to `sessions` — a view, see below |
| `ingest_log` | One row per source file (file-backed sources); tracks `mtime`, `size`, `processed_offset` |

Since `v008`, **`messages` is a view**, not a table — a `UNION ALL` over monthly `messages_YYYYMM` partition tables (plus `messages_unknown` for malformed timestamps). The writer routes each insert to the partition matching the record's timestamp, creating partitions on demand. Each partition table carries the `UNIQUE (session_fk, seq)` constraint, so `(session_fk, seq)` is the per-message key. `seq` is the byte offset of the line within its JSONL file; for legacy `history.jsonl` records it is a 0-based line counter instead.

Every row carries a `raw_json` column with the full original JSONL object, so nothing is discarded during ingest — downstream consumers rebuild whatever they need from the raw payload.

Selected `messages` columns:
- `seq` (INTEGER) — byte offset of the source line; part of the `(session_fk, seq)` key
- `role` (TEXT) — `"user"` or `"assistant"`
- `model` (TEXT) — model identifier when present in the source line (`null` for `"<synthetic>"` placeholder records)
- `input_tokens`, `output_tokens`, `cache_create_tokens`, `cache_read_tokens` (INTEGER)
- `content_text` (TEXT) — flattened message text
- `tools_json` (TEXT) — JSON array of tool names called in this message
- `speed` (TEXT, added in `v003`) — `"standard"` or `"fast"` (Anthropic priority tier)
- `raw_json` (TEXT) — the complete original JSONL object
- `is_sidechain` (INTEGER 0/1) — set when `isSidechain` is true in the source
- `uuid`, `parent_uuid` (TEXT) — message threading fields from the JSONL

Ingest also feeds an ETL layer: once a per-file transaction commits, `ingest_file()` normalises the new messages into `usage_events` rows and refreshes the mart tables — the SQL-driven cost path, separate from the query-time stats chain above. That layer was introduced by `v006_etl_layer.sql`.

All typed query helpers that read from the store live in `python-legacy: store/queries.py`. Application code imports helpers from there rather than writing raw SQL.

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
**Solution**: Interaction-level deduplication in `stats/enricher.py` at query time — interactions with the same `interaction_id` collapse to one; raw records stay intact in the store

### Issue 2: Wrong Tool Counts
**Cause**: Streaming entries, Task tool limitations, duplicated interactions
**Solution**: When two interactions collapse, the survivor absorbs the other's tool-use blocks; `finalise_tools()` then deduplicates them by tool-call `id` so each call is counted once

### Issue 3: Missing Model Names
**Cause**: Some assistant entries (placeholders, crash fragments) carry no model id
**Solution**: An interaction adopts the model from any assistant response that names one; `get_session_stats()` uses `MAX(CASE WHEN model IS NOT NULL AND model != '' THEN model END)`, so a session reports a model as long as one of its messages recorded one
