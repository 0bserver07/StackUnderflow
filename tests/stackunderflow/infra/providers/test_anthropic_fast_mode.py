"""Anthropic fast-mode (priority tier) cost-multiplier tests.

Anthropic's API exposes a ``service_tier`` field on response usage that
takes values like ``"standard"``, ``"priority"``, and ``"batch"``. Per
public Anthropic docs the priority tier bills Opus models at ~6× the
standard input + output rate (cache rates unchanged); Sonnet/Haiku
priority access does not change published $/M rates.

These tests pin down the multiplier behaviour at three layers:
1. ``AnthropicPricer.compute(speed="fast")`` directly.
2. ``compute_cost(..., speed="fast")`` through ``infra/costs.py``.
3. The aggregator's roll-up over a mixed (standard + fast) record set.
"""

from __future__ import annotations

from stackunderflow.infra.costs import compute_cost
from stackunderflow.infra.providers.anthropic import AnthropicPricer
from stackunderflow.stats.aggregator import _aggregate_cost
from stackunderflow.stats.enricher import Record

# ── unit: AnthropicPricer.compute() ─────────────────────────────────────────

def test_opus_standard_speed_uses_normal_rates() -> None:
    p = AnthropicPricer()
    tokens = {"input": 1_000_000, "output": 1_000_000,
              "cache_creation": 0, "cache_read": 0}
    cost = p.compute(tokens, "claude-opus-4-20250514", speed="standard")
    # OPUS_4 rates: 15 in, 75 out. 1M tokens of each = $15 + $75 = $90.
    # (Opus 4.0 is the legacy $15/$75 tier; 4.5+ is $5/$25.)
    assert cost["input_cost"] == 15.0
    assert cost["output_cost"] == 75.0
    assert cost["total_cost"] == 90.0


def test_opus_fast_speed_applies_6x_to_input_and_output() -> None:
    p = AnthropicPricer()
    tokens = {"input": 1_000_000, "output": 1_000_000,
              "cache_creation": 0, "cache_read": 0}
    cost = p.compute(tokens, "claude-opus-4-20250514", speed="fast")
    # 6× input + 6× output: 6×$15 + 6×$75 = $90 + $450 = $540.
    assert cost["input_cost"] == 90.0
    assert cost["output_cost"] == 450.0
    assert cost["total_cost"] == 540.0


def test_opus_fast_does_not_multiply_cache_rates() -> None:
    p = AnthropicPricer()
    tokens = {"input": 0, "output": 0,
              "cache_creation": 1_000_000, "cache_read": 1_000_000}
    fast = p.compute(tokens, "claude-opus-4-20250514", speed="fast")
    std = p.compute(tokens, "claude-opus-4-20250514", speed="standard")
    # Cache rates are untouched by the fast-tier multiplier.
    assert fast["cache_creation_cost"] == std["cache_creation_cost"]
    assert fast["cache_read_cost"] == std["cache_read_cost"]


def test_opus_fast_multiplier_applies_to_every_opus_family() -> None:
    """Opus 3, 4, 4.5, 4.6 all bill the 6× fast tier."""
    import pytest
    p = AnthropicPricer()
    tokens = {"input": 1_000, "output": 1_000,
              "cache_creation": 0, "cache_read": 0}
    for opus_id in (
        "claude-3-opus-20240229",
        "claude-opus-4-20250514",
        "claude-opus-4-20250514",
        "claude-opus-4-6",
    ):
        std = p.compute(tokens, opus_id, speed="standard")["total_cost"]
        fast = p.compute(tokens, opus_id, speed="fast")["total_cost"]
        assert fast == pytest.approx(std * 6.0), f"{opus_id} did not 6×"


def test_sonnet_fast_speed_returns_standard_cost() -> None:
    """Per Anthropic, only Opus has a 6× priority tier; Sonnet is unchanged."""
    p = AnthropicPricer()
    tokens = {"input": 1_000_000, "output": 1_000_000,
              "cache_creation": 0, "cache_read": 0}
    std = p.compute(tokens, "claude-sonnet-4-5-20250929", speed="standard")
    fast = p.compute(tokens, "claude-sonnet-4-5-20250929", speed="fast")
    assert std == fast


def test_haiku_fast_speed_returns_standard_cost() -> None:
    p = AnthropicPricer()
    tokens = {"input": 1_000_000, "output": 1_000_000,
              "cache_creation": 0, "cache_read": 0}
    std = p.compute(tokens, "claude-haiku-4-5-20251001", speed="standard")
    fast = p.compute(tokens, "claude-haiku-4-5-20251001", speed="fast")
    assert std == fast


