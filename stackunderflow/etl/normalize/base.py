"""Normalizer ABC + registry — `messages.row → usage_events.row(s)`.

Per-provider transforms from raw `messages` rows into the canonical
`usage_events` shape declared in ``docs/specs/etl-architecture.md``.

The base class stays minimal because every provider quirk lives inside
its own subclass. Cost is computed **once** here (via
``infra.costs.compute_cost``) so downstream marts read a single number,
never recomputed.

Wave 2A ships the four default-on providers (claude, codex, cursor,
cline). Wave 1's foundation pieces (the migration, the watermark
helpers, the backfill orchestrator) will land separately on
``feat/etl-foundation``; this module is intentionally self-sufficient
so we don't need Wave 1 merged before Wave 2A's tests can run.

The dict contract for ``msg_row`` matches a row from the ``messages``
table joined to its session and project — i.e. it carries enough
columns for the normalizer to emit a self-contained usage_events row
without needing extra DB lookups:

  * ``id``                      → ``source_message_fk``
  * ``provider``                → ``provider``  (joined from ``projects``)
  * ``project_id``              → ``project_id``
  * ``session_id``              → ``session_id`` (joined from ``sessions``)
  * ``timestamp``               → ``ts``  (and ``day`` derived from it)
  * ``role``                    → ``role``
  * ``model``                   → ``model``
  * ``speed``                   → ``speed`` (Anthropic priority/fast tier)
  * ``input_tokens``,
    ``output_tokens``,
    ``cache_read_tokens``,
    ``cache_create_tokens``     canonical 4-token shape persisted by adapters
  * ``content_text``            text body (cursor falls back to ``len//4``)
  * ``raw_json``                provider-specific JSON; preserved verbatim
    in ``raw_extras``

Synthetic test rows can omit any field — every normalizer handles
defaults defensively so unit tests don't need full DB-shape fixtures.
"""

from __future__ import annotations

import json
from abc import ABC, abstractmethod
from collections.abc import Iterable

from stackunderflow.infra.costs import compute_cost

# ── cost_source enum (string literals; spec §schema) ─────────────────
COST_SOURCE_LIVE = "live"
COST_SOURCE_RATE_CARD = "rate_card"
COST_SOURCE_ESTIMATED = "estimated"
COST_SOURCE_UNKNOWN = "unknown"

_VALID_COST_SOURCES = frozenset({
    COST_SOURCE_LIVE,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_UNKNOWN,
})


class Normalizer(ABC):
    """Per-provider transform: ``messages.row → usage_events.row(s)``."""

    provider_name: str = ""

    @abstractmethod
    def normalize(self, msg_row: dict) -> Iterable[dict]:
        """Yield 0..N ``usage_events`` row dicts for one ``messages`` row.

        Skip non-billable rows (user messages, system, tool results
        without usage). For billable rows yield one or more dicts shaped
        per the ``usage_events`` schema.
        """

    # ── helpers shared by every subclass ─────────────────────────────

    def _build_event(
        self,
        msg_row: dict,
        *,
        input_tokens: int,
        output_tokens: int,
        cache_read_tokens: int,
        cache_create_tokens: int,
        cost_source: str,
        model: str | None = None,
        role: str | None = None,
        speed: str | None = None,
        ts: str | None = None,
        raw_extras: dict | None = None,
    ) -> dict:
        """Assemble one usage_events row from a normalized token shape.

        Computes ``cost_usd`` once via ``compute_cost`` and stamps the
        provided ``cost_source``. Auto-derives ``day`` from ``ts``.
        Returns a plain dict — caller (backfill / incremental ingest)
        is responsible for the actual ``INSERT``.
        """
        if cost_source not in _VALID_COST_SOURCES:
            raise ValueError(
                f"cost_source must be one of {_VALID_COST_SOURCES!r}; "
                f"got {cost_source!r}"
            )
        ts_value = ts if ts is not None else str(msg_row.get("timestamp") or "")
        model_value = model if model is not None else (msg_row.get("model") or "")
        role_value = role if role is not None else str(msg_row.get("role") or "")
        speed_value = (
            speed if speed is not None else str(msg_row.get("speed") or "standard")
        )

        cost_usd = self._compute_cost_usd(
            input_tokens=input_tokens,
            output_tokens=output_tokens,
            cache_read_tokens=cache_read_tokens,
            cache_create_tokens=cache_create_tokens,
            model=model_value,
            speed=speed_value,
            cost_source=cost_source,
            at_ts=ts_value,
        )

        return {
            "source_message_fk": msg_row.get("id"),
            "provider": str(msg_row.get("provider") or self.provider_name),
            "account": str(msg_row.get("account") or "default"),
            "project_id": msg_row.get("project_id"),
            "session_id": str(msg_row.get("session_id") or ""),
            "ts": ts_value,
            "day": _day_from_ts(ts_value),
            "model": model_value,
            "speed": speed_value,
            "input_tokens": int(input_tokens),
            "output_tokens": int(output_tokens),
            "cache_read_tokens": int(cache_read_tokens),
            "cache_create_tokens": int(cache_create_tokens),
            "cost_usd": float(cost_usd),
            "cost_source": cost_source,
            "role": role_value,
            "raw_extras": json.dumps(raw_extras) if raw_extras else None,
        }

    def _compute_cost_usd(
        self,
        *,
        input_tokens: int,
        output_tokens: int,
        cache_read_tokens: int,
        cache_create_tokens: int,
        model: str,
        speed: str,
        cost_source: str,
        at_ts: str | None = None,
    ) -> float:
        """One-shot price lookup; never raises.

        Returns 0.0 when the model has no rate-card entry — that case
        is reflected in the ``cost_source='unknown'`` flag the caller
        is expected to set.
        """
        if not model:
            return 0.0
        # Pricer expects the canonical 4-key tokens shape (see
        # ``infra/costs.py``). All four fields default to 0.
        tokens = {
            "input": int(input_tokens),
            "output": int(output_tokens),
            "cache_read": int(cache_read_tokens),
            "cache_creation": int(cache_create_tokens),
        }
        try:
            breakdown = compute_cost(
                tokens, model, provider=_provider_for(self.provider_name),
                speed=speed, at_ts=at_ts,
            )
        except Exception:  # noqa: BLE001 — pricing must never break ingest
            return 0.0
        return float(breakdown.get("total_cost", 0.0))


