"""Tests for the honest per-adapter support matrix.

Locks the two things the matrix exists to guarantee: it stays **truthful** (a
field an adapter does not capture reads ``captured=False``) and it stays **in
sync** with the adapters actually installed (a new adapter can't ship without a
curated row).
"""

from __future__ import annotations

from pathlib import Path

import pytest

import stackunderflow.adapters as adapters
from stackunderflow.services import support_matrix as sm

REPO_ROOT = Path(__file__).resolve().parents[3]


# ── shape ─────────────────────────────────────────────────────────────────────


def test_envelope_shape():
    m = sm.support_matrix()
    assert m["schema"] == sm.SCHEMA
    assert m["statuses"] == list(sm.STATUSES)
    assert m["fidelity_levels"] == list(sm.FIDELITY_LEVELS)
    assert m["adapter_count"] == len(m["adapters"]) == len(sm.discover_adapters())
    assert [f["key"] for f in m["fields"]] == list(sm.FIELDS)


def test_every_field_entry_is_internally_consistent():
    """captured is EXACTLY (fidelity != none) for every adapter × field."""
    for adapter in sm.support_matrix()["adapters"]:
        assert adapter["status"] in sm.STATUSES
        assert set(adapter["fields"]) == set(sm.FIELDS)
        for field, entry in adapter["fields"].items():
            assert entry["fidelity"] in sm.FIDELITY_LEVELS
            assert entry["captured"] is (entry["fidelity"] != "none")


def test_adapters_sorted_supported_first_then_partial_last():
    statuses = [a["status"] for a in sm.support_matrix()["adapters"]]
    order = {"supported": 0, "beta": 1, "partial": 2}
    assert statuses == sorted(statuses, key=lambda s: order[s])


# ── truthfulness ──────────────────────────────────────────────────────────────


def test_uncaptured_fields_read_false():
    # Claude carries no reasoning split (Anthropic usage has none).
    assert sm.captures("claude", "reasoning") is False
    # Cline gives cost/tokens but no structured tool calls.
    assert sm.captures("cline", "tool_calls") is False
    # Antigravity is encrypted at rest — no tokens, no cost.
    assert sm.captures("antigravity", "tokens") is False
    assert sm.captures("antigravity", "cost") is False
    # Codeium is a discovery stub — it captures nothing yet.
    for field in sm.FIELDS:
        assert sm.captures("codeium", field) is False


def test_captured_fields_read_true_with_a_fidelity():
    assert sm.captures("claude", "tokens") is True
    assert sm.field_fidelity("claude", "tokens") == "exact"
    # Codex is the one provider that attributes a reasoning split.
    assert sm.captures("codex", "reasoning") is True
    assert sm.field_fidelity("codex", "reasoning") == "exact"
    # Estimated is captured=True but flagged so a caller can distinguish it.
    assert sm.captures("grok", "tokens") is True
    assert sm.field_fidelity("grok", "tokens") == "estimated"


def test_cost_fidelity_never_exceeds_token_fidelity():
    """Cost is computed from tokens, so it can be no more precise than them."""
    rank = {"exact": 3, "full": 3, "partial": 2, "estimated": 1, "none": 0}
    for adapter in sm.support_matrix()["adapters"]:
        tok = adapter["fields"]["tokens"]["fidelity"]
        cost = adapter["fields"]["cost"]["fidelity"]
        assert rank[cost] <= rank[tok], adapter["provider"]


def test_field_fidelity_rejects_unknown_field():
    with pytest.raises(KeyError):
        sm.field_fidelity("claude", "not_a_field")


# ── sync with the real adapter set ────────────────────────────────────────────


def test_curated_table_matches_discovered_adapters_exactly():
    discovered = set(sm.discover_adapters())
    curated = set(sm._CAPABILITIES)
    assert discovered == curated, (
        f"drift — discovered-only: {discovered - curated}; "
        f"table-only: {curated - discovered}"
    )


def test_discovery_finds_the_default_on_adapters():
    discovered = sm.discover_adapters()
    registered = {a.name for a in adapters.registered()}
    # Everything registered unconditionally must be discoverable and marked on.
    assert sm._DEFAULT_ON <= set(discovered)
    assert sm._DEFAULT_ON <= registered
    for name in sm._DEFAULT_ON:
        assert discovered[name]["default_on"] is True
        assert discovered[name]["active"] is True


def test_registered_real_adapters_are_documented():
    """Every real, live adapter is either default-on or a documented opt-in.

    Scoped to adapters discoverable from the package: the registry is a mutable
    global that other tests inject doubles into, so intersect with the real set
    rather than trusting ``registered()`` to be clean.
    """
    discovered = set(sm.discover_adapters())
    live_real = {a.name for a in adapters.registered()} & discovered
    assert sm._DEFAULT_ON <= live_real  # the default-on set is always live
    for name in live_real:
        entry = sm.adapter_support(name)
        assert entry is not None
        assert entry["default_on"] or entry["env_var"], name


def test_opt_in_env_vars_are_real():
    """Every curated env var actually gates its adapter in adapters/__init__.py."""
    init_src = (REPO_ROOT / "stackunderflow" / "adapters" / "__init__.py").read_text()
    for name, cap in sm._CAPABILITIES.items():
        env_var = cap["env_var"]
        default_on = name in sm._DEFAULT_ON
        assert (env_var is None) == default_on, name
        if env_var is not None:
            assert env_var in init_src, f"{env_var} not found in adapters/__init__.py"


# ── public helpers ────────────────────────────────────────────────────────────


def test_adapter_support_lookup():
    assert sm.adapter_support("does-not-exist") is None
    claude = sm.adapter_support("claude")
    assert claude["provider"] == "claude"
    assert claude["status"] == "supported"
    assert claude["opt_in"] is False


def test_renderers_cover_every_adapter():
    matrix = sm.support_matrix()
    md = sm.render_markdown(matrix)
    txt = sm.render_text(matrix)
    for adapter in matrix["adapters"]:
        assert adapter["provider"] in md
        assert adapter["provider"] in txt
    # Markdown table: header + separator + one row per adapter, at minimum.
    assert md.count("\n|") >= len(matrix["adapters"]) + 1
