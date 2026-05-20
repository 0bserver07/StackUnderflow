"""Unit tests for the agent-output envelope module.

``stackunderflow.cli_helpers.agent_output`` is pure — it builds and
returns dicts, never prints, never opens a store — so these tests need
no store fixture, only the functions themselves.
"""

from __future__ import annotations

import json

from stackunderflow.cli_helpers import agent_output

# ── schema constant ─────────────────────────────────────────────────────────


def test_schema_is_versioned():
    assert agent_output.SCHEMA == "stackunderflow.memory/1"
    assert agent_output.SCHEMA == f"stackunderflow.memory/{agent_output.SCHEMA_VERSION}"
    assert agent_output.SCHEMA_VERSION == 1


# ── estimate_tokens ─────────────────────────────────────────────────────────


def test_estimate_tokens_is_chars_over_four():
    # 200-char string → serialised ~204 chars → ~51 tokens.
    obj = {"text": "x" * 200}
    est = agent_output.estimate_tokens(obj)
    assert isinstance(est, int)
    assert 40 < est < 70


def test_estimate_tokens_empty_list_is_cheap():
    assert agent_output.estimate_tokens([]) == 1  # "[]" → 2 // 4 + 1


# ── build_envelope ──────────────────────────────────────────────────────────


def test_build_envelope_has_the_eight_core_fields():
    env = agent_output.build_envelope(
        command="decisions",
        query={"text": "retry", "project": None, "since": None, "limit": 20},
        results=[{"session_id": "s1"}],
        budget=2000,
        truncated=False,
    )
    assert set(env.keys()) == {
        "schema", "command", "query", "results",
        "result_count", "token_estimate", "budget", "truncated",
    }
    assert env["schema"] == "stackunderflow.memory/1"
    assert env["command"] == "decisions"
    assert env["budget"] == 2000
    assert env["truncated"] is False


def test_build_envelope_result_count_tracks_results():
    env = agent_output.build_envelope(
        command="sessions", query={}, results=[{"a": 1}, {"b": 2}, {"c": 3}],
        budget=0, truncated=False,
    )
    assert env["result_count"] == 3


def test_build_envelope_token_estimate_describes_results():
    results = [{"session_id": "s", "snippet": "y" * 400}]
    env = agent_output.build_envelope(
        command="decisions", query={}, results=results, budget=2000,
        truncated=False,
    )
    # token_estimate is recomputed from the final results, not passed in.
    assert env["token_estimate"] == agent_output.estimate_tokens(results)
    assert env["token_estimate"] > 50


def test_build_envelope_extra_merges_documented_keys():
    env = agent_output.build_envelope(
        command="file", query={}, results=[], budget=2000, truncated=False,
        extra={"risk": {"reverted": 2, "failed": 1, "worked": 5}},
    )
    assert env["risk"] == {"reverted": 2, "failed": 1, "worked": 5}


def test_build_envelope_extra_cannot_shadow_core_fields():
    env = agent_output.build_envelope(
        command="file", query={}, results=[{"real": 1}], budget=2000,
        truncated=False,
        extra={"results": [{"injected": "nope"}], "schema": "evil"},
    )
    # Core fields are protected — the extra's `results` / `schema` are dropped.
    assert env["results"] == [{"real": 1}]
    assert env["schema"] == "stackunderflow.memory/1"


# ── build_error_envelope ────────────────────────────────────────────────────


def test_build_error_envelope_carries_error_and_context():
    env = agent_output.build_error_envelope(
        command="decisions",
        query={"text": "x", "since": "garbage"},
        error="Invalid since value 'garbage'",
    )
    assert env["error"] == "Invalid since value 'garbage'"
    assert env["command"] == "decisions"
    assert env["schema"] == "stackunderflow.memory/1"
    assert env["query"]["since"] == "garbage"
    # An error envelope is not a result envelope — no `results` key.
    assert "results" not in env


# ── render ──────────────────────────────────────────────────────────────────


def test_render_produces_parseable_json():
    env = agent_output.build_envelope(
        command="worked", query={"action": "Edit"}, results=[{"session_id": "s"}],
        budget=2000, truncated=False,
    )
    parsed = json.loads(agent_output.render(env))
    assert parsed == env


def test_render_is_deterministic():
    env = agent_output.build_envelope(
        command="decisions", query={"text": "retry", "limit": 20},
        results=[{"session_id": "a"}, {"session_id": "b"}],
        budget=2000, truncated=True,
    )
    assert agent_output.render(env) == agent_output.render(env)
