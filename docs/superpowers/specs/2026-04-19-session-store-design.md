# Session Store Design

**Status:** Draft — pending user review
**Date:** 2026-04-19
**Scope:** Replace the in-memory + cold-JSON caching model with a single SQLite-backed session store fronted by a pluggable source-adapter interface. Ship the Claude adapter; design the interface so additional adapters (Codex, etc.) can be added by dropping in a new file.

---

## 1. Problem

Today the pipeline reparses JSONL end-to-end every time a project's cache misses. With 2.6 GB of session data across ~297 projects the current approach has three symptoms:

1. **Full-file reparse on refresh.** When Claude Code appends new lines to a session file, StackUnderflow reparses the whole file — not just the new tail.
2. **1.9 GB cold cache duplicates raw data.** The cold JSON cache at `~/.stackunderflow/cache/` stores parsed messages per project. There's no cross-project query primitive, so any cross-project view fans out to N cache loads.
3. **One-tool lock-in.** The reader assumes Claude Code's JSONL layout. Adding Codex (or anything else) today means editing the reader.

## 2. Goals

- Cut typical refresh cost to "read only the bytes added since last ingest".
- One canonical on-disk representation that route handlers query directly with SQL.
- A minimal source-adapter interface so adding a new tool is a new file, not an edit to the pipeline.
- Keep the existing REST API shapes (URLs, JSON payloads) unchanged so the frontend doesn't move.

## 3. Non-goals

- Shipping a second adapter. Only the Claude adapter ships this round.
- Real-time file watching (inotify/FSEvents). A manual `refresh` button stays the trigger; we just make refresh cheap.
- Replacing the Q&A and search FTS databases. Those keep their existing SQLite files; the new store is a peer, not a parent.
- Cross-machine sync. The store lives at `~/.stackunderflow/store.db`, single machine, single user.

## 4. Architecture

Three layers, each with one clear job:

```
adapters/          — one file per tool, returns normalised records
  base.py            interface + shared dataclasses
  claude.py          current JSONL + history.jsonl logic, behind the interface

ingest/            — drives adapters, writes to store, tracks per-file offsets
  enumerate.py       discovers what to ingest
  writer.py          inserts/upserts rows, updates ingest_log

store/             — SQLite schema, migrations, query helpers
  db.py              connection + PRAGMA setup
  schema.py          CREATE TABLE / INDEX statements, current version
  migrations/        numbered scripts for future changes
  queries.py         typed helpers route handlers call

routes/            — unchanged filenames; handlers rewritten to query via store/queries.py
```

### 4.1 Adapter interface

```python
# stackunderflow/adapters/base.py
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Protocol

@dataclass(frozen=True, slots=True)
class SessionRef:
    provider: str            # "claude"
    project_slug: str        # "-Users-me-code-app"
    session_id: str          # UUID from the file
    file_path: Path
    file_mtime: float
    file_size: int

@dataclass(frozen=True, slots=True)
class Record:
    provider: str
    session_id: str
    seq: int                 # monotonic within session
    timestamp: str           # ISO 8601
    role: str                # "user" | "assistant" | "tool_result" | "system"
    model: str | None
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    content_text: str        # flattened text for search + previews
    tools: tuple[str, ...]   # tool names used in this record
    cwd: str | None
    is_sidechain: bool
    uuid: str
    parent_uuid: str | None
    raw: dict                # passthrough of original line for power-user queries

class SourceAdapter(Protocol):
    name: str

    def enumerate(self) -> Iterable[SessionRef]:
        """Yield every session this adapter can see on disk."""

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        """Yield records from `ref`, starting at `since_offset` bytes into the file."""
```

Two methods. No lifecycle, no init arguments. Adapters are registered by instantiating them in `stackunderflow/adapters/__init__.py`.

### 4.2 SQLite schema (v1)

