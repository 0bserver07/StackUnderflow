"""Codeium IDE plugin adapter — discovery-only stub.

The Codeium plugin stores its chat state under ``~/.codeium/`` as
**protobuf-encoded binary blobs** alongside JSON config files
(``local-inventory.md`` §8). On the user's machine the directory is
449 MB, but the most recent activity is January 2025 — well before
StackUnderflow began tracking conversation data — and the wire format
has no published schema.

Decision: ship as a discovery-only stub.

Reasons:
  - Decoding protobuf without an official ``.proto`` file (or a stable
    reverse-engineered parser) is high-risk and cost-prohibitive for the
    first release.
  - The chat state files might require a Codeium-specific message schema
    we don't have access to.
  - The data on this machine is stale (Jan 2025), so even a working
    parser would surface no recent activity.

Reactivate this adapter when an official schema or stable
reverse-engineered parser is available. Until then ``enumerate()`` and
``read()`` return nothing — the adapter registers like every other,
but it produces no records and never raises.

Spec: ``docs/specs/multi-provider/local-inventory.md`` §8.
"""

from __future__ import annotations

from collections.abc import Iterator
from pathlib import Path

from .base import Record, SessionRef

# Discovery path. Kept here for documentation and so future implementers
# can grep for it; ``enumerate()`` does not currently walk this tree.
_CODEIUM_ROOT = Path.home() / ".codeium"


class CodeiumAdapter:
    """Stub adapter — no records yielded.

    Registered by default like every adapter, but ``enumerate()``
    yields nothing and ``read()`` is a no-op. See the module docstring
    for the rationale.
    """

    name = "codeium"

    def __init__(self, root: Path | None = None) -> None:
        self._root = root or _CODEIUM_ROOT

    def source_roots(self) -> list[Path]:
        """Roots ``backup create`` copies — the same data
        ``enumerate()`` reads. Self-declared here (like ``name``),
        never listed centrally.
        """
        return [self._root]

    def enumerate(self) -> Iterator[SessionRef]:
        """Discovery is not implemented — yield nothing.

        Once a stable parser exists this method will walk
        ``~/.codeium/`` for chat-state protobuf blobs and produce one
        ``SessionRef`` per conversation. Today it returns immediately so
        the adapter is registered-but-inert.
        """
        return
        yield  # pragma: no cover — make this a generator

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterator[Record]:
        """No-op. Yields nothing."""
        return
        yield  # pragma: no cover — make this a generator
