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


def test_capability_table_is_loaded_from_json_data():
    """The table is data (capabilities.json inside the adapters package),
    not Python literals: the file must parse, carry no gating keys, cite a
    source basis per entry, and be exactly what the module loaded."""
    import json as _json

    path = REPO_ROOT / "stackunderflow" / "adapters" / "capabilities.json"
    raw = _json.loads(path.read_text(encoding="utf-8"))
    assert set(raw["adapters"]) == set(sm._CAPABILITIES)
    for name, entry in raw["adapters"].items():
        assert "env_var" not in entry, name  # gating is not a concept in data
        assert entry.get("basis", "").strip(), f"{name} entry cites no source basis"


def test_discovery_marks_every_adapter_default_on_and_active():
    discovered = sm.discover_adapters()
    registered = {a.name for a in adapters.registered()}
    assert discovered, "discovery found nothing"
    # Every adapter is always on now; the walk and the registry must agree.
    assert set(discovered) <= registered
    for name, meta in discovered.items():
        assert meta["default_on"] is True
        assert meta["active"] is True, name


def test_registered_real_adapters_are_documented():
    """Every real, live adapter is either default-on or a documented opt-in.

    Scoped to adapters discoverable from the package: the registry is a mutable
    global that other tests inject doubles into, so intersect with the real set
    rather than trusting ``registered()`` to be clean.
    """
    discovered = set(sm.discover_adapters())
    live_real = {a.name for a in adapters.registered()} & discovered
    assert live_real  # the registry is never empty
    for name in live_real:
        entry = sm.adapter_support(name)
        assert entry is not None
        assert entry["default_on"] is True, name


def test_no_adapter_is_gated_behind_an_env_var():
    """Beta gating was removed — every adapter is default-on, no capability
    carries an opt-in env var, and the registry references none."""
    init_src = (REPO_ROOT / "stackunderflow" / "adapters" / "__init__.py").read_text()
    assert "STACKUNDERFLOW_BETA_" not in init_src
    for cap in sm._CAPABILITIES.values():
        assert cap["env_var"] is None


# ── public helpers ────────────────────────────────────────────────────────────


def test_adapter_support_lookup():
    assert sm.adapter_support("does-not-exist") is None
    claude = sm.adapter_support("claude")
    assert claude is not None
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
