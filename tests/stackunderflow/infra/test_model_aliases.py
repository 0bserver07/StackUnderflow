"""Model alias resolution unit tests.

Aliases let users map a proxy-rewritten model id (e.g.
``openrouter/claude-opus``) to a canonical id our rate tables know about,
so ``compute_cost()`` returns non-zero spend instead of $0. The resolver
sits at the top of ``compute_cost()`` and runs before any provider
dispatch — see ``stackunderflow/infra/costs.py``.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from stackunderflow.infra.costs import (
    compute_cost,
    get_model_pricing,
    resolve_model_alias,
)


# ── pure helper ─────────────────────────────────────────────────────────────

def test_resolve_model_alias_hit_returns_target():
    aliases = {"openrouter/claude-opus": "claude-opus-4-6"}
    assert resolve_model_alias("openrouter/claude-opus", aliases) == "claude-opus-4-6"


def test_resolve_model_alias_miss_returns_input():
    aliases = {"some-other-key": "claude-opus-4-6"}
    assert resolve_model_alias("openrouter/claude-opus", aliases) == "openrouter/claude-opus"


def test_resolve_model_alias_empty_map_is_noop():
    assert resolve_model_alias("any-model", {}) == "any-model"


def test_resolve_model_alias_self_alias_terminates():
    """A self-alias must not loop — single-step lookup only."""
    aliases = {"foo": "foo"}
    assert resolve_model_alias("foo", aliases) == "foo"


def test_resolve_model_alias_does_not_chase_chain():
    """Single-step only — ``a→b→c`` returns ``b``, not ``c``.

    Chasing would mean an attacker-supplied (or malformed) chain could
    loop forever or hide the original mapping intent. Stick to one hop.
    """
    aliases = {"a": "b", "b": "c"}
    assert resolve_model_alias("a", aliases) == "b"


def test_resolve_model_alias_handles_non_dict_input():
    """Defensive: a corrupt config that yields ``None`` must not raise."""
    assert resolve_model_alias("foo", None) == "foo"  # type: ignore[arg-type]


# ── compute_cost integration ────────────────────────────────────────────────

def _patch_aliases(tmp_path: Path, aliases: dict[str, str]):
    """Redirect the settings file to ``tmp_path`` and seed it with ``aliases``.

    Returns a (patch, patch) pair to be entered as context managers.
    Mirrors the helper used in ``tests/stackunderflow/test_cli.py``.
    """
    import json
    app_dir = tmp_path / ".stackunderflow"
    app_dir.mkdir(exist_ok=True)
    cfg_file = app_dir / "config.json"
    cfg_file.write_text(json.dumps({"model_aliases": aliases}))
    return (
        patch("stackunderflow.settings._APP_DIR", app_dir),
        patch("stackunderflow.settings._CFG_FILE", cfg_file),
    )


def test_compute_cost_no_aliases_is_unchanged(tmp_path):
    """Empty alias map → behaviour identical to pre-alias world."""
    p1, p2 = _patch_aliases(tmp_path, {})
    with p1, p2:
        cost = compute_cost(
            {"input": 1000, "output": 1000},
            "claude-opus-4-6",
        )
    # Opus 4.6: input $5/M, output $25/M → 0.005 + 0.025 = 0.03
    # (corrected from the stale $15/$75 that priced this at 0.09).
    assert cost["total_cost"] == pytest.approx(0.03)


def test_compute_cost_alias_to_known_canonical_resolves_to_nonzero(tmp_path):
    """The headline use case: a proxy-rewritten id resolves correctly."""
    aliases = {"openrouter/claude-opus": "claude-opus-4-6"}
    p1, p2 = _patch_aliases(tmp_path, aliases)
    with p1, p2:
        # Without the alias, ``openrouter/claude-opus`` falls into the
        # Anthropic fallback (Sonnet 3.5), so we'd see Sonnet rates. With
        # the alias it should match Opus 4.6 exactly.
        aliased = compute_cost(
            {"input": 1000, "output": 1000},
            "openrouter/claude-opus",
        )
        canonical = compute_cost(
            {"input": 1000, "output": 1000},
            "claude-opus-4-6",
        )
    assert aliased["total_cost"] == pytest.approx(canonical["total_cost"])
    assert aliased["total_cost"] > 0


def test_compute_cost_alias_to_unknown_canonical_falls_through(tmp_path):
    """Aliasing to a still-unknown id is a miss, not a double-alias chase.

    The result should match what ``compute_cost`` would do with the
    unknown id directly — i.e. the Anthropic Sonnet 3.5 fallback rates.
    """
    aliases = {"my-proxy": "still-not-real"}
    p1, p2 = _patch_aliases(tmp_path, aliases)
    with p1, p2:
        aliased = compute_cost(
            {"input": 1000, "output": 1000},
            "my-proxy",
        )
        direct = compute_cost(
            {"input": 1000, "output": 1000},
            "still-not-real",
        )
    assert aliased == direct


def test_compute_cost_self_alias_does_not_loop(tmp_path):
    """``foo → foo`` must terminate with the original behaviour."""
    aliases = {"foo": "foo"}
    p1, p2 = _patch_aliases(tmp_path, aliases)
    with p1, p2:
        cost = compute_cost(
            {"input": 1000, "output": 1000},
            "foo",
        )
    # No assertion on a specific number — just that we returned a dict
    # with the standard shape and didn't recurse forever.
    assert "total_cost" in cost
    assert isinstance(cost["total_cost"], float)


def test_compute_cost_alias_respects_provider_argument(tmp_path):
    """Alias resolution happens before provider dispatch.

    Aliasing an OpenAI proxy id to ``gpt-5-codex`` while passing
    ``provider="openai"`` should produce the same cost as calling
    ``compute_cost(..., "gpt-5-codex", provider="openai")`` directly.
    """
    aliases = {"my-proxy/codex-thing": "gpt-5-codex"}
    p1, p2 = _patch_aliases(tmp_path, aliases)
    with p1, p2:
        aliased = compute_cost(
            {"input": 1000, "output": 1000},
            "my-proxy/codex-thing",
            provider="openai",
        )
        canonical = compute_cost(
            {"input": 1000, "output": 1000},
            "gpt-5-codex",
            provider="openai",
        )
    assert aliased == canonical


def test_get_model_pricing_honours_aliases(tmp_path):
    """The legacy single-arg ``get_model_pricing`` also resolves aliases."""
    aliases = {"openrouter/sonnet": "claude-sonnet-4-6"}
    p1, p2 = _patch_aliases(tmp_path, aliases)
    with p1, p2:
        aliased = get_model_pricing("openrouter/sonnet")
        canonical = get_model_pricing("claude-sonnet-4-6")
    assert aliased == canonical
