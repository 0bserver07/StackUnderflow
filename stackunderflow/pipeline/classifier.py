"""Classify raw log entries: message kind, error status, interruptions.

Error detection uses a two-tier system: a fast keyword pre-screen followed
by a detailed regex check.  This avoids running expensive regex on every
message while still catching all known error shapes.
"""

from __future__ import annotations

import re
from enum import Enum, auto
from typing import NamedTuple

from .reader import RawEntry

# ── interruption markers ─────────────────────────────────────────────────────

_CANCEL_MARKER = "[Request interrupted by user for tool use]"
_ABORT_SIGNAL = "API Error: Request was aborted."

# exported for use by aggregator
INTERRUPT_PREFIX = _CANCEL_MARKER
INTERRUPT_API = _ABORT_SIGNAL


# ── error taxonomy ───────────────────────────────────────────────────────────
# Two-tier detection: cheap keyword pre-screen → expensive regex confirm.

class _Tier(Enum):
    HALT = auto()        # user-initiated stops
    FILESYSTEM = auto()  # file state problems
    LOOKUP = auto()      # missing resources
    RUNTIME = auto()     # code execution failures
    PARSE = auto()       # syntax / validation
    MISC = auto()        # everything else


# (category_label, tier, fast_keyword_screen, confirming_regex)
_TAXONOMY: list[tuple[str, _Tier, str, re.Pattern]] = [
    # ── user-initiated halts ─────────────────────────────────────────
    ("User Interruption", _Tier.HALT, "want",
     re.compile(r"user doesn.t want to (?:proceed|take this action)", re.I)),
    ("User Interruption", _Tier.HALT, "interrupted",
     re.compile(r"\[Request interrupted", re.I)),
    ("Command Timeout", _Tier.HALT, "timed out",
     re.compile(r"command timed out", re.I)),

    # ── filesystem state ─────────────────────────────────────────────
    ("File Not Read", _Tier.FILESYSTEM, "not been read",
     re.compile(r"file has not been read yet", re.I)),
    ("File Modified", _Tier.FILESYSTEM, "modified since",
     re.compile(r"file has been modified since read", re.I)),
    ("File Too Large", _Tier.FILESYSTEM, "maximum allowed",
     re.compile(r"exceeds maximum allowed", re.I)),

    # ── resource lookup failures ─────────────────────────────────────
    ("Content Not Found", _Tier.LOOKUP, "not found",
     re.compile(r"string (?:to replace )?not found|no module named|no such file", re.I)),
    ("Content Not Found", _Tier.LOOKUP, "does not exist",
     re.compile(r"file does not exist", re.I)),
    ("Content Not Found", _Tier.LOOKUP, "enoent",
     re.compile(r"npm error enoent", re.I)),
    ("No Changes", _Tier.LOOKUP, "no changes",
     re.compile(r"no changes to make", re.I)),

    # ── permissions & access ─────────────────────────────────────────
    ("Permission Error", _Tier.RUNTIME, "permission denied",
     re.compile(r"permission denied", re.I)),
    ("Permission Error", _Tier.RUNTIME, "was blocked",
     re.compile(r"cd to.*was blocked|was blocked.*cd to", re.I)),

    # ── tooling problems ─────────────────────────────────────────────
    ("Tool Not Found", _Tier.RUNTIME, "command not found",
     re.compile(r"command not found", re.I)),
    ("Wrong Tool", _Tier.RUNTIME, "notebookread",
     re.compile(r"jupyter notebook.*notebookread", re.I)),

    # ── code execution ───────────────────────────────────────────────
    ("Code Runtime Error", _Tier.RUNTIME, "cannot find module",
     re.compile(r"cannot find module", re.I)),
    ("Code Runtime Error", _Tier.RUNTIME, "traceback",
     re.compile(r"traceback", re.I)),
    ("Port Binding Error", _Tier.RUNTIME, "bind on address",
     re.compile(r"attempting to bind on address", re.I)),

    # ── syntax / validation ──────────────────────────────────────────
    ("Syntax Error", _Tier.PARSE, "syntaxerror",
     re.compile(r"syntax\s*error", re.I)),
    ("Syntax Error", _Tier.PARSE, "replace_all is false",
     re.compile(r"replace_all is false", re.I)),
    ("Syntax Error", _Tier.PARSE, "null (null)",
     re.compile(r"null \(null\) has no keys", re.I)),
    ("Syntax Error", _Tier.PARSE, "jq: error",
     re.compile(r"jq: error|inputvalidationerror", re.I)),

    # ── notebook ─────────────────────────────────────────────────────
    ("Notebook Cell Not Found", _Tier.MISC, "not found in notebook",
     re.compile(r'cell with id "[0-9a-f]+" not found in notebook', re.I)),

    # ── catch-all ────────────────────────────────────────────────────
    ("Other Tool Errors", _Tier.MISC, "[details] error",
     re.compile(r"\[details\] error: error", re.I)),
]


