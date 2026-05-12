"""Tests for ``/api/playback/*`` routes.

Mounts only the playback router against a fresh schema-applied store and
seeds ``messages`` rows directly. Locks the JSON contract:

* ``GET /api/playback/{session_id}`` → ``{session_id, events, total, truncated}``;
* ``tool_filter`` (comma-separated) + ``limit`` query params;
* 404 for an unknown session, 200 + empty list for a tool-call-free session;
* ``GET /api/playback/project/{slug}`` → ``{project_slug, events, total, truncated}``;
* ``since=7d`` relative parsing + 404 for an unknown slug.

See ``.notes/specs/10-playback-timeline.md``.
"""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.playback import router as playback_router
from stackunderflow.store import db, schema


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)
    app = FastAPI()
    app.include_router(playback_router)
    return TestClient(app), store_db


# ── seed helpers ─────────────────────────────────────────────────────────────


def _seed(store_db, *, slug="demo", session_id="sess-1", pairs=None):
    """Seed one project + session with ``pairs`` = list of ``(tool, input)``.

    Each pair becomes an assistant ``tool_use`` message followed by a user
    ``tool_result`` message (no error). Returns the project id.
    """
    pairs = pairs or [("Read", {"file_path": "a.py"})]
    conn = db.connect(store_db)
    try:
        pid = int(
            conn.execute(
                "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
                "VALUES ('claude', ?, ?, 0.0, 1.0)",
                (slug, slug),
            ).lastrowid
        )
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, '2026-05-01T00:00:00Z', '2026-05-01T01:00:00Z', 0)",
            (pid, session_id),
        )
        sfk = int(
            conn.execute(
                "SELECT id FROM sessions WHERE session_id = ?", (session_id,)
            ).fetchone()["id"]
        )
        seq = 0
        minute = 0
        for i, (tname, tinp) in enumerate(pairs):
            tuid = f"tu{i}"
            for role, raw in (
                ("assistant", {"type": "assistant", "message": {"role": "assistant", "content": [
                    {"type": "tool_use", "id": tuid, "name": tname, "input": tinp}]}}),
                ("user", {"type": "user", "message": {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": tuid, "content": f"r{i}", "is_error": False}]}}),
            ):
                ts = f"2026-05-01T00:{minute:02d}:00Z"
                minute += 1
                conn.execute(
                    "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
                    " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
                    " raw_json, is_sidechain, uuid, parent_uuid) "
                    "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, '', '[]', ?, 0, ?, NULL)",
                    (sfk, seq, ts, role, json.dumps(raw), f"u{seq}"),
                )
                seq += 1
        conn.commit()
        return pid
    finally:
        conn.close()


# ── session endpoint ─────────────────────────────────────────────────────────


def test_session_playback_returns_event_stream(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s1", pairs=[
        ("Read", {"file_path": "stackunderflow/routes/cost.py"}),
        ("Edit", {"file_path": "routes/cost.py", "old_string": "x", "new_string": "y"}),
        ("Bash", {"command": "pytest tests/ -q"}),
    ])
    resp = client.get("/api/playback/s1")
    assert resp.status_code == 200
    body = resp.json()
    assert body["session_id"] == "s1"
    assert body["total"] == 3
    assert body["truncated"] is False
    assert [e["tool_name"] for e in body["events"]] == ["Read", "Edit", "Bash"]
    assert [e["seq"] for e in body["events"]] == [0, 1, 2]
    assert body["events"][0]["summary"] == "Read routes/cost.py"
    assert body["events"][2]["summary"] == "Bash: pytest"
    # Every documented field is present on each event.
    for e in body["events"]:
        assert set(e) == {
            "seq", "ts", "message_id", "tool_name", "summary", "target_path",
            "byte_count", "success", "duration_ms", "payload_excerpt", "session_id",
        }


def test_session_playback_tool_filter_query_param(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s2", pairs=[
        ("Read", {"file_path": "a.py"}),
        ("Edit", {"file_path": "b.py", "old_string": "1", "new_string": "2"}),
        ("Bash", {"command": "ls"}),
        ("Edit", {"file_path": "c.py", "old_string": "3", "new_string": "4"}),
    ])
    resp = client.get("/api/playback/s2", params={"tool_filter": "Edit,Bash"})
    assert resp.status_code == 200
    body = resp.json()
    assert [(e["seq"], e["tool_name"]) for e in body["events"]] == [(1, "Edit"), (2, "Bash"), (3, "Edit")]
    assert body["total"] == 3


def test_session_playback_limit_and_truncated(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s3", pairs=[("Read", {"file_path": f"f{i}.py"}) for i in range(8)])
    resp = client.get("/api/playback/s3", params={"limit": 3})
    body = resp.json()
    assert body["total"] == 3
    assert body["truncated"] is True
    assert len(body["events"]) == 3


def test_session_playback_include_payload_toggle(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s4", pairs=[("Bash", {"command": "echo hi"})])
    on = client.get("/api/playback/s4").json()
    assert on["events"][0]["payload_excerpt"]  # default include_payload=True
    off = client.get("/api/playback/s4", params={"include_payload": "false"}).json()
    assert off["events"][0]["payload_excerpt"] == ""


def test_session_playback_empty_session_is_200_empty(app_client):
    client, store_db = app_client
    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'p', 'p', 0.0, 1.0)"
    )
    pid = conn.execute("SELECT id FROM projects WHERE slug = 'p'").fetchone()["id"]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 'empty-sess', '2026-05-01T00:00:00Z', '2026-05-01T00:01:00Z', 0)",
        (pid,),
    )
    conn.commit()
    conn.close()
    resp = client.get("/api/playback/empty-sess")
    assert resp.status_code == 200
    assert resp.json() == {"session_id": "empty-sess", "events": [], "total": 0, "truncated": False}