```sql
CREATE TABLE projects (
  id             INTEGER PRIMARY KEY,
  provider       TEXT NOT NULL,
  slug           TEXT NOT NULL,
  path           TEXT,
  display_name   TEXT NOT NULL,
  first_seen     REAL NOT NULL,
  last_modified  REAL NOT NULL,
  UNIQUE (provider, slug)
);

CREATE TABLE sessions (
  id             INTEGER PRIMARY KEY,
  project_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  session_id     TEXT NOT NULL,
  first_ts       TEXT,
  last_ts        TEXT,
  message_count  INTEGER NOT NULL DEFAULT 0,
  UNIQUE (project_id, session_id)
);

CREATE TABLE messages (
  id                    INTEGER PRIMARY KEY,
  session_fk            INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq                   INTEGER NOT NULL,
  timestamp             TEXT NOT NULL,
  role                  TEXT NOT NULL,
  model                 TEXT,
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  cache_create_tokens   INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  content_text          TEXT NOT NULL DEFAULT '',
  tools_json            TEXT NOT NULL DEFAULT '[]',
  raw_json              TEXT NOT NULL,
  is_sidechain          INTEGER NOT NULL DEFAULT 0,
  uuid                  TEXT,
  parent_uuid           TEXT,
  UNIQUE (session_fk, seq)
);

CREATE TABLE ingest_log (
  file_path          TEXT PRIMARY KEY,
  provider           TEXT NOT NULL,
  mtime              REAL NOT NULL,
  size               INTEGER NOT NULL,
  processed_offset   INTEGER NOT NULL,
  last_ingest_ts     REAL NOT NULL
);

CREATE INDEX idx_messages_session_seq  ON messages(session_fk, seq);
CREATE INDEX idx_messages_timestamp    ON messages(timestamp);
CREATE INDEX idx_messages_model        ON messages(model);
CREATE INDEX idx_sessions_project      ON sessions(project_id);
CREATE INDEX idx_sessions_last_ts      ON sessions(last_ts);

PRAGMA user_version = 1;
PRAGMA journal_mode = WAL;
```

**PRAGMA choices:**
- `journal_mode = WAL` — readers don't block the writer during refresh.
- `foreign_keys = ON` at runtime — enforced once per connection, not in schema.
- `synchronous = NORMAL` — acceptable durability for a local cache; full FSYNC per write would double refresh latency for no user-visible benefit.

### 4.3 Ingest algorithm

```
for adapter in registered_adapters:
    for ref in adapter.enumerate():
        prior = ingest_log.get(ref.file_path)

        if prior and prior.mtime == ref.file_mtime and prior.size == ref.file_size:
            continue                        # unchanged, skip

        if prior and ref.file_size < prior.size:
            full_reparse(ref)               # rotation/truncation, rare
            continue

        offset = prior.processed_offset if prior else 0
        for record in adapter.read(ref, since_offset=offset):
            writer.upsert(record)
        ingest_log.update(ref.file_path, ref.file_mtime, ref.file_size,
                          new_offset=ref.file_size, now())
```

The adapter's `read()` is responsible for producing records with monotonic `seq` numbers and a usable `timestamp` — the ingest layer does no parsing itself.

### 4.4 Writer semantics

Message rows are inserted with `INSERT OR IGNORE` on `(session_fk, seq)`. Adapters pick a deterministic `seq` (for Claude, byte-offset of the line in the file works). Session and project rows use `INSERT … ON CONFLICT … DO UPDATE` to bump `last_ts` and `message_count`.

All inserts for a single file happen in one transaction. A crashed refresh leaves the store in a pre-transaction state; `ingest_log` only updates after commit.

### 4.5 Route migration

Route files are rewritten one at a time. Order:

1. `routes/projects.py` — project list, set-current-project
2. `routes/data.py` — dashboard-data, messages, refresh, stats
3. `routes/sessions.py` — JSONL files list, content drill-down
4. `routes/search.py` — keeps its own FTS DB, but pulls the project list from the new store
5. `routes/qa.py`, `routes/tags.py`, `routes/bookmarks.py` — already SQLite-backed, just need to reference the new projects/sessions tables for joins

Each route migration is a standalone commit. The old in-memory `deps.cache` stays alive until the last handler is migrated; at that point it's deleted. Tests catch regressions per route.

### 4.6 Query helpers (`store/queries.py`)

Route handlers never write raw SQL; they call typed helpers:

```python
def list_projects() -> list[ProjectRow]: ...
def get_project(slug: str) -> ProjectRow | None: ...
def list_sessions(project_id: int, *, since: str | None = None) -> list[SessionRow]: ...
def get_messages(session_fk: int, *, limit: int, offset: int) -> list[MessageRow]: ...
def count_tokens_by_day(project_id: int, *, tz_offset: int) -> list[DayTotals]: ...
# etc.
```

