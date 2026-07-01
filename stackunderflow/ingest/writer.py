"""Transactional writer: one file → one transaction → one ingest_log row.

Wave 4B adds a per-record **normalize + insert** hook so newly-ingested
messages auto-create ``usage_events`` rows in the same transaction
that wrote the ``messages`` rows. The hook reuses the registered
provider normalizer (Wave 2A) and writes through ``INSERT OR IGNORE``
against the ``uniq_events_msg`` UNIQUE index — the watcher path and
the backfill path share the same insert SQL. After the per-file
transaction commits we call :func:`refresh_all_marts` so each mart's
watermark advances by the new event ids.

If the provider has no normalizer registered (rare — users with beta
adapters disabled, or a brand-new provider that ships before its
normalizer), the hook silently no-ops and a debug line is logged. The
``messages`` row still lands; the ETL just doesn't materialise an
event for it. The next ``stackunderflow etl backfill`` will pick it
up.

v008 partitioning
-----------------
``messages`` is now a UNION-ALL view over per-month ``messages_YYYYMM``
partition tables (see ``stackunderflow/store/migrations/
v008_messages_partitioning.py``). Writes route through
:func:`_partition_for` and land in the matching partition table; the
per-row id comes from the global ``_messages_id_seq`` so ids stay
unique across partitions (preserving the dedup key
``usage_events.uniq_events_msg``). When a record's timestamp falls
into a month without a partition yet, :func:`_ensure_partition`
creates the table + indexes and rebuilds the ``messages`` view inside
the same per-file transaction.
"""

from __future__ import annotations

import json
import logging
import re
import sqlite3
import time

from stackunderflow.adapters.base import Record, SessionRef, SourceAdapter

_log = logging.getLogger(__name__)

# Columns every partition table exposes — kept in sync with the
# migration's ``_PARTITION_COLUMNS`` (the migration module is loaded by
# pathname, not as a regular package member, so we duplicate rather
# than import). Future schema additions to ``messages`` must update
# both lists, every existing partition table, and the view rebuild.
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

# Default literals for the INSTEAD OF INSERT trigger — kept in sync
# with the migration's identically-named map. NOT NULL + DEFAULT
# columns need an explicit COALESCE in the trigger because NEW.col is
# NULL when the original INSERT didn't supply the column.
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


