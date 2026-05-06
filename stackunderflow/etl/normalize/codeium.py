"""Codeium normalizer — discovery-only stub.

Codeium's adapter (when enabled via the beta flag) only enumerates
sessions; the on-disk format used by the Codeium client is not parsed
into individual messages, so no billable rows ever land in the
``messages`` table for this provider. The normalizer therefore yields
nothing — it exists solely so the registry has an entry for every
provider in the codeburn catalog and the lookup at the ingest seam
never KeyErrors when Codeium is enabled.

If a future spec adds a parsed message format we'll implement the
transform here; until then this is intentionally a no-op.
"""

from __future__ import annotations

from collections.abc import Iterable

from .base import Normalizer


class CodeiumNormalizer(Normalizer):
    provider_name = "codeium"

    def normalize(self, msg_row: dict) -> Iterable[dict]:  # noqa: ARG002
        # Discovery-only — never any billable rows. Returning early as
        # a generator function gives an empty iterator without raising.
        return
        yield  # pragma: no cover — unreachable, makes this a generator
