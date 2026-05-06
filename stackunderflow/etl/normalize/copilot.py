"""GitHub Copilot normalizer.

Copilot persists in two distinct shapes:

1. **Legacy** — ``~/.copilot/session-state/{sessionId}/events.jsonl`` with
   ``{type: 'assistant.message', outputTokens, ...}`` events. ``inputTokens``
   may not be present; when it isn't we estimate from the preceding
   user message length (the adapter forwards that as ``content_text``)
   and stamp ``cost_source='estimated'``.

2. **VS Code transcripts** — ``workspaceStorage/<hash>/GitHub.copilot-chat/
   transcripts/*.jsonl`` with explicit ``inputTokens`` + ``outputTokens``
   per turn. When both fields are present (or just non-zero on either
   side) we trust them and stamp ``cost_source='rate_card'`` for known
   models / ``'unknown'`` otherwise.

Cache fields stay 0 — Copilot's transcript shape doesn't bill for prompt
caching. Model id is preserved verbatim from the transcript; for legacy
events we fall back to ``copilot-auto``.
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

_DEFAULT_MODEL = "copilot-auto"
_RAW_EXTRAS_FIELDS = ("toolCallId", "producer", "transcriptVersion")


class CopilotNormalizer(Normalizer):
    provider_name = "copilot"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        input_tokens = int(msg_row.get("input_tokens") or 0)
        output_tokens = int(msg_row.get("output_tokens") or 0)

        # The transcript may also surface tokens nested in raw_json under
        # ``data.outputTokens`` / ``data.inputTokens`` (newer transcript
        # shape) — pick those up if the adapter didn't pre-flatten them.
        if input_tokens == 0 and output_tokens == 0:
            payload = _safe_load_raw(msg_row.get("raw_json"))
            data = payload.get("data") if isinstance(payload, dict) else None
            if isinstance(data, dict):
                input_tokens = max(int(data.get("inputTokens") or 0), 0)
                output_tokens = max(int(data.get("outputTokens") or 0), 0)

        estimated = False
        if output_tokens == 0:
            # Legacy events without an explicit output count — estimate
            # from text length. Skip the row entirely if we have neither
            # explicit tokens nor any text to estimate from.
            text = str(msg_row.get("content_text") or "")
            if input_tokens == 0 and not text:
                return
            if not text:
                # input_tokens is set but output isn't — that's a weird
                # half-shape; estimate output from text length anyway
                # (which here is empty), so we just keep the explicit
                # input and let output stay 0. Mark estimated.
                estimated = True
            else:
                output_tokens = max(len(text) // 4, 0)
                if input_tokens == 0:
                    # Estimate input on the user-message length we don't
                    # have either; use the same text rather than zero so
                    # the row prices to *something*.
                    input_tokens = output_tokens
                estimated = True

        if input_tokens == 0 and output_tokens == 0:
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
            cache_read_tokens=0,
            cache_create_tokens=0,
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
    # Surface ``data.producer`` from VS Code transcripts.
    data = payload.get("data")
    if isinstance(data, dict):
        producer = data.get("producer")
        if producer and "producer" not in out:
            out["producer"] = producer
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
