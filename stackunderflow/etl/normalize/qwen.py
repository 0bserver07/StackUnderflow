"""Qwen normalizer.

Qwen logs assistant turns with a Gemini-shaped ``usageMetadata`` block:

    { "type": "assistant",
      "model": "...",
      "message": { "role": "assistant", "parts": [...] },
      "usageMetadata": {
        "promptTokenCount": ...,         # *includes* cached input
        "candidatesTokenCount": ...,
        "thoughtsTokenCount": ...,
        "cachedContentTokenCount": ...
      } }

Per the codeburn catalog the canonical mapping is **identical to
Gemini's**:

* ``input  = promptTokenCount - cachedContentTokenCount``
* ``output = candidatesTokenCount + thoughtsTokenCount``
* ``cache_read   = cachedContentTokenCount``
* ``cache_create = 0``

This is intentionally a parallel implementation rather than reusing
Gemini's class — provider-specific provenance differs (Qwen surfaces
``functionCall`` arrays, Gemini surfaces ``finishReason``) and the
``provider_name`` / pricer routing differ.
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

_DEFAULT_MODEL = "qwen-auto"
_RAW_EXTRAS_FIELDS = ("uuid", "sessionId", "functionCall")


class QwenNormalizer(Normalizer):
    provider_name = "qwen"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        canonical = _canonical_tokens(msg_row)
        if canonical is None:
            return

        if all(canonical[k] == 0 for k in ("input", "output", "cache_read", "cache_create")):
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
            input_tokens=canonical["input"],
            output_tokens=canonical["output"],
            cache_read_tokens=canonical["cache_read"],
            cache_create_tokens=canonical["cache_create"],
            cost_source=cost_source,
            model=model,
            raw_extras=raw_extras,
        )


def _canonical_tokens(msg_row: dict) -> dict[str, int] | None:
    raw = _raw_usage_metadata(msg_row)
    if raw is not None:
        prompt = max(int(raw.get("promptTokenCount") or 0), 0)
        cached = max(int(raw.get("cachedContentTokenCount") or 0), 0)
        candidates = max(int(raw.get("candidatesTokenCount") or 0), 0)
        thoughts = max(int(raw.get("thoughtsTokenCount") or 0), 0)
        fresh_input = max(prompt - cached, 0)
        return {
            "input": fresh_input,
            "output": candidates + thoughts,
            "cache_read": cached,
            "cache_create": 0,
        }
    return {
        "input": int(msg_row.get("input_tokens") or 0),
        "output": int(msg_row.get("output_tokens") or 0),
        "cache_read": int(msg_row.get("cache_read_tokens") or 0),
        "cache_create": int(msg_row.get("cache_create_tokens") or 0),
    }


def _raw_usage_metadata(msg_row: dict) -> dict | None:
    if (
        "promptTokenCount" in msg_row
        or "candidatesTokenCount" in msg_row
        or "cachedContentTokenCount" in msg_row
        or "thoughtsTokenCount" in msg_row
    ):
        return {
            "promptTokenCount": msg_row.get("promptTokenCount", 0),
            "candidatesTokenCount": msg_row.get("candidatesTokenCount", 0),
            "cachedContentTokenCount": msg_row.get("cachedContentTokenCount", 0),
            "thoughtsTokenCount": msg_row.get("thoughtsTokenCount", 0),
        }
    payload = _safe_load_raw(msg_row.get("raw_json"))
    if not isinstance(payload, dict):
        return None
    md = payload.get("usageMetadata")
    if isinstance(md, dict):
        return md
    return None


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
