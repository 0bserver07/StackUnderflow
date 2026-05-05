"""Normalizer ABC — per-provider transform from ``messages`` rows to
``usage_events`` rows.

See ``docs/specs/etl-architecture.md`` §"Normalizer ABC". Wave 2 ships
the four default subclasses (claude, codex, cursor, cline). Wave 1 only
defines the contract.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from collections.abc import Iterable


class Normalizer(ABC):
    """Per-provider transform: ``messages`` row → ``usage_events`` row(s).

    Subclasses set ``provider_name`` (the registry key) and implement
    :meth:`normalize`. Most providers yield 0 or 1 events per messages
    row (assistant turns with usage); some (cline tasks) may yield N.

    Provider-specific quirks resolved here ONLY:

    * codex: subtract cached from input, fold reasoning into output
    * cursor: estimate tokens from text length when zero, mark
      ``cost_source='estimated'``
    * cline: per-task → per-event split keyed by ``api_req_started``

    Cost is computed during normalization and stored on the row, so the
    mart layer never re-applies a rate card.
    """

    provider_name: str  # "claude" | "codex" | "cursor" | ...

    @abstractmethod
    def normalize(self, msg_row: dict) -> Iterable[dict]:
        """Convert one ``messages`` row into 0..N ``usage_events`` rows.

        ``msg_row`` is a ``sqlite3.Row``-style mapping with the columns
        documented in ``v001_initial.sql`` (plus ``speed`` from v003).
        Yielded dicts must match the ``usage_events`` column shape from
        ``v006_etl_layer.sql`` — at minimum:

        * ``source_message_fk`` (int, the messages.id)
        * ``provider`` (str, == ``self.provider_name``)
        * ``project_id`` (int)
        * ``session_id`` (str)
        * ``ts`` (ISO8601 UTC)
        * ``day`` (YYYY-MM-DD, derived from ts)
        * ``role`` (user | assistant | tool | system)

        Optional keys default per the schema's DEFAULT clauses.
        """
