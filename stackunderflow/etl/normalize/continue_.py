"""Continue (continue.dev) normalizer.

Continue stores chat history in SQLite ``.db`` files under its config
directory. Token counts may or may not be present per row depending on
the Continue version and the underlying provider — newer versions
record explicit input/output counts on the assistant turn, older
versions persist only the rendered text.

The normalizer keeps the policy simple and **defensive**:

* Trust the canonical token columns the adapter wrote when at least
  one is non-zero — stamp ``cost_source='rate_card'`` if the model is
  in the canonical rate card, ``'unknown'`` otherwise.
* Otherwise fall back to ``len(content_text) // 4`` on the input side
  with output = 0, and stamp ``cost_source='estimated'``. This mirrors
  the same recovery path the Cursor v3 normalizer uses for missing
  per-bubble counts.

Provider-specific provenance worth preserving in ``raw_extras``: the
``provider`` field Continue records for the underlying model gateway
(``"anthropic"`` / ``"openai"`` / ``"ollama"`` / ...) and the
``modelTitle`` display name so the UI can disambiguate proxy-routed
models from native ones.
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

_DEFAULT_MODEL = "continue-auto"
_RAW_EXTRAS_FIELDS = ("provider", "modelTitle", "completionOptions")


class ContinueNormalizer(Normalizer):
    provider_name = "continue"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        input_tokens = int(msg_row.get("input_tokens") or 0)
        output_tokens = int(msg_row.get("output_tokens") or 0)
        cache_read = int(msg_row.get("cache_read_tokens") or 0)
        cache_create = int(msg_row.get("cache_create_tokens") or 0)

        estimated = False
        if (
            input_tokens == 0
            and output_tokens == 0
            and cache_read == 0
            and cache_create == 0
        ):
            text = str(msg_row.get("content_text") or "")
            if not text:
                return  # nothing to estimate from — drop
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
