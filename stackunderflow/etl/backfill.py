"""Backfill orchestrator — one-shot conversion + mart rebuild.

Walks every existing ``messages`` row through the registered normalizers
to materialize ``usage_events`` rows, then refreshes every registered
mart from those events.

Wave 1 ships only the orchestrator skeleton — the registries are empty
until Wave 2 lands, so calling :func:`backfill` on a Wave 1 install is a
no-op (returns zero counts, empty marts dict). The method contract is
locked so Wave 2 can dispatch in parallel.

See ``docs/specs/etl-architecture.md``.
"""

from __future__ import annotations

import logging
import sqlite3
import time
from dataclasses import dataclass, field

from .normalize import all as _all_normalizers
from .watermark import refresh_all_marts

_log = logging.getLogger(__name__)


@dataclass
class BackfillReport:
    """Summary of one ``backfill()`` call.

    Returned to the caller (CLI, API, watcher) so they can render
    progress, log timing, or drive further work. ``marts_refreshed``
    is a copy of the dict returned by
    :func:`stackunderflow.etl.watermark.refresh_all_marts` — empty
    when no mart builders are registered (Wave 1 default).
    """

    events_inserted: int = 0
    events_skipped_duplicate: int = 0
    marts_refreshed: dict[str, int] = field(default_factory=dict)
    duration_seconds: float = 0.0


def _drop_events_and_marts(conn: sqlite3.Connection) -> None:
    """``force=True`` path: empty every fact + mart table, reset watermarks.

    Schema stays intact (``DELETE``, not ``DROP``). The next call to
    :func:`backfill` rebuilds everything from raw ``messages``.
    """
    conn.execute("DELETE FROM usage_events")
    conn.execute("DELETE FROM daily_mart")
    conn.execute("DELETE FROM session_mart")
    conn.execute("DELETE FROM project_mart")
    conn.execute("DELETE FROM provider_day_mart")
    conn.execute("DELETE FROM model_day_mart")
    conn.execute("DELETE FROM mart_watermark")


def backfill(
    conn: sqlite3.Connection,
    *,
    force: bool = False,
) -> BackfillReport:
    """One-shot: convert all existing ``messages`` into ``usage_events``,
    then refresh every mart from the new watermark.

    ``force=True`` empties events + marts + watermarks and rebuilds from
    scratch. Default is incremental — already-converted messages are
    skipped via the ``UNIQUE(source_message_fk)`` index.

    Wave 1 behaviour
    ----------------
    Both registries are empty in Wave 1, so:

    * No normalizer runs → ``events_inserted = 0``,
      ``events_skipped_duplicate = 0``
    * :func:`refresh_all_marts` returns ``{}`` → ``marts_refreshed = {}``

    Wave 2 fills both in. The orchestrator shape stays put.
    """
    start = time.perf_counter()
    report = BackfillReport()

    if force:
        _drop_events_and_marts(conn)

    normalizers = _all_normalizers()
    if not normalizers:
        # Wave 1: nothing to convert, nothing to refresh that depends on
        # new events. Still call refresh_all_marts so empty marts can
        # finalize their watermarks (no-op when their registry is also
        # empty, so this is the canonical Wave 1 fall-through).
        report.marts_refreshed = refresh_all_marts(conn)
        report.duration_seconds = time.perf_counter() - start
        return report

    # Wave 2 fills this in. The shape is locked: each normalizer is
    # applied to its provider's ``messages`` rows, yielded events are
    # inserted with ``INSERT OR IGNORE`` (so the UNIQUE source_message_fk
    # index turns duplicates into a counted skip).
    inserted, skipped = _run_normalizers(conn, normalizers)
    report.events_inserted = inserted
    report.events_skipped_duplicate = skipped

    report.marts_refreshed = refresh_all_marts(conn)
    report.duration_seconds = time.perf_counter() - start
    return report


def _run_normalizers(
    conn: sqlite3.Connection,
    normalizers: dict,
) -> tuple[int, int]:
    """Run every registered normalizer over its provider's messages.

    Returns ``(inserted, skipped_duplicate)``. The implementation here is
    a placeholder skeleton that Wave 2 fills in; we keep it threaded so
    the orchestrator's report shape is exercised even before real
    normalizers register.

    Skeleton contract (Wave 2 will replace, not extend):

    1. For each ``(provider, NormalizerCls)`` pair:

       * Open a cursor selecting ``messages`` rows for that provider's
         projects (joined via ``sessions`` → ``projects``).
       * For each row, call ``NormalizerCls().normalize(row)`` and
         ``INSERT OR IGNORE`` each yielded event.
       * Count inserts vs ignored duplicates (via ``conn.changes``).

    2. Sum the per-provider counters, return.
    """
    # Wave 1: registries are populated only in tests with a stub builder
    # that has no real messages to walk. We log the intent and return
    # zeros so the rest of the orchestrator still exercises the
    # ``marts_refreshed`` path.
    _log.debug(
        "backfill: %d normalizer(s) registered, Wave-2 implementation "
        "pending — skipping event insertion",
        len(normalizers),
    )
    return 0, 0
