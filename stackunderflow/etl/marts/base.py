"""MartBuilder ABC — incremental + full-rebuild contract for marts.

See ``docs/specs/etl-architecture.md`` §"MartBuilder ABC". Wave 2 ships
the five default mart builders (daily, session, project, provider_day,
model_day). Wave 1 only defines the contract.
"""

from __future__ import annotations

import sqlite3
from abc import ABC, abstractmethod


class MartBuilder(ABC):
    """Per-mart transform: ``usage_events`` rows → mart rows.

    Subclasses set ``name`` (the registry key) and implement
    :meth:`refresh`. Each mart builder owns its rebuild SQL; no mart
    depends on another, and each mart maintains an independent
    ``mart_watermark.last_event_id`` so partial failures self-heal.
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
        ``INSERT ... ON CONFLICT DO UPDATE`` so a re-run after a partial
        failure self-heals.

        Returning the same value as *since_event_id* (or 0) means there
        was nothing new to process — the watermark stays put.
        """

    def rebuild_from_scratch(  # noqa: B027 — concrete no-op default by design
        self, conn: sqlite3.Connection
    ) -> None:
        """Drop + re-create + full backfill of this mart.

        Default implementation is a no-op so subclasses can override only
        when their incremental refresh has a structural change. Used by
        the ``--force`` / ``--rebuild`` paths in :mod:`backfill`. Must be
        idempotent.

        Concrete (not abstract) so subclasses with a perfectly-incremental
        ``refresh`` don't need to write a stub. The ``backfill(force=True)``
        path uses ``DELETE FROM <mart>`` directly, so most builders never
        need to override this.
        """
