"""CLI tests for ``stackunderflow context-replay`` — context replay (#96).

Monkeypatches ``deps.store_path`` to a tmp store, seeds a synthetic session,
drives the command with Click's ``CliRunner``. Locks:

* the text summary;
* the ``--json`` envelope CONFORMS to
  ``contracts/stackunderflow-memory-v1/schema.json`` (validated with the same
  stdlib checker CI runs) and tags ``command: context-replay``;
* the ``--at`` cutoff is honoured in the JSON results;
* the running ``cumulative_tokens`` total is monotonic in the JSON results;
* an unknown session is an empty-but-valid envelope, exit 0 (never an error);
* ``--limit`` and ``--context-budget`` trim + set ``truncated``;
* the ``--project`` same-project fence.

The real ``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema

# <repo>/tests/stackunderflow/cli/test_context_replay_cli.py -> parents[3] == <repo>.
_REPO_ROOT = Path(__file__).resolve().parents[3]
_SCHEMA_PATH = _REPO_ROOT / "contracts" / "stackunderflow-memory-v1" / "schema.json"

_CORE_ENVELOPE_FIELDS = {
    "schema", "command", "query", "results",
    "result_count", "token_estimate", "budget", "truncated",
}


def _load_checker():
    """Import ``scripts/check_memory_contract.py`` (not an installed package)."""
    spec = importlib.util.spec_from_file_location(
        "check_memory_contract", _REPO_ROOT / "scripts" / "check_memory_contract.py"
    )
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


checker = _load_checker()
_SCHEMA = json.loads(_SCHEMA_PATH.read_text())


# ── seeding ─────────────────────────────────────────────────────────────────


def _seed(store_db, *, slug="proj-a", session_id="s1"):
    conn = db.connect(store_db)
    schema.apply(conn)
    pid = int(conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, "
        " last_modified) VALUES ('claude', ?, ?, 0.0, 1.0)", (slug, slug),
    ).lastrowid)
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, '2026-05-01T00:00:00Z', "
        "'2026-05-01T01:00:00Z', 0)", (pid, session_id),
    )
    sfk = int(conn.execute(
        "SELECT id FROM sessions WHERE session_id = ?", (session_id,)
    ).fetchone()["id"])
    turns = [
        ("user", "implement the feature",
         {"type": "user", "message": {"role": "user",
          "content": [{"type": "text", "text": "implement the feature"}]}}),
        ("assistant", "",
         {"type": "assistant", "message": {"role": "assistant", "content": [
             {"type": "tool_use", "id": "t1", "name": "Edit",
              "input": {"file_path": "a.py", "old_string": "x", "new_string": "y"}}]}}),
        ("user", "thanks that worked",
         {"type": "user", "message": {"role": "user",
          "content": [{"type": "text", "text": "thanks that worked"}]}}),
    ]
    for seq, (role, content, raw) in enumerate(turns):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain, uuid, parent_uuid) VALUES "
            "(?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, ?, '[]', ?, 0, ?, NULL)",
            (sfk, seq, f"2026-05-01T00:{seq:02d}:00Z", role, content,
             json.dumps(raw), f"u{seq}"),
        )
    conn.commit()
    conn.close()


def _run(tmp_path, monkeypatch, args):
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr(deps, "store_path", store_db)
    monkeypatch.setattr(deps, "current_log_path", None)
    return CliRunner().invoke(cli, ["context-replay", *args])


# ── text ────────────────────────────────────────────────────────────────────


def test_text_summary(tmp_path, monkeypatch):
    res = _run(tmp_path, monkeypatch, ["s1"])
    assert res.exit_code == 0, res.output
    assert "Context replay for s1" in res.output
    assert "messages: 3" in res.output
    assert "Edit a.py" in res.output


# ── json envelope + schema conformance ──────────────────────────────────────


def test_json_conforms_to_contract_schema(tmp_path, monkeypatch):
    res = _run(tmp_path, monkeypatch, ["s1", "--json"])
    assert res.exit_code == 0, res.output
    env = json.loads(res.output)
    errors = checker.validate(env, _SCHEMA, _SCHEMA)
    assert errors == [], errors
    assert env["schema"] == "stackunderflow.memory/1"
    assert env["command"] == "context-replay"
    assert _CORE_ENVELOPE_FIELDS <= set(env)
    assert env["result_count"] == len(env["results"]) == 3
    # documented extras for this command
    assert env["session_id"] == "s1"
    assert env["message_count"] == 3
    assert isinstance(env["total_tokens"], int)


def test_json_at_cutoff_is_honoured(tmp_path, monkeypatch):
    res = _run(tmp_path, monkeypatch, ["s1", "--at", "1", "--json"])
    env = json.loads(res.output)
    assert [e["seq"] for e in env["results"]] == [0, 1]
    assert checker.validate(env, _SCHEMA, _SCHEMA) == []


def test_json_cumulative_is_monotonic(tmp_path, monkeypatch):
    res = _run(tmp_path, monkeypatch, ["s1", "--json"])
    env = json.loads(res.output)
    cum = [e["cumulative_tokens"] for e in env["results"]]
    assert cum == sorted(cum), cum


def test_unknown_session_is_empty_envelope_exit_0(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr(deps, "store_path", store_db)
    monkeypatch.setattr(deps, "current_log_path", None)
    res = CliRunner().invoke(cli, ["context-replay", "ghost", "--json"])
    assert res.exit_code == 0, res.output
    env = json.loads(res.output)
    assert env["result_count"] == 0
    assert env["results"] == []
    assert any("not found" in w for w in env["warnings"])
    assert checker.validate(env, _SCHEMA, _SCHEMA) == []


# ── limit / budget trimming ─────────────────────────────────────────────────


def test_limit_trims_and_marks_truncated(tmp_path, monkeypatch):
    res = _run(tmp_path, monkeypatch, ["s1", "--limit", "2", "--json"])
    env = json.loads(res.output)
    assert env["result_count"] == 2
    assert [e["seq"] for e in env["results"]] == [0, 1]
    assert env["truncated"] is True
    # message_count still reflects the FULL reconstruction, not the trim.
    assert env["message_count"] == 3


def test_context_budget_packs_and_marks_truncated(tmp_path, monkeypatch):
    # budget=1 admits only the first event, then stops.
    res = _run(tmp_path, monkeypatch, ["s1", "--context-budget", "1", "--json"])
    env = json.loads(res.output)
    assert env["budget"] == 1
    assert env["truncated"] is True
    assert env["result_count"] < 3


# ── same-project fence ──────────────────────────────────────────────────────


def test_project_fence_excludes_other_project_session(tmp_path, monkeypatch):
    res = _run(tmp_path, monkeypatch, ["s1", "--project", "proj-b", "--json"])
    env = json.loads(res.output)
    assert env["result_count"] == 0
    assert any("outside project" in w for w in env["warnings"])
    assert checker.validate(env, _SCHEMA, _SCHEMA) == []


def test_project_fence_allows_own_project(tmp_path, monkeypatch):
    res = _run(tmp_path, monkeypatch, ["s1", "--project", "proj-a", "--json"])
    env = json.loads(res.output)
    assert env["result_count"] == 3
