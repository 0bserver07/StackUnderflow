"""MartBuilder ABC — incremental + full-rebuild contract for marts.

See ``docs/specs/etl-architecture.md`` §"MartBuilder ABC". Wave 2B ships
the five default mart builders (daily, session, project, provider_day,
model_day). The ABC defines two methods:

* :meth:`refresh` — incremental, watermarked, idempotent
* :meth:`rebuild_from_scratch` — drop + full backfill, idempotent
"""

from __future__ import annotations

import sqlite3
from abc import ABC, abstractmethod


class MartBuilder(ABC):
    """Per-mart transform: ``usage_events`` rows → mart rows.

    Subclasses set ``name`` (the registry key) and implement both
    :meth:`refresh` and :meth:`rebuild_from_scratch`. Each mart owns
    its rebuild SQL; no mart depends on another, and each mart maintains
    an independent ``mart_watermark.last_event_id`` so partial failures
    self-heal.
    """

    name: str  # "daily" | "session" | "project" | "provider_day" | "model_day"

    @abstractmethod
    def refresh(self, conn: sqlite3.Connection, since_event_id: int) -> int:
        """Upsert mart rows for ``usage_events`` with ``id > since_event_id``.

        Returns the highest ``event_id`` consumed. Caller persists this as
        the new watermark via
        :func:`stackunderflow.etl.watermark.set_watermark`.

        **Idempotent**: re-running with the same *since_event_id* is a
        no-op for already-built rows. Implementations use
        ``INSERT ... ON CONFLICT DO UPDATE`` (additive marts) or
        ``INSERT OR REPLACE`` over a recomputed aggregate
        (per-entity marts) so re-runs after a partial failure self-heal.

        Returning the same value as *since_event_id* (or 0) means there
        was nothing new to process — the watermark stays put.
        """

    @abstractmethod
    def rebuild_from_scratch(self, conn: sqlite3.Connection) -> None:
        """Drop + repopulate this mart from scratch.

        Implemented as ``DELETE FROM <mart>; refresh(conn, since_event_id=0)``
        so it's idempotent and produces the same final state as a clean
        incremental run. Used by the ``--rebuild`` CLI path.
        """
