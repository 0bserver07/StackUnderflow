"""Gemini (Google) normalizer.

Gemini CLI's transcripts (single-JSON ≤0.38 / JSONL ≥0.39) record a
``usage`` block on each assistant message:

    { "promptTokenCount": ...,        # *includes* cached input
      "candidatesTokenCount": ...,    # the visible output
      "thoughtsTokenCount": ...,      # reasoning output (≥1.5-pro)
      "cachedContentTokenCount": ...  # subset of promptTokenCount
    }

Per the codeburn catalog the canonical Anthropic-shape mapping is:

* ``input  = promptTokenCount - cachedContentTokenCount``  (fresh input)
* ``output = candidatesTokenCount + thoughtsTokenCount``    (visible + reasoning)
* ``cache_read   = cachedContentTokenCount``
* ``cache_create = 0``  (Gemini doesn't bill prompt-cache writes the
  same way; cache content is implicitly created when the same prefix
  appears repeatedly within the cached-content window)

The adapter may pre-flatten these into the canonical 4-token columns
(``input_tokens`` etc.); when that's already happened we trust the
columns. When it hasn't (synthetic test rows / older adapter
behaviour) we read the raw shape from ``raw_json.usageMetadata`` or
``raw_json.tokens`` and apply the transform here.
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

_DEFAULT_MODEL = "gemini-auto"
_RAW_EXTRAS_FIELDS = ("responseId", "finishReason", "safetyRatings")


class GeminiNormalizer(Normalizer):
    provider_name = "gemini"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        # Gemini logs assistant turns as ``role='gemini'`` in some
        # adapter versions and ``role='assistant'`` in others; accept
        # both.
        if role not in ("assistant", "gemini"):
            return

        canonical = _canonical_tokens(msg_row)
        if canonical is None:
            return

        if (
            canonical["input"] == 0
            and canonical["output"] == 0
            and canonical["cache_read"] == 0
            and canonical["cache_create"] == 0
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
            input_tokens=canonical["input"],
            output_tokens=canonical["output"],
            cache_read_tokens=canonical["cache_read"],
            cache_create_tokens=canonical["cache_create"],
            cost_source=cost_source,
            model=model,
            raw_extras=raw_extras,
        )


def _canonical_tokens(msg_row: dict) -> dict[str, int] | None:
    """Return the canonical 4-key shape, applying Gemini's transform.

    Resolution order:
      1. Raw Gemini ``usageMetadata`` block (``promptTokenCount`` etc.) on
         ``raw_json`` or directly on ``msg_row`` — apply cached-subtract
         + thoughts-fold here.
      2. Pre-canonicalised columns the adapter wrote.
    """
    raw = _raw_gemini_usage(msg_row)
    if raw is not None:
        prompt = max(int(raw.get("promptTokenCount") or 0), 0)
        cached = max(int(raw.get("cachedContentTokenCount") or 0), 0)
        candidates = max(int(raw.get("candidatesTokenCount") or 0), 0)
        thoughts = max(int(raw.get("thoughtsTokenCount") or 0), 0)
        # Cached is a subset of prompt — subtract to get the fresh slice.
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


def _raw_gemini_usage(msg_row: dict) -> dict | None:
    """Pull Gemini's usage block from msg_row or raw_json."""
    # Direct fields on msg_row (synthetic test path).
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
    # Newer JSONL: ``usageMetadata`` block.
    md = payload.get("usageMetadata")
    if isinstance(md, dict):
        return md
    # Older single-JSON: ``tokens`` block with friendlier names.
    tokens = payload.get("tokens")
    if isinstance(tokens, dict) and (
        "input" in tokens or "output" in tokens or "cached" in tokens or "thoughts" in tokens
    ):
        # Gemini CLI ≤0.38 shape: ``{input, output, cached, thoughts, ...}``
        # where ``input`` already has cached folded in (matches the
        # promptTokenCount semantic).
        return {
            "promptTokenCount": tokens.get("input", 0) or 0,
            "candidatesTokenCount": tokens.get("output", 0) or 0,
            "cachedContentTokenCount": tokens.get("cached", 0) or 0,
            "thoughtsTokenCount": tokens.get("thoughts", 0) or 0,
        }
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
