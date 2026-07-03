"""Adapter Protocol + shared dataclasses."""

from __future__ import annotations

import hashlib
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Protocol


def content_hash_id(*parts: object, prefix: str = "", length: int = 32) -> str:
    """Derive a deterministic id from *parts* by hashing their content.

    Two imports of identical content — on the same machine or a different
    one — produce the same id. That is what a content-addressed import
    needs: the store's integer primary keys are machine-local and cannot
    be merged across machines, but a stable content hash can, and a
    re-import of the same record maps back onto the same id instead of
    duplicating it.

    The digest is order- and boundary-sensitive. Each part is
    length-prefixed before it is folded in, so ``("a", "bc")`` and
    ``("ab", "c")`` never collide, and ``None`` hashes distinctly from the
    empty string. The part count is bound in first so a trailing ``None``
    cannot alias a shorter argument list. Non-``str`` parts are stringified
    with ``str()`` — callers pass already-canonical scalars (ints, a
    normalised ISO timestamp, the source id) so the mapping stays stable
    across Python versions and machines.

    ``prefix`` (e.g. a provider/source tag) is prepended verbatim to the
    returned id so ids minted in different namespaces stay visibly
    distinct. ``length`` truncates the hex digest — the default 32 hex
    chars is 128 bits, ample headroom against accidental collision at any
    realistic import volume.

    This helper is **additive**: nothing in the existing adapters or the
    ingest writer calls it, and it does not change any row id. New,
    content-addressed import paths (the ``custom`` history-source reader)
    opt in explicitly.
    """
    h = hashlib.blake2b(digest_size=32)
    # Bind the arity up front: a trailing None vs. a missing part must hash
    # differently.
    h.update(str(len(parts)).encode("ascii"))
    h.update(b"\x1e")
    for part in parts:
        token = b"\x00NULL\x00" if part is None else str(part).encode("utf-8")
        # Length-prefix each token so adjacent tokens can't be re-partitioned
        # into the same byte stream.
        h.update(str(len(token)).encode("ascii"))
        h.update(b"\x1f")
        h.update(token)
    digest = h.hexdigest()[: max(1, length)]
    return f"{prefix}{digest}" if prefix else digest


@dataclass(frozen=True, slots=True)
class SessionRef:
    """Points at one parseable session on disk.

    The two ``source_*`` fields let one adapter contract handle JSONL files,
    SQLite tables, and vscdb keys uniformly. JSONL adapters leave them at the
    defaults — see ``docs/specs/multi-provider/spec.md`` §1.1.
    """
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int
    # Storage mode for resumable reads: byte-offset for "file", rowid for
    # "database". Unknown adapters default to "file" so existing JSONL
    # adapters need no changes.
    source_kind: Literal["file", "database"] = "file"
    # Adapter-private metadata (table name, vscdb key prefix, conversation
    # id, etc.). Not interpreted outside the adapter that produced it.
    source_hint: dict[str, Any] | None = field(default=None)


@dataclass(frozen=True, slots=True)
class Record:
    """One normalised message-level record. Same shape across providers.

    ``speed`` flags Anthropic's priority/fast tier for Opus models (which
    bills at ~6× standard rates). Detected per-message from
    ``message.usage.service_tier`` on Claude JSONL records — see
    ``ClaudeAdapter._parse_line``. Defaults to ``"standard"`` for every
    other adapter; only the Anthropic pricer interprets the field today.
    """
    provider: str
    session_id: str
    seq: int
    timestamp: str
    role: str
    model: str | None
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    content_text: str
    tools: tuple[str, ...]
    cwd: str | None
    is_sidechain: bool
    uuid: str
    parent_uuid: str | None
    raw: dict
    speed: Literal["standard", "fast"] = "standard"


class SourceAdapter(Protocol):
    """What every source adapter must implement."""

    name: str

    def enumerate(self) -> Iterable[SessionRef]:
        """Yield every session this adapter can see on disk."""
        ...

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        """Yield records from `ref`, starting at `since_offset` bytes in."""
        ...

    def watch_paths(self) -> list[Path]:
        """Return root paths the Wave 2C ETL watcher should follow.

        Default contract (Wave 2C, ``stackunderflow/etl/watcher.py``):
        return a list of canonical roots whose changes should trigger
        an incremental re-ingest. JSONL adapters return their parent
        directory; vscdb-style adapters return the SQLite file itself
        (``watchfiles`` fires on byte-level change either way).

        Returning ``[]`` (or omitting the method entirely — the watcher
        defaults missing methods to ``[]``) means "don't watch this
        provider; fall back to periodic ingest." This is the chosen
        path for the dozen beta adapters that haven't been validated
        for live-watching yet.
        """
        ...
