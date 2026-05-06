"""Kiro (Amazon Kiro Agent) normalizer.

Kiro persists chat history in ``.chat`` files (JSON blobs) under VS
Code's globalStorage. The schema records execution metadata and a
chat array but **does not** carry token counts on any role:

    { "executionId": "...",
      "actionId": "...",
      "chat": [{"role": "human" | "bot" | "tool", "content": "..."}],
      "metadata": {"modelId": "claude.3.5.sonnet", "startTime": ...} }

Per the codeburn catalog the canonical recovery is to estimate from
content length / 4 and stamp ``cost_source='estimated'``. Model id
normalisation (dots → dashes for ``claude.*`` ids) lives in the
adapter; we trust the column and fall back to ``kiro-auto``.
"""

from __future__ import annotations

import json
from collections.abc import Iterable

from .base import COST_SOURCE_ESTIMATED, Normalizer

_DEFAULT_MODEL = "kiro-auto"
_RAW_EXTRAS_FIELDS = ("executionId", "actionId", "workflowId", "metadata")


class KiroNormalizer(Normalizer):
    provider_name = "kiro"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        # Kiro logs assistant turns as ``role='bot'`` in the source
        # format; the adapter may normalise to ``'assistant'``. Accept
        # both so the normalizer doesn't depend on which adapter
        # version wrote the row.
        if role not in ("assistant", "bot"):
            return

        text = str(msg_row.get("content_text") or "")
        # Trust pre-computed counts if present (e.g. an adapter upgrade
        # adds them later), otherwise estimate from text length.
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
