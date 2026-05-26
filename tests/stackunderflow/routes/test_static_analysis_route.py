"""Tests for ``stackunderflow.routes.static_analysis``.

Spec 21 / issue #93. Mirrors the pattern in
:mod:`tests.stackunderflow.routes.test_playback_route`: monkeypatch
``deps.store_path`` to a tmp store, seed minimal fixtures, hit the
endpoint via ``TestClient``.
"""

from __future__ import annotations

import json
from pathlib import Path

from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.server import app
from stackunderflow.services import static_analysis
from stackunderflow.store import db, schema


def _seed_python_session(store_db: Path, session_id: str = "rt-1") -> None:
    """Seed + analyze a single Python session in the given store."""
    conn = db.connect(store_db)
    schema.apply(conn)
    pcur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES "
        "('claude', '-Users-yad-dev-rt', NULL, 'rt', 0.0, 0.0)"
    )
    pid = int(pcur.lastrowid)
    first_ts = "2026-04-01T00:00:00+00:00"
    last_ts = "2026-04-01T00:01:00+00:00"
    sfk_cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, 4)",
        (pid, session_id, first_ts, last_ts),
    )
    sfk = int(sfk_cur.lastrowid)

    pre = "def f(x):\n    return x\n"
    post = "def f(x: int) -> int:\n    return x\n"
    file_path = "/tmp/rt_ex.py"
    msgs = [
        ("assistant", first_ts, {
            "type": "assistant", "timestamp": first_ts,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "r1", "name": "Read",
                 "input": {"file_path": file_path}},
            ]},
        }),
        ("user", first_ts, {
            "type": "user", "timestamp": first_ts,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "r1", "content": pre},
            ]},
        }),
        ("assistant", last_ts, {
            "type": "assistant", "timestamp": last_ts,
            "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "w1", "name": "Write",
                 "input": {"file_path": file_path, "content": post}},
            ]},
        }),
        ("user", last_ts, {
            "type": "user", "timestamp": last_ts,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "w1", "content": "ok"},
            ]},
        }),
    ]
    for seq, (role, ts, env) in enumerate(msgs):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', "
            " 0, 0, 0, 0, '', '[]', ?, 0)",
            (sfk, seq, ts, role, json.dumps(env)),
        )
    conn.commit()
    # Run the analyzer once so the route has something to return.
    static_analysis.analyze_session(conn, session_id)
    conn.close()


class TestSessionStaticAnalysisRoute:
    def test_unknown_session_returns_empty_payload(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        monkeypatch.setattr(deps, "store_path", store_db)
        client = TestClient(app)
        r = client.get("/api/static-analysis/session/no-such-session")
        # 200 + empty findings — see route docstring rationale.
        assert r.status_code == 200, r.text
        payload = r.json()
        assert payload["session_id"] == "no-such-session"
        assert payload["findings"] == []
        assert payload["summary"]["metrics"] == {}

    def test_known_session_returns_findings(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_python_session(store_db, session_id="rt-1")
        monkeypatch.setattr(deps, "store_path", store_db)
        client = TestClient(app)
        r = client.get("/api/static-analysis/session/rt-1")
        assert r.status_code == 200, r.text
        payload = r.json()
        assert payload["session_id"] == "rt-1"
        # Type completeness ran (it's pure-Python AST, always available)
        # and we expect at least one finding.
        assert len(payload["findings"]) >= 1
        assert "type_completeness" in payload["summary"]["metrics"]

    def test_response_shape_keys(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        monkeypatch.setattr(deps, "store_path", store_db)
        client = TestClient(app)
        r = client.get("/api/static-analysis/session/anything")
        assert r.status_code == 200
        payload = r.json()
        assert set(payload) == {"session_id", "findings", "summary"}
        assert set(payload["summary"]) == {"files", "languages", "metrics", "headline"}
