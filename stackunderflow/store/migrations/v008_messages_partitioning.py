"""v008: partition the ``messages`` table into monthly tables + view.

Background
----------
The ``messages`` table is the largest in the store — on the maintainer's
machine it holds 150K+ rows / ~1.9 GB and grows monotonically. SQLite
does not have native partitioning, but a UNION-ALL VIEW over per-month
partition tables gives the same operational benefit:

* **Predictable file growth.** Each ``messages_YYYYMM`` is bounded in
  size, so VACUUM / backup / litestream replication can scope to recent
  months.
* **Cheap retention.** Dropping cold months is one ``DROP TABLE`` and a
  view rebuild — no row-by-row DELETE pass.
* **Existing read code keeps working.** The view named ``messages``
  exposes the same column shape as the original table; every
  ``SELECT ... FROM messages`` in the codebase is unchanged.

Tradeoff: writes can no longer go through the ``messages`` name (SQLite
does not support inserting into a UNION-ALL view without INSTEAD OF
triggers, and we'd rather route at the writer level than hide partition
selection behind triggers). The single writer in
``stackunderflow/ingest/writer.py`` is updated to route inserts to the
partition for ``record.timestamp``.

What this migration does
------------------------
1. **Idempotency**: returns early if ``messages`` is already a view.
2. Discovers distinct ``YYYYMM`` values from the existing
   ``messages.timestamp`` column. Any row whose timestamp is empty or
   malformed routes to ``messages_unknown`` so no rows are lost.
3. For each discovered month, creates a partition table
   ``messages_YYYYMM`` with the same column shape + indexes as the
   original ``messages`` table, including the FK on ``session_fk`` to
   ``sessions(id) ON DELETE CASCADE`` and the ``UNIQUE (session_fk,
   seq)`` constraint.
4. Copies rows from ``messages`` into the matching partition (by
   ``substr(timestamp, 1, 7)``).
5. Verifies row counts match.
6. **Rebuilds ``usage_events``** to drop the FK on
   ``source_message_fk REFERENCES messages(id)`` — once ``messages``
   becomes a view that FK can no longer be enforced (SQLite FKs to a
   view are not supported), so dropping the constraint cleanly is
   safer than leaving a dangling FK that crashes future inserts when
   ``PRAGMA foreign_keys = ON``. The ``UNIQUE(source_message_fk)``
   index on ``usage_events`` (the dedup key the normalizer relies on)
   is preserved.
7. Drops the ``messages`` base table.
8. Creates a ``CREATE VIEW messages AS SELECT ... UNION ALL ...``
   spanning every partition.
9. Creates ``_messages_id_seq`` — a single-row table that holds the
   next global ``messages.id``. The writer increments it inside its
   per-file transaction. We bootstrap ``next_id = MAX(id) + 1`` so new
   inserts continue the existing id sequence without collisions.

Rollback
--------
See ``docs/specs/messages-partitioning.md`` for the manual rollback
procedure (consolidate every ``messages_YYYYMM`` back into a single
``messages`` table, restore the FK on ``usage_events``, drop the
sequence + view).

Future schema changes
---------------------
Any future migration that adds a column to ``messages`` must:
1. ALTER each ``messages_YYYYMM`` partition (or rebuild it).
2. Rebuild the ``messages`` view via the ``_rebuild_messages_view``
   helper so the new column appears in reads.
3. Update the writer's INSERT statement to include the new column.
"""

from __future__ import annotations

import logging
import re
import sqlite3
from datetime import UTC, datetime

_log = logging.getLogger(__name__)

# Columns the partition tables expose. Matches the original
# ``messages`` table after v003 (which added ``speed``). The view
# enumerates these columns explicitly so reads against ``messages``
# stay source-stable across SELECT * shape drift in any one partition.
_PARTITION_COLUMNS = (
    "id",
    "session_fk",
    "seq",
    "timestamp",
    "role",
    "model",
    "input_tokens",
    "output_tokens",
    "cache_create_tokens",
    "cache_read_tokens",
    "content_text",
    "tools_json",
    "raw_json",
    "is_sidechain",
    "uuid",
    "parent_uuid",
    "speed",
)

