"""Claude (Anthropic) normalizer.

Anthropic's wire shape is the canonical 4-token shape we standardised on
(see ``docs/specs/etl-architecture.md`` schema). The adapter (see
``stackunderflow/adapters/claude.py``) already lifts ``input_tokens``,
``output_tokens``, ``cache_creation_input_tokens``, and
``cache_read_input_tokens`` straight onto the messages-table columns, so
the normalizer's only job is:

1. Skip non-billable rows — user messages, system, summary entries, and
   assistant rows that carry zero usage. Routes that compute cost today
   already filter on ``role == 'assistant' AND model IS NOT NULL`` so
   we mirror that contract.
2. Forward the four token counts unchanged.
3. Stamp ``cost_source = 'rate_card'`` (or ``'unknown'`` when the model
   has no entry in the rate table).
4. Pass ``speed`` through verbatim — Anthropic's priority/fast tier
   already lives on the messages row (migration v003).

Anthropic-specific provenance worth preserving in ``raw_extras`` is
narrow; ``service_tier`` is already encoded in ``speed``, and the
``message.usage`` block is fully captured in ``raw_json`` upstream.
We leave ``raw_extras = None`` for Claude rows so storage stays cheap.
"""

from __future__ import annotations

from collections.abc import Iterable

from stackunderflow.infra.costs import RATE_CARD

from .base import (
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
    Normalizer,
)


class ClaudeNormalizer(Normalizer):
    provider_name = "claude"

    def normalize(self, msg_row: dict) -> Iterable[dict]:
        role = str(msg_row.get("role") or "")
        if role != "assistant":
            return

        model = msg_row.get("model")
        if not model:
            # Adapter strips ``"<synthetic>"`` to None; we honour that
            # signal — synthetic placeholder rows aren't billable.
            return

        input_tokens = int(msg_row.get("input_tokens") or 0)
        output_tokens = int(msg_row.get("output_tokens") or 0)
        cache_read = int(msg_row.get("cache_read_tokens") or 0)
        cache_create = int(msg_row.get("cache_create_tokens") or 0)

        # Pure no-token assistant rows happen when Claude returns an
        # error stub or a tool-result attachment with no usage. They're
        # not billable; downstream marts would record a $0 row that
        # only inflates message_count, so we drop them here.
        if (
            input_tokens == 0
            and output_tokens == 0
            and cache_read == 0
            and cache_create == 0
        ):
            return

        # Match the canonical rate-card by exact id. The pricers fall
        # back to a default family when the id is unrecognised, so a
        # ``get_model_pricing`` call would never return None — using
        # ``RATE_CARD`` membership instead is the only way to distinguish
        # "we know this model" from "we're guessing rates".
        cost_source = (
            COST_SOURCE_RATE_CARD
            if model in RATE_CARD
            else COST_SOURCE_UNKNOWN
        )

        yield self._build_event(
            msg_row,
            input_tokens=input_tokens,
            output_tokens=output_tokens,
            cache_read_tokens=cache_read,
            cache_create_tokens=cache_create,
            cost_source=cost_source,
            model=str(model),
        )
