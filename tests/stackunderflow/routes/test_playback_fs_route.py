"""Tests for ``GET /api/playback/{session_id}/fs`` — v2 virtual-FS route.

Mounts the playback router against a fresh schema-applied store and
seeds Read / Write / Edit messages directly. Locks the JSON contract:

* response shape (``session_id``, ``snapshot_ts``, ``files``, ``warnings``);
* 404 when the session id isn't in the store;
* 422 when ``at`` can't be parsed;
* ``include_content=false`` strips ``content`` but keeps the metadata;
* ``paths=`` filter narrows the files map;
* the ``at`` cutoff is honoured at the route boundary.
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


# ── seed helper ─────────────────────────────────────────────────────────────


def _seed(
    store_db,
    *,
    slug: str = "demo",
    session_id: str = "sess-1",
    triples: list[tuple[str, dict, str]] | None = None,
):
    """Seed one project + session with ``triples`` =
    ``(tool_name, tool_input, tool_result_text)``."""
    triples = triples or [("Write", {"file_path": "a.py", "content": "x"}, "ok")]
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
        for i, (tname, tinp, restxt) in enumerate(triples):
            tuid = f"tu{i}"
            for role, raw, ts_minute in (
                ("assistant", {"type": "assistant", "message": {"role": "assistant",
                    "content": [{"type": "tool_use", "id": tuid, "name": tname, "input": tinp}]}},
                 minute),
                ("user", {"type": "user", "message": {"role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": tuid,
                                 "content": restxt, "is_error": False}]}},
                 minute + 1),
            ):
                ts = f"2026-05-01T00:{ts_minute:02d}:00Z"
                conn.execute(
                    "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
                    " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
                    " raw_json, is_sidechain, uuid, parent_uuid) "
                    "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, '', '[]', ?, 0, ?, NULL)",
                    (sfk, seq, ts, role, json.dumps(raw), f"u{seq}"),
                )
                seq += 1
            minute += 2
        conn.commit()
        return pid
    finally:
        conn.close()


_AT_LATE = "2026-05-02T00:00:00Z"


# ── happy path ──────────────────────────────────────────────────────────────


def test_fs_snapshot_returns_contract_shape(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s1", triples=[
        ("Read", {"file_path": "src/a.py"}, "x = 1\n"),
        ("Edit", {"file_path": "src/a.py", "old_string": "x = 1",
                  "new_string": "x = 2"}, "ok"),
    ])
    resp = client.get("/api/playback/s1/fs", params={"at": _AT_LATE})
    assert resp.status_code == 200
    body = resp.json()
    assert set(body) == {"session_id", "snapshot_ts", "files", "warnings"}
    assert body["session_id"] == "s1"
    assert body["snapshot_ts"] == _AT_LATE
    assert set(body["files"]) == {"src/a.py"}
    f = body["files"]["src/a.py"]
    assert set(f) == {
        "content", "byte_count", "last_modified_ts",
        "operations_applied", "reconstruction_complete",
    }
    assert f["content"] == "x = 2\n"
    assert f["reconstruction_complete"] is True
    assert f["operations_applied"] == ["Read#0", "Edit#0"]


def test_fs_snapshot_empty_session_returns_empty_files(app_client):
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
    resp = client.get("/api/playback/empty-sess/fs", params={"at": _AT_LATE})
    assert resp.status_code == 200
    body = resp.json()
    assert body["files"] == {}
    assert body["warnings"] == []


# ── 404 / 422 ───────────────────────────────────────────────────────────────


def test_fs_snapshot_unknown_session_returns_404(app_client):
    client, _ = app_client
    resp = client.get("/api/playback/no-such/fs", params={"at": _AT_LATE})
    assert resp.status_code == 404
    assert "no-such" in resp.json()["detail"]


def test_fs_snapshot_unparseable_at_returns_422(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s2")
    resp = client.get("/api/playback/s2/fs", params={"at": "not-a-timestamp"})
    assert resp.status_code == 422
    # The body shape is FastAPI's default for HTTPException — detail string.
    assert "not-a-timestamp" in resp.json()["detail"]


def test_fs_snapshot_missing_at_param_returns_422(app_client):
    """``at`` is required — FastAPI returns 422 when it's absent."""
    client, store_db = app_client
    _seed(store_db, session_id="s3")
    resp = client.get("/api/playback/s3/fs")
    assert resp.status_code == 422


# ── include_content toggle ──────────────────────────────────────────────────


def test_fs_snapshot_include_content_false_strips_body(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s4", triples=[
        ("Write", {"file_path": "big.py", "content": "x" * 500}, "ok"),
    ])
    on = client.get("/api/playback/s4/fs", params={"at": _AT_LATE}).json()
    off = client.get(
        "/api/playback/s4/fs",
        params={"at": _AT_LATE, "include_content": "false"},
    ).json()
    assert on["files"]["big.py"]["content"] == "x" * 500
    assert "content" not in off["files"]["big.py"]
    # Metadata identical otherwise.
    assert off["files"]["big.py"]["byte_count"] == 500
    assert off["files"]["big.py"]["operations_applied"] == ["Write#0"]


# ── paths filter ────────────────────────────────────────────────────────────


