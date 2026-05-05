"""Adapter Protocol + shared dataclasses."""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Protocol


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
