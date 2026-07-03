"""``/api/sync/status`` + ``/api/sync/overview`` — the Phase 2 read surface.

The overriding contract: the **default** ``this-device`` scope runs NO union and
returns a tiny stub even when remote rows are present, so the existing dashboard
path is byte-identical whether or not sync is enabled. Only an explicit
``?scope=all-devices`` (with sync on) computes the merged view.
"""

from __future__ import annotations

import pytest

from stackunderflow.routes import sync as sync_route
from stackunderflow.store import db, schema


def _seed(store_db, *, with_remote=True):
    """Local marts (project alpha) + optionally a peer 'dev-B' in the _remote tables."""
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'alpha', '/a', 'Alpha', 0, 0)"
    )
    conn.execute(
        "INSERT INTO daily_mart (day, project_id, provider, model, speed, input_tokens, "
        "output_tokens, cache_read, cache_create, message_count, session_count, cost_usd) "
        "VALUES ('2026-07-01', 1, 'claude', 'opus', 'standard', 100, 50, 0, 0, 3, 1, 1.5)"
    )
    conn.execute(
        "INSERT INTO project_mart (project_id, provider, slug, display_name, first_ts, last_ts, "
        "total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        "total_cache_read, total_cache_create, total_cost_usd) "
        "VALUES (1, 'claude', 'alpha', 'Alpha', '2026-07-01', '2026-07-01', 3, 1, 100, 50, 0, 0, 1.5)"
    )
    conn.execute(
        "INSERT INTO session_mart (session_id, project_id, provider, primary_model, first_ts, "
        "last_ts, message_count, user_message_count, assistant_message_count, input_tokens, "
        "output_tokens, cache_read, cache_create, cost_usd, is_one_shot, cwd) "
        "VALUES ('s-local', 1, 'claude', 'opus', '2026-07-01', '2026-07-01', 3, 1, 2, 100, 50, 0, 0, 1.5, 0, '/a')"
    )
    if with_remote:
        conn.execute(
            "INSERT INTO daily_mart_remote (device_uuid, day, provider, slug, model, speed, "
            "input_tokens, output_tokens, cache_read, cache_create, message_count, session_count, cost_usd) "
            "VALUES ('dev-B', '2026-07-01', 'claude', 'alpha', 'opus', 'standard', 200, 80, 0, 0, 4, 1, 2.5)"
        )
        conn.execute(
            "INSERT INTO project_mart_remote (device_uuid, provider, slug, display_name, first_ts, "
            "last_ts, total_messages, total_sessions, total_input_tokens, total_output_tokens, "
            "total_cache_read, total_cache_create, total_cost_usd) "
            "VALUES ('dev-B', 'claude', 'alpha', 'Alpha', '2026-07-01', '2026-07-01', 4, 1, 200, 80, 0, 0, 2.5)"
        )
        conn.execute(
            "INSERT INTO session_mart_remote (device_uuid, session_id, provider, slug, primary_model, "
            "first_ts, last_ts, message_count, user_message_count, assistant_message_count, "
            "input_tokens, output_tokens, cache_read, cache_create, cost_usd, is_one_shot) "
            "VALUES ('dev-B', 's-remote', 'claude', 'alpha', 'opus', '2026-07-01', '2026-07-01', "
            "4, 1, 3, 200, 80, 0, 0, 2.5, 0)"
        )
        conn.execute(
            "INSERT INTO sync_remote_devices (remote_device_uuid, alias, key_fingerprint, "
            "first_seen, last_seen, last_generation) VALUES ('dev-B', 'work-mac', 'fp', 't', 't', 3)"
        )
    conn.commit()
    conn.close()


def _enable_sync(store_db):
    conn = db.connect(store_db)
    conn.execute(
        "INSERT OR REPLACE INTO sync_identity (id, device_uuid, key_fingerprint, bucket_url, "
        "endpoint_url, layout_version, created_at) "
        "VALUES (1, 'dev-A', 'fp-A', 's3://b', NULL, 1, 't')"
    )
    conn.commit()
    conn.close()


def _prep(tmp_path, monkeypatch, *, with_remote=True, enabled=False):
    store_db = tmp_path / "store.db"
    _seed(store_db, with_remote=with_remote)
    if enabled:
        _enable_sync(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    return store_db


# ── status ──────────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_status_off_reports_disabled(tmp_path, monkeypatch):
    _prep(tmp_path, monkeypatch, with_remote=False, enabled=False)
    body = await sync_route.get_sync_status()
    assert body["enabled"] is False
    assert body["peers"] == []
    assert body["all_devices_available"] is False


@pytest.mark.asyncio
async def test_status_on_lists_peers_and_availability(tmp_path, monkeypatch):
    _prep(tmp_path, monkeypatch, with_remote=True, enabled=True)
    body = await sync_route.get_sync_status()
    assert body["enabled"] is True
    assert body["peer_count"] == 1
    assert body["peers"][0]["remote_device_uuid"] == "dev-B"
    assert body["peers"][0]["alias"] == "work-mac"
    assert body["remote_rows"] > 0
    assert body["all_devices_available"] is True


# ── overview: default this-device is inert ──────────────────────────────────────


@pytest.mark.asyncio
async def test_overview_default_scope_never_merges(tmp_path, monkeypatch):
    """Default scope returns the stub and merges nothing — even with peers pulled
    and sync enabled. This is the byte-identical default path."""
    _prep(tmp_path, monkeypatch, with_remote=True, enabled=True)
    body = await sync_route.get_sync_overview()          # no scope arg = default
    assert body["merged"] is False
    assert body["scope"] == "this-device"
    assert "totals" not in body                          # no union computed


@pytest.mark.asyncio
async def test_overview_all_devices_disabled_returns_stub(tmp_path, monkeypatch):
    """?scope=all-devices with sync OFF still returns the stub (nothing to merge)."""
    _prep(tmp_path, monkeypatch, with_remote=True, enabled=False)
    body = await sync_route.get_sync_overview(scope="all-devices")
    assert body["merged"] is False
    assert body["sync_enabled"] is False


# ── overview: opt-in merged view ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_overview_all_devices_merges_local_and_remote(tmp_path, monkeypatch):
    _prep(tmp_path, monkeypatch, with_remote=True, enabled=True)
    body = await sync_route.get_sync_overview(scope="all-devices")
    assert body["merged"] is True
    assert body["scope"] == "all-devices"
    # local(1.5) + remote(2.5) = 4.0 cost; disjoint sessions s-local + s-remote = 2.
    assert body["totals"]["cost_usd"] == pytest.approx(4.0)
    assert body["totals"]["input_tokens"] == 300
    assert body["totals"]["session_count"] == 2
    alpha = next(p for p in body["by_project"] if p["slug"] == "alpha")
    assert alpha["total_cost_usd"] == pytest.approx(4.0)
    device_ids = {d["device_uuid"] for d in body["devices"]}
    assert device_ids == {"(local)", "dev-B"}
    assert body["merge_warnings"] == 0
    assert body["currency"]["code"] == "USD"


@pytest.mark.asyncio
async def test_overview_all_devices_no_peers_is_local_only(tmp_path, monkeypatch):
    """Enabled but nothing pulled ⇒ merged view equals the local totals."""
    _prep(tmp_path, monkeypatch, with_remote=False, enabled=True)
    body = await sync_route.get_sync_overview(scope="all-devices")
    assert body["merged"] is True
    assert body["totals"]["cost_usd"] == pytest.approx(1.5)
    assert body["totals"]["session_count"] == 1
