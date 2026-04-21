---
title: Performance
description: Store-backed architecture and latency characteristics.
---

# Memory and Latency: StackUnderflow Performance Notes

## Overview

This document describes how StackUnderflow handles performance now that all session
data lives in a single SQLite store. Storage is `~/.stackunderflow/store.db` and
all performance characteristics derive from that. There is no in-process memory
cache for session data.

## Measured Numbers

These are real numbers from a corpus of 2.7 GB of raw JSONL across 297 projects,
resulting in a ~1.6 GB store (60% of raw, due to structured column extraction).

| Path | Time | Notes |
|------|------|-------|
| Initial ingest (first run) | ~22 s | 198k records, 297 projects |
| Refresh, no changes | ~175 ms | Walk files, compare mtime+size with ingest_log, skip |
| Refresh, files appended | proportional to new bytes | Adapter reads from `since_offset` only |
| Dashboard query, typical project | ~51 ms | SQLite read + pipeline (classify, enrich, aggregate) |
| Dashboard query, large project (10k messages) | ~962 ms | Same path, more rows |

There is no warm-up phase. Every request goes to SQLite directly. The OS page
cache keeps hot pages in RAM automatically; StackUnderflow does not manage that
memory.

## Storage Architecture

`~/.stackunderflow/store.db` — WAL mode, one file, all projects.

```
~/.stackunderflow/
├── store.db            # All sessions and messages (WAL mode)
└── store.db-wal        # WAL journal (auto-checkpointed by SQLite)
```

Tables: `projects`, `sessions`, `messages`, `ingest_log`. Cross-project queries
(`get_global_stats`, `cross_project_daily_totals`) are single `GROUP BY` queries
over the `messages` table, indexed on `(session_fk, seq)`, `timestamp`, and `model`.

## SQLite PRAGMA Choices

Set in `store/db.py` for every connection:

```python
conn.execute("PRAGMA journal_mode = WAL")
conn.execute("PRAGMA synchronous = NORMAL")
conn.execute("PRAGMA foreign_keys = ON")
```

- **WAL**: Allows concurrent readers and a single writer without blocking each
  other. The server handles many simultaneous API requests.
- **synchronous = NORMAL**: Flushes to OS buffer, not to disk, on each commit.
  Faster than FULL; safe against crash but not power loss. Acceptable for a
  local-only tool.
- **foreign_keys = ON**: Enforces referential integrity (sessions → projects,
  messages → sessions). Default is off in SQLite.

## Dashboard Query Path

`store.queries.get_project_stats(conn, project_id=...)` is the hot path:

1. Fetch all `raw_json` rows for the project from `messages` (single indexed
   join: `messages → sessions → projects`).
2. Reconstruct `RawEntry` objects and run the pipeline:
   `classifier.tag → enricher.build → formatter.to_dicts + aggregator.summarise`.
3. Return `(messages, stats)` to the route handler, which serializes to JSON.

No result is cached between requests. Memory peaks during step 2 when the full
message list for the project is held in Python. Typical project: 50–100 MB
transient. Largest projects (~10k messages): ~400 MB transient. Memory is
released once the request completes.

## Ingest Path

`ingest/writer.py` — one file, one transaction:

1. Walk every JSONL file on disk via `ingest/enumerate.py`.
2. For each file: compare `mtime + size` against `ingest_log`. If unchanged, skip.
3. If new or grown: open a transaction, call `adapter.read(ref, since_offset=N)`
   to start reading at the last processed byte, bulk-insert new rows, update
   `ingest_log`, commit.
4. Roll back on any error; `ingest_log` is left untouched, so the next refresh
   retries cleanly.

The `since_offset` approach means refreshes are proportional to new bytes only,
not total file size.

## What Is Explicitly Cached

StackUnderflow does not maintain a programmatic in-process cache for session data.
Two narrow exceptions:

- **Pricing data** (`infra/costs.py`): model pricing table loaded once at startup,
  held in memory for the lifetime of the process.
- **FTS databases** (search, Q&A, tags): separate SQLite databases, not part of
  `store.db`. Written during ingest, read-only at query time.

SQLite's built-in page cache handles repetitive reads of hot pages. There is no
eviction policy to configure; page cache size is controlled by SQLite's
`PRAGMA cache_size` (default: 2 MB per connection), which is left at its default.

## API Payload Size

The `/api/dashboard-data` endpoint returns statistics plus a page of messages.
Only the first 50 messages are included in the initial response; subsequent pages
are fetched on demand. This keeps the initial payload small regardless of project
size.

## Memory Footprint Summary

| Condition | Server-side memory |
|-----------|--------------------|
| Idle (no active request) | ~30–60 MB (Python process baseline) |
| During get_project_stats, typical project | +50–100 MB transient |
| During get_project_stats, 10k-message project | +400 MB transient |
| After request completes | Returned to baseline |

There is no persistent per-project memory cache. Each request allocates and
releases its own working set.
