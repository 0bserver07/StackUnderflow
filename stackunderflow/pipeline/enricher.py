"""Transform classified log entries into structured domain objects.

Produces an ``EnrichedDataset`` containing typed ``Record`` objects,
``Interaction`` chains (user→assistant→tool), and session metadata.
The extraction, grouping, and reconciliation steps are implemented as
methods on a builder that accumulates state progressively.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field
from typing import Any

from .classifier import TaggedEntry

# ── domain types ─────────────────────────────────────────────────────────────

@dataclass(slots=True)
class Record:
    """One fully-parsed log entry."""
    session_id: str
    kind: str
    timestamp: str
    model: str
    content: str
    tokens: dict[str, int]
    tools: list[dict[str, Any]]
    is_error: bool
    error_category: str | None
    is_interruption: bool
    has_tool_result: bool
    uuid: str
    parent_uuid: str | None
    is_sidechain: bool
    message_id: str
    cwd: str
    raw_data: dict


@dataclass(slots=True)
class Interaction:
    """A user prompt and everything that followed until the next prompt."""
    interaction_id: str
    command: Record
    responses: list[Record] = field(default_factory=list)
    tool_results: list[Record] = field(default_factory=list)
    session_id: str = ""
    start_time: str = ""
    end_time: str = ""
    model: str = "N/A"
    tool_count: int = 0
    assistant_steps: int = 0
    is_continuation: bool = False
    tools_used: list[dict] = field(default_factory=list)
    has_task_tool: bool = False


@dataclass(slots=True)
class SessionMeta:
    session_id: str
    start_time: str = ""
    end_time: str = ""
    message_count: int = 0


@dataclass
class EnrichedDataset:
    records: list[Record]
    interactions: list[Interaction]
    sessions: dict[str, SessionMeta]


# ── public entry ─────────────────────────────────────────────────────────────

def build(tagged: list[TaggedEntry], log_dir: str) -> EnrichedDataset:
    b = _Builder(tagged)
    b.extract_records()
    b.group_interactions()
    b.deduplicate_interactions()
    b.finalise_tools()
    b.scan_sessions()
    return EnrichedDataset(
        records=b.records,
        interactions=b.interactions,
        sessions=b.sessions,
    )


# ── builder ──────────────────────────────────────────────────────────────────

class _Builder:
    __slots__ = ("_tagged", "records", "interactions", "sessions")

    def __init__(self, tagged: list[TaggedEntry]) -> None:
        self._tagged = tagged
        self.records: list[Record] = []
        self.interactions: list[Interaction] = []
        self.sessions: dict[str, SessionMeta] = {}

    # step 1
    def extract_records(self) -> None:
        for te in self._tagged:
            self.records.append(_parse_entry(te))

    # step 2
    def group_interactions(self) -> None:
        by_time = sorted(self.records, key=lambda r: r.timestamp or "")
        active: Interaction | None = None

        for rec in by_time:
            if rec.kind in ("summary", "compact_summary", "task"):
                continue

            is_user_command = rec.kind == "user" and not rec.has_tool_result

            if is_user_command:
                if active is not None:
                    self.interactions.append(active)
                active = Interaction(
                    interaction_id=_make_id(rec),
                    command=rec,
                    session_id=rec.session_id,
                    start_time=rec.timestamp,
                    end_time=rec.timestamp,
                )
                continue

            if active is None:
                continue

            if rec.kind == "assistant":
                active.responses.append(rec)
                if rec.model and rec.model != "N/A":
                    active.model = rec.model
                active.tools_used.extend(rec.tools)
                if rec.timestamp and rec.timestamp > active.end_time:
                    active.end_time = rec.timestamp
            elif rec.has_tool_result:
                active.tool_results.append(rec)

        if active is not None:
            self.interactions.append(active)

    # step 3
    def deduplicate_interactions(self) -> None:
        best: dict[str, Interaction] = {}
        for ix in self.interactions:
            prev = best.get(ix.interaction_id)
            if prev is None:
                best[ix.interaction_id] = ix
                continue
            winner, loser = (ix, prev) if len(ix.responses) > len(prev.responses) else (prev, ix)
            _absorb_tools(winner, loser)
            best[ix.interaction_id] = winner
        self.interactions = list(best.values())

    # step 4
    def finalise_tools(self) -> None:
        for ix in self.interactions:
            seen: set[str] = set()
            deduped: list[dict] = []
            for t in ix.tools_used:
                tid = t.get("id", "")
                if tid:
                    if tid in seen:
                        continue
                    seen.add(tid)
                deduped.append(t)
            ix.tools_used = deduped
            ix.tool_count = len(deduped)
            ix.assistant_steps = len(ix.responses)
            ix.has_task_tool = any(t.get("name") == "Task" for t in deduped)

    # step 5
    def scan_sessions(self) -> None:
        for rec in self.records:
            sm = self.sessions.get(rec.session_id)
            if sm is None:
                sm = SessionMeta(session_id=rec.session_id)
                self.sessions[rec.session_id] = sm
            sm.message_count += 1
            ts = rec.timestamp
            if ts:
                if not sm.start_time or ts < sm.start_time:
                    sm.start_time = ts
                if not sm.end_time or ts > sm.end_time:
                    sm.end_time = ts


# ── record parsing ───────────────────────────────────────────────────────────

def _parse_entry(te: TaggedEntry) -> Record:
    raw = te.payload
    msg = raw.get("message") if isinstance(raw.get("message"), dict) else {}

    return Record(
        session_id=te.session_id,
        kind=te.kind,
        timestamp=raw.get("timestamp", ""),
        model=msg.get("model", "N/A") if msg else "N/A",
        content=_text_from(raw),
        tokens=_usage_from(msg),
        tools=_tools_from(msg),
        is_error=te.is_error,
        error_category=te.error_category,
        is_interruption=te.is_interruption,
        has_tool_result=_has_result_block(msg),
        uuid=raw.get("uuid", ""),
        parent_uuid=raw.get("parentUuid"),
        is_sidechain=raw.get("isSidechain", False),
        message_id=msg.get("id", "") if msg else "",
        cwd=raw.get("cwd", ""),
        raw_data=raw,
    )


# ── field extraction (from Claude JSONL structure) ───────────────────────────

# Token field mapping: Claude API key → our normalised key
_TOKEN_FIELDS = {
    "input_tokens": "input",
    "output_tokens": "output",
    "cache_creation_input_tokens": "cache_creation",
    "cache_read_input_tokens": "cache_read",
}
_EMPTY_USAGE = {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}


def _text_from(raw: dict) -> str:
    """Extract readable text from a JSONL entry."""
    # summary entries have a top-level 'summary' string
    summary = raw.get("summary")
    if isinstance(summary, str):
        return summary

    msg = raw.get("message")
    if not isinstance(msg, dict):
        return ""

    body = msg.get("content", "")
    if isinstance(body, str):
        return body
    if not isinstance(body, list):
        return ""

    return "\n".join(_flatten_content_blocks(body))


def _flatten_content_blocks(blocks: list) -> list[str]:
    """Recursively extract text from Claude's nested content block structure."""
    out: list[str] = []
    for blk in blocks:
        if isinstance(blk, str):
            out.append(blk)
            continue
        if not isinstance(blk, dict):
            continue
        bt = blk.get("type", "")
        if bt == "text":
            out.append(blk.get("text", ""))
        elif bt == "tool_use":
            out.append(f"[Tool: {blk.get('name', '?')}]")
        elif bt == "tool_result":
            inner = blk.get("content", "")
            if isinstance(inner, str):
                out.append(inner)
            elif isinstance(inner, list):
                out.extend(_flatten_content_blocks(inner))
    return out


