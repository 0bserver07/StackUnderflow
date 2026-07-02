"""Tests for ``GET /api/forks`` — the fork/sidechain economics route."""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes import forks as forks_route
from stackunderflow.store import db, schema


@pytest.fixture(autouse=True)
def _clear_fork_cache():
    """The route memoizes reports process-wide — clear between tests so a
    cached entry from one store can't answer another."""
    forks_route._FORK_CACHE.clear()
    yield
    forks_route._FORK_CACHE.clear()


def _seed(store_db, *, slug="demo"):
    """Seed a store with one project + a fork/abandoned-sidechain session."""
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', ?, ?, 0, 0)",
        (slug, slug),
    )
    pid = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()["id"]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, message_count) VALUES (?, 'sess', 0)",
        (pid,),
    )
    sid = conn.execute("SELECT id FROM sessions WHERE session_id = 'sess'").fetchone()["id"]

    def add(seq, ts, role, uuid, parent, *, model="claude-opus-4-6", side=False, inp=0, out=0):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, speed) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, '', '[]', '{}', ?, ?, ?, 'standard')",
            (sid, seq, ts, role, model, inp, out, int(side), uuid, parent),
        )

    add(0, "2026-05-01T10:00:00+00:00", "user", "U0", None, model="")
    add(1, "2026-05-01T10:00:10+00:00", "assistant", "A0", "U0", inp=100)
    add(2, "2026-05-01T10:05:00+00:00", "user", "U1", "A0", model="")
    add(3, "2026-05-01T10:06:00+00:00", "assistant", "A1", "U1", out=200)
    add(4, "2026-05-01T10:00:30+00:00", "assistant", "B0", "A0", side=True, inp=300)
    add(5, "2026-05-01T10:01:00+00:00", "assistant", "B1", "B0", side=True, out=400)
    conn.commit()
    conn.close()


@pytest.mark.asyncio
async def test_forks_route_returns_report_and_warning(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    body = await forks_route.get_forks(period="all")

    assert body["period"] == "all"
    assert body["scope"] == "all time"
    assert body["warning"]
    assert "currency" in body

    report = body["report"]
    assert report["fork_point_count"] == 1
    assert report["abandoned_branch_count"] == 1
    assert report["sidechain_message_count"] == 2
    # Fork branch + sidechain both present with priced figures.
    assert report["sidechain_cost_usd"] > 0.0
    assert report["abandoned_branches"][0]["branch_head_uuid"] == "B0"


@pytest.mark.asyncio
async def test_forks_route_scopes_to_log_path_project(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db, slug="demo")
    # Add a second project whose sessions must NOT appear when we scope to demo.
    conn = db.connect(store_db)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'other', 'other', 0, 0)"
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    # log_path basename → slug 'demo'.
    body = await forks_route.get_forks(period="all", log_path="/logs/demo")
    assert body["report"]["fork_point_count"] == 1

    # An unknown project slug resolves to an empty scope (advisory, no 500).
    empty = await forks_route.get_forks(period="all", log_path="/logs/does-not-exist")
    assert empty["report"]["fork_point_count"] == 0
    assert empty["report"]["total_message_count"] == 0


@pytest.mark.asyncio
async def test_forks_route_converts_currency(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    # Baseline (USD, rate 1.0).
    base = await forks_route.get_forks(period="all")
    base_side = base["report"]["sidechain_cost_usd"]
    base_branch = base["report"]["abandoned_branches"][0]["cost_usd"]

    # Force a 2x FX rate and assert every dollar field scales.
    monkeypatch.setattr(
        "stackunderflow.routes.forks.active_currency_payload",
        lambda: {"code": "EUR", "symbol": "€", "rate_from_usd": 2.0, "warning": None},
    )
    converted = await forks_route.get_forks(period="all")
    assert converted["report"]["sidechain_cost_usd"] == pytest.approx(base_side * 2.0)
    assert converted["report"]["total_cost_usd"] == pytest.approx(base["report"]["total_cost_usd"] * 2.0)
    assert converted["report"]["abandoned_branches"][0]["cost_usd"] == pytest.approx(base_branch * 2.0)


@pytest.mark.asyncio
async def test_forks_route_rejects_bad_period(tmp_path, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", tmp_path / "store.db")
    with pytest.raises(HTTPException) as exc:
        await forks_route.get_forks(period="decade")
    assert exc.value.status_code == 400


def test_forks_route_registered_on_app():
    """The route is wired into the FastAPI app in server.py."""
    from tests.conftest import app_route_paths
    from stackunderflow.server import app

    assert "/api/forks" in app_route_paths(app)


@pytest.mark.asyncio
async def test_forks_route_memoizes_within_signature(tmp_path, monkeypatch):
    """Second call with no new ingest returns the cached report — the
    expensive ``analyze_forks`` runs exactly once."""
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    calls = {"n": 0}
    real = forks_route.analyze_forks

    def counting(conn, **kw):
        calls["n"] += 1
        return real(conn, **kw)

    monkeypatch.setattr(forks_route, "analyze_forks", counting)

    first = await forks_route.get_forks(period="all")
    second = await forks_route.get_forks(period="all")

    assert calls["n"] == 1  # second served from cache
    assert first["report"] == second["report"]


@pytest.mark.asyncio
async def test_forks_cache_invalidates_on_new_messages(tmp_path, monkeypatch):
    """A new ingest (session row + messages) moves the signature, so the next
    call recomputes rather than serving a stale report."""
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    calls = {"n": 0}
    real = forks_route.analyze_forks

    def counting(conn, **kw):
        calls["n"] += 1
        return real(conn, **kw)

    monkeypatch.setattr(forks_route, "analyze_forks", counting)

    await forks_route.get_forks(period="all")
    assert calls["n"] == 1

    # Simulate an ingest cycle: another session with messages bumps the
    # (max last_ts, summed message_count) signature.
    conn = db.connect(store_db)
    pid = conn.execute("SELECT id FROM projects LIMIT 1").fetchone()["id"]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, message_count, last_ts) "
        "VALUES (?, 'sess2', 2, '2026-06-01T00:00:00+00:00')",
        (pid,),
    )
    conn.commit()
    conn.close()

    await forks_route.get_forks(period="all")
    assert calls["n"] == 2  # signature changed → recomputed


@pytest.mark.asyncio
async def test_forks_cache_keyed_by_period(tmp_path, monkeypatch):
    """Different period selectors are distinct cache entries — a cached
    'all' report must not answer a 'today' request."""
    store_db = tmp_path / "store.db"
    _seed(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    calls = {"n": 0}
    real = forks_route.analyze_forks

    def counting(conn, **kw):
        calls["n"] += 1
        return real(conn, **kw)

    monkeypatch.setattr(forks_route, "analyze_forks", counting)

    await forks_route.get_forks(period="all")
    await forks_route.get_forks(period="today")
    assert calls["n"] == 2  # distinct scopes → two computes
