# Codex Adapter Specification

Adds OpenAI Codex (CLI coding agent) as a second data source alongside Claude Code.

## 1. Data Sources

Codex stores everything under `~/.codex/`.

### 1.1 `state_5.sqlite` (primary)

The canonical source for session/thread data. Schema:

```sql
CREATE TABLE threads (
    id TEXT PRIMARY KEY,         -- UUID
    created_at INTEGER,          -- unix epoch seconds
    updated_at INTEGER,          -- unix epoch seconds
    source TEXT,                 -- 'cli' or 'vscode'
    model_provider TEXT,         -- 'openai'
    cwd TEXT,                    -- working directory at session start
    title TEXT,                  -- first user message (verbatim)
    tokens_used INTEGER,         -- cumulative token count (no input/output split)
    model TEXT,                  -- e.g. 'gpt-5.2-codex'
    reasoning_effort TEXT        -- 'low', 'medium', 'high'
);

CREATE TABLE thread_spawn_edges (
    parent_thread_id TEXT,
    child_thread_id TEXT,
    -- tracks sub-agent spawning (analogous to Claude's Task tool sidechains)
);

CREATE TABLE agent_jobs (
    -- batch/workflow-level metadata
);

CREATE TABLE agent_job_items (
    -- individual items within a batch job
);

CREATE TABLE logs (
    -- runtime diagnostic logs, not conversation content
);
```

### 1.2 `history.jsonl` (supplemental)

Line-delimited JSON with prompt history entries. Useful as a cross-reference but **not the canonical source** -- `state_5.sqlite` is authoritative for session metadata. The JSONL may contain entries not yet flushed to SQLite, or vice versa.

### 1.3 `logs_1.sqlite` (diagnostic only)

Runtime/debug logs. Not useful for the analytics pipeline. Ignore for ETL purposes.

## 2. Concept Mapping

| Codex concept | StackUnderflow domain | Notes |
|---|---|---|
| `threads.id` | `Session.session_id` | Direct 1:1 mapping |
| `threads.cwd` | `Project` | Group threads by `cwd` to derive project, analogous to Claude's `~/.claude/projects/{slug}/` directory structure |
| `threads.title` | First user prompt (`Interaction.command.content`) | Codex stores the first message as `title`; Claude logs every message individually |
| `threads.created_at` | `Session.start_time` | Convert from unix epoch to ISO 8601 |
| `threads.updated_at` | `Session.end_time` | Convert from unix epoch to ISO 8601 |
| `threads.tokens_used` | Token analytics | Single aggregate number -- no input/output/cache breakdown |
| `threads.model` | `Record.model` | e.g. `gpt-5.2-codex` |
| `threads.source` | New field: `source` | `'cli'` or `'vscode'` -- no Claude equivalent (Claude Code is always CLI) |
| `threads.reasoning_effort` | New field: `reasoning_effort` | `'low'`, `'medium'`, `'high'` -- no Claude equivalent |
| `thread_spawn_edges` | Sub-agent tracking | Parent-child thread relationships, analogous to Claude's `isSidechain` + `Task` tool |
| `agent_jobs` / `agent_job_items` | Batch workflow tracking | No Claude equivalent -- new analytics dimension |

### 2.1 Project derivation

Claude maps projects via filesystem convention: `~/.claude/projects/-Users-me-code-app/` contains all JSONL files for `/Users/me/code/app`. The slug is deterministic from the path.

Codex has no equivalent directory structure. Instead, project identity must be derived from `threads.cwd`:
- Group threads by `cwd` value.
- Normalize paths (resolve symlinks, trailing slashes) to avoid splitting one project into many.
- A single thread has exactly one `cwd`, so the mapping is unambiguous.

## 3. Adapter Interface

The current pipeline is Claude-specific at the reader and discovery layers. A Codex adapter must implement equivalents of both.

### 3.1 Discovery: `CodexDiscovery`

Equivalent of `stackunderflow/infra/discovery.py`. That module:
- Resolves a project path to its Claude log directory via `locate_logs(project_dir) -> str | None`
- Enumerates all known projects via `enumerate_projects() -> list[tuple[str, str]]`
- Provides project metadata via `project_metadata() -> list[dict]`
- Returns `ProjectInfo` dataclass instances with `dir_name`, `log_path`, `file_count`, `total_size_mb`, `last_modified`, `first_seen`, `display_name`

The Codex equivalent must:

```python
def locate_codex_threads(project_dir: str) -> list[str]:
    """Return thread IDs whose cwd matches project_dir."""
    # Query: SELECT id FROM threads WHERE cwd = ?

def enumerate_codex_projects() -> list[tuple[str, str]]:
    """Return (display_name, cwd) for each distinct cwd in threads."""
    # Query: SELECT DISTINCT cwd FROM threads

def codex_project_metadata() -> list[dict]:
    """Return ProjectInfo-compatible dicts derived from threads table."""
    # Query: SELECT cwd, COUNT(*) as thread_count,
    #               MIN(created_at), MAX(updated_at),
    #               SUM(tokens_used)
    #        FROM threads GROUP BY cwd
```