_PARTITION_NAME_RE = re.compile(r"^messages_(\d{6}|unknown)$")

# Default literals for partition columns that are NOT NULL with a
# DEFAULT clause — the INSTEAD OF trigger has to apply these manually
# because NEW.col is NULL when a column is omitted from the original
# INSERT (the partition's DEFAULT only fires on direct table inserts).
_COLUMN_DEFAULTS = {
    "input_tokens": "0",
    "output_tokens": "0",
    "cache_create_tokens": "0",
    "cache_read_tokens": "0",
    "content_text": "''",
    "tools_json": "'[]'",
    "is_sidechain": "0",
    "speed": "'standard'",
}


def apply(conn: sqlite3.Connection) -> None:
    """Run the partitioning conversion against *conn*.

    Wrapped in a transaction by ``schema._run_python_migration`` — any
    exception leaves the DB on the previous ``user_version`` so
    re-running on the next process start replays cleanly.
    """
    # ── 1. Idempotency guard ─────────────────────────────────────────────
    row = conn.execute(
        "SELECT type FROM sqlite_master WHERE name = 'messages'"
    ).fetchone()
    if row is not None and _value(row) == "view":
        _log.info("v008: messages is already a view — skipping")
        return

    # ── 2. Discover months in existing data ──────────────────────────────
    pre_count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]

    rows = conn.execute(
        "SELECT DISTINCT "
        "  CASE "
        "    WHEN length(timestamp) >= 7 "
        "         AND substr(timestamp, 5, 1) = '-' "
        "         AND substr(timestamp, 1, 4) GLOB '[0-9][0-9][0-9][0-9]' "
        "         AND substr(timestamp, 6, 2) GLOB '[0-9][0-9]' "
        "    THEN substr(timestamp, 1, 4) || substr(timestamp, 6, 2) "
        "    ELSE 'unknown' "
        "  END AS yyyymm "
        "FROM messages"
    ).fetchall()
    months = sorted({str(_value(r)) for r in rows})

    if not months:
        # Empty store — bootstrap with the current month so the view
        # has at least one source SELECT. The writer will create more
        # partitions on demand.
        months = [datetime.now(UTC).strftime("%Y%m")]

    # ── 3. Create partition tables ───────────────────────────────────────
    for ym in months:
        partition = f"messages_{ym}"
        _create_partition_table(conn, partition)

    # ── 4. Copy rows to partitions ───────────────────────────────────────
    cols_csv = ", ".join(_PARTITION_COLUMNS)
    for ym in months:
        partition = f"messages_{ym}"
        if ym == "unknown":
            conn.execute(
                f"INSERT OR IGNORE INTO {partition} ({cols_csv}) "  # noqa: S608
                f"SELECT {cols_csv} FROM messages "
                "WHERE NOT ("
                "  length(timestamp) >= 7 "
                "  AND substr(timestamp, 5, 1) = '-' "
                "  AND substr(timestamp, 1, 4) GLOB '[0-9][0-9][0-9][0-9]' "
                "  AND substr(timestamp, 6, 2) GLOB '[0-9][0-9]'"
                ")"
            )
        else:
            yyyy_mm = f"{ym[:4]}-{ym[4:]}"
            conn.execute(
                f"INSERT OR IGNORE INTO {partition} ({cols_csv}) "  # noqa: S608
                f"SELECT {cols_csv} FROM messages "
                "WHERE substr(timestamp, 1, 7) = ?",
                (yyyy_mm,),
            )

    # ── 5. Verify row counts ─────────────────────────────────────────────
    post_count = sum(
        conn.execute(
            f"SELECT COUNT(*) FROM messages_{ym}"  # noqa: S608
        ).fetchone()[0]
        for ym in months
    )
    if post_count != pre_count:
        raise RuntimeError(
            f"v008: partition copy lost rows — pre={pre_count} post={post_count}; "
            "rolling back",
        )

    max_id_row = conn.execute(
        "SELECT COALESCE(MAX(id), 0) FROM messages"
    ).fetchone()
    max_id = int(_value(max_id_row))

    # ── 6. Rebuild usage_events to drop the FK on messages(id) ───────────
    _rebuild_usage_events_no_fk(conn)

    # ── 7. Drop the original messages table ──────────────────────────────
    conn.execute("DROP TABLE messages")

    # ── 8. Create the messages view spanning every partition ─────────────
    _rebuild_messages_view(conn)
    _rebuild_messages_insert_trigger(conn)

    # ── 9. Create the global id sequence table ───────────────────────────
    conn.execute(
        "CREATE TABLE _messages_id_seq ("
        "  rowid_kind INTEGER PRIMARY KEY CHECK (rowid_kind = 1),"
        "  next_id INTEGER NOT NULL"
        ")"
    )
    conn.execute(
        "INSERT INTO _messages_id_seq (rowid_kind, next_id) VALUES (1, ?)",
        (max_id + 1,),
    )

    _log.info(
        "v008: partitioned %d rows across %d months — view + sequence ready "
        "(next_id=%d)",
        pre_count, len(months), max_id + 1,
    )


