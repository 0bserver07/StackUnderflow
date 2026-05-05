"""Cline (and Cline-family) normalizer.

Cline writes one ``ui_messages.json`` per task containing a stream of
events; the adapter (``stackunderflow/adapters/cline.py``) splits that
stream into one ``Record`` per ``api_req_started`` event. Each Record
lands as a single row in the ``messages`` table — i.e. the per-event
grain is preserved at the storage layer, not aggregated to the task.

The spec ("Cline persists per-task, not per-message") refers to the
on-disk source-of-truth, not the table grain we ingest into. Per
``docs/specs/etl-architecture.md`` Wave 2A:

    The adapter already emits per-``api_req_started`` events; preserve
    that grain. Tokens from the ``api_req_started.text`` JSON.

So one msg_row → one event. A task with 3 ``api_req_started`` events
produces 3 messages-table rows, which normalize into 3 usage_events
rows — one per call.

Token shape — ``api_req_started.text`` is a JSON-stringified blob
``{tokensIn, tokensOut, cacheWrites, cacheReads, cost}``. We parse
that here so the normalizer is the authoritative source even when an
upgrade path leaves stale column values on the row. The pricer-side
``cost`` field (which Cline pre-computes against its own rate table)
is preserved in ``raw_extras`` for cross-reference but **not** used —
we recompute via ``compute_cost`` for parity with the rest of the
pipeline.

Cline runs against the Anthropic API directly (the user's own key), so
the canonical 4-token shape applies as-is — ``cacheWrites`` →
``cache_create``, ``cacheReads`` → ``cache_read``.

Model resolution — Cline declares the model once on the first user
message via ``<model>...</model>``. The adapter lifts that onto every
``Record.model``, so the messages-table row already carries it. We
trust the column.
"""

from __future__ import annotations

import json
from collections.abc import Iterable

from stackunderflow.infra.costs import RATE_CARD

from .base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
    Normalizer,
)

# Default model — keep aligned with the adapter's _DEFAULT_MODEL.
_DEFAULT_MODEL = "cline-auto"

# Cline embeds the upstream-computed cost on every api_req_started
# event; we preserve it for debugging but do not consume it.
_RAW_EXTRAS_FIELDS = ("cost", "request", "apiProtocol")


class ClineNormalizer(Normalizer):
    provider_name = "cline"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        tokens = _parse_api_req_tokens(msg_row)
        if tokens is None:
            return

        if (
            tokens["input"] == 0
            and tokens["output"] == 0
            and tokens["cache_read"] == 0
            and tokens["cache_create"] == 0
        ):
            return

        model = str(msg_row.get("model") or "") or _DEFAULT_MODEL

        cost_source = (
            COST_SOURCE_RATE_CARD
            if model in RATE_CARD
            else COST_SOURCE_UNKNOWN
        )

        raw_extras = _extras_from_raw_json(msg_row.get("raw_json"))

        yield self._build_event(
            msg_row,
            input_tokens=tokens["input"],
            output_tokens=tokens["output"],
            cache_read_tokens=tokens["cache_read"],
            cache_create_tokens=tokens["cache_create"],
            cost_source=cost_source,
            model=model,
            raw_extras=raw_extras,
        )


# ── helpers ─────────────────────────────────────────────────────────


def _parse_api_req_tokens(msg_row: dict) -> dict[str, int] | None:
    """Return canonical 4-key tokens parsed from the api_req_started event.

    Resolution order:
      1. The event's ``text`` field (a JSON-stringified blob) reachable
         via ``raw_json.text``. This is the on-disk source of truth.
      2. Direct ``text`` field on msg_row (synthetic test fixtures
         can pass it without an enclosing raw_json).
      3. Pre-canonicalised messages-table columns the adapter wrote.

    Returns None when the row carries no usage data at all (e.g. an
    api_req_started event whose text payload was malformed). User
    feedback / text events are filtered upstream by the role check
    and never reach this helper.
    """
    text_field = _extract_text_field(msg_row)
    if text_field is not None:
        parsed = _safe_parse_json(text_field)
        if isinstance(parsed, dict):
            return _canonicalize(parsed)

    # Fall back to the columns the adapter already wrote.
    return {
        "input": int(msg_row.get("input_tokens") or 0),
        "output": int(msg_row.get("output_tokens") or 0),
        "cache_read": int(msg_row.get("cache_read_tokens") or 0),
        "cache_create": int(msg_row.get("cache_create_tokens") or 0),
    }


def _extract_text_field(msg_row: dict) -> str | None:
    """Return the ``api_req_started.text`` JSON string, or None."""
    direct = msg_row.get("text")
    if isinstance(direct, str) and direct:
        return direct
    payload = _safe_load_raw(msg_row.get("raw_json"))
    if isinstance(payload, dict):
        text = payload.get("text")
        if isinstance(text, str) and text:
            return text
    return None


def _canonicalize(parsed: dict) -> dict[str, int]:
    """Map Cline's tokens dict to the canonical 4-key shape."""
    return {
        "input": _safe_int(parsed.get("tokensIn")),
        "output": _safe_int(parsed.get("tokensOut")),
        "cache_read": _safe_int(parsed.get("cacheReads")),
        "cache_create": _safe_int(parsed.get("cacheWrites")),
    }


def _safe_int(value: object) -> int:
    try:
        return max(int(value or 0), 0)
    except (TypeError, ValueError):
        return 0


def _extras_from_raw_json(raw_json: object) -> dict | None:
    payload = _safe_load_raw(raw_json)
    if not isinstance(payload, dict):
        return None

    # Cline's payload may itself be the ``api_req_started`` event, whose
    # ``text`` carries the cost. Try to surface ``cost`` either way.
    out: dict = {}
    parsed_text = _safe_parse_json(payload.get("text"))
    if isinstance(parsed_text, dict):
        cost = parsed_text.get("cost")
        if cost is not None:
            out["cost"] = cost
        for key in _RAW_EXTRAS_FIELDS:
            if key in parsed_text and key != "cost":
                out[key] = parsed_text[key]

    for key in _RAW_EXTRAS_FIELDS:
        val = payload.get(key)
        if val is not None and key not in out:
            out[key] = val

    return out or None


def _safe_load_raw(raw_json: object) -> object | None:
    if isinstance(raw_json, dict):
        return raw_json
    if not isinstance(raw_json, str | bytes | bytearray):
        return None
    try:
        if isinstance(raw_json, bytes | bytearray):
            return json.loads(raw_json.decode("utf-8", errors="replace"))
        return json.loads(raw_json)
    except (json.JSONDecodeError, ValueError, UnicodeDecodeError):
        return None


def _safe_parse_json(text: object) -> object | None:
    if not isinstance(text, str) or not text:
        return None
    try:
        return json.loads(text)
    except (json.JSONDecodeError, ValueError):
        return None