class TaggedEntry(NamedTuple):
    """A raw entry annotated with classification metadata."""
    payload: dict
    session_id: str
    origin: str
    kind: str
    is_error: bool
    error_category: str | None
    is_interruption: bool


def tag(entries: list[RawEntry]) -> list[TaggedEntry]:
    return [_classify(e) for e in entries]


# also re-export for aggregator's error analysis
_ERROR_RULES = [(cat, pat) for cat, _, _, pat in _TAXONOMY]


# ── internals ────────────────────────────────────────────────────────────────

def _classify(entry: RawEntry) -> TaggedEntry:
    data = entry.payload
    kind = _determine_kind(data)
    text = _surface_text(data)

    is_interruption = text.startswith(_CANCEL_MARKER) or text.startswith(_ABORT_SIGNAL)

    is_error, error_cat = _detect_error(data, kind, text)

    return TaggedEntry(
        payload=data,
        session_id=entry.session_id,
        origin=entry.origin,
        kind=kind,
        is_error=is_error,
        error_category=error_cat,
        is_interruption=is_interruption,
    )


def _determine_kind(data: dict) -> str:
    raw = data.get("type", "")
    if raw == "human":
        return "user"
    if raw == "assistant":
        return "assistant"
    if raw in ("summary", "compact_summary"):
        return raw
    if raw in ("task_start", "task"):
        return "task"
    msg = data.get("message")
    if isinstance(msg, dict):
        role = msg.get("role", "")
        if role in ("user", "assistant"):
            return role
    return "assistant"


def _detect_error(data: dict, kind: str, text: str) -> tuple[bool, str | None]:
    """Return (is_error, category) by inspecting tool_result blocks and text."""
    msg = data.get("message")
    if isinstance(msg, dict):
        content = msg.get("content")
        if isinstance(content, list):
            for block in content:
                if not isinstance(block, dict):
                    continue
                if block.get("type") != "tool_result" or not block.get("is_error"):
                    continue
                err_body = block.get("content", "")
                if isinstance(err_body, list):
                    err_body = " ".join(
                        b.get("text", "") for b in err_body if isinstance(b, dict)
                    )
                return True, _categorise(str(err_body))

    if kind == "assistant" and text.strip() == _ABORT_SIGNAL:
        return True, "User Interruption"

    return False, None


def _categorise(text: str) -> str:
    """Two-tier match: fast lowercase keyword screen, then regex confirm."""
    lowered = text.lower()
    for label, _, keyword, regex in _TAXONOMY:
        if keyword in lowered and regex.search(text):
            return label
    return "Other"


def _surface_text(data: dict) -> str:
    """Extract a quick text representation for interruption / error checks."""
    if isinstance(data.get("summary"), str):
        return data["summary"]
    msg = data.get("message")
    if not isinstance(msg, dict):
        return ""
    body = msg.get("content", "")
    if isinstance(body, str):
        return body
    if isinstance(body, list):
        pieces: list[str] = []
        for blk in body:
            if isinstance(blk, str):
                pieces.append(blk)
            elif isinstance(blk, dict) and blk.get("type") == "text":
                pieces.append(blk.get("text", ""))
        return "\n".join(pieces)
    return ""
