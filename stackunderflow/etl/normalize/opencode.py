"""OpenCode normalizer.

OpenCode persists chats in a SQLite database (``opencode*.db`` under
``$XDG_DATA_HOME/opencode/``). The ``message`` table's ``data`` column
holds the per-turn payload as JSON, with the canonical token block at
``data.tokens``:

    { "role": "assistant",
      "modelID": "...",
      "tokens": {
        "input": ...,
        "output": ...,
        "reasoning": ...,
        "cache": {"read": ..., "write": ...}
      },
      "cost": ... }

Per the codeburn catalog the canonical mapping is:

* ``input  = tokens.input``
* ``output = tokens.output + tokens.reasoning``  (fold reasoning into output)
* ``cache_read   = tokens.cache.read``
* ``cache_create = tokens.cache.write``

Model id is preserved verbatim from ``data.modelID`` (the adapter
should already lift it onto the row column). ``cost`` is not
consumed — we recompute via ``compute_cost`` for parity, preserving
the source-side cost in ``raw_extras``.
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

_DEFAULT_MODEL = "opencode-auto"
_RAW_EXTRAS_FIELDS = ("modelID", "providerID", "embeddedCost")


class OpenCodeNormalizer(Normalizer):
    provider_name = "opencode"

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
    """Return canonical 4-key shape with reasoning folded into output."""
    raw = _raw_tokens(msg_row)
    if raw is not None:
        cache = raw.get("cache") if isinstance(raw.get("cache"), dict) else {}
        return {
            "input": _safe_int(raw.get("input")),
            "output": _safe_int(raw.get("output")) + _safe_int(raw.get("reasoning")),
            "cache_read": _safe_int(cache.get("read") if isinstance(cache, dict) else 0),
            "cache_create": _safe_int(cache.get("write") if isinstance(cache, dict) else 0),
        }
    return {
        "input": int(msg_row.get("input_tokens") or 0),
        "output": int(msg_row.get("output_tokens") or 0),
        "cache_read": int(msg_row.get("cache_read_tokens") or 0),
        "cache_create": int(msg_row.get("cache_create_tokens") or 0),
    }


def _raw_tokens(msg_row: dict) -> dict | None:
    """Locate the OpenCode tokens block on msg_row or raw_json."""
    direct = msg_row.get("tokens")
    if isinstance(direct, dict):
        return direct
    payload = _safe_load_raw(msg_row.get("raw_json"))
    if isinstance(payload, dict):
        # Canonical persistence — the message.data JSON has tokens at
        # the top level.
        tokens = payload.get("tokens")
        if isinstance(tokens, dict):
            return tokens
        # If the raw_json includes a wrapping ``data`` key (some adapter
        # versions persist the row that way), unwrap once.
        data = payload.get("data")
        if isinstance(data, dict):
            tokens = data.get("tokens")
            if isinstance(tokens, dict):
                return tokens
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
    inner = payload.get("data") if isinstance(payload.get("data"), dict) else payload
    out: dict = {}
    for key in _RAW_EXTRAS_FIELDS:
        val = inner.get(key) if isinstance(inner, dict) else None
        if val is not None and val != "":
            out[key] = val
    cost = inner.get("cost") if isinstance(inner, dict) else None
    if cost is not None and "embeddedCost" not in out:
        out["embeddedCost"] = cost
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
