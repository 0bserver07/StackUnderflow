"""Grok (xAI ``grok`` CLI) normalizer — estimate from text length, always estimated.

The grok CLI persists chat history as ``chat_history.jsonl`` under
``~/.grok/sessions/<cwd>/<uuid>/`` and records **no** token counts on any
record (nor in the sibling ``events.jsonl`` / ``summary.json``). Per the
Kiro precedent the canonical recovery is to estimate from content length
/ 4 and stamp ``cost_source='estimated'``.

Billable turns are the model's own output:

* ``assistant`` — the visible reply text (often empty on a pure tool-call
  turn) plus ``tool_calls``.
* ``reasoning`` — chain-of-thought, but it's stored ``encrypted_content``
  and never decrypted, so its text is empty and it estimates to 0 tokens.

Both map to a billable assistant-side turn (mirrors how ``KiroNormalizer``
accepts both ``assistant`` and ``bot``). ``user`` / ``tool`` / ``system``
rows are non-billable and skipped.

**Cost is $0 until a real rate lands.** ``grok-build`` has no xAI
rate-card entry, and grok rows would otherwise route to the Anthropic
*fallback* pricer (Sonnet 3.5), accruing phantom dollars for a
non-Anthropic model. ``_compute_cost_usd`` is overridden to force $0 for
any grok model not explicitly in the rate card; once a rate is added
(follow-up: ``stackunderflow/data/models.toml`` / ``RATE_CARD``), the
normal pricer takes over automatically. ``cost_source`` stays
``estimated`` so the token provenance is still visible.
"""

from __future__ import annotations

import json
from collections.abc import Iterable

from stackunderflow.infra.costs import RATE_CARD

from .base import COST_SOURCE_ESTIMATED, Normalizer

_DEFAULT_MODEL = "grok-build"
_BILLABLE_ROLES = ("assistant", "reasoning")
_RAW_EXTRAS_FIELDS = (
    "id",
    "model_id",
    "model_fingerprint",
    "status",
    "synthetic_reason",
    "tool_call_id",
)


class GrokNormalizer(Normalizer):
    provider_name = "grok"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        # ``reasoning`` and ``assistant`` are the model's billable turns;
        # the adapter preserves the distinct role, so accept both. Accept
        # ``bot`` too for parity with the Kiro source shape.
        if role not in _BILLABLE_ROLES and role != "bot":
            return

        text = str(msg_row.get("content_text") or "")
        # Trust pre-computed counts if the adapter set them (it estimates
        # output from content for model turns); otherwise estimate here.
        input_tokens = int(msg_row.get("input_tokens") or 0)
        output_tokens = int(msg_row.get("output_tokens") or 0)
        if input_tokens == 0 and output_tokens == 0:
            if not text:
                # Encrypted reasoning / empty tool-call turn — nothing to
                # bill (matches the Kiro "no text → no event" contract).
                return
            output_tokens = max(len(text) // 4, 0)

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
            # reasoning_tokens stays 0 (the default): Grok's chain-of-thought is
            # stored ``encrypted_content`` and never decrypted, so its length —
            # and therefore its token count — is unmeasurable. There is nothing
            # to attribute even though the model plainly reasons.
            cost_source=COST_SOURCE_ESTIMATED,
            model=model,
            raw_extras=raw_extras,
        )

    def _compute_cost_usd(self, *, model: str, **kwargs) -> float:
        """Force $0 for grok models with no rate-card entry.

        ``grok-build`` is not in any rate card and routes to the Anthropic
        fallback pricer, which would price it as Sonnet 3.5 — phantom
        dollars for a non-Anthropic model. Return $0 unless the model is
        explicitly priced; the day a real xAI rate is added to the rate
        card (the documented follow-up), this falls through to the normal
        pricer with no further changes here.
        """
        if model and model in RATE_CARD:
            return super()._compute_cost_usd(model=model, **kwargs)
        return 0.0


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
