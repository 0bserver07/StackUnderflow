"""Adapter Protocol + shared dataclasses."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Protocol


@dataclass(frozen=True, slots=True)
class SessionRef:
    """Points at one parseable session on disk."""
    provider: str
    project_slug: str
    session_id: str
    file_path: Path
    file_mtime: float
    file_size: int


@dataclass(frozen=True, slots=True)
class Record:
    """One normalised message-level record. Same shape across providers."""
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


class SourceAdapter(Protocol):
    """What every source adapter must implement."""

    name: str

    def enumerate(self) -> Iterable[SessionRef]:
        """Yield every session this adapter can see on disk."""
        ...

    def read(self, ref: SessionRef, *, since_offset: int = 0) -> Iterable[Record]:
        """Yield records from `ref`, starting at `since_offset` bytes in."""
        ...
