"""Tests for the agent-output envelope module and its versioned contract.

Two layers:

* ``stackunderflow.cli_helpers.agent_output`` is pure -- it builds and returns
  dicts, never prints, never opens a store -- so the builder tests need no store
  fixture, only the functions themselves.
* The ``stackunderflow.memory/1`` contract is pinned by golden fixtures under
  ``contracts/stackunderflow-memory-v1/`` (one per envelope-emitting subcommand
  x {success, empty, error}) plus ``scripts/check_memory_contract.py`` (the
  stdlib schema checker). Rather than re-asserting example envelopes from inline
  dicts, the contract tests below load those golden fixtures and validate them
  against the shipped JSON-Schema, so the tests and CI check the same artefacts.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest

from stackunderflow.cli_helpers import agent_output

# ── contract artefacts: golden fixtures + the stdlib checker ─────────────────
# <repo>/tests/stackunderflow/cli/test_agent_output.py -> parents[3] == <repo>.
_REPO_ROOT = Path(__file__).resolve().parents[3]
_CONTRACT_DIR = _REPO_ROOT / "contracts" / "stackunderflow-memory-v1"
_FIXTURES_DIR = _CONTRACT_DIR / "fixtures"
_ENVELOPE_COMMANDS = {"decisions", "file", "worked", "sessions", "ask"}


def _load_checker():
    """Import ``scripts/check_memory_contract.py`` (not an installed package)."""
    spec = importlib.util.spec_from_file_location(
        "check_memory_contract", _REPO_ROOT / "scripts" / "check_memory_contract.py"
    )
    assert spec is not None and spec.loader is not None, (
        "could not load scripts/check_memory_contract.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


checker = _load_checker()
_SCHEMA = checker.load_schema()
_FIXTURE_FILES = sorted(_FIXTURES_DIR.glob("*.json"))
_FIXTURE_IDS = [p.stem for p in _FIXTURE_FILES]


def _fixture(name: str) -> dict:
    return json.loads((_FIXTURES_DIR / f"{name}.json").read_text())


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


# ── golden-fixture contract (stackunderflow.memory/1) ────────────────────────
# These load the shipped golden fixtures and validate them against the shipped
# JSON-Schema via the same stdlib checker CI runs -- the envelope contract, not
# hand-written example dicts, is the source of truth.


def test_one_fixture_per_command_and_case():
    # 5 envelope-emitting commands x {success, empty, error} = 15 golden files.
    assert len(_FIXTURE_FILES) == 15, [p.name for p in _FIXTURE_FILES]
    expected = {
        f"{cmd}.{case}"
        for cmd in _ENVELOPE_COMMANDS
        for case in ("success", "empty", "error")
    }
    assert set(_FIXTURE_IDS) == expected


@pytest.mark.parametrize("name", _FIXTURE_IDS)
def test_fixture_conforms_to_schema(name):
    errors = checker.validate(_fixture(name), _SCHEMA, _SCHEMA)
    assert errors == [], errors


def test_full_checker_passes():
    # Runs conformance + forward-compat + the negative self-test, exactly as CI.
    assert checker.main() == 0


@pytest.mark.parametrize(
    "name", [n for n in _FIXTURE_IDS if not n.endswith("error")]
)
def test_result_envelope_fixtures_carry_the_core_fields(name):
    env = _fixture(name)
    assert set(agent_output._CORE_FIELDS) <= set(env)
    assert env["schema"] == "stackunderflow.memory/1"
    assert env["command"] in _ENVELOPE_COMMANDS
    assert isinstance(env["results"], list)
    assert env["result_count"] == len(env["results"])
    assert isinstance(env["budget"], int)
    assert isinstance(env["truncated"], bool)


@pytest.mark.parametrize("name", [n for n in _FIXTURE_IDS if n.endswith("error")])
def test_error_envelope_fixtures_have_the_error_shape(name):
    env = _fixture(name)
    assert set(env) == {"schema", "command", "query", "error"}
    assert "results" not in env
    assert env["schema"] == "stackunderflow.memory/1"
    assert isinstance(env["error"], str) and env["error"]


def test_file_fixtures_carry_the_risk_extra():
    for name in ("file.success", "file.empty"):
        assert isinstance(_fixture(name)["risk"], dict)


def test_ask_fixtures_carry_note_and_vector_used():
    for name in ("ask.success", "ask.empty"):
        env = _fixture(name)
        assert isinstance(env["note"], str)
        assert isinstance(env["vector_used"], bool)


def test_builder_reproduces_a_golden_success_envelope():
    # Repoint: drive build_envelope from a golden fixture (not an inline dict)
    # and assert it round-trips the frozen outer shape byte-for-byte.
    fx = _fixture("decisions.success")
    rebuilt = agent_output.build_envelope(
        command=fx["command"], query=fx["query"], results=fx["results"],
        budget=fx["budget"], truncated=fx["truncated"],
    )
    for key in (
        "schema", "command", "query", "results",
        "result_count", "token_estimate", "budget", "truncated",
    ):
        assert rebuilt[key] == fx[key], key


def test_builder_reproduces_a_golden_error_envelope():
    fx = _fixture("worked.error")
    rebuilt = agent_output.build_error_envelope(
        command=fx["command"], query=fx["query"], error=fx["error"],
    )
    assert rebuilt == fx


def test_unknown_additive_field_is_forward_compatible():
    # A field a future version might add must validate (be ignored), not reject.
    env = _fixture("sessions.success")
    env["x_future_additive_field"] = {"added": "later"}
    env["results"][0]["x_future_row_field"] = "ignored"
    assert checker.validate(env, _SCHEMA, _SCHEMA) == []
