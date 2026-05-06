"""Backfill orchestrator — convert ``messages`` → ``usage_events`` → marts.

Walks every existing ``messages`` row through the registered normalizers
to materialize ``usage_events`` rows, then refreshes every registered
mart from those events.

Wave 4B fills in the body that Wave 1 skeleton-locked:

* Streams the ``messages`` table (joined to ``sessions`` + ``projects``
  so the normalizer has provider + project_id + session_id without an
  extra round-trip) in chunks of :data:`_CHUNK_SIZE` rows. The user's
  store has ~228K messages — ``fetchall()`` would balloon RAM and
  block the connection for the duration of the run.
* One transaction per chunk so a partial failure leaves a recoverable
  state. Re-runs are idempotent via ``uniq_events_msg`` UNIQUE on
  ``source_message_fk`` (``INSERT OR IGNORE``).
* ``force=True`` first DELETEs from ``usage_events`` + every mart via
  ``rebuild_from_scratch``, **then** runs the normalize pass fresh.
* After events are written, calls
  :func:`stackunderflow.etl.watermark.refresh_all_marts` so every mart
  picks up the new events. (Each mart is watermarked + idempotent on
  its own, so this is the canonical refresh entry-point.)

Logging is intentionally chatty for backfill — emits a progress line
every :data:`_PROGRESS_EVERY_EVENTS` events so the operator knows the
run is alive on a 228K-message store.

See ``docs/specs/etl-architecture.md``.
"""

from __future__ import annotations

import logging
import sqlite3
import time
from collections.abc import Iterator
from dataclasses import dataclass, field

from .marts import all as _all_marts
from .normalize import all as _all_normalizers
from .watermark import refresh_all_marts

_log = logging.getLogger(__name__)

# Chunk size for the streaming SELECT — keeps RAM bounded while still
# amortising the per-chunk transaction overhead. 5,000 rows is roughly
# a few MB of decoded sqlite3.Row objects.
_CHUNK_SIZE = 5_000

# Emit a progress log line every N events inserted (or attempted —
# whichever crosses the boundary first). Keeps the log readable on
# small stores without going silent on large ones.
_PROGRESS_EVERY_EVENTS = 10_000


@dataclass
class BackfillReport:
    """Summary of one ``backfill()`` call.

    Returned to the caller (CLI, API, watcher) so they can render
    progress, log timing, or drive further work. ``marts_refreshed``
    is a copy of the dict returned by
    :func:`stackunderflow.etl.watermark.refresh_all_marts` — empty
    when no mart builders are registered.
    """

    events_inserted: int = 0
    events_skipped_duplicate: int = 0
    marts_refreshed: dict[str, int] = field(default_factory=dict)
    duration_seconds: float = 0.0


def _drop_events_and_marts(conn: sqlite3.Connection) -> None:
    """``force=True`` path: rebuild every mart from scratch + zero out events.

    Schema stays intact (``DELETE``, not ``DROP``). Each mart's
    :meth:`rebuild_from_scratch` deletes its own table; we explicitly
    wipe ``usage_events`` + ``mart_watermark`` here so the next pass
    starts from event id ``1`` against fresh marts.

    Order matters: empty events first (so a mart's
    ``rebuild_from_scratch`` reading partial events doesn't repopulate
    against soon-to-be-deleted rows), then watermarks, then walk every
    registered mart. The mart's own ``DELETE FROM <name>_mart`` call
    inside ``rebuild_from_scratch`` is the canonical clear — looping
    via the registry keeps the orchestrator agnostic of which marts
    exist.
    """
    conn.execute("DELETE FROM usage_events")
    conn.execute("DELETE FROM mart_watermark")
    for mart_cls in _all_marts().values():
        mart_cls().rebuild_from_scratch(conn)


def backfill(
    conn: sqlite3.Connection,
    *,
    force: bool = False,
    progress_callback=None,
) -> BackfillReport:
    """One-shot: convert all existing ``messages`` into ``usage_events``,
    then refresh every mart from the new watermark.

    Default is incremental — already-converted messages are skipped via
    the ``UNIQUE(source_message_fk)`` index (``INSERT OR IGNORE``).

    ``force=True`` first wipes ``usage_events`` + ``mart_watermark`` and
    rebuilds every mart from scratch (via each mart's
    ``rebuild_from_scratch``) before running the normalize pass fresh.

    *progress_callback* is an optional ``Callable[[int, int], None]``
    invoked as ``cb(events_so_far, messages_seen)`` once per chunk.
    Used by the CLI to drive a tqdm bar; library callers leave it
    ``None``.
    """
    start = time.perf_counter()
    report = BackfillReport()

    if force:
        _drop_events_and_marts(conn)

    normalizers = _all_normalizers()
    if not normalizers:
        # No providers registered — nothing to convert. Still call
        # refresh_all_marts so empty marts can finalize their watermarks.
        report.marts_refreshed = refresh_all_marts(conn)
        report.duration_seconds = time.perf_counter() - start
        return report

    inserted, skipped, messages_seen = _run_normalizers(
        conn, normalizers, progress_callback=progress_callback,
    )
    report.events_inserted = inserted
    report.events_skipped_duplicate = skipped

    _log.info(
        "backfill: normalize pass complete — events_inserted=%d "
        "events_skipped_duplicate=%d messages_seen=%d",
        inserted, skipped, messages_seen,
    )

    report.marts_refreshed = refresh_all_marts(conn)

    _log.info(
        "backfill: refresh_all_marts complete — %s",
        " ".join(f"{n}={c}" for n, c in sorted(report.marts_refreshed.items()))
            or "(no marts registered)",
    )

    report.duration_seconds = time.perf_counter() - start
    return report


