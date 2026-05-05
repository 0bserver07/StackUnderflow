"""Codex (OpenAI) normalizer.

Spec contract (``docs/specs/etl-architecture.md``):

* Subtract ``cached_input_tokens`` from ``input_tokens`` so canonical
  ``input`` reflects only the freshly-billed input — matches Anthropic's
  shape where cached reads are accounted separately.
* Fold ``reasoning_output_tokens`` into ``output_tokens`` so the canonical
  ``output`` is the fully-billable assistant output.
* Map ``cached_input_tokens`` → ``cache_read_tokens``.
* ``cache_create_tokens`` stays 0 — OpenAI doesn't bill prompt-cache writes.

Single source of truth choice
-----------------------------
``OpenAIPricer.normalize_tokens()`` (in ``infra/providers/openai.py``)
already implements the exact transform. Two existing call sites depend
on it as a pricer-level seam:

  * ``adapters/codex.py`` calls it lazily to flatten per-turn token
    payloads before they land in the ``messages`` table — that is, the
    Codex adapter already writes *canonical* tokens to the DB.
  * ``infra.costs.compute_cost`` calls it inside the price path so any
    legacy caller passing raw OpenAI shape gets the same flattening.

Removing the pricer's copy would break ``compute_cost`` callers that
still pass raw token shapes (LiteLLM overlay tests, the API contract
test for Codex cost equivalence) without buying us much — both copies
are 8 lines of arithmetic against the same field names. Per the PR
guidance ("leave it in place if other code still depends on it"), we
**delegate** to the pricer's helper from this normalizer rather than
forking a second copy. That keeps the normalizer the *user-facing*
single source of truth for the ETL pipeline while honouring the
pricer-level back-compat.

Token shape acceptance
----------------------
``msg_row`` carries the canonical columns (``input_tokens`` etc.) the
adapter already wrote, **and** the raw provider payload in ``raw_json``.
We try the raw payload first because that lets the pricer helper do
the cached-subtract / reasoning-fold from the original keys; if the
raw payload isn't shaped that way we fall back to the canonical
columns directly.
"""

from __future__ import annotations

import json
from collections.abc import Iterable

from stackunderflow.infra.costs import RATE_CARD
from stackunderflow.infra.providers.openai import OpenAIPricer

from .base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
    Normalizer,
)

# Codex-specific fields we copy verbatim into ``raw_extras`` so
# downstream UI / debugging paths can still see them.
_RAW_EXTRAS_FIELDS = ("service_tier", "model_provider", "originator")


class CodexNormalizer(Normalizer):
    provider_name = "codex"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        canonical = self._canonical_tokens(msg_row)
        if canonical is None:
            return  # not billable

        model = str(msg_row.get("model") or "")
        if not model:
            return

        # Use exact-id membership — the OpenAI pricer falls back to a
        # default Codex family for any unrecognised gpt-* id, so
        # ``get_model_pricing`` always returns a number. ``RATE_CARD``
        # is the only "do we actually know this model" signal we have.
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
            cache_create_tokens=canonical["cache_creation"],
            cost_source=cost_source,
            model=model,
            raw_extras=raw_extras,
        )

    # ── internals ──────────────────────────────────────────────────

    @staticmethod
    def _canonical_tokens(msg_row: dict) -> dict[str, int] | None:
        """Return the canonical 4-key shape, or None if non-billable.

        Prefers raw OpenAI shape (``input_tokens`` + ``cached_input_tokens``
        + ``reasoning_output_tokens``) when present in the row directly
        or reachable via ``raw_json``. Falls back to the messages-table
        columns the adapter already wrote.
        """
        raw_shape = _raw_openai_shape(msg_row)
        if raw_shape is not None:
            canonical = OpenAIPricer().normalize_tokens(raw_shape)
        else:
            canonical = {
                "input": int(msg_row.get("input_tokens") or 0),
                "output": int(msg_row.get("output_tokens") or 0),
                "cache_read": int(msg_row.get("cache_read_tokens") or 0),
                "cache_creation": int(msg_row.get("cache_create_tokens") or 0),
            }
        if (
            canonical["input"] == 0
            and canonical["output"] == 0
            and canonical["cache_read"] == 0
            and canonical["cache_creation"] == 0
        ):
            return None
        return canonical


def _raw_openai_shape(msg_row: dict) -> dict[str, int] | None:
    """Return raw OpenAI token keys when discoverable on this row.

    Two locations matter:
      1. The msg_row dict itself — synthetic test fixtures may pass
         ``cached_input_tokens`` / ``reasoning_output_tokens`` directly.
      2. ``raw_json`` parsed payload — production adapter writes the
         flattened shape into the columns and the original payload
         into raw_json. The OpenAI rollout shape is
         ``payload.info.last_token_usage`` containing
         ``input_tokens``, ``cached_input_tokens``, ``output_tokens``,
         ``reasoning_output_tokens``.
    """
    if (
        "cached_input_tokens" in msg_row
        or "reasoning_output_tokens" in msg_row
    ):
        return {
            "input_tokens": int(msg_row.get("input_tokens") or 0),
            "output_tokens": int(msg_row.get("output_tokens") or 0),
            "cached_input_tokens": int(msg_row.get("cached_input_tokens") or 0),
            "reasoning_output_tokens": int(
                msg_row.get("reasoning_output_tokens") or 0
            ),
        }

    payload = _safe_load_raw(msg_row.get("raw_json"))
    if not isinstance(payload, dict):
        return None
    info = (payload.get("payload") or payload).get("info")
    if not isinstance(info, dict):
        return None
    last = info.get("last_token_usage")
    if not isinstance(last, dict):
        return None
    if (
        "cached_input_tokens" not in last
        and "reasoning_output_tokens" not in last
    ):
        return None
    return {
        "input_tokens": int(last.get("input_tokens") or 0),
        "output_tokens": int(last.get("output_tokens") or 0),
        "cached_input_tokens": int(last.get("cached_input_tokens") or 0),
        "reasoning_output_tokens": int(last.get("reasoning_output_tokens") or 0),
    }


def _extras_from_raw_json(raw_json: object) -> dict | None:
    """Pull provider-specific keepsakes out of raw_json into raw_extras.

    Returns None when there's nothing useful to preserve so the JSON
    column stays NULL rather than an empty ``{}``.
    """
    payload = _safe_load_raw(raw_json)
    if not isinstance(payload, dict):
        return None
    inner = payload.get("payload") if isinstance(payload.get("payload"), dict) else payload
    out: dict = {}
    for key in _RAW_EXTRAS_FIELDS:
        val = inner.get(key)
        if val:
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
