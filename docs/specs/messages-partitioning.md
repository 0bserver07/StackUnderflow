# messages_YYYYMM partitioning — design + ops notes

**Status:** v008 (`stackunderflow/store/migrations/v008_messages_partitioning.py`)
**Closes:** HANDOFF §"What's left" #5, §"What I'd do next" #5

---

## Why partition

The `messages` table is the largest in the store. On the maintainer's machine
it's already 150K+ rows / ~1.9 GB and grows monotonically. At multi-year
scale the table becomes the dominant cost driver for:

- **Backups** (one big BLOB to copy or replicate)
- **VACUUM** (rebuilds the entire table)
- **Cold-start migrations** (every future schema change has to walk every row)
- **Retention** (no cheap "drop everything older than X" path — DELETE then VACUUM)

SQLite has no native partitioning. The two practical approaches are:

| Approach | Pros | Cons |
|---|---|---|
| **A — UNION-ALL VIEW** over per-month partition tables in one DB file | Existing read code is unchanged; partitions live in the same file (no ATTACH); drop-month is one DDL | Reads fan out to all partitions; FK to a view is not enforceable |
| **B — ATTACH DATABASE per month** | Maximum isolation; per-file size capped; independent backup per month | Double the operational complexity; adapter / writer / backfill must know about ATTACH; cross-month queries are still UNIONs but across DBs |

**Decision: Option A.** B is overkill for v1. The read fan-out cost on A is
bounded by the number of partitions (~12-36 for a typical multi-year store)
and SQLite's optimizer pushes per-partition `WHERE` predicates down through
the UNION. We can revisit B later if a single store grows past tens of
partitions or backup latency matters.

---

## Schema

After v008 the messages-related schema looks like this:

```
messages_YYYYMM            -- one table per (year, month)
messages_unknown           -- fallback for empty / malformed timestamps
messages                   -- VIEW: SELECT cols FROM <each partition> UNION ALL ...
_messages_id_seq           -- single-row table holding the next global id
messages_insert_route      -- INSTEAD OF INSERT trigger on the view
```

Each `messages_YYYYMM` carries the same column shape + indexes the v007
`messages` table did (including the FK on `session_fk` to `sessions(id)
ON DELETE CASCADE` and the `UNIQUE(session_fk, seq)` constraint).

### Why a view, not a UNION ALL of base-table inserts

The view name preserves every existing `SELECT ... FROM messages` query
across the codebase — `routes/`, `services/`, `etl/backfill.py`,
`etl/watcher.py`, every `store/queries.py` helper, every test. None of
that code changes in v008.

### Why `_messages_id_seq`

Each partition is an independent table with its own rowid space; auto-
incrementing per partition would give us colliding ids across partitions.
A global sequence keeps `messages.id` unique store-wide, which is the
property `usage_events.uniq_events_msg` (the dedup key the normalizer
relies on) requires.

### Why drop the FK on `usage_events.source_message_fk`

`usage_events.source_message_fk REFERENCES messages(id) ON DELETE CASCADE`
in v006. SQLite does not support FK constraints that reference a view,
and `messages` becomes a view in v008. Three choices:

1. **Drop the FK** (chosen). Application-level integrity replaces it; the
   `UNIQUE(source_message_fk)` index that backs the normalizer's dedup
   `INSERT OR IGNORE` is preserved.
2. Make `source_message_fk` encode the partition (e.g. `(month, id)`).
   Costly to retrofit and breaks every existing normalizer.
3. Leave the FK definition on the column and rely on `PRAGMA
   foreign_keys = OFF` for INSERTs into `usage_events`. Fragile — the
   first INSERT after a process restart with FKs re-enabled would
   crash because the parent (a view) cannot be resolved.

Option 1 wins on simplicity. The UNIQUE index is the load-bearing
constraint anyway; the FK was only catching dangling references on
manual deletions, which `ON DELETE CASCADE` from `messages` to
`usage_events` no longer fires (cascades from a view aren't supported
either).

### What the migration runs

1. **Idempotency guard.** If `messages` is already a view, return
   immediately so a re-run is a no-op.