def test_session_playback_unknown_session_404(app_client):
    client, _ = app_client
    resp = client.get("/api/playback/does-not-exist")
    assert resp.status_code == 404
    assert "does-not-exist" in resp.json()["detail"]


def test_session_playback_limit_bounds(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s5")
    assert client.get("/api/playback/s5", params={"limit": 0}).status_code == 422
    assert client.get("/api/playback/s5", params={"limit": 999999}).status_code == 422


# ── project endpoint ─────────────────────────────────────────────────────────


def test_project_timeline_returns_stream(app_client):
    client, store_db = app_client
    pid = _seed(store_db, slug="myproj", session_id="ps1", pairs=[
        ("Read", {"file_path": "a.py"}),
        ("Edit", {"file_path": "b.py", "old_string": "1", "new_string": "2"}),
    ])
    assert pid  # sanity
    resp = client.get("/api/playback/project/myproj")
    assert resp.status_code == 200
    body = resp.json()
    assert body["project_slug"] == "myproj"
    assert body["total"] == 2
    assert body["truncated"] is False
    assert [e["tool_name"] for e in body["events"]] == ["Read", "Edit"]
    # include_payload defaults OFF on the project surface.
    assert all(e["payload_excerpt"] == "" for e in body["events"])
    on = client.get("/api/playback/project/myproj", params={"include_payload": "1"}).json()
    assert any(e["payload_excerpt"] for e in on["events"])


def test_project_timeline_since_and_tool_filter(app_client):
    client, store_db = app_client
    _seed(store_db, slug="pj2", session_id="ps2", pairs=[
        ("Read", {"file_path": "a.py"}),   # assistant @ 00:00
        ("Bash", {"command": "ls"}),       # assistant @ 00:02
        ("Edit", {"file_path": "c.py", "old_string": "x", "new_string": "y"}),  # assistant @ 00:04
    ])
    # since cuts off the first pair (whose assistant msg is at 00:00).
    resp = client.get("/api/playback/project/pj2", params={"since": "2026-05-01T00:01:00Z"})
    body = resp.json()
    assert [e["tool_name"] for e in body["events"]] == ["Bash", "Edit"]
    # tool_filter narrows further.
    resp = client.get("/api/playback/project/pj2", params={"tool_filter": "Edit"})
    assert [e["tool_name"] for e in resp.json()["events"]] == ["Edit"]


def test_project_timeline_relative_since_does_not_crash(app_client):
    client, store_db = app_client
    _seed(store_db, slug="pj3", session_id="ps3")
    # "7d" → an ISO instant a week ago; all our seeded data is older, so the
    # window is empty — but the request must still succeed.
    resp = client.get("/api/playback/project/pj3", params={"since": "7d"})
    assert resp.status_code == 200
    assert resp.json()["events"] == []


def test_project_timeline_unknown_slug_404(app_client):
    client, _ = app_client
    resp = client.get("/api/playback/project/nope")
    assert resp.status_code == 404
    assert "nope" in resp.json()["detail"]


def test_project_timeline_limit_pagination(app_client):
    client, store_db = app_client
    _seed(store_db, slug="pj4", session_id="ps4", pairs=[("Read", {"file_path": f"f{i}.py"}) for i in range(12)])
    body = client.get("/api/playback/project/pj4", params={"limit": 4}).json()
    assert body["total"] == 4
    assert body["truncated"] is True
