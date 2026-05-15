"""Force a fresh ingest pass from a read-only CLI command.

Read-only commands (``status``, ``today``, ``month``, ``report``,
``compare``, ``yield``, ``optimize``, ``export``) query the SQLite store
at ``~/.stackunderflow/store.db``. The store is kept fresh by the
filesystem watcher that ``stackunderflow start`` spawns. When ``start``
is not running, the store reflects whatever the last watcher snapshot
wrote — which can be days stale.

This module gives those commands two opt-in paths to refresh the store
synchronously in the lifetime of the CLI process:

* **``--ingest``** — caller asks for it explicitly. Always runs.
* **``--auto-ingest`` / ``--no-auto-ingest``** — caller delegates the
  decision to a staleness check. ``ensure_fresh`` consults
  :func:`is_stale` and runs the same path only when the store's most
  recent event is older than :data:`STALENESS_THRESHOLD_HOURS` hours.

The refresh path is the same as :mod:`stackunderflow.server`'s startup
ingest:

  1. :func:`stackunderflow.ingest.run_ingest` over every registered
     adapter — walks each adapter's source files, calls ``ingest_file``
     for new bytes, and runs the per-record normalize hook so
     ``usage_events`` rows are materialised in the same transaction.
  2. :func:`stackunderflow.etl.backfill.backfill` with ``force=False``
     — an incremental pass that picks up any messages whose normalize
     hook silently no-op'd (rare: beta provider with no normalizer
     registered) and refreshes every mart watermark. Re-running over
     an already-converted store is a counted-skip-only operation
     (``INSERT OR IGNORE`` against ``uniq_events_msg``).

The function is **synchronous**: it blocks the calling CLI command for
the full duration of the pass (typically <1 s on a quiet store, longer
on a backlog). That's the deliberate trade — the user asked for fresh
data, so the next ``status`` line is worth waiting on. No background
threads, no FastAPI lifespan, no watchfiles spawning here.
"""

from __future__ import annotations

import logging
import sqlite3
from datetime import UTC, datetime, timedelta

import click

_log = logging.getLogger(__name__)

# Six hours matches the brief and is roughly the longest a casual user
# would tolerate stale numbers without prompting. Anything shorter
# turns into a hidden per-command performance tax on systems where the
# user prefers to keep the watcher running.
STALENESS_THRESHOLD_HOURS: float = 6.0


def is_stale(
    conn: sqlite3.Connection,
    *,
    threshold_hours: float = STALENESS_THRESHOLD_HOURS,
    now: datetime | None = None,
) -> bool:
    """Return True when the newest ``usage_events`` row is older than the threshold.

    Empty store → ``False`` (no events to threshold against; refusing
    to ingest unprompted keeps fresh-install dashboards from accidentally
    walking every adapter root on the first CLI call). The user can
    still force the pass with ``--ingest``.

    Unparseable timestamp → ``False`` (we'd rather under-trigger than
    thrash the store on a corrupt row).

    *now* is overridable for tests.
    """
    cur = conn.execute("SELECT MAX(ts) AS max_ts FROM usage_events")
    row = cur.fetchone()
    max_ts_raw = None
    if row is not None:
        # sqlite3.Row supports indexing by name; plain tuple by position.
        try:
            max_ts_raw = row["max_ts"]
        except (IndexError, TypeError):
            max_ts_raw = row[0] if row else None

    if not max_ts_raw:
        return False

    try:
        # usage_events.ts is canonical ISO 8601. fromisoformat handles
        # both naive and offset-aware strings on 3.11+.
        max_ts = datetime.fromisoformat(str(max_ts_raw))
    except ValueError:
        _log.debug("ingest helper: unparseable usage_events.ts=%r", max_ts_raw)
        return False

    now = now or datetime.now(UTC)
    # Compare in the same timezone awareness as max_ts to avoid a
    # naive/aware TypeError when one side has a tzinfo and the other
    # doesn't.
    if max_ts.tzinfo is None:
        now = now.replace(tzinfo=None)
    else:
        if now.tzinfo is None:
            now = now.replace(tzinfo=UTC)

    return (now - max_ts) > timedelta(hours=threshold_hours)


def ensure_fresh(
    conn: sqlite3.Connection,
    *,
    force: bool = False,
    auto: bool = True,
    notify: bool = True,
) -> bool:
    """Run an incremental ingest+backfill pass when the store is stale.

    Parameters
    ----------
    conn:
        Open store connection. The function commits its own work via
        the underlying ``run_ingest`` / ``backfill`` calls.
    force:
        When ``True``, always run the pass (corresponds to the
        ``--ingest`` flag). Skips the staleness check.
    auto:
        When ``True`` (the ``--auto-ingest`` default) **and** the store
        is stale, run the pass and print a one-line notice. When
        ``False`` (``--no-auto-ingest``), never run the pass on the
        staleness path; only ``force=True`` will trigger ingest.
    notify:
        When ``True`` and the pass runs because of the staleness check
        (not ``force``), print ``[stale data — ingesting...]`` to
        stderr so the user knows why ``status`` paused. ``force`` runs
        silently — the user explicitly asked for it.

    Returns
    -------
    bool
        ``True`` when the ingest+backfill pass actually ran. ``False``
        when it was skipped (fresh store + auto on, or auto off + not
        forced).
    """
    if not force and not auto:
        return False

    if not force and not is_stale(conn):
        return False

    if not force and notify:
        click.echo("[stale data — ingesting...]", err=True)

    _run_ingest_pass(conn)
    return True


def _run_ingest_pass(conn: sqlite3.Connection) -> None:
    """The actual incremental refresh — lazily imports to keep the CLI cold-start cheap.

    The two-step shape (``run_ingest`` then ``backfill(force=False)``)
    mirrors what the server's lifespan does at startup. ``run_ingest``
    handles the per-file watermark + transactional write +
    per-record normalize hook; ``backfill`` is the safety net for any
    messages whose normalize hook silently no-op'd plus the mart
    watermark refresh.
    """
    from stackunderflow.adapters import registered
    from stackunderflow.etl.backfill import backfill
    from stackunderflow.ingest import run_ingest

    try:
        run_ingest(conn, registered())
    except Exception as exc:  # noqa: BLE001 — surface, but don't crash the read command
        _log.warning("ingest helper: run_ingest raised: %s", exc)

    try:
        backfill(conn, force=False)
    except Exception as exc:  # noqa: BLE001 — same: backfill should never block a read
        _log.warning("ingest helper: backfill raised: %s", exc)