This keeps SQL in one file, keeps routes readable, and gives a natural seam for adding caches later if any single query gets hot.

## 5. Migration for existing users

First run of a version that ships the store:

1. Detect missing `~/.stackunderflow/store.db` → create and build from scratch.
2. CLI: print "Indexing N projects, this runs once…" with a counter.
3. Dashboard: on startup, if store is empty, serve a small "indexing…" state at `/api/projects`; the Overview shows a spinner instead of an empty-state.
4. After the build succeeds, delete `~/.stackunderflow/cache/` (the old cold JSON) to reclaim ~1.9 GB.

No user action required. A failed build leaves the cold cache untouched and falls back to the old code path until the build succeeds — but this fallback is only needed during the migration window and is deleted in the cleanup commit.

## 6. Testing

- **Contract test** (`tests/adapters/contract.py`): any adapter fixture runs through `enumerate()` + `read()` and must produce records with monotonic `seq`, ISO timestamps, non-negative tokens. Each adapter inherits this and supplies fixture data.
- **Claude adapter tests** (`tests/adapters/test_claude.py`): use the existing `tests/mock-data/` JSONL fixtures plus a small `history.jsonl` fixture for the legacy path.
- **Schema + migrations** (`tests/store/test_schema.py`): apply v1 schema to a blank DB, apply against a previous-version DB (future-proofing), assert PRAGMA values.
- **Ingest** (`tests/ingest/test_incremental.py`): simulate initial load, append, unchanged, and truncation against a fixture adapter that yields deterministic records.
- **Route parity** (`tests/routes/*`): existing route tests keep their assertions. Each route migration commit must leave the full suite green.

No new test-infra is required; pytest + tmp-dir fixtures are sufficient.

## 7. Implementation breakdown

Chunked into units that can proceed with minimal coordination. Items 1–3 are independent (parallel-agent ready). Items 4–6 depend on 1–3.

| # | Unit | Depends on | Rough scope |
|---|------|------------|-------------|
| 1 | `store/` schema, migrations, connection, query-helper stubs | — | ~300 lines |
| 2 | `adapters/base.py` + dataclasses + registry | — | ~80 lines |
| 3 | `adapters/claude.py` — port of `reader.py` + `history_reader.py` behind the interface | 2 | ~250 lines |
| 4 | `ingest/` enumerate + writer, incremental logic | 1, 2, 3 | ~200 lines |
| 5 | `routes/*.py` rewrites (one commit per route file) | 1, 4 | 5 × ~100 lines |
| 6 | CLI `stackunderflow reindex`, first-run detection, cold-cache cleanup, remove `TieredCache` | 5 | ~150 lines |

Total: ~1200 lines of production code plus tests. The old `pipeline/`, `TieredCache`, and cold JSON cache get deleted as part of unit 6.

## 8. Open questions (resolved)

- **Do we keep `pipeline/classifier.py` and `pipeline/enricher.py`?** — Yes, relocated. Their logic (error categorisation, interaction chaining) runs inside `adapters/claude.py` before records are yielded. What goes away is the in-memory `EnrichedDataset` — its fields are now columns on the `messages` row.
- **What about `pipeline/aggregator.py`?** — Mostly deleted. Daily/hourly/model rollups become SQL queries in `store/queries.py`. The interaction-chaining logic moves to the adapter.
- **What about the legacy `~/.claude/history.jsonl`?** — The Claude adapter keeps the fallback behaviour currently in `history_reader.py`: when a project dir has `.continuation_cache.json` but no JSONL, it synthesises records from `history.jsonl` and yields them.
- **What about the 2 skipped init tests?** — They stay skipped. Unrelated to this work.

## 9. Success criteria

- A refresh that adds one new message to an existing session takes < 100 ms in the steady state (no full reparse).
- `~/.stackunderflow/store.db` is < 1 GB for the current 2.6 GB of raw data (target ≥ 2.5× compression vs. cold JSON).
- Every existing route test passes against the new store backend.
- Adding a second adapter requires only creating `stackunderflow/adapters/<name>.py` and registering it — no changes to `store/`, `ingest/`, or any route.
