"""OpenAIPricer unit tests — canonicalize, normalize_tokens (the cached-input
subtraction), rates_for hits/misses, fallback, supports_per_message_tokens."""

from __future__ import annotations

from stackunderflow.infra.providers.openai import OpenAIPricer


def test_canonicalize_codex_variants():
    p = OpenAIPricer()
    assert p.canonicalize("gpt-5-codex") == "GPT_5_CODEX"
    assert p.canonicalize("gpt-5.2-codex") == "GPT_52_CODEX"
    assert p.canonicalize("gpt-5.3-codex") == "GPT_53_CODEX"


def test_canonicalize_base_gpt():
    p = OpenAIPricer()
    assert p.canonicalize("gpt-5.4") == "GPT_54"
    assert p.canonicalize("gpt-5") == "GPT_5"
    assert p.canonicalize("gpt-5-mini") == "GPT_5_MINI"
    assert p.canonicalize("gpt-4o") == "GPT_4O"
    assert p.canonicalize("gpt-4o-mini") == "GPT_4O_MINI"
    assert p.canonicalize("gpt-4.1") == "GPT_41"


def test_canonicalize_unknown_falls_back():
    p = OpenAIPricer()
    assert p.canonicalize("") == "GPT_5_CODEX"
    assert p.canonicalize("not-real") == "GPT_5_CODEX"


def test_normalize_tokens_subtracts_cached_from_input():
    """The migration from ``adapters/codex.py:_attach_tokens_to_last_assistant``
    happens here. Cached counts INSIDE raw input_tokens; canonical input
    counts only fresh (uncached) input."""
    p = OpenAIPricer()
    raw = {
        "input_tokens": 1200,
        "cached_input_tokens": 200,
        "output_tokens": 350,
        "reasoning_output_tokens": 150,
    }
    out = p.normalize_tokens(raw)
    assert out == {
        "input": 1000,         # 1200 - 200
        "output": 500,         # 350 + 150
        "cache_creation": 0,   # OpenAI does not bill writes
        "cache_read": 200,     # = cached
    }


def test_normalize_tokens_handles_more_cached_than_input():
    """Defensive: if the API shape is malformed, never go negative."""
    p = OpenAIPricer()
    out = p.normalize_tokens({"input_tokens": 100, "cached_input_tokens": 500})
    assert out["input"] == 0


def test_normalize_tokens_passthrough_when_already_canonical():
    """Adapters that pre-normalise (the current Codex adapter does this for
    DB compatibility) must not have their numbers re-subtracted."""
    p = OpenAIPricer()
    canonical = {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 200}
    assert p.normalize_tokens(canonical) == canonical


def test_normalize_tokens_handles_partial_raw():
    p = OpenAIPricer()
    out = p.normalize_tokens({"input_tokens": 100, "output_tokens": 50})
    assert out == {"input": 100, "output": 50, "cache_creation": 0, "cache_read": 0}


def test_rates_for_known_codex():
    p = OpenAIPricer()
    assert p.rates_for("GPT_5_CODEX") == (1.25, 10.0, 0.0, 0.125)
    assert p.rates_for("GPT_53_CODEX") == (1.25, 10.0, 0.0, 0.125)


def test_rates_for_known_base_gpt():
    p = OpenAIPricer()
    # GPT_54 rates are manifest-owned now (data/models.toml, effective-dated
    # $20→$15 output cut at 2026-04-26); undated rates_for = current era.
    assert p.rates_for("GPT_54") == (2.50, 15.0, 0.0, 0.25)
    assert p.rates_for("GPT_4O") == (2.50, 10.0, 0.0, 1.25)


def test_rates_for_unknown_falls_back():
    p = OpenAIPricer()
    # Fallback is GPT_5_CODEX rates; the contract is "never None" for OpenAI.
    assert p.rates_for("nonsense") == (1.25, 10.0, 0.0, 0.125)


def test_supports_per_message_tokens():
    assert OpenAIPricer().supports_per_message_tokens() is True


def test_cache_write_is_zero_for_all_openai():
    """OpenAI does not bill prompt-cache writes — every rate row's third
    value (cache-write) must be 0.0."""
    p = OpenAIPricer()
    for canonical in (
        "GPT_5_CODEX", "GPT_52_CODEX", "GPT_53_CODEX",
        "GPT_54", "GPT_5", "GPT_5_MINI",
        "GPT_4O", "GPT_4O_MINI", "GPT_41",
    ):
        rates = p.rates_for(canonical)
        assert rates is not None
        assert rates[2] == 0.0


def test_compute_with_raw_shape_equals_compute_with_canonical():
    """End-to-end: feeding raw OpenAI shape through compute() yields the same
    dollars as feeding canonical shape — proves the normalize-then-compute
    seam is consistent."""
    p = OpenAIPricer()
    raw = {
        "input_tokens": 1200, "cached_input_tokens": 200,
        "output_tokens": 350, "reasoning_output_tokens": 150,
    }
    raw_norm = p.normalize_tokens(raw)
    canonical = {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 200}
    assert raw_norm == canonical
    assert p.compute(raw_norm, "gpt-5.4") == p.compute(canonical, "gpt-5.4")


def test_normalize_tokens_tolerates_garbage_values():
    """``normalize_tokens`` is handed raw provider JSON at ingest time
    (Codex ``last_token_usage``); string/list/inf values must coerce to 0
    instead of raising out of the adapter's read() generator."""
    p = OpenAIPricer()
    out = p.normalize_tokens({
        "input_tokens": "garbage",
        "cached_input_tokens": [1],
        "output_tokens": None,
        "reasoning_output_tokens": float("inf"),
    })
    assert out == {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}
    # Canonical-shape branch too.
    out = p.normalize_tokens({"input": "x", "output": float("nan"), "cache_read": {}})
    assert out == {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}
    # Numeric strings still coerce (pre-existing tolerance preserved).
    out = p.normalize_tokens({"input_tokens": "100", "output_tokens": "50"})
    assert out["input"] == 100
    assert out["output"] == 50