# ── helpers used by the migration body ───────────────────────────────────────


def _value(row):
    """Return the first column from *row*, indifferent to Row/tuple shape."""
    if hasattr(row, "keys"):
        keys = list(row.keys())
        return row[keys[0]]
    return row[0]


def _create_partition_table(conn: sqlite3.Connection, partition: str) -> None:
    """Create one ``messages_YYYYMM`` table + its indexes if missing.

    Mirrors the original ``messages`` schema: ``INTEGER PRIMARY KEY`` on
    ``id`` (we explicitly assign ids from the global sequence at write
    time), the FK on ``session_fk`` with cascade delete, and the
    ``UNIQUE(session_fk, seq)`` constraint that backs ``INSERT OR
    IGNORE`` dedup. Index names are namespaced with the partition so
    multiple partitions can coexist without conflict.

    Per-statement ``execute`` (not ``executescript``) — the latter
    implicitly COMMITs the open transaction, breaking the
    ``schema._run_python_migration`` rollback-on-error contract.
    """
    if not _PARTITION_NAME_RE.match(partition):
        raise ValueError(f"Invalid partition name: {partition!r}")

    existing = conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
        (partition,),
    ).fetchone()
    if existing:
        return

    conn.execute(f"""
        CREATE TABLE {partition} (
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
            speed                 TEXT NOT NULL DEFAULT 'standard',
            UNIQUE (session_fk, seq)
        )
    """)
    conn.execute(
        f"CREATE INDEX IF NOT EXISTS idx_{partition}_session_seq "
        f"ON {partition}(session_fk, seq)"
    )
    conn.execute(
        f"CREATE INDEX IF NOT EXISTS idx_{partition}_timestamp "
        f"ON {partition}(timestamp)"
    )
    conn.execute(
        f"CREATE INDEX IF NOT EXISTS idx_{partition}_model "
        f"ON {partition}(model)"
    )


def _rebuild_messages_view(conn: sqlite3.Connection) -> None:
    """(Re)create the ``messages`` view spanning every partition table.

    Discovers every ``messages_YYYYMM`` and ``messages_unknown`` table
    by name, sorts them, and emits a UNION ALL with explicit column
    selection. Explicit columns (rather than ``SELECT *``) make column
    drift across partitions a hard error at view-definition time.

    Safe to call repeatedly — the view is dropped before recreation.
    """
    rows = conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' "
        "AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]' "
        "     OR name = 'messages_unknown')"
    ).fetchall()
    partitions = sorted(str(_value(r)) for r in rows)
    if not partitions:
        return

    cols_csv = ", ".join(_PARTITION_COLUMNS)
    union_sql = " UNION ALL ".join(
        f"SELECT {cols_csv} FROM {p}" for p in partitions  # noqa: S608
    )
    conn.execute("DROP VIEW IF EXISTS messages")
    conn.execute(f"CREATE VIEW messages AS {union_sql}")  # noqa: S608


