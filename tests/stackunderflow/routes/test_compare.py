"""Tests for ``GET /api/compare``."""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.compare import get_compare
from stackunderflow.store import db, schema

# ── seeding helper ──────────────────────────────────────────────────────────


def _seed(store_db, *, projects, messages):
    conn = db.connect(store_db)
    schema.apply(conn)
    project_pk = {}
    for prov, slug in projects:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            (prov, slug, slug, 0.0, 0.0),
        )
        project_pk[(prov, slug)] = cur.lastrowid
    sess_pk: dict = {}
    seq_counter: dict[int, int] = {}
    for m in messages:
        prov = m.get("provider", "claude")
        slug = m["project_slug"]
        ppk = project_pk[(prov, slug)]
        sk = (ppk, m["session_id"])
        if sk not in sess_pk:
            cur = conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, ?, ?, ?)",
                (ppk, m["session_id"], m["timestamp"], m["timestamp"], 0),
            )
            sess_pk[sk] = cur.lastrowid
        sfk = sess_pk[sk]
        seq = seq_counter.get(sfk, 0)
        seq_counter[sfk] = seq + 1
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                sfk, seq, m["timestamp"], m["role"], m.get("model"),
                m.get("in_tok", 0), m.get("out_tok", 0),
                m.get("cache_w", 0), m.get("cache_r", 0),
                "", "[]", "{}", 0, None, None,
            ),
        )
    conn.commit()
    conn.close()


# ── happy path ───────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_happy_path_returns_models(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("codex", "gamma")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:01Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:01Z", "role": "assistant",
             "model": "gpt-X", "in_tok": 50, "out_tok": 25},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_compare(period="all")
    assert payload["period"] == "all"
    assert isinstance(payload["models"], list)
    models = {m["model"] for m in payload["models"]}
    assert models == {"claude-A", "gpt-X"}
    assert isinstance(payload["generated"], float)


@pytest.mark.asyncio
async def test_provider_filter(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("codex", "gamma")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "assistant",
             "model": "gpt-X", "in_tok": 50, "out_tok": 25},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_compare(period="all", provider="claude")
    models = {m["model"] for m in payload["models"]}
    assert models == {"claude-A"}


@pytest.mark.asyncio
async def test_project_filter(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("claude", "beta")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50},
            {"project_slug": "beta", "session_id": "B1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "assistant",
             "model": "claude-B", "in_tok": 200, "out_tok": 100},
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_compare(period="all", project=["alpha"])
    models = {m["model"] for m in payload["models"]}
    assert models == {"claude-A"}


# ── empty DB ────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_empty_db_returns_empty_models(tmp_path, monkeypatch):
    store_db = tmp_path / "empty.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_compare(period="all")
    assert payload["models"] == []
    assert payload["period"] == "all"


# ── invalid period ──────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_invalid_period_400(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    with pytest.raises(HTTPException) as exc_info:
        await get_compare(period="yesterday")
    assert exc_info.value.status_code == 400


# ── route registration ──────────────────────────────────────────────────────


def test_compare_route_registered_on_app():
    from stackunderflow.server import app

    from tests.conftest import app_route_paths

    assert "/api/compare" in app_route_paths(app)