# ── helpers ─────────────────────────────────────────────────────────

# Internal: map StackUnderflow's provider_name (the ``projects.provider``
# / ``Normalizer.provider_name`` value) to the pricer-side provider key.
# Anything not listed falls back to ``"anthropic"`` so cost lookups don't
# raise — the Anthropic pricer's family heuristic returns conservative
# rates for unknown ids. Beta providers each route to their own pricer
# (which may itself delegate by model-id prefix, e.g. cline / copilot).
_PROVIDER_TO_PRICER = {
    "claude": "anthropic",
    "anthropic": "anthropic",
    "codex": "openai",
    "openai": "openai",
    "cursor": "anthropic",  # Cursor uses Anthropic + OpenAI mix; default to Anthropic rates
    "cline": "cline",       # Cline pricer routes by vendor prefix
    "kilocode": "kilocode",
    "roocode": "roocode",
    "opencode": "opencode",
    "cursor-agent": "cursor-agent",
    "cursor_agent": "cursor-agent",  # registry key uses underscore
    "qwen": "qwen",
    "gemini": "gemini",
    "copilot": "copilot",
    "codeium": "codeium",
    "continue": "continue",
    "droid": "droid",
    "kiro": "kiro",
    "openclaw": "openclaw",
    "pi": "pi",
    "omp": "pi",
}


def _provider_for(name: str) -> str:
    return _PROVIDER_TO_PRICER.get(name, "anthropic")


def _day_from_ts(ts: str) -> str:
    """Derive ``YYYY-MM-DD`` from an ISO 8601 timestamp.

    Defensive: anything that doesn't parse returns an empty string —
    callers can decide whether that's a hard error (most marts treat
    empty-day rows as filterable) or filter it out.
    """
    if not ts or not isinstance(ts, str):
        return ""
    # Cheap path: ISO timestamps always start ``YYYY-MM-DD`` so a slice
    # avoids a full datetime parse on every row.
    if len(ts) >= 10 and ts[4] == "-" and ts[7] == "-":
        return ts[:10]
    # Last-ditch: try datetime.fromisoformat (handles ``Z`` suffix in
    # Python 3.11+ when normalised).
    from datetime import datetime
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
    except ValueError:
        return ""
    return dt.date().isoformat()


__all__ = [
    "COST_SOURCE_ESTIMATED",
    "COST_SOURCE_LIVE",
    "COST_SOURCE_RATE_CARD",
    "COST_SOURCE_UNKNOWN",
    "Normalizer",
]