2. **Discover months** by `substr(timestamp, 1, 7)` over the existing
   `messages` table. Empty / malformed timestamps map to `unknown`.
   Empty store → bootstrap with the current month so the view has at
   least one source SELECT.
3. **Create partition tables.** Same column shape as v007 `messages`,
   FK on `session_fk`, `UNIQUE(session_fk, seq)`, plus the three
   indexes (`session_fk+seq`, `timestamp`, `model`) namespaced per
   partition.
4. **Copy rows** with `INSERT OR IGNORE INTO messages_YYYYMM SELECT
   ... FROM messages WHERE substr(timestamp, 1, 7) = '...'`. The
   guard against malformed timestamps in the SELECT mirrors the
   discovery query, so every row lands somewhere.
5. **Verify counts.** Sum of partition counts must equal the
   pre-migration `messages` count. If they differ, raise — the
   migration runner rolls back.
6. **Rebuild `usage_events`** (drop the FK on `source_message_fk`).
7. **`DROP TABLE messages`**.
8. **`CREATE VIEW messages`** as `UNION ALL` across partitions with
   explicit columns.
9. **Create `_messages_id_seq`** and bootstrap `next_id = MAX(id) + 1`.
10. **Create `messages_insert_route`** — INSTEAD OF INSERT trigger
    that handles raw `INSERT INTO messages` (tests, ad-hoc tooling).
    Production writes route directly via the writer; the trigger is
    the slow path.

---

## Writer routing

`stackunderflow/ingest/writer.py` is the only writer in the codebase.
Its `_insert_message` helper now does:

1. `_partition_for(record.timestamp)` → `"messages_YYYYMM"` or
   `"messages_unknown"`.
2. `_ensure_partition(conn, partition)` — creates the partition table +
   indexes if missing, then rebuilds the `messages` view + the INSTEAD
   OF trigger.
3. `_next_message_id(conn)` — reserves the next global id from
   `_messages_id_seq` (read-then-update inside the per-file transaction).
4. `INSERT OR IGNORE INTO messages_YYYYMM (id, ...) VALUES (...)`.

The writer never goes through the trigger. The trigger only matters
for callers that use the `messages` name in an INSERT (tests, future
maintenance tooling).

---

## Performance expectations

### Reads

`SELECT * FROM messages WHERE id = ?`: O(P × log n) — each partition's
`INTEGER PRIMARY KEY` index is consulted. Acceptable for typical
multi-year stores (P ≈ 12-36).

`SELECT * FROM messages WHERE session_fk = ? ORDER BY seq`: each
partition's `idx_<partition>_session_seq` covers the predicate, then
SQLite merges. Order-preserving merge is bounded by P.

`SELECT * FROM messages WHERE timestamp BETWEEN ? AND ?`: SQLite can
prune partitions whose timestamp range doesn't overlap if the optimizer
sees the per-partition timestamp index. In practice we pay
O(P × log n) for the partition descent.

The dashboard's hot routes already read from marts (`daily_mart`,
`session_mart`, etc.), so most of the dashboard is unaffected by
partition fan-out. The slow path is `optimize.py` and the per-session
detail views which still hit `messages` — those run rarely.

### Writes

Per-record overhead: one `SELECT FROM sqlite_master` (cheap — schema
cache hit) + one `UPDATE _messages_id_seq` + one `INSERT OR IGNORE
INTO messages_YYYYMM`. The view + trigger rebuild only fire when a
new month is encountered (typically once per month boundary).

Backfill: walks every row via the `messages` view. The query is
unchanged from v007 (`SELECT m.* FROM messages m JOIN sessions s ...
ORDER BY m.id LIMIT ?`). Partition fan-out adds a constant factor but
backfill is already O(N) over the message table, so the overhead
is amortised.

---

## Operational rollout (1.9 GB store)

This migration is **not auto-applied** to `~/.stackunderflow/store.db`.
The maintainer reviews the plan and applies manually in three steps:

1. **Backup.**
   ```bash
   cp ~/.stackunderflow/store.db ~/.stackunderflow/store.db.pre-v008.bak
   ```

