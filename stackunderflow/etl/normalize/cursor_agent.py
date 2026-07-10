"""Cursor Agent normalizer.

Cursor Agent persists turns as either marker-based plaintext
transcripts (legacy ``.txt``) or per-turn JSONL bubbles. **Neither
shape carries token counts.** Per the codeburn catalog, every Cursor
Agent record is estimated from text length / 4.

Therefore the normalizer's policy is unconditional: assistant rows
estimate ``input = len(content_text) // 4``, ``output = 0``, stamp
``cost_source='estimated'``. Even when an upstream future adds explicit
tokens we still want the estimated flag — Cursor Agent never reports
billing-grade counts.

Model id is forwarded from whatever the adapter resolved (the adapter
queries the ``conversation_summaries`` SQLite table for it); when that
fallback misses we use ``cursor-agent-auto``.
"""

from __future__ import annotations

import json
from collections.abc import Iterable

from .base import COST_SOURCE_ESTIMATED, Normalizer

_DEFAULT_MODEL = "cursor-agent-auto"
_RAW_EXTRAS_FIELDS = ("conversationId", "transcriptType", "toolCalls")


class CursorAgentNormalizer(Normalizer):
    provider_name = "cursor-agent"  # must equal the adapter's provider string

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        text = str(msg_row.get("content_text") or "")
        # Prefer the explicit estimate when an adapter pre-computed one
        # onto the input column; fall back to text//4 estimation here.
        input_tokens = int(msg_row.get("input_tokens") or 0)
        output_tokens = int(msg_row.get("output_tokens") or 0)
        if input_tokens == 0 and output_tokens == 0:
            if not text:
                return
            input_tokens = max(len(text) // 4, 0)

        if input_tokens == 0 and output_tokens == 0:
            return

        model = str(msg_row.get("model") or "") or _DEFAULT_MODEL

        raw_extras = _extras_from_raw_json(msg_row.get("raw_json"))

        yield self._build_event(
            msg_row,
            input_tokens=input_tokens,
            output_tokens=output_tokens,
            cache_read_tokens=0,
            cache_create_tokens=0,
            cost_source=COST_SOURCE_ESTIMATED,
            model=model,
            raw_extras=raw_extras,
        )


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
