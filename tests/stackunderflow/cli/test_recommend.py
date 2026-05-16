"""End-to-end tests for ``stackunderflow recommend mode`` (Spec 18 v1).

Exercises both text + json output, the ``--current-model`` cost-delta,
the ``--no-cache`` flag, and the empty-store behaviour.
"""

from __future__ import annotations

import json

from click.testing import CliRunner

from stackunderflow.cli import cli
from stackunderflow.store import db, schema


def _seed(store_db, sessions):
    """Seed projects, sessions, messages, session_mart."""
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'p1', 'p1', 0, 0)"
    )
    pid = conn.execute("SELECT id FROM projects").fetchone()[0]
    for sid, model, cost, prompt in sessions:
        cur = conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, ?, ?, ?)",
            (pid, sid, "2026-04-01T10:00:00Z", "2026-04-01T11:00:00Z", 2),
        )
        sfk = cur.lastrowid
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, content_text, raw_json) "
            "VALUES (?, 0, ?, ?, ?, ?)",
            (sfk, "2026-04-01T10:00:00Z", "user", prompt, "{}"),
        )
        conn.execute(
            "INSERT INTO session_mart (session_id, project_id, provider, primary_model, "
            "first_ts, last_ts, message_count, cost_usd) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            (sid, pid, "claude", model,
             "2026-04-01T10:00:00Z", "2026-04-01T11:00:00Z", 2, cost),
        )
    conn.commit()
    conn.close()


def _invoke(runner, args, store_db, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    return runner.invoke(cli, args)


_SEEDED = [
    ("s1", "sonnet", 0.05, "fix the failing test in foo.py"),
    ("s2", "sonnet", 0.04, "debug the broken pytest in bar.py"),
    ("s3", "sonnet", 0.06, "fix bug in baz.py"),
    ("s4", "opus",   0.45, "fix the failing test in x.py"),
    ("s5", "opus",   0.55, "fix bug in y.py pytest"),
]


class TestText:
    def test_recommend_renders_pick(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db, _SEEDED)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["recommend", "mode",
             "--prompt", "fix the broken test in qux.py pytest",
             "--current-model", "opus"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "sonnet" in r.output
        assert "Confidence" in r.output
        assert "Estimated savings" in r.output

    def test_empty_store_clean_message(self, tmp_path, monkeypatch):
        store_db = tmp_path / "empty.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        runner = CliRunner()
        r = _invoke(
            runner,
            ["recommend", "mode", "--prompt", "fix the broken test"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        assert "Confidence:         0.00" in r.output


class TestJson:
    def test_json_shape(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db, _SEEDED)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["recommend", "mode",
             "--prompt", "fix the broken test in qux.py pytest",
             "--current-model", "opus",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        payload = json.loads(r.output)
        for key in (
            "recommended_model", "current_model", "confidence",
            "cost_delta_usd", "similar_session_count",
            "evidence_session_ids", "features", "task_pattern_hash",
            "rationale", "cache_hit",
        ):
            assert key in payload
        assert payload["recommended_model"] == "sonnet"

    def test_no_cache_flag_recomputes(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db, _SEEDED)
        runner = CliRunner()
        # First call: miss, populates cache
        r1 = _invoke(
            runner,
            ["recommend", "mode",
             "--prompt", "fix the broken test in foo.py",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r1.exit_code == 0
        p1 = json.loads(r1.output)
        assert p1["cache_hit"] is False
        # Second call with --no-cache: still a miss
        r2 = _invoke(
            runner,
            ["recommend", "mode",
             "--prompt", "fix the broken test in foo.py",
             "--no-cache",
             "--format", "json"],
            store_db, monkeypatch,
        )
        p2 = json.loads(r2.output)
        assert p2["cache_hit"] is False