def ingest_file(
    conn: sqlite3.Connection,
    adapter: SourceAdapter,
    ref: SessionRef,
    *,
    since_offset: int = 0,
) -> None:
    """Ingest all new records from *ref* in a single transaction.

    Raises whatever the adapter raises; the transaction rolls back and
    the ingest_log is left untouched.

    For ``ref.source_kind == "file"`` the ingest_log row stores
    ``processed_offset = ref.file_size`` (byte position into a JSONL).
    For ``"database"`` the row stores ``last_rowid = max(record.seq)``
    seen in this batch — the next pass resumes from that rowid keyed on
    ``(file_path, session_id)``.
    """
    conn.execute("BEGIN")
    try:
        project_id = _upsert_project(conn, ref)
        session_fk = _upsert_session(conn, project_id, ref)

        max_ts: str | None = None
        # max_seq carries the highest record.seq we observed in this batch.
        # For both source kinds the semantic on the next ingest is "give me
        # records strictly past this seq" — for database mode that's a
        # rowid; for file mode that's the byte offset of the last line.
        max_seq: int = since_offset
        count_added = 0
        # Track the new message rowids so the post-insert normalize pass
        # only walks the rows this batch added — no need to re-scan the
        # whole table.
        new_message_ids: list[int] = []
        for record in adapter.read(ref, since_offset=since_offset):
            changes, msg_id = _insert_message(conn, session_fk, record)
            if changes:
                count_added += 1
                if msg_id is not None:
                    new_message_ids.append(msg_id)
                if max_ts is None or record.timestamp > max_ts:
                    max_ts = record.timestamp
                if record.seq > max_seq:
                    max_seq = record.seq

        if count_added:
            conn.execute(
                "UPDATE sessions SET message_count = message_count + ?, "
                "                     last_ts = COALESCE(MAX(COALESCE(last_ts, ''), ?), last_ts), "
                "                     first_ts = COALESCE(first_ts, ?) "
                "WHERE id = ?",
                (count_added, max_ts or "", max_ts or "", session_fk),
            )

        if ref.source_kind == "database":
            # Database-backed sources resume by rowid keyed on (file_path,
            # session_id). The partial unique index covers session_id IS
            # NOT NULL rows; processed_offset stays NULL.
            conn.execute(
                "INSERT INTO ingest_log "
                "(file_path, provider, session_id, storage_kind, "
                " mtime, size, processed_offset, last_rowid, last_ingest_ts) "
                "VALUES (?, ?, ?, 'database', ?, ?, NULL, ?, ?) "
                "ON CONFLICT(file_path, session_id) WHERE session_id IS NOT NULL "
                "DO UPDATE SET "
                "  mtime=excluded.mtime, size=excluded.size, "
                "  storage_kind=excluded.storage_kind, "
                "  processed_offset=NULL, "
                "  last_rowid=excluded.last_rowid, "
                "  last_ingest_ts=excluded.last_ingest_ts",
                (
                    str(ref.file_path),
                    ref.provider,
                    ref.session_id,
                    ref.file_mtime,
                    ref.file_size,
                    max_seq,
                    time.time(),
                ),
            )
        else:
            # File-backed sources resume from the highest seq observed
            # (= byte offset of the last yielded line). session_id is NULL
            # so a single .jsonl is one ingest_log row regardless of how
            # many sessions live inside it. The partial unique index on
            # file_path WHERE session_id IS NULL is the conflict target.
            #
            # First-time ingest with no records: store the file_size so we
            # don't re-scan empty/non-conversational files on every pass.
            stored_offset = max_seq if count_added else ref.file_size
            conn.execute(
                "INSERT INTO ingest_log "
                "(file_path, provider, session_id, storage_kind, "
                " mtime, size, processed_offset, last_rowid, last_ingest_ts) "
                "VALUES (?, ?, NULL, 'file', ?, ?, ?, NULL, ?) "
                "ON CONFLICT(file_path) WHERE session_id IS NULL "
                "DO UPDATE SET "
                "  mtime=excluded.mtime, size=excluded.size, "
                "  storage_kind=excluded.storage_kind, "
                "  processed_offset=excluded.processed_offset, "
                "  last_rowid=NULL, "
                "  last_ingest_ts=excluded.last_ingest_ts",
                (
                    str(ref.file_path),
                    ref.provider,
                    ref.file_mtime,
                    ref.file_size,
                    stored_offset,
                    time.time(),
                ),
            )

        # ── Wave 4B: per-file normalize + insert hook ─────────────────
        #
        # Convert the messages we just inserted into ``usage_events``
        # rows in the same transaction. Idempotent via the
        # ``uniq_events_msg`` UNIQUE index; no-op when the provider has
        # no normalizer registered.
        events_inserted = 0
        if new_message_ids:
            try:
                events_inserted = _normalize_new_messages(
                    conn, ref.provider, new_message_ids,
                )
            except Exception as exc:  # noqa: BLE001 — never fail ingest because of normalize
                _log.warning(
                    "ingest.writer: normalize failed for %s (%s): %s — "
                    "messages still committed; run `stackunderflow etl backfill` to recover",
                    ref.provider, ref.file_path, exc,
                )

        conn.execute("COMMIT")
    except Exception:
        conn.execute("ROLLBACK")
        raise

    # ── Wave 4B: refresh marts after the per-file commit ──────────────
    #
    # Done outside the per-file transaction so the mart upserts run
    # against fully-committed events. Each mart is watermarked +
    # idempotent on its own — if marts can't refresh (registry empty,
    # SQL error), we log and move on; the next pass will catch up.
    if events_inserted:
        try:
            from stackunderflow.etl.watermark import refresh_all_marts
            refresh_all_marts(conn)
        except Exception as exc:  # noqa: BLE001
            _log.debug(
                "ingest.writer: refresh_all_marts after %s failed: %s",
                ref.provider, exc,
            )


def _upsert_project(conn: sqlite3.Connection, ref: SessionRef) -> int:
    row = conn.execute(
        "SELECT id FROM projects WHERE provider = ? AND slug = ?",
        (ref.provider, ref.project_slug),
    ).fetchone()
    if row:
        conn.execute(
            "UPDATE projects SET last_modified = MAX(last_modified, ?) WHERE id = ?",
            (ref.file_mtime, row["id"]),
        )
        return row["id"]
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (
            ref.provider,
            ref.project_slug,
            None,
            ref.project_slug,
            ref.file_mtime,
            ref.file_mtime,
        ),
    )
    assert cur.lastrowid is not None  # noqa: S101
    return cur.lastrowid


def _upsert_session(conn: sqlite3.Connection, project_id: int, ref: SessionRef) -> int:
    row = conn.execute(
        "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
        (project_id, ref.session_id),
    ).fetchone()
    if row:
        return row["id"]
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (project_id, ref.session_id),
    )
    assert cur.lastrowid is not None  # noqa: S101
    return cur.lastrowid