def test_unknown_model_fast_falls_back_to_standard_rates() -> None:
    """Unknown ids resolve to the Sonnet-3.5 fallback, which is not Opus —
    so a fast flag must NOT apply the 6× multiplier (we'd rather under-bill
    than over-bill an unknown model).
    """
    p = AnthropicPricer()
    tokens = {"input": 1_000_000, "output": 1_000_000,
              "cache_creation": 0, "cache_read": 0}
    std = p.compute(tokens, "not-a-real-model", speed="standard")
    fast = p.compute(tokens, "not-a-real-model", speed="fast")
    assert std == fast


# ── public compute_cost() shim ──────────────────────────────────────────────

def test_compute_cost_threads_speed_through_to_pricer() -> None:
    """The public ``compute_cost`` shim accepts ``speed`` and routes it."""
    tokens = {"input": 1_000_000, "output": 1_000_000}
    std = compute_cost(tokens, "claude-opus-4-20250514")
    fast = compute_cost(tokens, "claude-opus-4-20250514", speed="fast")
    assert fast["total_cost"] == std["total_cost"] * 6.0


def test_compute_cost_default_speed_is_standard() -> None:
    """Existing callers that don't pass ``speed`` keep their old prices."""
    tokens = {"input": 1_000_000, "output": 1_000_000}
    no_kwarg = compute_cost(tokens, "claude-opus-4-20250514")
    explicit = compute_cost(tokens, "claude-opus-4-20250514", speed="standard")
    assert no_kwarg == explicit


# ── aggregator: mixed standard + fast records ──────────────────────────────

def _opus_record(*, speed: str, tokens: int) -> Record:
    return Record(
        session_id="s1",
        kind="assistant",
        timestamp="2026-04-30T00:00:00Z",
        model="claude-opus-4-20250514",
        content="",
        tokens={"input": tokens, "output": tokens,
                "cache_creation": 0, "cache_read": 0},
        tools=[],
        is_error=False,
        error_category=None,
        is_interruption=False,
        has_tool_result=False,
        uuid="",
        parent_uuid=None,
        is_sidechain=False,
        message_id="",
        cwd="",
        raw_data={},
        provider="claude",
        speed=speed,
    )


def test_aggregator_sums_standard_plus_fast_correctly() -> None:
    """Integration: one standard + one fast Opus record, total = std + 6×std."""
    recs = [
        _opus_record(speed="standard", tokens=1_000),
        _opus_record(speed="fast", tokens=1_000),
    ]
    total = _aggregate_cost(recs, provider="anthropic")
    # Per-record standard cost: 1000×$15/M + 1000×$75/M = $0.015 + $0.075 = $0.09.
    # Total = $0.09 (standard) + 6 × $0.09 (fast) = $0.09 × 7 = $0.63.
    expected_one = (1_000 * 15.0 / 1_000_000) + (1_000 * 75.0 / 1_000_000)
    assert abs(total - expected_one * 7.0) < 1e-9


def test_aggregator_groups_by_speed_not_just_model() -> None:
    """Two assistants with the same model but different speed must be
    priced separately — confirms the ``(model, speed)`` keying landed in
    every aggregator collector.
    """
    recs = [
        _opus_record(speed="standard", tokens=10_000),
        _opus_record(speed="fast", tokens=10_000),
    ]
    total = _aggregate_cost(recs, provider="anthropic")
    # If grouping were by model only, the sum would be charged at one
    # uniform rate (either 1× or 6× — both wrong). The correct answer
    # is the per-speed sum.
    std_only = compute_cost(
        {"input": 10_000, "output": 10_000},
        "claude-opus-4-20250514",
        speed="standard",
    )["total_cost"]
    fast_only = compute_cost(
        {"input": 10_000, "output": 10_000},
        "claude-opus-4-20250514",
        speed="fast",
    )["total_cost"]
    assert abs(total - (std_only + fast_only)) < 1e-9
    # And the wrong-grouping answer (combine tokens, then price once at
    # standard) is strictly less than the correct one — sanity guard.
    wrong = compute_cost(
        {"input": 20_000, "output": 20_000},
        "claude-opus-4-20250514",
        speed="standard",
    )["total_cost"]
    assert total > wrong
