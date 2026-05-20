# Memory and latency

## Overview

All session data lives in a single SQLite store at
`~/.stackunderflow/store.db`, and most performance characteristics
derive from that. The one in-process cache is a memo on the
`/api/dashboard-data` payload; every other request reads SQLite
directly.

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

There is no warm-up phase. Apart from the dashboard memo described below, every
request reads SQLite directly. The OS page cache keeps hot pages in RAM
automatically; StackUnderflow does not manage that memory.

## Storage Architecture

`~/.stackunderflow/store.db` — WAL mode, one file, all projects.

```
~/.stackunderflow/
├── store.db            # All sessions and messages
├── store.db-wal        # Write-ahead log (auto-checkpointed by SQLite)
└── store.db-shm        # Shared-memory index for the WAL
```

The core tables are `projects`, `sessions`, `messages`, and `ingest_log`. The ETL
layer adds eight precomputed mart tables (`daily_mart`, `session_mart`,
`project_mart`, `tool_mart`, `command_mart`, `message_tool_mart`,
`provider_day_mart`, `model_day_mart`) plus `usage_events` and bookkeeping tables.
The schema version is held in `PRAGMA user_version` and migrated on startup.

The `messages` table carries the bulk of the rows. It is indexed on
`(session_fk, seq)`, `timestamp`, and `model`, which back the joins and `GROUP BY`
rollups that the non-mart query paths run.

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

`/api/dashboard-data` serves the dashboard in three tiers:

1. **Memo hit.** An in-process dict holds the last payload per
   `(project, timezone_offset)`. Its key carries a signature — `MAX(last_ts)` and
   `SUM(message_count)` from the `sessions` table — that moves whenever ingest
   writes new rows, so a stale entry cannot survive a refresh. A hit returns
   after one signature query.
2. **Mart read.** On a memo miss, if the project has a row in `project_mart`, the
   statistics come from mart reads rather than the message pipeline.
3. **Full pipeline.** Otherwise `store.queries.get_project_stats(conn,
   project_id=...)` runs: fetch the project's `raw_json` rows from `messages`
   (indexed join `messages → sessions → projects`), reconstruct pipeline entries,
   run `classifier.tag → enricher.build → formatter.to_dicts +
   aggregator.summarise`, and return `(messages, stats)` for the route to
   serialize.

Memory peaks on the full-pipeline path, when the project's whole message list is
held in Python: 50–100 MB transient for a typical project, ~400 MB for the
largest (~10k messages). It is released when the request completes. The memo and
mart paths never build that list, so their footprint stays flat.

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

In-process caching is deliberately minimal:

- **Dashboard payload memo** (`routes/data.py`): the last `/api/dashboard-data`
  response per `(project, timezone_offset)`, invalidated by a signature derived
  from the `sessions` table. `POST /api/refresh` also drops it for the project it
  touched.
- **Pricing data** (`infra/costs.py`): the model pricing table, loaded once at
  startup and held for the process lifetime.
- **FTS databases** (search, Q&A, tags): separate SQLite databases, not part of
  `store.db`. Written during ingest, read-only at query time.

SQLite's built-in page cache handles repetitive reads of hot pages. There is no
eviction policy to configure; page cache size is controlled by SQLite's
`PRAGMA cache_size` (default: 2 MB per connection), which is left at its default.

## API Payload Size

`/api/dashboard-data` returns statistics plus the first page of messages — 50 at
most, and none at all when the payload is served from marts. Later pages load on
demand from `/api/messages`. Heavy analytics sections (per-command lists, cost
breakdowns, tool distributions) load lazily from their own endpoints rather than
riding along here. The initial payload stays small regardless of project size.

## Memory Footprint Summary

| Condition | Server-side memory |
|-----------|--------------------|
| Idle (no active request) | ~30–60 MB (Python process baseline) |
| During get_project_stats, typical project | +50–100 MB transient |
| During get_project_stats, 10k-message project | +400 MB transient |
| After request completes | Returned to baseline |

Working sets are transient — each request allocates and releases its own. Only
the dashboard payload memo persists between requests, and it holds a finished
payload, not the pipeline's intermediate objects.