def _usage_from(msg: dict) -> dict[str, int]:
    """Pull token counts from the ``usage`` sub-object."""
    if not isinstance(msg, dict):
        return dict(_EMPTY_USAGE)
    usage = msg.get("usage")
    if not isinstance(usage, dict):
        return dict(_EMPTY_USAGE)
    return {our_key: usage.get(api_key, 0) or 0 for api_key, our_key in _TOKEN_FIELDS.items()}


def _tools_from(msg: dict) -> list[dict[str, Any]]:
    """Extract tool-use blocks from message content."""
    if not isinstance(msg, dict):
        return []
    body = msg.get("content")
    if not isinstance(body, list):
        return []
    return [
        {"name": blk.get("name", "Unknown"), "id": blk.get("id", ""), "input": blk.get("input", {})}
        for blk in body
        if isinstance(blk, dict) and blk.get("type") == "tool_use"
    ]


def _has_result_block(msg: dict) -> bool:
    if not isinstance(msg, dict):
        return False
    body = msg.get("content")
    if not isinstance(body, list):
        return False
    return any(isinstance(b, dict) and b.get("type") == "tool_result" for b in body)


# ── helpers ──────────────────────────────────────────────────────────────────

def _make_id(rec: Record) -> str:
    material = f"{rec.timestamp}|{rec.content[:64]}"
    return hashlib.sha256(material.encode(), usedforsecurity=False).hexdigest()[:16]


def _absorb_tools(winner: Interaction, loser: Interaction) -> None:
    existing = {t.get("id") for t in winner.tools_used if t.get("id")}
    for t in loser.tools_used:
        if t.get("id") and t["id"] not in existing:
            winner.tools_used.append(t)
            existing.add(t["id"])