def _insert_message(
    conn: sqlite3.Connection, session_fk: int, rec: Record,
) -> tuple[int, int | None]:
    """Insert one message row into the partition for ``rec.timestamp``.

    Returns ``(rowcount, rowid_or_none)``. ``rowcount`` is ``1`` on a
    successful insert, ``0`` if the row was already present (the
    ``INSERT OR IGNORE`` path). The rowid is the new ``messages.id``
    when an insert happened, ``None`` otherwise — callers use it to
    drive the per-record normalize hook (Wave 4B).

    v008 partitions ``messages`` into monthly ``messages_YYYYMM``
    tables behind a UNION-ALL view. The writer routes inserts to the
    partition matching ``rec.timestamp`` (or ``messages_unknown`` for
    malformed timestamps), creating the partition + extending the
    ``messages`` view on demand. Ids come from the global
    ``_messages_id_seq`` table so they stay unique across partitions
    (preserving the dedup key the normalizer relies on).
    """
    # ``speed`` carries Anthropic's priority/fast tier flag (PR #44).
    # Persisted to the messages partitions by v003 so SQL-driven cost paths
    # (get_global_stats, services/compare, reports/export, build_enriched_dataset)
    # can apply the 6× Opus multiplier without round-tripping raw_json.
    partition = _partition_for(rec.timestamp)
    _ensure_partition(conn, partition)
    new_id = _next_message_id(conn)
    cur = conn.execute(
        f"INSERT OR IGNORE INTO {partition} ("  # noqa: S608 — partition is regex-validated
        "  id, session_fk, seq, timestamp, role, model, "
        "  input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "  content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, "
        "  speed"
        ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            new_id,
            session_fk,
            rec.seq,
            rec.timestamp,
            rec.role,
            rec.model,
            rec.input_tokens,
            rec.output_tokens,
            rec.cache_create_tokens,
            rec.cache_read_tokens,
            rec.content_text,
            json.dumps(list(rec.tools)),
            json.dumps(rec.raw, default=str),
            int(rec.is_sidechain),
            rec.uuid,
            rec.parent_uuid,
            rec.speed,
        ),
    )
    if cur.rowcount:
        return 1, new_id
    # UNIQUE conflict on (session_fk, seq) — the assigned id is unused;
    # we leak it (sequence keeps moving forward). Acceptable: bounded by
    # the number of duplicate INSERT attempts, which on a normal run is 0.
    return 0, None


# ── Wave 4B: normalize + insert hook ─────────────────────────────────────────
#
# Shared by the ingest writer (this module — runs after each per-file
# transaction) and the backfill orchestrator
# (``stackunderflow/etl/backfill.py``). One source of truth for the
# ``usage_events`` insert SQL keeps the watcher and backfill paths in
# lockstep — diverging would mean a "watcher saw it but backfill
# didn't" or vice versa, which is exactly the class of bug the unified
# helper guards against.

# Columns we read off ``messages`` (joined to sessions + projects) to
# hand the normalizer a self-contained dict. Same shape as the watcher
# uses in ``etl/watcher.py::_normalize_recent``.
_MSG_JOIN_SQL = """
    SELECT m.id            AS id,
           m.session_fk    AS session_fk,
           m.seq           AS seq,
           m.timestamp     AS timestamp,
           m.role          AS role,
           m.model         AS model,
           m.input_tokens  AS input_tokens,
           m.output_tokens AS output_tokens,
           m.cache_read_tokens AS cache_read_tokens,
           m.cache_create_tokens AS cache_create_tokens,
           m.content_text  AS content_text,
           m.tools_json    AS tools_json,
           m.raw_json      AS raw_json,
           m.is_sidechain  AS is_sidechain,
           m.uuid          AS uuid,
           m.parent_uuid   AS parent_uuid,
           m.speed         AS speed,
           s.session_id    AS session_id,
           s.project_id    AS project_id,
           p.provider      AS provider
      FROM messages m
      JOIN sessions s ON s.id = m.session_fk
      JOIN projects p ON p.id = s.project_id
"""