2. **Apply on a copy + verify.**
   ```bash
   cp ~/.stackunderflow/store.db /tmp/store.test.db
   # Use a Python one-liner to run the migration against the copy:
   python -c "
   from pathlib import Path
   from stackunderflow.store import db, schema
   conn = db.connect(Path('/tmp/store.test.db'))
   schema.apply(conn)
   pre_total = conn.execute(\"SELECT COUNT(*) FROM messages\").fetchone()[0]
   parts = [r[0] for r in conn.execute(\"SELECT name FROM sqlite_master WHERE name LIKE 'messages_%' AND type = 'table'\")]
   per_part = sum(conn.execute(f'SELECT COUNT(*) FROM {p}').fetchone()[0] for p in parts)
   print(f'view total: {pre_total}, partition total: {per_part}, partitions: {len(parts)}')
   "
   ```
   Expected: view total == partition total; per-month counts spread
   across 12-36 partitions.

3. **Run a dashboard sanity sweep against the test copy** to confirm
   reads still produce expected aggregates:
   ```bash
   STACKUNDERFLOW_STORE_PATH=/tmp/store.test.db stackunderflow start
   # Visit / and the per-project drill-down. Compare against the
   # pre-migration backup if any number looks off.
   ```

4. **Swap.** Once satisfied:
   ```bash
   stackunderflow stop  # ensure no live writer
   mv ~/.stackunderflow/store.db ~/.stackunderflow/store.db.pre-v008.bak
   mv /tmp/store.test.db ~/.stackunderflow/store.db
   ```

The migration is **transactional** — a crash mid-migration leaves the
DB on `user_version = 6` (or whatever was the prior version), so a
failed run is just `mv ~/.stackunderflow/store.db.pre-v008.bak
~/.stackunderflow/store.db` away from clean.

---

## Rollback procedure

If post-migration the maintainer needs to revert (regression in a read
path, performance issue, etc.), the backup-and-swap is the canonical
escape hatch:

```bash
stackunderflow stop
mv ~/.stackunderflow/store.db ~/.stackunderflow/store.db.v008-failed.bak
mv ~/.stackunderflow/store.db.pre-v008.bak ~/.stackunderflow/store.db
```

If for some reason the backup is gone and the migration needs to be
**reversed in place**, the sequence is:

1. **Recreate the legacy `messages` table** with the original schema.
2. **Copy every partition row** into it:
   ```sql
   CREATE TABLE messages_legacy AS SELECT * FROM messages;
   DROP VIEW messages;
   DROP TRIGGER IF EXISTS messages_insert_route;
   CREATE TABLE messages (...);  -- v007 schema, ALL columns
   INSERT INTO messages SELECT * FROM messages_legacy;
   DROP TABLE messages_legacy;
   -- recreate the v007 indexes
   CREATE INDEX idx_messages_session_seq ON messages(session_fk, seq);
   CREATE INDEX idx_messages_timestamp   ON messages(timestamp);
   CREATE INDEX idx_messages_model       ON messages(model);
   -- drop every messages_YYYYMM and messages_unknown
   -- drop _messages_id_seq
   ```
3. **Restore the FK on `usage_events.source_message_fk`** by rebuilding
   `usage_events` with `REFERENCES messages(id) ON DELETE CASCADE`.
4. **`PRAGMA user_version = 7`** (or whatever the prior head was).

This is documented but not coded — if reversal is ever needed, write
the rollback as a one-shot script, run it on a copy, verify, then
swap. Don't make rollback a regular operation; backup-and-swap is the
supported path.

---

## Future schema changes

Any future migration that adds a column to `messages` must:

1. ALTER each `messages_YYYYMM` partition (or rebuild it).
2. Update `_PARTITION_COLUMNS` in both
   `stackunderflow/store/migrations/v008_messages_partitioning.py`
   AND `stackunderflow/ingest/writer.py` (kept in sync — there is no
   shared module because the migration is loaded by pathname).
3. Rebuild the `messages` view (`_rebuild_messages_view`) and the
   INSTEAD OF trigger (`_rebuild_messages_insert_trigger`).
4. Update the writer's INSERT statement to include the new column.

A simpler future alternative: build a small generator that emits the
view + trigger from `_PARTITION_COLUMNS`, so a column addition is one
edit. Out of scope for v008.
