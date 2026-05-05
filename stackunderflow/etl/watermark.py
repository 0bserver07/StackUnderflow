"""Watermark helpers for the marts layer.

Each mart maintains an independent ``last_event_id`` watermark so that
incremental refresh can pick up where the previous run left off. The
helpers in this module wrap the four operations the orchestrator needs:

* :func:`get_watermark` — read the current ``last_event_id`` for a mart
  (returns ``0`` when the mart has never been built).
* :func:`set_watermark` — upsert ``last_event_id`` + ``last_refresh_ts``.
* :func:`refresh_all_marts` — for each registered mart, read its
  watermark, call ``refresh(since=<watermark>)``, persist the returned
  ``last_event_id`` back. Returns ``{mart_name: events_processed}``.

All three are idempotent and safe to call from any thread holding a
connection (callers manage transactions themselves).
"""

from __future__ import annotations

import sqlite3
from datetime import UTC, datetime

from .marts import all as _all_marts


def get_watermark(conn: sqlite3.Connection, mart_name: str) -> int:
    """Return the current ``last_event_id`` watermark for *mart_name*.

    Missing watermark → ``0`` (never been refreshed). The caller passes
    this into :meth:`MartBuilder.refresh` as ``since_event_id``.
    """
    row = conn.execute(
        "SELECT last_event_id FROM mart_watermark WHERE mart_name = ?",
        (mart_name,),
    ).fetchone()
    if row is None:
        return 0
    # sqlite3.Row supports both index and name access; raw tuples don't
    # have ``keys`` so we fall back to positional indexing.
    return int(row["last_event_id"]) if hasattr(row, "keys") else int(row[0])


def set_watermark(
    conn: sqlite3.Connection,
    mart_name: str,
    last_event_id: int,
) -> None:
    """Upsert the watermark for *mart_name* to *last_event_id*.

    Stamps ``last_refresh_ts`` with the current UTC time. Idempotent
    by virtue of ``ON CONFLICT DO UPDATE``.
    """
    now = datetime.now(UTC).isoformat()
    conn.execute(
        """
        INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts)
        VALUES (?, ?, ?)
        ON CONFLICT(mart_name) DO UPDATE SET
            last_event_id = excluded.last_event_id,
            last_refresh_ts = excluded.last_refresh_ts
        """,
        (mart_name, int(last_event_id), now),
    )


def refresh_all_marts(conn: sqlite3.Connection) -> dict[str, int]:
    """Refresh every registered mart from its current watermark.

    For each mart in :func:`stackunderflow.etl.marts.all`:

    1. Read the current watermark (``0`` if missing)
    2. Instantiate the mart builder and call ``refresh(conn, watermark)``
    3. Persist the returned ``last_event_id`` via :func:`set_watermark`
    4. Record ``events_processed = new_watermark - old_watermark`` in
       the result dict

    Returns ``{mart_name: events_processed}``. An empty registry returns
    ``{}`` — that's the Wave 1 default until Wave 2 lands. Idempotent:
    re-running with no new events returns ``{name: 0}`` for each mart.
    """
    out: dict[str, int] = {}
    for name, cls in _all_marts().items():
        old = get_watermark(conn, name)
        builder = cls()
        new = builder.refresh(conn, old)
        set_watermark(conn, name, new)
        out[name] = max(0, int(new) - int(old))
    return out
