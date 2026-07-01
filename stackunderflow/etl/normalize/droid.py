"""Droid (Factory) normalizer.

Droid is the Factory.ai agent. Its on-disk shape (``~/.factory/sessions/
<projectHash>/<file>.jsonl`` plus a sidecar ``<file>.settings.json``)
splits assistant turns from billing data: the JSONL records every
event but does **not** carry per-message token usage; the
``.settings.json`` carries one session-level ``tokenUsage`` block:

    { "tokenUsage": {
        "inputTokens": ...,
        "outputTokens": ...,
        "cacheCreationTokens": ...,
        "cacheReadTokens": ...,
        "thinkingTokens": ...
      },
      "model": "..."
    }

The adapter (``stackunderflow/adapters/droid.py``) is responsible for
distributing the session-level totals across the assistant messages
inside that session before they land in the ``messages`` table — the
distribution policy is the adapter's choice (codeburn picks even
distribution; we mirror that). By the time a row reaches this
normalizer, its token columns already hold a per-message slice of the
session total.

Policy:

* Trust the per-row token columns when at least one is non-zero.
* Stamp ``cost_source='rate_card'`` for known models / ``'unknown'``
  otherwise — the per-row counts came from a real total, even if the
  per-row split is approximate. The estimation here is *attribution*,
  not counting; the session sum is exact.
* ``cost_source='estimated'`` only when the row carries no token data
  at all, which means the adapter couldn't read the settings file —
  fall back to ``len(content_text)//4`` on input.
* Fold ``thinkingTokens`` into output if the adapter surfaced it on the
  row directly (column ``thinking_tokens`` or in ``raw_json``).
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

_DEFAULT_MODEL = "droid-auto"
_RAW_EXTRAS_FIELDS = ("sessionId", "tokenUsage", "factoryVersion")


class DroidNormalizer(Normalizer):
    provider_name = "droid"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        input_tokens = int(msg_row.get("input_tokens") or 0)
        output_tokens = int(msg_row.get("output_tokens") or 0)
        cache_read = int(msg_row.get("cache_read_tokens") or 0)
        cache_create = int(msg_row.get("cache_create_tokens") or 0)

        # Fold thinking tokens (Droid's reasoning slot) into output if a
        # per-row column is set — Droid bills thinking as output, so keeping it
        # inside ``output_tokens`` is what makes ``cost_usd`` correct. We ALSO
        # carry the same count as ``reasoning_tokens`` (an additive-metadata
        # subset of output, never priced) so the composition views can report
        # what share of output was reasoning. ``output_tokens`` is unchanged by
        # that second use.
        thinking = int(msg_row.get("thinking_tokens") or 0)
        if thinking == 0:
            payload = _safe_load_raw(msg_row.get("raw_json"))
            if isinstance(payload, dict):
                tu = payload.get("tokenUsage")
                if isinstance(tu, dict):
                    thinking = max(int(tu.get("thinkingTokens") or 0), 0)
        if thinking > 0:
            output_tokens += thinking

        estimated = False
        if (
            input_tokens == 0
            and output_tokens == 0
            and cache_read == 0
            and cache_create == 0
        ):
            text = str(msg_row.get("content_text") or "")
            if not text:
                return
            input_tokens = max(len(text) // 4, 0)
            estimated = True

        if input_tokens == 0 and output_tokens == 0 and cache_read == 0 and cache_create == 0:
            return

        model = str(msg_row.get("model") or "") or _DEFAULT_MODEL

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
            cache_read_tokens=cache_read,
            cache_create_tokens=cache_create,
            reasoning_tokens=thinking,
            cost_source=cost_source,
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
