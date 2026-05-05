"""Cursor IDE normalizer.

Cursor v3 stores chat bubbles in a SQLite vscdb with **no per-message
token counts** — every ``tokenCount.{inputTokens,outputTokens}`` is
zero. The existing adapter (``stackunderflow/adapters/cursor.py``)
falls back to ``len(text) // 4`` for those rows and stamps
``cost_source = 'estimated'`` on the raw payload. The normalizer
mirrors that policy:

* Prefer explicit token counts when **either** field is non-zero —
  stamp ``cost_source='rate_card'``.
* Otherwise estimate ``len(content_text) // 4`` for the input side
  (assistant rows still need a price point, even if we under-count) —
  stamp ``cost_source='estimated'``.
* Model: prefer the value already lifted onto the row by the adapter
  (``parsed.modelInfo.modelName`` or ``providerOptions.cursor.modelName``).
  Fall back to the configured Cursor default ``'composer-1'`` when the
  row carries the adapter's own ``'cursor-auto'`` placeholder.

Provider-specific provenance worth preserving in ``raw_extras``: the
``composerData`` reference, ``conversationId``, and the
``cost_source`` flag the adapter already stamped (so a downstream
reader can distinguish adapter-level estimation from normalizer-level
estimation, even though the two should always agree).
"""

from __future__ import annotations

import json
from collections.abc import Iterable

from stackunderflow.infra.costs import RATE_CARD

from .base import (
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
    Normalizer,
)

# Default model when the adapter wrote the placeholder ``"cursor-auto"`` —
# Cursor's modern default agent model. Spec: PR description.
_DEFAULT_MODEL = "composer-1"

# Keys we copy out of the persisted raw payload into ``raw_extras`` so
# downstream consumers can still trace back to Cursor's bubble.
_RAW_EXTRAS_FIELDS = ("conversationId", "composerData", "cost_source")


class CursorNormalizer(Normalizer):
    provider_name = "cursor"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        input_tokens, output_tokens, estimated = _resolve_tokens(msg_row)

        # If neither real nor estimated tokens land us with a non-zero
        # number — and the row also has no text to estimate from — drop
        # it. A pure-empty assistant message is not billable.
        if input_tokens == 0 and output_tokens == 0:
            return

        model = _resolve_model(msg_row)

        if estimated:
            cost_source = COST_SOURCE_ESTIMATED
        elif model in RATE_CARD:
            cost_source = COST_SOURCE_RATE_CARD
        else:
            cost_source = COST_SOURCE_UNKNOWN

        raw_extras = _extras_from_raw_json(msg_row.get("raw_json"))

        yield self._build_event(
            msg_row,
            input_tokens=input_tokens,
            output_tokens=output_tokens,
            cache_read_tokens=int(msg_row.get("cache_read_tokens") or 0),
            cache_create_tokens=int(msg_row.get("cache_create_tokens") or 0),
            cost_source=cost_source,
            model=model,
            raw_extras=raw_extras,
        )


# ── helpers ─────────────────────────────────────────────────────────


def _resolve_tokens(msg_row: dict) -> tuple[int, int, bool]:
    """Return ``(input, output, estimated)``.

    Decision order:
      1. ``tokenCount.inputTokens`` / ``tokenCount.outputTokens`` from
         either the msg_row directly (synthetic test fixtures) or the
         persisted raw payload — when at least one is non-zero, these
         are authoritative and ``estimated=False``.
      2. Pre-canonicalised columns ``input_tokens`` / ``output_tokens``
         when at least one is non-zero — also authoritative.
      3. Estimate ``input = len(content_text) // 4``, ``output = 0``.
         Cursor v3 doesn't differentiate prompt vs. completion text on
         a single bubble, so estimation goes on the input side only —
         that mirrors the existing adapter logic and matches the test
         contract.
    """
    explicit = _explicit_token_count(msg_row)
    if explicit is not None:
        inp, out = explicit
        return inp, out, False

    inp_col = int(msg_row.get("input_tokens") or 0)
    out_col = int(msg_row.get("output_tokens") or 0)
    if inp_col > 0 or out_col > 0:
        return inp_col, out_col, False

    text = str(msg_row.get("content_text") or "")
    return max(len(text) // 4, 0), 0, True


def _explicit_token_count(msg_row: dict) -> tuple[int, int] | None:
    """Return ``(input, output)`` from a ``tokenCount`` block if set.

    Synthetic dicts can pass ``tokenCount`` directly on msg_row. Real
    rows surface the same block as ``raw_json.tokenCount``.
    """
    tc = msg_row.get("tokenCount")
    if not isinstance(tc, dict):
        payload = _safe_load_raw(msg_row.get("raw_json"))
        if isinstance(payload, dict):
            tc = payload.get("tokenCount")
    if not isinstance(tc, dict):
        return None
    inp = int(tc.get("inputTokens", 0) or 0)
    out = int(tc.get("outputTokens", 0) or 0)
    if inp == 0 and out == 0:
        return None
    return max(inp, 0), max(out, 0)


def _resolve_model(msg_row: dict) -> str:
    """Pick the most specific model id available, defaulting to
    ``composer-1`` when only the adapter's ``cursor-auto`` placeholder
    is present.
    """
    direct = msg_row.get("model")
    if isinstance(direct, str) and direct and direct != "cursor-auto":
        return direct

    payload = _safe_load_raw(msg_row.get("raw_json"))
    if isinstance(payload, dict):
        info = payload.get("modelInfo")
        if isinstance(info, dict):
            name = info.get("modelName")
            if isinstance(name, str) and name:
                return name
        opts = payload.get("providerOptions")
        if isinstance(opts, dict):
            cursor_opts = opts.get("cursor")
            if isinstance(cursor_opts, dict):
                name = cursor_opts.get("modelName")
                if isinstance(name, str) and name:
                    return name

    return _DEFAULT_MODEL


def _extras_from_raw_json(raw_json: object) -> dict | None:
    payload = _safe_load_raw(raw_json)
    if not isinstance(payload, dict):
        return None
    out: dict = {}
    for key in _RAW_EXTRAS_FIELDS:
        val = payload.get(key)
        if val is not None and val != "":
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
