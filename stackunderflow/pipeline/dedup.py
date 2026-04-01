"""Single-pass deduplication of raw log entries.

Claude Code emits streaming partial messages that share the same
``message.id``.  We merge those (keeping the longest content) and
drop cross-session duplicates via a fingerprint set.
"""

from __future__ import annotations

import hashlib
import logging

from .reader import RawEntry

_log = logging.getLogger(__name__)


def collapse(entries: list[RawEntry]) -> list[RawEntry]:
    """Merge streaming partials and remove duplicates in one pass."""

    # Phase 1 — merge streaming messages that share a message.id
    by_msg_id: dict[str, RawEntry] = {}
    no_id: list[RawEntry] = []

    for entry in entries:
        msg = entry.payload.get("message", {})
        mid = msg.get("id") if isinstance(msg, dict) else None

        if mid:
            if mid in by_msg_id:
                existing = by_msg_id[mid]
                by_msg_id[mid] = _pick_longer(existing, entry)
            else:
                by_msg_id[mid] = entry
        else:
            no_id.append(entry)

    merged = list(by_msg_id.values()) + no_id

    # Phase 2 — drop exact duplicates via content fingerprint
    seen: set[str] = set()
    unique: list[RawEntry] = []

    for entry in merged:
        fp = _fingerprint(entry)
        if fp in seen:
            continue
        seen.add(fp)
        unique.append(entry)

    dropped = len(entries) - len(unique)
    if dropped:
        _log.debug("Dedup removed %d entries (%d → %d)", dropped, len(entries), len(unique))

    return unique


# ── helpers ──────────────────────────────────────────────────────────────────

def _pick_longer(a: RawEntry, b: RawEntry) -> RawEntry:
    """Given two entries with the same message id, keep the one with more content."""
    len_a = _content_length(a)
    len_b = _content_length(b)
    return b if len_b > len_a else a


def _content_length(entry: RawEntry) -> int:
    msg = entry.payload.get("message", {})
    if not isinstance(msg, dict):
        return 0
    content = msg.get("content", "")
    if isinstance(content, str):
        return len(content)
    if isinstance(content, list):
        total = 0
        for block in content:
            if isinstance(block, dict):
                total += len(block.get("text", ""))
            elif isinstance(block, str):
                total += len(block)
        return total
    return 0


def _fingerprint(entry: RawEntry) -> str:
    """Quick fingerprint: timestamp + first 200 chars of content + uuid."""
    data = entry.payload
    ts = data.get("timestamp", "")
    uuid = data.get("uuid", "")
    text = _quick_text(data)[:200]
    raw = f"{ts}|{text}|{uuid}"
    return hashlib.md5(raw.encode(), usedforsecurity=False).hexdigest()


def _quick_text(data: dict) -> str:
    if "summary" in data and isinstance(data["summary"], str):
        return data["summary"]
    msg = data.get("message", {})
    if isinstance(msg, dict):
        c = msg.get("content", "")
        if isinstance(c, str):
            return c
        if isinstance(c, list):
            parts = []
            for b in c:
                if isinstance(b, dict):
                    parts.append(b.get("text", ""))
                elif isinstance(b, str):
                    parts.append(b)
            return " ".join(parts)
    return ""