def _run_normalizers(
    conn: sqlite3.Connection,
    normalizers: dict,
    *,
    progress_callback=None,
) -> tuple[int, int, int]:
    """Stream the messages table and dispatch each row to its normalizer.

    Joins ``messages → sessions → projects`` so the normalizer receives
    a row dict with ``provider`` / ``project_id`` / ``session_id`` for
    free (no per-row lookup). Filters to providers that actually have a
    registered normalizer — there's no point streaming rows we'd skip.

    Each chunk is its own transaction:
      * BEGIN
      * SELECT next chunk
      * Per-row: dispatch to normalizer, ``INSERT OR IGNORE`` each yielded
        event, count inserted vs. skipped via ``cur.rowcount``.
      * COMMIT (or ROLLBACK on error — the caller re-raises).

    A partial failure mid-stream leaves the already-committed chunks in
    place. The next ``backfill()`` resumes via the UNIQUE source_message_fk
    index — already-inserted rows turn into counted skips.

    Returns ``(events_inserted, events_skipped_duplicate, messages_seen)``.
    """
    providers = tuple(sorted(normalizers))
    if not providers:
        return 0, 0, 0

    placeholders = ",".join("?" * len(providers))
    select_sql = f"""
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
         WHERE p.provider IN ({placeholders})
           AND m.id > ?
         ORDER BY m.id
         LIMIT ?
    """  # noqa: S608 — placeholders are a fixed number of '?', not user input

    inserted = 0
    skipped = 0
    messages_seen = 0
    last_id = 0
    last_progress_log = 0

    while True:
        chunk = _fetch_chunk(conn, select_sql, providers, last_id)
        if not chunk:
            break

        # One transaction per chunk — partial failure leaves prior
        # chunks committed and the next pass resumes via the UNIQUE
        # source_message_fk index.
        conn.execute("BEGIN")
        try:
            for row in chunk:
                msg_row = dict(row)
                last_id = int(msg_row["id"])
                messages_seen += 1

                provider = str(msg_row.get("provider") or "")
                normalizer_cls = normalizers.get(provider)
                if normalizer_cls is None:
                    # Filtered out at the SQL level above, but defensive.
                    continue
                normalizer = normalizer_cls()

                ins, skp = _normalize_and_insert(conn, normalizer, msg_row)
                inserted += ins
                skipped += skp

            conn.execute("COMMIT")
        except Exception:
            conn.execute("ROLLBACK")
            raise

        if progress_callback is not None:
            try:
                progress_callback(inserted, messages_seen)
            except Exception as exc:  # noqa: BLE001 — progress UI must never break ingest
                _log.debug("backfill: progress_callback raised: %s", exc)

        if inserted - last_progress_log >= _PROGRESS_EVERY_EVENTS:
            _log.info(
                "backfill: %d events inserted (%d messages seen)",
                inserted, messages_seen,
            )
            last_progress_log = inserted

        if len(chunk) < _CHUNK_SIZE:
            break

    return inserted, skipped, messages_seen


def _fetch_chunk(
    conn: sqlite3.Connection,
    select_sql: str,
    providers: tuple[str, ...],
    last_id: int,
) -> list:
    """Fetch one chunk of joined messages rows past *last_id*.

    Kept as its own helper so the streaming loop in :func:`_run_normalizers`
    is short and readable, and so tests can patch it to inject a
    failing chunk if needed.
    """
    cur = conn.execute(select_sql, (*providers, last_id, _CHUNK_SIZE))
    return cur.fetchall()


def _normalize_and_insert(
    conn: sqlite3.Connection,
    normalizer,
    msg_row: dict,
) -> tuple[int, int]:
    """Dispatch one ``messages`` row through *normalizer* and persist events.

    Returns ``(inserted, skipped_duplicate)``. Yielded events are
    inserted with ``INSERT OR IGNORE`` against ``uniq_events_msg``;
    when the message has already been converted, ``rowcount`` comes
    back ``0`` and the event counts as a skip.

    Re-uses :func:`stackunderflow.ingest.writer.normalize_and_insert_event`
    so the backfill path and the watcher / ingest writer path share the
    same insert SQL — a single source of truth for the events row shape.
    """
    # Local import — keeps ``ingest`` out of the import path of any
    # caller that just imports the normalize / mart registries.
    from stackunderflow.ingest.writer import normalize_and_insert_event

    inserted = 0
    skipped = 0
    try:
        events = list(normalizer.normalize(msg_row))
    except Exception as exc:  # noqa: BLE001 — never let a poison row stop the run
        _log.debug(
            "backfill: normalizer raised for msg %s: %s",
            msg_row.get("id"), exc,
        )
        return 0, 0

    for ev in events:
        ins, skp = normalize_and_insert_event(conn, msg_row, ev)
        inserted += ins
        skipped += skp
    return inserted, skipped


def _stream_messages_for(
    conn: sqlite3.Connection, providers: tuple[str, ...],
) -> Iterator[dict]:
    """Public-ish helper for tests: stream every joined messages row.

    Returns dict-shaped rows in id order. Mostly used by Wave 4B's tests
    to verify the streaming path doesn't fetchall().
    """
    placeholders = ",".join("?" * len(providers))
    sql = f"""
        SELECT m.id, p.provider
          FROM messages m
          JOIN sessions s ON s.id = m.session_fk
          JOIN projects p ON p.id = s.project_id
         WHERE p.provider IN ({placeholders})
         ORDER BY m.id
    """  # noqa: S608 — fixed-size placeholder list
    for row in conn.execute(sql, providers):
        yield dict(row)
