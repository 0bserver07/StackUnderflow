"""Cross-provider what-if repricing (audit #7 part 2).

"What would this exact workload have cost on a different model?" Given the
token totals a project (or the whole store) actually consumed — input, output,
cache-read, cache-create — we reprice that *same* token shape against a curated
set of candidate models from every provider and report the delta versus what
was actually spent.

The repricing treats :func:`stackunderflow.infra.costs.compute_cost` as a black
box: we hand it the token dict + a ``(provider, model)`` pair and read back
``total_cost``. We never touch the pricing internals, the manifest, or the rate
tables — a model's rates changing upstream automatically flows through here.

Caveats the UI is expected to surface:

* Cache tokens are repriced at the *candidate's* cache rates. A provider with
  no cache-pricing concept (most non-Anthropic pricers fold cache reads into
  input) will price them differently — the comparison is "what the candidate's
  own rate card would charge for these token counts", not a re-run.
* The token *counts* are held fixed. A different model might tokenize the same
  text differently or need more/fewer output tokens for the same task; this is
  a rate-card swap, not a simulation.

Public surface:

* :data:`CANDIDATES`        — the ``(provider, model)`` comparison set.
* :class:`TokenTotals`      — the 4-way token aggregate to reprice.
* :func:`reprice`           — totals → list of per-candidate cost rows.
* :func:`build_whatif`      — totals + actual spend → the full response payload.
"""

from __future__ import annotations

from dataclasses import dataclass

from stackunderflow.infra.costs import compute_cost

__all__ = [
    "CANDIDATES",
    "TokenTotals",
    "build_whatif",
    "reprice",
]


# Candidate comparison set — one representative model per provider tier we can
# price. Kept deliberately small so the bar chart stays readable. Each entry is
# ``(provider, model_id, label)``; the ``provider`` string is what
# ``compute_cost`` routes on and the ``model_id`` is a canonical id the rate
# tables resolve. Order is roughly cheap → premium within a provider.
#
# This list names *models*, not rates — it does not duplicate or hardcode any
# pricing. ``compute_cost`` is the single source of the numbers.
CANDIDATES: tuple[tuple[str, str, str], ...] = (
    # Anthropic
    ("anthropic", "claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ("anthropic", "claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
    ("anthropic", "claude-opus-4-8", "Claude Opus 4.8"),
    # OpenAI
    ("openai", "gpt-5-mini", "GPT-5 mini"),
    ("openai", "gpt-5", "GPT-5"),
    ("openai", "gpt-5-codex", "GPT-5 Codex"),
    # Google Gemini
    ("gemini", "gemini-2.5-flash", "Gemini 2.5 Flash"),
    ("gemini", "gemini-2.5-pro", "Gemini 2.5 Pro"),
    # Alibaba Qwen
    ("qwen", "qwen-coder-plus", "Qwen Coder Plus"),
    # ZhipuAI GLM (priced via the Anthropic-shape proxy)
    ("anthropic", "glm-5", "GLM-5"),
)


@dataclass(frozen=True)
class TokenTotals:
    """The 4-way token aggregate a what-if repricing operates on."""

    input: int = 0
    output: int = 0
    cache_read: int = 0
    cache_create: int = 0

    @property
    def total(self) -> int:
        return self.input + self.output + self.cache_read + self.cache_create

    def as_cost_tokens(self) -> dict[str, int]:
        """Shape for :func:`compute_cost` (note: ``cache_creation`` key name)."""
        return {
            "input": int(self.input),
            "output": int(self.output),
            "cache_creation": int(self.cache_create),
            "cache_read": int(self.cache_read),
        }


def _candidate_cost(provider: str, model: str, tokens: dict[str, int]) -> float:
    """Reprice ``tokens`` on one candidate. Defensive against a pricer raising.

    A candidate the local rate tables can't resolve (or a pricer that throws on
    an odd token shape) contributes ``0.0`` rather than failing the whole
    comparison — the row is still emitted so the UI can show "n/a" instead of
    dropping the model silently.
    """
    try:
        return float(compute_cost(tokens, model, provider=provider)["total_cost"])
    except Exception:  # noqa: BLE001 — one bad candidate must not sink the rest
        return 0.0


def reprice(
    totals: TokenTotals,
    *,
    actual_cost_usd: float,
    candidates: tuple[tuple[str, str, str], ...] = CANDIDATES,
) -> list[dict]:
    """Reprice ``totals`` against every candidate, sorted cheapest first.

    Each row::

        {
            "provider": str,
            "model": str,            # canonical id
            "label": str,            # short display name
            "cost_usd": float,       # repriced total for this token shape
            "delta_usd": float,      # cost_usd - actual_cost_usd (− = cheaper)
            "delta_pct": float|None, # delta vs actual, None when actual is 0
        }

    ``delta_usd`` is negative when the candidate would have been cheaper than
    what was actually spent. ``delta_pct`` is ``None`` when there is no actual
    spend to compare against (a fresh / zero-cost project).
    """
    cost_tokens = totals.as_cost_tokens()
    rows: list[dict] = []
    for provider, model, label in candidates:
        cost = _candidate_cost(provider, model, cost_tokens)
        delta = cost - actual_cost_usd
        delta_pct = (
            (delta / actual_cost_usd * 100.0) if actual_cost_usd > 0 else None
        )
        rows.append(
            {
                "provider": provider,
                "model": model,
                "label": label,
                "cost_usd": cost,
                "delta_usd": delta,
                "delta_pct": delta_pct,
            }
        )
    rows.sort(key=lambda r: r["cost_usd"])
    return rows


def build_whatif(
    totals: TokenTotals,
    *,
    actual_cost_usd: float,
    actual_models: list[str] | None = None,
    candidates: tuple[tuple[str, str, str], ...] = CANDIDATES,
) -> dict:
    """Assemble the full what-if payload (USD; the route applies the FX rate).

    Shape::

        {
            "tokens": {input, output, cache_read, cache_create, total},
            "actual": {
                "cost_usd": float,
                "models": [str, ...],   # the models actually used
            },
            "candidates": [<reprice row>, ...],   # cheapest first
            "cheapest": <reprice row|null>,       # convenience pointer
        }

    ``cheapest`` is the first (lowest-cost) candidate row, or ``null`` when the
    candidate set is empty.
    """
    rows = reprice(totals, actual_cost_usd=actual_cost_usd, candidates=candidates)
    return {
        "tokens": {
            "input": int(totals.input),
            "output": int(totals.output),
            "cache_read": int(totals.cache_read),
            "cache_create": int(totals.cache_create),
            "total": totals.total,
        },
        "actual": {
            "cost_usd": float(actual_cost_usd),
            "models": sorted(actual_models or []),
        },
        "candidates": rows,
        "cheapest": rows[0] if rows else None,
    }