Key difference: Claude discovery scans the filesystem for directories containing `.jsonl` files. Codex discovery queries a single SQLite database. The `log_path` field in `ProjectInfo` should be set to `~/.codex/state_5.sqlite` (constant), and a new `query_filter` or `cwd` field should carry the project directory.

### 3.2 Reader: `CodexReader`

Equivalent of `stackunderflow/pipeline/reader.py`. That module:
- Reads `*.jsonl` files from a log directory via `scan(log_dir) -> list[RawEntry]`
- Returns `RawEntry(payload=dict, session_id=str, origin=str)` named tuples
- Detects continuation files and merges session IDs

The Codex reader must produce the same `RawEntry` shape so downstream pipeline stages work unchanged. Since Codex stores structured rows (not free-form JSONL), the reader synthesizes `RawEntry` payloads from SQL:

```python
def scan_codex(project_cwd: str) -> list[RawEntry]:
    """Read all threads for a project and return synthetic RawEntry objects."""
    # For each thread row, emit one RawEntry with a payload dict shaped like:
    # {
    #     "type": "user",
    #     "timestamp": iso8601(thread.created_at),
    #     "sessionId": thread.id,
    #     "message": {
    #         "role": "user",
    #         "content": thread.title
    #     },
    #     "cwd": thread.cwd,
    #     "codex_meta": {
    #         "model": thread.model,
    #         "model_provider": thread.model_provider,
    #         "source": thread.source,
    #         "reasoning_effort": thread.reasoning_effort,
    #         "tokens_used": thread.tokens_used,
    #     }
    # }
```

The synthetic payload uses Claude-compatible field names (`type`, `timestamp`, `message`) so the classifier and enricher can process it without branching. Codex-specific fields go under a `codex_meta` namespace.

### 3.3 Unified source abstraction

Introduce a `Source` protocol that both Claude and Codex adapters implement:

```python
from typing import Protocol

class Source(Protocol):
    def enumerate_projects(self) -> list[tuple[str, str]]: ...
    def locate_project(self, project_dir: str) -> str | None: ...
    def project_metadata(self) -> list[dict]: ...
    def scan(self, project_key: str) -> list[RawEntry]: ...
```

`ClaudeSource` wraps the existing `discovery` + `reader` modules. `CodexSource` wraps the new SQLite-backed equivalents.

## 4. Pipeline Integration

The current pipeline (`stackunderflow/pipeline/__init__.py`):

```
reader.scan(log_dir) -> raw entries
  -> dedup.collapse() -> merged entries
    -> classifier.tag() -> tagged entries
      -> enricher.build() -> EnrichedDataset (Records, Interactions, Sessions)
        -> aggregator.summarise() -> statistics dict
        -> formatter.to_dicts() -> message dicts for API
```

### 4.1 What changes

**Reader layer**: Replace `reader.scan(log_dir)` with `source.scan(project_key)`. The Source abstraction picks the right reader. The `log_dir` parameter becomes `project_key` -- a string that means "log directory path" for Claude or "cwd value" for Codex.

**Dedup**: Works as-is. Codex entries will have unique session IDs (thread UUIDs) and won't produce streaming duplicates (Codex doesn't emit partial messages). The dedup pass will be a no-op for Codex data, which is fine.

**Classifier**: Needs minor adaptation. The current `_determine_kind()` reads `data["type"]` and `data["message"]["role"]` -- both of which the Codex reader should set. Error detection via `_detect_error()` inspects `tool_result` blocks, which Codex data won't have (see section 5). The classifier will classify all Codex entries as non-error user messages, which is correct given the available data.

**Enricher**: The `_parse_entry()` function extracts tokens from `message.usage`, tools from `message.content[type=tool_use]`, etc. For Codex entries, these will all be empty/default. Token data should be injected from `codex_meta.tokens_used` into the `Record.tokens` dict as `input` and `output` keys, using a 30/70 split assumption (30% input, 70% output) to match the enricher's expected structure. The enricher's interaction grouping (`group_interactions`) expects alternating user/assistant records -- Codex only provides one "user" record per thread (the title), so each thread becomes a single-record interaction with no responses.

**Aggregator**: Works as-is on the `EnrichedDataset`. Stats will be sparse (no tool breakdown, no cache analytics, no hourly patterns within sessions) but structurally valid. The cost computation needs OpenAI pricing added to `infra/costs.py`.

**Formatter**: Works as-is.

### 4.2 Multi-source pipeline entry point

```python
def process(
    project_key: str,
    source: Source,
    *,
    limit: int | None = None,
    tz_offset: int = 0,
) -> tuple[list[dict], dict]:
    raw_entries = source.scan(project_key)
    merged = dedup.collapse(raw_entries)
    tagged = classifier.tag(merged)
    dataset = enricher.build(tagged, project_key)
    messages = formatter.to_dicts(dataset, limit=limit)
    statistics = aggregator.summarise(dataset, project_key, tz_offset=tz_offset)
    return messages, statistics
```

The existing `process(log_dir)` signature becomes `process(log_dir, source=ClaudeSource())` for backward compatibility.

## 5. What's Missing

Codex's `threads` table is a session-level summary. Claude's JSONL logs are message-level event streams. This creates fundamental gaps:

| Capability | Claude | Codex | Impact |
|---|---|---|---|
| Full message history | Every user/assistant message logged individually | Only `title` (first user message) stored | No conversation replay, no per-message analytics |
| Tool use details | Each tool invocation logged with name, input, output, errors | Not available | No tool usage stats, no error classification |
| Per-message tokens | `usage` block on every assistant response with input/output/cache breakdown | Single `tokens_used` aggregate per thread | No input vs output split, no cache analytics, no per-message cost attribution |
| Streaming chunks | Multiple entries per message ID for partial responses | Not applicable | Dedup is a no-op (acceptable) |
| Error detection | `is_error` on tool results, error text in content | Not available | No error rate, no error categorization |
| Interruption tracking | Specific markers (`[Request interrupted by user for tool use]`) | Not available | No interruption rate analytics |
| Conversation compaction | `isCompactSummary` entries when context is truncated | Not available | Cannot detect context-limit events |
| Sidechain / sub-agent content | `isSidechain` flag, `parentUuid` linking | `thread_spawn_edges` table has parent/child IDs | Structure is available but content of sub-agent threads is equally sparse |
| Hourly activity patterns | Timestamps on every message enable hour-of-day histograms | Only `created_at` and `updated_at` per thread | Can do daily patterns but not intra-session hourly resolution |

### 5.1 Handling the gaps

**Degraded-but-valid output**: All aggregator sections must return valid structures even when data is sparse. For Codex, this means:
- `tools`: `{"usage_counts": {}, "error_counts": {}, "error_rates": {}}`
- `cache`: All zeros
- `errors`: `{"total": 0, "rate": 0, ...}`
- `user_interactions.command_details`: One entry per thread with `tools_used: 0`, `assistant_steps: 0`

**Token cost estimation**: With only `tokens_used` (no input/output split), cost estimation requires assumptions. Reasonable default: 30% input, 70% output (based on typical coding agent ratios). Document this assumption prominently and make it configurable.

**`history.jsonl` as enrichment source**: If `history.jsonl` contains prompt text beyond the title, parse it to fill in conversation content. This is supplemental -- do not depend on it for correctness, and handle its absence gracefully.

## 6. Implementation Plan

### Phase 1: Read-only SQLite adapter (MVP)

1. **`stackunderflow/adapters/codex_discovery.py`** -- Implement `enumerate_codex_projects()`, `locate_codex_threads()`, `codex_project_metadata()` against `state_5.sqlite`. Use `sqlite3` from stdlib, read-only mode (`?mode=ro` URI), WAL-safe.

2. **`stackunderflow/adapters/codex_reader.py`** -- Implement `scan_codex(project_cwd)` returning `list[RawEntry]` with Claude-compatible synthetic payloads. Include `codex_meta` namespace for Codex-specific fields.

3. **`stackunderflow/infra/costs.py`** -- Add OpenAI model pricing. Start with `gpt-5.2-codex` and the GPT-4.1 family. The `_identify()` function needs a new branch for non-Anthropic model IDs.

4. **Integration tests** -- Test with a fixture `state_5.sqlite` containing known data. Verify that `scan_codex` output passes through `dedup -> classifier -> enricher -> aggregator -> formatter` without errors and produces structurally valid output.

### Phase 2: Source abstraction

5. **`stackunderflow/adapters/source.py`** -- Define the `Source` protocol. Implement `ClaudeSource` (wrapping existing `discovery` + `reader`) and `CodexSource` (wrapping phase 1 modules).

6. **Refactor `pipeline/__init__.py`** -- Change `process()` to accept a `Source` parameter. Default to `ClaudeSource` for backward compatibility.

7. **Refactor `infra/discovery.py`** -- Extract the `ProjectInfo` dataclass and common types into a shared module. Both sources return `ProjectInfo` instances.

8. **Refactor routes** -- `routes/projects.py` and `routes/sessions.py` need to enumerate projects from all sources and tag each project with its source type.

### Phase 3: Enrichment and UI

9. **`thread_spawn_edges` support** -- Query the edges table to build a parent-child tree. Expose in the API as a `sub_agents` field on session metadata. Render in the UI as an expandable tree.

10. **`history.jsonl` parsing** -- Parse as supplemental data to fill conversation content where available. Match entries to threads by timestamp proximity or thread ID if present.

11. **`agent_jobs` / `agent_job_items`** -- Expose batch workflow analytics as a new section in the aggregator output. This has no Claude equivalent, so it needs a new API endpoint and UI component.

12. **Mixed-source cross-project aggregation** -- Extend `cross_project.py` to aggregate across both Claude and Codex projects. The `aggregate()` function already accepts generic `list[dict]` project metadata, so the main work is ensuring both sources populate that list.

### Phase 4: Polish

13. **Source indicator in UI** -- Badge each project/session with "Claude" or "Codex" source.
14. **Codex-specific analytics** -- `reasoning_effort` distribution, `source` (CLI vs VSCode) breakdown, model provider stats.
15. **Settings** -- Add `codex_db_path` setting (default `~/.codex/state_5.sqlite`) to `settings.py`.