def normalize_and_insert_event(
    conn: sqlite3.Connection,
    msg_row: dict,
    event: dict,
) -> tuple[int, int]:
    """Insert one normalizer-yielded event row into ``usage_events``.

    Returns ``(inserted, skipped_duplicate)`` where each is ``0`` or
    ``1`` (a single ``INSERT OR IGNORE``). Duplicate detection rides
    on the ``uniq_events_msg`` UNIQUE index over ``source_message_fk``.

    Used by both the ingest writer's per-file hook and the backfill
    orchestrator so the insert SQL stays a single source of truth.
    Pre-Wave-4B the watcher had its own copy of this shape; Wave 4B
    leaves the watcher's copy untouched (per scope rules) but new
    callers route through here.
    """
    cur = conn.execute(
        """
        INSERT OR IGNORE INTO usage_events (
            source_message_fk, provider, account, project_id,
            session_id, ts, day, model, speed,
            input_tokens, output_tokens,
            cache_read_tokens, cache_create_tokens, reasoning_tokens,
            cost_usd, cost_source, role, raw_extras
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        (
            msg_row["id"],
            event.get("provider") or msg_row.get("provider") or "",
            event.get("account") or "default",
            event.get("project_id") or msg_row.get("project_id"),
            event.get("session_id") or msg_row.get("session_id") or "",
            event.get("ts") or msg_row.get("timestamp") or "",
            event.get("day") or _day_of(str(event.get("ts") or msg_row.get("timestamp") or "")),
            event.get("model") or msg_row.get("model") or "",
            event.get("speed") or msg_row.get("speed") or "standard",
            int(event.get("input_tokens") or 0),
            int(event.get("output_tokens") or 0),
            int(event.get("cache_read_tokens") or 0),
            int(event.get("cache_create_tokens") or 0),
            # Attribution-only subset of output_tokens (0 unless a normalizer
            # surfaced a real reasoning count); never enters cost. Defaults 0 so
            # events yielded before v026 / by providers with no reasoning are
            # unaffected.
            int(event.get("reasoning_tokens") or 0),
            float(event.get("cost_usd") or 0.0),
            event.get("cost_source") or "rate_card",
            event.get("role") or msg_row.get("role") or "",
            event.get("raw_extras"),
        ),
    )
    if cur.rowcount:
        return 1, 0
    return 0, 1


def _normalize_new_messages(
    conn: sqlite3.Connection, provider: str, message_ids: list[int],
) -> int:
    """Run the registered *provider* normalizer over *message_ids*.

    Reads each row back via the standard ``messages → sessions →
    projects`` join so the normalizer receives the full row dict it
    expects. ``INSERT OR IGNORE`` against ``uniq_events_msg`` makes
    re-runs idempotent — a watcher cycle that races a backfill won't
    double-insert.

    Returns the number of events inserted. Returns 0 silently when the
    provider has no normalizer (rare — beta-disabled providers, or a
    new provider that ships before its normalizer).
    """
    if not message_ids:
        return 0
    # Lazy-import the registry: keeps ``ingest.writer`` usable in test
    # contexts that don't want the etl package on their import path.
    try:
        from stackunderflow.etl.normalize import get as _get_normalizer
    except ImportError:
        return 0

    normalizer_cls = _get_normalizer(provider)
    if normalizer_cls is None:
        _log.debug(
            "ingest.writer: no normalizer registered for provider %r — "
            "skipping (run `stackunderflow etl backfill` to materialise "
            "events later if a normalizer is added)",
            provider,
        )
        return 0
    normalizer = normalizer_cls()

    # SQLite has a per-statement parameter limit (~32K by default,
    # 999 on older builds). Ingest batches are typically << 1K rows,
    # but cap at 500 just to be paranoid; chunk if larger.
    inserted = 0
    chunk_size = 500
    for i in range(0, len(message_ids), chunk_size):
        chunk = message_ids[i:i + chunk_size]
        placeholders = ",".join("?" * len(chunk))
        rows = conn.execute(
            f"{_MSG_JOIN_SQL} WHERE m.id IN ({placeholders}) ORDER BY m.id",  # noqa: S608 — placeholders are '?'
            chunk,
        ).fetchall()
        for row in rows:
            msg_row = dict(row)
            try:
                events = list(normalizer.normalize(msg_row))
            except Exception as exc:  # noqa: BLE001 — poison row must not stop the batch
                _log.debug(
                    "ingest.writer: normalizer raised for msg %s: %s",
                    msg_row.get("id"), exc,
                )
                continue
            for ev in events:
                ins, _skp = normalize_and_insert_event(conn, msg_row, ev)
                inserted += ins
    return inserted


def _day_of(ts: str) -> str:
    """Best-effort YYYY-MM-DD slice from an ISO 8601 timestamp.

    Mirrors :func:`stackunderflow.etl.normalize.base._day_from_ts`'s
    cheap path. Defensive — returns "" when the input is empty / mal-
    formed; the normalizer's ``_build_event`` already handles that
    case in ``raw_extras``-style logging.
    """
    if not ts or len(ts) < 10:
        return ""
    return ts[:10] if ts[4] == "-" and ts[7] == "-" else ""


# ── v008: messages partitioning helpers ──────────────────────────────────────


def _partition_for(ts: str) -> str:
    """Return the partition table name for an ISO-8601 *ts*.

    Maps ``"2026-04-15T..."`` → ``"messages_202604"``. Falls back to
    ``"messages_unknown"`` on empty or malformed timestamps so no row
    is ever lost — the writer + view both treat ``messages_unknown``
    as a regular partition.
    """
    if not ts or len(ts) < 7 or ts[4] != "-":
        return "messages_unknown"
    year = ts[:4]
    month = ts[5:7]
    if not (year.isdigit() and month.isdigit()):
        return "messages_unknown"
    return f"messages_{year}{month}"


def _ensure_partition(conn: sqlite3.Connection, partition: str) -> bool:
    """Create *partition* + indexes + extend the messages view if missing.

    Returns ``True`` when a new partition was created (the view was
    rebuilt). The caller can ignore the return value — it's only there
    so tests can assert the expected state transitions.

    Cheap on the hot path: a single ``SELECT FROM sqlite_master`` per
    insert checks whether the partition exists. The DDL + view rebuild
    only runs on the first insert into a brand-new month — typically
    once per month boundary.

    The partition name is regex-validated (``messages_YYYYMM`` or
    ``messages_unknown``) before formatting into SQL so we cannot be
    coerced into running arbitrary DDL.
    """
    if not _PARTITION_NAME_RE.match(partition):
        raise ValueError(f"Invalid partition name: {partition!r}")
    existing = conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
        (partition,),
    ).fetchone()
    if existing:
        return False

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
    _rebuild_messages_view(conn)
    _rebuild_messages_insert_trigger(conn)
    return True


def _list_partitions(conn: sqlite3.Connection) -> list[str]:
    """Return every partition table name in sorted order."""
    rows = conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' "
        "AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]' "
        "     OR name = 'messages_unknown')"
    ).fetchall()
    return sorted(str(r[0] if not hasattr(r, "keys") else r["name"]) for r in rows)


def _rebuild_messages_view(conn: sqlite3.Connection) -> None:
    """(Re)create the ``messages`` view spanning every existing partition.

    Discovers partitions via ``sqlite_master`` GLOB, sorts them, and
    emits ``CREATE VIEW messages AS SELECT cols FROM p1 UNION ALL
    SELECT cols FROM p2 ...``. Explicit columns guard against silent
    drift in any one partition.
    """
    partitions = _list_partitions(conn)
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

    Mirrors the trigger built by the v008 migration so callers that
    use the ``messages`` name directly (e.g. fixture-seeding tests, ad
    hoc tooling) keep working. Production writes route directly via
    :func:`_insert_message` and bypass this trigger.

    Rebuilt every time a new partition is added so the trigger's WHEN
    clauses cover every active month.
    """
    partitions = _list_partitions(conn)
    if not partitions:
        return

    cols_csv = ", ".join(_PARTITION_COLUMNS)
    select_exprs: list[str] = []
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

    known_months = [p[len("messages_"):] for p in partitions if p != "messages_unknown"]
    inserts: list[str] = []
    for ym in known_months:
        yyyy_mm = f"{ym[:4]}-{ym[4:]}"
        inserts.append(
            f"INSERT OR IGNORE INTO messages_{ym} ({cols_csv}) "  # noqa: S608
            f"SELECT {base_select} "
            f"WHERE substr(NEW.timestamp, 1, 7) = '{yyyy_mm}';"
        )
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
        f"BEGIN {body} END"  # noqa: S608
    )


def _next_message_id(conn: sqlite3.Connection) -> int:
    """Atomically reserve and return the next global ``messages.id``.

    The caller is already inside a transaction (``ingest_file`` opens
    ``BEGIN`` before the first ``_insert_message``), so the read-then-
    update pair is serialised against any concurrent writer on the
    same connection.

    On a v008-migrated DB the ``_messages_id_seq`` row is initialised
    to ``MAX(id) + 1`` from the pre-migration ``messages`` table. On
    a brand-new DB it starts at 1.
    """
    row = conn.execute(
        "SELECT next_id FROM _messages_id_seq WHERE rowid_kind = 1"
    ).fetchone()
    if row is None:  # pragma: no cover — defensive: would mean migration didn't run
        raise RuntimeError(
            "ingest.writer: _messages_id_seq is missing — run schema.apply() first"
        )
    next_id = int(row[0] if not hasattr(row, "keys") else row["next_id"])
    conn.execute(
        "UPDATE _messages_id_seq SET next_id = next_id + 1 WHERE rowid_kind = 1"
    )
    return next_id
