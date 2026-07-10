"""Pi (and OMP) normalizer.

Pi and OMP share the same JSONL format — each assistant message carries
an explicit ``usage`` block:

    { "type": "message",
      "message": {
        "role": "assistant",
        "content": [...],
        "model": "...",
        "usage": {
          "input": ...,
          "output": ...,
          "cacheRead": ...,
          "cacheWrite": ...
        }
      } }

Per the codeburn catalog the mapping is direct: ``cacheWrite`` →
``cache_create_tokens``, ``cacheRead`` → ``cache_read_tokens``.

This single normalizer covers both Pi and OMP — they only differ in
their on-disk root directory (``~/.pi/agent/sessions/`` vs.
``~/.omp/agent/sessions/``), which is an adapter concern. The
``provider_name`` defaults to ``pi`` but the registry can route OMP
rows here too via ``register("omp", PiNormalizer)`` if the adapter
ever distinguishes them at the provider column level.
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

_DEFAULT_MODEL = "gpt-5"
_RAW_EXTRAS_FIELDS = ("responseId", "sessionId", "cwd")


class PiNormalizer(Normalizer):
    provider_name = "pi"
    # Pi/OMP share the parser — one transform, two provider strings.
    provider_aliases = ("omp",)

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
    raw = _raw_usage(msg_row)
    if raw is not None:
        return {
            "input": _safe_int(raw.get("input")),
            "output": _safe_int(raw.get("output")),
            "cache_read": _safe_int(raw.get("cacheRead")),
            "cache_create": _safe_int(raw.get("cacheWrite")),
        }
    return {
        "input": int(msg_row.get("input_tokens") or 0),
        "output": int(msg_row.get("output_tokens") or 0),
        "cache_read": int(msg_row.get("cache_read_tokens") or 0),
        "cache_create": int(msg_row.get("cache_create_tokens") or 0),
    }


def _raw_usage(msg_row: dict) -> dict | None:
    direct = msg_row.get("usage")
    if isinstance(direct, dict):
        return direct
    payload = _safe_load_raw(msg_row.get("raw_json"))
    if isinstance(payload, dict):
        msg = payload.get("message")
        if isinstance(msg, dict):
            usage = msg.get("usage")
            if isinstance(usage, dict):
                return usage
        usage = payload.get("usage")
        if isinstance(usage, dict):
            return usage
    return None


def _safe_int(value: object) -> int:
    try:
        return max(int(value or 0), 0)
    except (TypeError, ValueError):
        return 0


def _extras_from_raw_json(raw_json: object) -> dict | None:
    payload = _safe_load_raw(raw_json)
    if not isinstance(payload, dict):
        return None
    out: dict = {}
    inner = payload.get("message") if isinstance(payload.get("message"), dict) else payload
    for key in _RAW_EXTRAS_FIELDS:
        val = inner.get(key) if isinstance(inner, dict) else None
        if val is None and isinstance(payload, dict):
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