def _rebuild_messages_insert_trigger(conn: sqlite3.Connection) -> None:
    """(Re)create the INSTEAD OF INSERT trigger on the ``messages`` view.

    Tests + ad-hoc tooling across the codebase do raw ``INSERT INTO
    messages (...)`` to seed fixtures. Without an INSTEAD OF trigger
    those would fail with ``cannot modify messages because it is a
    view`` after v008. The trigger handles the slow path:

    1. Advance ``_messages_id_seq`` so it stays ahead of any explicit
       ``NEW.id`` and bumps by one when ``NEW.id`` is NULL.
    2. Route the row into the partition matching
       ``substr(NEW.timestamp, 1, 7)``. Each partition's ``INSERT OR
       IGNORE`` clause makes ``INSERT INTO messages`` idempotent on
       ``UNIQUE(session_fk, seq)`` conflicts — matching the original
       table behaviour the tests assume.
    3. Fall back to ``messages_unknown`` for any timestamp that does
       not match a known month — no row is silently dropped.

    The trigger body lists every partition explicitly. Whenever a new
    partition is created the trigger must be rebuilt; the writer's
    ``_ensure_partition`` does that automatically.

    Production writes go through ``stackunderflow.ingest.writer``
    which inserts directly into the partition + bumps the sequence
    inline — bypassing this trigger for performance. The trigger only
    fires when callers use the ``messages`` view name in an INSERT.
    """
    rows = conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' "
        "AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]' "
        "     OR name = 'messages_unknown')"
    ).fetchall()
    partitions = sorted(str(_value(r)) for r in rows)
    # Always ensure ``messages_unknown`` exists as the fallback target;
    # the trigger uses it for any timestamp that doesn't match a known
    # month. The migration's bootstrap path may have skipped it on a
    # fully-populated store.
    if "messages_unknown" not in partitions:
        _create_partition_table(conn, "messages_unknown")
        partitions = sorted(set(partitions) | {"messages_unknown"})
        _rebuild_messages_view(conn)

    cols_csv = ", ".join(_PARTITION_COLUMNS)
    # SELECT expressions per column. ``id`` resolves through the
    # sequence when NEW.id is NULL. NOT NULL columns with DEFAULTs need
    # explicit COALESCE because INSTEAD OF triggers see NEW.col as NULL
    # for columns not supplied in the original INSERT — the DEFAULT
    # only fires on direct table inserts, not on trigger-driven inserts.
    select_exprs = []
    for col in _PARTITION_COLUMNS:
        if col == "id":
            select_exprs.append(
                "COALESCE(NEW.id, "
                "(SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1))"
            )
        elif col in _COLUMN_DEFAULTS:
            select_exprs.append(f"COALESCE(NEW.{col}, {_COLUMN_DEFAULTS[col]})")
        else:
            select_exprs.append(f"NEW.{col}")
    base_select = ", ".join(select_exprs)

    known_months = [
        p[len("messages_"):] for p in partitions if p != "messages_unknown"
    ]
    inserts: list[str] = []
    for ym in known_months:
        yyyy_mm = f"{ym[:4]}-{ym[4:]}"
        inserts.append(
            f"INSERT OR IGNORE INTO messages_{ym} ({cols_csv}) "  # noqa: S608
            f"SELECT {base_select} "
            f"WHERE substr(NEW.timestamp, 1, 7) = '{yyyy_mm}';"
        )
    # Fallback: anything that doesn't match a known month →
    # messages_unknown. ``length(NEW.timestamp) < 7`` short-circuits
    # before the substr/IN check, which is an exact match against the
    # known months.
    if known_months:
        known_list = ", ".join(f"'{ym[:4]}-{ym[4:]}'" for ym in known_months)
        fallback_where = (
            f"length(NEW.timestamp) < 7 "
            f"OR substr(NEW.timestamp, 5, 1) <> '-' "
            f"OR substr(NEW.timestamp, 1, 7) NOT IN ({known_list})"
        )
    else:
        fallback_where = "1 = 1"
    inserts.append(
        f"INSERT OR IGNORE INTO messages_unknown ({cols_csv}) "
        f"SELECT {base_select} "
        f"WHERE {fallback_where};"
    )

    # Sequence bump: stay strictly ahead of any explicit NEW.id, and
    # advance by 1 when no id was supplied (so the next INSERT with
    # NULL id sees a fresh value). Composing both into one UPDATE keeps
    # the trigger atomic.
    bump_sql = (
        "UPDATE _messages_id_seq SET next_id = MAX("
        "  next_id + (CASE WHEN NEW.id IS NULL THEN 1 ELSE 0 END),"
        "  COALESCE(NEW.id + 1, next_id)"
        ") WHERE rowid_kind = 1;"
    )
    body = bump_sql + "".join(inserts)

    conn.execute("DROP TRIGGER IF EXISTS messages_insert_route")
    conn.execute(
        "CREATE TRIGGER messages_insert_route INSTEAD OF INSERT ON messages "
        f"BEGIN {body} END"  # noqa: S608 — partition names + months are validated
    )


