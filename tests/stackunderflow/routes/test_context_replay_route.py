"""Tests for ``GET /api/context-replay/{session_id}`` — context replay (#96).

Mounts the router against a fresh schema-applied store and seeds messages
directly. Locks the JSON contract:

* response shape + the ``at`` cutoff at the route boundary;
* an unknown session → 200 empty-but-valid (advisory, NOT 404);
* same-project fencing (cross-project → empty + warning; in-scope → full;
  no scope → full; ``deps.current_log_path`` as the default scope);
* the read-through cache is populated and self-invalidates on a new message.
"""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes import context_replay as context_replay_route
from stackunderflow.store import db, schema


@pytest.fixture(autouse=True)
def _clear_cache():
    """The route memoizes timelines process-wide — clear between tests."""
    context_replay_route._CONTEXT_CACHE.clear()
    yield
    context_replay_route._CONTEXT_CACHE.clear()


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)
    monkeypatch.setattr(deps, "current_log_path", None)
    app = FastAPI()
    app.include_router(context_replay_route.router)
    return TestClient(app), store_db


# ── seed helper ─────────────────────────────────────────────────────────────


def _seed(store_db, *, slug="proj-a", session_id="s1", turns=None):
    """Seed one project + session with ``turns`` = ``(role, content_text, raw)``."""
    if turns is None:
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
    conn = db.connect(store_db)
    pid_row = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()
    if pid_row is None:
        pid = int(conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, "
            " last_modified) VALUES ('claude', ?, ?, 0.0, 1.0)", (slug, slug),
        ).lastrowid)
    else:
        pid = int(pid_row["id"])
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, '2026-05-01T00:00:00Z', "
        "'2026-05-01T01:00:00Z', 0)", (pid, session_id),
    )
    sfk = int(conn.execute(
        "SELECT id FROM sessions WHERE session_id = ?", (session_id,)
    ).fetchone()["id"])
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
    return pid, sfk


# ── happy path + shape ──────────────────────────────────────────────────────


def test_returns_contract_shape_and_cutoff(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s1")
    body = client.get("/api/context-replay/s1", params={"at": 1}).json()
    assert set(body) == {
        "session_id", "at_seq", "message_count", "total_tokens",
        "events", "warnings",
    }
    assert body["session_id"] == "s1"
    assert body["at_seq"] == 1
    assert [e["seq"] for e in body["events"]] == [0, 1]
    ev = body["events"][0]
    assert set(ev) >= {
        "seq", "role", "content_preview", "tokens",
        "cumulative_tokens", "tool_calls",
    }
    # cumulative is monotonic at the route boundary too
    cum = [e["cumulative_tokens"] for e in body["events"]]
    assert cum == sorted(cum)


def test_no_at_returns_whole_session(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s1")
    body = client.get("/api/context-replay/s1").json()
    assert [e["seq"] for e in body["events"]] == [0, 1, 2]
    assert body["at_seq"] is None


# ── advisory: unknown session is 200 empty-but-valid (not 404) ──────────────


def test_unknown_session_is_200_empty(app_client):
    client, _ = app_client
    resp = client.get("/api/context-replay/no-such")
    assert resp.status_code == 200
    body = resp.json()
    assert body["events"] == []
    assert body["message_count"] == 0
    assert any("not found" in w for w in body["warnings"])


# ── same-project fencing ────────────────────────────────────────────────────


def test_cross_project_scope_is_fenced(app_client):
    client, store_db = app_client
    _seed(store_db, slug="proj-a", session_id="s1")
    _seed(store_db, slug="proj-b", session_id="s2")
    # s1 lives in proj-a; requesting it scoped to proj-b is fenced.
    body = client.get("/api/context-replay/s1", params={"project": "proj-b"}).json()
    assert body["events"] == []
    assert any("outside the active project scope" in w for w in body["warnings"])


def test_in_scope_project_returns_full(app_client):
    client, store_db = app_client
    _seed(store_db, slug="proj-a", session_id="s1")
    body = client.get("/api/context-replay/s1", params={"project": "proj-a"}).json()
    assert [e["seq"] for e in body["events"]] == [0, 1, 2]


def test_no_scope_returns_full(app_client):
    client, store_db = app_client
    _seed(store_db, slug="proj-a", session_id="s1")
    body = client.get("/api/context-replay/s1").json()
    assert [e["seq"] for e in body["events"]] == [0, 1, 2]


def test_current_log_path_is_the_default_fence(app_client, monkeypatch):
    client, store_db = app_client
    _seed(store_db, slug="proj-a", session_id="s1")
    # Active project is proj-b via current_log_path → s1 (proj-a) is fenced.
    monkeypatch.setattr(deps, "current_log_path", "/x/y/proj-b")
    body = client.get("/api/context-replay/s1").json()
    assert body["events"] == []
    assert body["warnings"]


# ── read-through cache ──────────────────────────────────────────────────────


def test_cache_is_populated_and_reused(app_client):
    client, store_db = app_client
    _pid, sfk = _seed(store_db, session_id="s1")
    context_replay_route._CONTEXT_CACHE.clear()
    b1 = client.get("/api/context-replay/s1", params={"at": 2}).json()
    key = (str(store_db), sfk)
    assert key in context_replay_route._CONTEXT_CACHE
    # A second scrub (different at) reuses the cached full build → same events.
    b2 = client.get("/api/context-replay/s1", params={"at": 1}).json()
    assert [e["seq"] for e in b2["events"]] == [0, 1]
    assert [e["seq"] for e in b1["events"]] == [0, 1, 2]


def test_cache_self_invalidates_on_new_message(app_client):
    client, store_db = app_client
    _pid, sfk = _seed(store_db, session_id="s1")
    first = client.get("/api/context-replay/s1").json()
    assert first["message_count"] == 3
    # Append a 4th message; the (max ts, count) signature changes → rebuild.
    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, 3, '2026-05-01T00:03:00Z', 'assistant', 'claude-sonnet-4-5', "
        " 0, 0, 0, 0, 'done', '[]', '{}', 0, 'u3', NULL)",
        (sfk,),
    )
    conn.commit()
    conn.close()
    again = client.get("/api/context-replay/s1").json()
    assert again["message_count"] == 4
    assert [e["seq"] for e in again["events"]] == [0, 1, 2, 3]
