"""Typed row dataclasses returned by store.queries helpers.

Route handlers and CLI reports consume these; they never see sqlite3.Row.
Keeping the shape explicit makes downstream code self-documenting and
lets IDE/type-checker catch column typos.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class ProjectRow:
    id: int
    provider: str
    slug: str
    path: str | None
    display_name: str
    first_seen: float
    last_modified: float


@dataclass(frozen=True, slots=True)
class SessionRow:
    id: int
    project_id: int
    session_id: str
    first_ts: str | None
    last_ts: str | None
    message_count: int


@dataclass(frozen=True, slots=True)
class MessageRow:
    id: int
    session_fk: int
    seq: int
    timestamp: str
    role: str
    model: str | None
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    content_text: str
    tools_json: str
    raw_json: str
    is_sidechain: bool
    uuid: str | None
    parent_uuid: str | None
    # Anthropic priority/fast tier flag persisted by migration v003.
    # ``"standard"`` for everything except Claude records whose
    # ``message.usage.service_tier == "priority"``. Defaulted so older
    # callers and test fixtures that omit the field stay valid.
    speed: str = "standard"


@dataclass(frozen=True, slots=True)
class DayTotals:
    date: str
    input_tokens: int
    output_tokens: int
    cache_create_tokens: int
    cache_read_tokens: int
    message_count: int