def _rebuild_usage_events_no_fk(conn: sqlite3.Connection) -> None:
    """Rebuild ``usage_events`` to remove the FK on ``messages(id)``.

    SQLite can't drop a column-level FK with ALTER TABLE, so we follow
    the standard 4-step rebuild dance:

        1. CREATE TABLE usage_events_new (... no REFERENCES messages ...)
        2. INSERT INTO usage_events_new SELECT * FROM usage_events
        3. DROP TABLE usage_events
        4. ALTER TABLE usage_events_new RENAME TO usage_events

    Then recreate the indexes — ALTER RENAME does not carry indexes.

    The UNIQUE index ``uniq_events_msg`` on ``source_message_fk`` is
    preserved because it backs the normalizer's dedup
    ``INSERT OR IGNORE`` path. Without it, watcher + backfill races
    could double-insert events. Application-level integrity replaces
    the FK constraint — no FK on a view is enforceable, so this
    constraint just goes away.
    """
    conn.execute("""
        CREATE TABLE usage_events_new (
            id                  INTEGER PRIMARY KEY,
            source_message_fk   INTEGER NOT NULL,
            provider            TEXT    NOT NULL,
            account             TEXT    NOT NULL DEFAULT 'default',
            project_id          INTEGER NOT NULL REFERENCES projects(id),
            session_id          TEXT    NOT NULL,
            ts                  TEXT    NOT NULL,
            day                 TEXT    NOT NULL,
            model               TEXT    NOT NULL DEFAULT '',
            speed               TEXT    NOT NULL DEFAULT 'standard',
            input_tokens        INTEGER NOT NULL DEFAULT 0,
            output_tokens       INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
            cache_create_tokens INTEGER NOT NULL DEFAULT 0,
            cost_usd            REAL    NOT NULL DEFAULT 0.0,
            cost_source         TEXT    NOT NULL DEFAULT 'rate_card',
            role                TEXT    NOT NULL,
            raw_extras          TEXT
        )
    """)
    conn.execute("""
        INSERT INTO usage_events_new (
            id, source_message_fk, provider, account, project_id,
            session_id, ts, day, model, speed,
            input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,
            cost_usd, cost_source, role, raw_extras
        )
        SELECT
            id, source_message_fk, provider, account, project_id,
            session_id, ts, day, model, speed,
            input_tokens, output_tokens, cache_read_tokens, cache_create_tokens,
            cost_usd, cost_source, role, raw_extras
        FROM usage_events
    """)
    conn.execute("DROP TABLE usage_events")
    conn.execute("ALTER TABLE usage_events_new RENAME TO usage_events")
    conn.execute("CREATE INDEX idx_events_day      ON usage_events(day)")
    conn.execute("CREATE INDEX idx_events_project  ON usage_events(project_id, day)")
    conn.execute("CREATE INDEX idx_events_provider ON usage_events(provider, day)")
    conn.execute("CREATE INDEX idx_events_session  ON usage_events(session_id)")
    conn.execute("CREATE INDEX idx_events_model    ON usage_events(model, day)")
    conn.execute(
        "CREATE UNIQUE INDEX uniq_events_msg ON usage_events(source_message_fk)"
    )
