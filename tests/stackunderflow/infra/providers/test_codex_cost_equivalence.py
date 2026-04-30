"""Cost-equivalence regression — the Codex normalization moved from the
adapter to ``OpenAIPricer.normalize_tokens`` MUST NOT change the final
dollar number for a given (raw OpenAI usage, model) pair.

Strategy: take a known token bundle the way the Codex API emits it (raw
shape — cached nested in input, reasoning separate from output), compute the
cost two ways:
1. The pre-refactor convention: caller manually subtracts cached and folds
   reasoning, then calls ``compute_cost(canonical_tokens, model)``.
2. The post-refactor convention: caller passes raw shape directly and
   ``compute_cost(raw_tokens, model, provider="openai")`` does the
   normalization internally.

If they disagree, the refactor would silently change every Codex session's
billed cost — exactly what the spec §1.5 + §2 contract forbids.
"""

from __future__ import annotations

import pytest

from stackunderflow.infra.costs import compute_cost
from stackunderflow.infra.providers.openai import OpenAIPricer


# (model_id, input_tokens, cached_input, output_tokens, reasoning_output)
# These match the numbers used in tests/stackunderflow/adapters/test_codex.py
# (`test_token_count_attaches_to_previous_assistant`) plus a couple of
# edge cases (zero usage, all-cached input).
_FIXTURES = [
    ("gpt-5.4",   1200, 200, 350, 150),
    ("gpt-5.4",    800, 100, 200,  50),
    ("gpt-5-codex", 5000, 1000, 1500, 500),
    ("gpt-5.4",      0,   0,   0,   0),
    ("gpt-5.4",    500, 500, 100,   0),  # 100% cached input
]


def _legacy_normalize(
    raw_input: int, cached: int, raw_output: int, reasoning: int,
) -> dict[str, int]:
    """Reproduce the pre-refactor adapter normalization byte-for-byte —
    same logic that lived in ``adapters/codex.py:_attach_tokens_to_last_assistant``
    before this PR."""
    return {
        "input": max(raw_input - cached, 0),
        "output": raw_output + reasoning,
        "cache_creation": 0,
        "cache_read": cached,
    }


@pytest.mark.parametrize("model,raw_in,cached,raw_out,reasoning", _FIXTURES)
def test_codex_cost_equivalent_pre_and_post_refactor(
    model: str,
    raw_in: int,
    cached: int,
    raw_out: int,
    reasoning: int,
) -> None:
    legacy_tokens = _legacy_normalize(raw_in, cached, raw_out, reasoning)
    legacy_cost = compute_cost(legacy_tokens, model, provider="openai")

    raw_tokens = {
        "input_tokens": raw_in,
        "cached_input_tokens": cached,
        "output_tokens": raw_out,
        "reasoning_output_tokens": reasoning,
    }
    new_cost = compute_cost(raw_tokens, model, provider="openai")

    assert legacy_cost == new_cost, (
        f"Cost diverged after refactor for {model} "
        f"(raw_in={raw_in}, cached={cached}, raw_out={raw_out}, reasoning={reasoning}): "
        f"legacy={legacy_cost}, new={new_cost}"
    )


def test_pricer_normalize_matches_legacy_subtraction():
    """The OpenAIPricer.normalize_tokens output must equal the legacy
    ``_legacy_normalize`` output for every fixture."""
    p = OpenAIPricer()
    for model, raw_in, cached, raw_out, reasoning in _FIXTURES:
        legacy = _legacy_normalize(raw_in, cached, raw_out, reasoning)
        new = p.normalize_tokens({
            "input_tokens": raw_in,
            "cached_input_tokens": cached,
            "output_tokens": raw_out,
            "reasoning_output_tokens": reasoning,
        })
        assert new == legacy, f"shape diverged for {model}"