def test_fs_snapshot_paths_filter_narrows_files(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s5", triples=[
        ("Write", {"file_path": "a.py", "content": "aa"}, "ok"),
        ("Write", {"file_path": "b.py", "content": "bb"}, "ok"),
        ("Write", {"file_path": "c.py", "content": "cc"}, "ok"),
    ])
    body = client.get(
        "/api/playback/s5/fs",
        params={"at": _AT_LATE, "paths": "a.py,c.py"},
    ).json()
    assert set(body["files"]) == {"a.py", "c.py"}


# ── at cutoff ───────────────────────────────────────────────────────────────


def test_fs_snapshot_at_cutoff_excludes_later_edits(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s6", triples=[
        # Pair 0: assistant 00:00, result 00:01
        ("Read", {"file_path": "z.py"}, "x = 1\n"),
        # Pair 1: assistant 00:02, result 00:03
        ("Edit", {"file_path": "z.py", "old_string": "x = 1",
                  "new_string": "x = 2"}, "ok"),
        # Pair 2: assistant 00:04, result 00:05
        ("Edit", {"file_path": "z.py", "old_string": "x = 2",
                  "new_string": "x = 3"}, "ok"),
    ])
    body = client.get(
        "/api/playback/s6/fs",
        params={"at": "2026-05-01T00:03:00Z"},
    ).json()
    # 00:03 includes Read (00:00) and Edit#0 (00:02) but NOT Edit#1 (00:04).
    f = body["files"]["z.py"]
    assert f["content"] == "x = 2\n"
    assert f["operations_applied"] == ["Read#0", "Edit#0"]


# ── warning surface ─────────────────────────────────────────────────────────


def test_fs_snapshot_edit_without_read_surfaces_warning(app_client):
    client, store_db = app_client
    _seed(store_db, session_id="s7", triples=[
        ("Edit", {"file_path": "v.py", "old_string": "old",
                  "new_string": "new"}, "ok"),
    ])
    body = client.get(
        "/api/playback/s7/fs", params={"at": _AT_LATE},
    ).json()
    assert body["files"]["v.py"]["reconstruction_complete"] is False
    assert any("no initial Read or Write" in w for w in body["warnings"])


# ── risk overlay (Spec 16) ──────────────────────────────────────────────────


def _seed_failing_history(store_db, *, file_path: str = "/x/cost.py") -> None:
    """Seed a separate session that *failed* an edit on ``file_path``.

    The risk overlay walks ``messages`` for the file's path; the past
    failure-mode session does NOT need to be the current snapshot's
    session — it just needs to exist in the store.
    """
    conn = db.connect(store_db)
    pid_row = conn.execute(
        "SELECT id FROM projects WHERE slug = 'p-fail'"
    ).fetchone()
    if pid_row is None:
        pid = int(
            conn.execute(
                "INSERT INTO projects (provider, slug, display_name, "
                " first_seen, last_modified) VALUES "
                "('claude', 'p-fail', 'p-fail', 0.0, 1.0)"
            ).lastrowid
        )
    else:
        pid = int(pid_row["id"])
    sfk = int(
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
            " message_count) VALUES (?, 'past-fail', "
            "'2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z', 2)",
            (pid,),
        ).lastrowid
    )
    edit_blob = json.dumps([
        {"name": "Edit", "input": {"file_path": file_path}}
    ])
    for seq, (role, content_text, tools_json) in enumerate([
        ("assistant", "", edit_blob),
        ("user", "no, that broke the cost endpoint", "[]"),
    ]):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES "
            "(?, ?, '2026-04-01T00:00:00Z', ?, 'claude-sonnet-4-5', "
            " 0, 0, 0, 0, ?, ?, '{}', 0)",
            (sfk, seq, role, content_text, tools_json),
        )
    conn.commit()
    conn.close()


def test_fs_snapshot_emits_risk_block_when_file_has_history(app_client):
    """Spec 16 — files with ≥ 1 reverted/failed past session get a ``risk`` overlay."""
    client, store_db = app_client
    file_path = "/x/cost.py"
    _seed_failing_history(store_db, file_path=file_path)
    # Now seed the *current* snapshot session that touches the same file.
    _seed(store_db, session_id="now-sess", triples=[
        ("Write", {"file_path": file_path, "content": "y = 2"}, "ok"),
    ])
    body = client.get(
        "/api/playback/now-sess/fs", params={"at": _AT_LATE},
    ).json()
    assert file_path in body["files"]
    risk = body["files"][file_path].get("risk")
    assert risk is not None
    assert risk["failed_count"] >= 1
    assert set(risk) == {
        "reverted_count", "failed_count", "worked_count", "total_sessions",
    }


def test_fs_snapshot_no_risk_block_on_clean_history(app_client):
    """Files without any failure-mode history must NOT carry a ``risk`` key
    (the badge is rendered conditionally on its presence)."""
    client, store_db = app_client
    _seed(store_db, session_id="clean-sess", triples=[
        ("Write", {"file_path": "fresh.py", "content": "z = 0"}, "ok"),
    ])
    body = client.get(
        "/api/playback/clean-sess/fs", params={"at": _AT_LATE},
    ).json()
    assert "fresh.py" in body["files"]
    assert "risk" not in body["files"]["fresh.py"]
