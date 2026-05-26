"""CLI tests for ``stackunderflow analyze {session, backfill}``.

Spec 21 — per-session static analysis pass (issue #93). Mirrors the
pattern in :mod:`tests.stackunderflow.cli.test_risk_cli`: monkeypatch
``deps.store_path`` to a tmp store, seed a minimal fixture, run the
command through ``CliRunner``.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema


def _seed_python_session(
    store_db: Path,
    *,
    session_id: str = "py-cli-1",
    pre: str = "def f(x):\n    return x\n",
    post: str = "def f(x: int) -> int:\n    return x\n",
    file_path: str = "/tmp/ex.py",
) -> None:
    """Seed a project + session + a Read/Write pair (pre/post deltas).

    Timestamps are intentionally far in the future so ``--since 1m``
    filter tests always include the row regardless of when the test runs.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    pcur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-cli', NULL, 'cli', 0.0, 0.0)"
    )
    pid = int(pcur.lastrowid)
    first_ts = "2099-04-01T00:00:00+00:00"
    last_ts = "2099-04-01T00:01:00+00:00"
    sfk_cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, 4)",
        (pid, session_id, first_ts, last_ts),
    )
    sfk = int(sfk_cur.lastrowid)

    read_use_id = f"r_{session_id}"
    read_use = {
        "type": "assistant", "timestamp": first_ts,
        "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": read_use_id, "name": "Read",
             "input": {"file_path": file_path}},
        ]},
    }
    read_res = {
        "type": "user", "timestamp": first_ts,
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": read_use_id, "content": pre},
        ]},
    }
    write_use_id = f"w_{session_id}"
    write_use = {
        "type": "assistant", "timestamp": last_ts,
        "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": write_use_id, "name": "Write",
             "input": {"file_path": file_path, "content": post}},
        ]},
    }
    write_res = {
        "type": "user", "timestamp": last_ts,
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": write_use_id, "content": "ok"},
        ]},
    }
    for seq, (role, env_obj, ts) in enumerate([
        ("assistant", read_use, first_ts),
        ("user", read_res, first_ts),
        ("assistant", write_use, last_ts),
        ("user", write_res, last_ts),
    ]):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', "
            " 0, 0, 0, 0, '', '[]', ?, 0)",
            (sfk, seq, ts, role, json.dumps(env_obj)),
        )
    conn.commit()
    conn.close()


def _invoke(runner: CliRunner, args: list[str], store_db: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    return runner.invoke(cli, args)


class TestAnalyzeSession:
    def test_text_output_known_session(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_python_session(store_db)
        runner = CliRunner()
        r = _invoke(runner, ["analyze", "session", "py-cli-1"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "py-cli-1" in r.output
        assert "files analyzed" in r.output
        assert "languages" in r.output

    def test_json_output_known_session(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_python_session(store_db)
        runner = CliRunner()
        r = _invoke(
            runner, ["analyze", "session", "py-cli-1", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        payload = json.loads(r.output)
        assert payload["session_id"] == "py-cli-1"
        assert set(payload) >= {
            "session_id", "files_analyzed", "rows_written",
            "languages", "warnings", "skipped_files",
        }

    def test_unknown_session_no_crash(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        runner = CliRunner()
        r = _invoke(
            runner, ["analyze", "session", "does-not-exist"],
            store_db, monkeypatch,
        )
        # Unknown session is fine — we just produce zero rows.
        assert r.exit_code == 0
        assert "files analyzed: 0" in r.output

    def test_language_filter_repeatable(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_python_session(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["analyze", "session", "py-cli-1", "--language", "go", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0
        payload = json.loads(r.output)
        # The python file is filtered out by --language go ⇒ 0 analyzed.
        assert payload["files_analyzed"] == 0


class TestAnalyzeBackfill:
    def test_empty_store_text(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        runner = CliRunner()
        r = _invoke(runner, ["analyze", "backfill"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "candidates:    0" in r.output

    def test_with_seeded_session_json(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_python_session(store_db)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["analyze", "backfill", "--since", "1m", "--concurrency", "1",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        payload = json.loads(r.output)
        assert payload["candidates"] >= 1
        assert payload["analyzed"] >= 1

    def test_backfill_idempotent_re_run(self, tmp_path, monkeypatch):
        """Second backfill against the same store has no candidates."""
        store_db = tmp_path / "store.db"
        _seed_python_session(store_db)
        runner = CliRunner()
        r1 = _invoke(
            runner,
            ["analyze", "backfill", "--since", "1m", "--concurrency", "1",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r1.exit_code == 0
        p1 = json.loads(r1.output)
        assert p1["analyzed"] >= 1

        r2 = _invoke(
            runner,
            ["analyze", "backfill", "--since", "1m", "--concurrency", "1",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r2.exit_code == 0
        p2 = json.loads(r2.output)
        assert p2["candidates"] == 0
        assert p2["analyzed"] == 0

    def test_since_invalid_raises(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        runner = CliRunner()
        r = _invoke(
            runner, ["analyze", "backfill", "--since", "yesterday-ish"],
            store_db, monkeypatch,
        )
        # parse_since rejects ⇒ click.BadParameter ⇒ exit code 2.
        assert r.exit_code != 0
