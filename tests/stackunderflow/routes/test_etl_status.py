"""Tests for ``GET /api/etl/status`` — Wave 4C ETL status surface.

Locks the response shape (every key present, every type stable), the
``health`` enum's transition rules (live → syncing → stale → error
based on watermark deltas), and the watcher graceful-degrade behaviour
when ``deps.watcher_handle`` isn't populated.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
import stackunderflow.etl.backfill_jobs as backfill_jobs
from stackunderflow.routes.etl import router as etl_router
from stackunderflow.store import db, schema


# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    """Mount only the etl router with a fresh, schema-applied store."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()

    monkeypatch.setattr(deps, "store_path", store_db)
    # Default to no watcher handle — most tests want the "unknown" branch.
    monkeypatch.setattr(deps, "watcher_handle", None, raising=False)

    # Reset the per-session seq counter so tests across the module don't
    # collide on the shared dict.
    _SEQ_COUNTERS.clear()
    # Clear any leftover backfill slot from a sibling test module.
    backfill_jobs._reset_for_tests()

    app = FastAPI()
    app.include_router(etl_router)
    return TestClient(app), store_db


def _insert_project(conn, *, provider: str, slug: str) -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, *, project_id: int, session_id: str) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, "2026-04-01T00:00:00Z", "2026-04-01T00:00:00Z", 1),
    )
    return int(cur.lastrowid)


_SEQ_COUNTERS: dict[int, int] = {}


def _insert_message(conn, *, session_fk: int) -> int:
    """Insert a message; auto-increment ``seq`` per session so the
    ``UNIQUE(session_fk, seq)`` index is respected in the multi-event tests.

    v008: ``messages`` is a UNION-ALL view with an INSTEAD OF trigger.
    ``cur.lastrowid`` doesn't propagate the trigger's nested INSERT id,
    so we read the freshly-allocated id from ``_messages_id_seq``
    (``next_id - 1``).
    """
    seq = _SEQ_COUNTERS.get(session_fk, 0)
    _SEQ_COUNTERS[session_fk] = seq + 1
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, '2026-04-01T00:00:00Z', 'assistant', 'claude-sonnet-4-5',"
        " 0, 0, 0, 0, '', '[]', '{}', 0)",
        (session_fk, seq),
    )
    return int(conn.execute(
        "SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1"
    ).fetchone()[0])


def _insert_event(
    conn,
    *,
    source_message_fk: int,
    provider: str = "claude",
    project_id: int,
    session_id: str = "s1",
    cost_source: str = "rate_card",
) -> int:
    cur = conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, account, project_id, session_id, ts, day, "
        " model, speed, input_tokens, output_tokens, cache_read_tokens, "
        " cache_create_tokens, cost_usd, cost_source, role, raw_extras) "
        "VALUES (?, ?, 'default', ?, ?, '2026-04-01T00:00:00Z', '2026-04-01', "
        "'claude-sonnet-4-5', 'standard', 0, 0, 0, 0, 0.0, ?, 'assistant', NULL)",
        (source_message_fk, provider, project_id, session_id, cost_source),
    )
    return int(cur.lastrowid)


def _set_watermark(conn, *, mart_name: str, last_event_id: int, ts: str | None = None) -> None:
    if ts is None:
        ts = datetime.now(UTC).isoformat()
    conn.execute(
        "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts) "
        "VALUES (?, ?, ?) ON CONFLICT(mart_name) DO UPDATE SET "
        "last_event_id = excluded.last_event_id, "
        "last_refresh_ts = excluded.last_refresh_ts",
        (mart_name, last_event_id, ts),
    )


def _seed_one_event(store_db: Path) -> tuple[int, int]:
    """Insert one project/session/message/event. Returns (project_id, event_id)."""
    conn = db.connect(store_db)
    schema.apply(conn)
    pid = _insert_project(conn, provider="claude", slug="-alpha")
    sfk = _insert_session(conn, project_id=pid, session_id="s1")
    mid = _insert_message(conn, session_fk=sfk)
    eid = _insert_event(conn, source_message_fk=mid, project_id=pid, session_id="s1")
    conn.commit()
    conn.close()
    return pid, eid


# ── shape tests ──────────────────────────────────────────────────────────────


class TestResponseShape:
    def test_empty_store_returns_complete_shape(self, app_client):
        client, _ = app_client
        r = client.get("/api/etl/status")
        assert r.status_code == 200
        body = r.json()

        # Top-level keys
        assert set(body.keys()) == {
            "watcher", "marts", "events", "lag_seconds", "health", "current_job",
        }
        # Idle store: no backfill in flight.
        assert body["current_job"] is None

        # Watcher subshape
        assert set(body["watcher"].keys()) == {
            "enabled", "running", "last_refresh_ts",
            "seconds_since_refresh", "events_in_last_cycle",
            "lock_held_by",
        }

        # All five mart names present, even on a fresh store
        assert set(body["marts"].keys()) == {
            "daily", "session", "project", "provider_day", "model_day",
        }
        for mart in body["marts"].values():
            assert set(mart.keys()) == {"watermark", "row_count", "last_refresh_ts"}
            assert mart["watermark"] == 0
            assert mart["row_count"] == 0
            assert mart["last_refresh_ts"] is None

        # Events subshape
        assert set(body["events"].keys()) == {
            "total", "max_id", "by_provider", "by_cost_source",
        }
        assert body["events"]["total"] == 0
        assert body["events"]["max_id"] == 0
        assert body["events"]["by_provider"] == {}
        assert body["events"]["by_cost_source"] == {}

        # Empty store is "live" — nothing to catch up to.
        assert body["health"] == "live"
        assert body["lag_seconds"] == 0

    def test_populated_store_reports_real_counts(self, app_client):
        client, store_db = app_client
        # Seed three events across two providers, one with cost_source=estimated.
        conn = db.connect(store_db)
        pid_c = _insert_project(conn, provider="claude", slug="-c")
        pid_x = _insert_project(conn, provider="codex", slug="-x")
        sfk1 = _insert_session(conn, project_id=pid_c, session_id="s1")
        sfk2 = _insert_session(conn, project_id=pid_x, session_id="s2")
        m1 = _insert_message(conn, session_fk=sfk1)
        m2 = _insert_message(conn, session_fk=sfk1)
        m3 = _insert_message(conn, session_fk=sfk2)
        _insert_event(conn, source_message_fk=m1, project_id=pid_c, session_id="s1",
                      provider="claude", cost_source="rate_card")
        _insert_event(conn, source_message_fk=m2, project_id=pid_c, session_id="s1",
                      provider="claude", cost_source="estimated")
        _insert_event(conn, source_message_fk=m3, project_id=pid_x, session_id="s2",
                      provider="codex", cost_source="rate_card")
        # Mart watermarks at max event id → no lag.
        max_id_row = conn.execute("SELECT MAX(id) FROM usage_events").fetchone()
        max_id = int(max_id_row[0])
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            _set_watermark(conn, mart_name=name, last_event_id=max_id)
        # Insert one row in each mart so the row_count is non-zero.
        conn.execute(
            "INSERT INTO daily_mart (day, project_id, provider, model, speed) "
            "VALUES ('2026-04-01', ?, 'claude', 'claude-sonnet-4-5', 'standard')",
            (pid_c,),
        )
        conn.execute(
            "INSERT INTO session_mart (session_id, project_id, provider, first_ts, last_ts) "
            "VALUES ('s1', ?, 'claude', '2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z')",
            (pid_c,),
        )
        conn.execute(
            "INSERT INTO project_mart (project_id, provider, slug, display_name) "
            "VALUES (?, 'claude', '-c', '-c')",
            (pid_c,),
        )
        conn.execute(
            "INSERT INTO provider_day_mart (day, provider) VALUES ('2026-04-01', 'claude')"
        )
        conn.execute(
            "INSERT INTO model_day_mart (day, model) VALUES ('2026-04-01', 'claude-sonnet-4-5')"
        )
        conn.commit()
        conn.close()

        r = client.get("/api/etl/status")
        assert r.status_code == 200
        body = r.json()

        assert body["events"]["total"] == 3
        assert body["events"]["max_id"] == max_id
        assert body["events"]["by_provider"] == {"claude": 2, "codex": 1}
        assert body["events"]["by_cost_source"] == {"rate_card": 2, "estimated": 1}

        for mart_name, mart in body["marts"].items():
            assert mart["watermark"] == max_id, mart_name
            assert mart["row_count"] == 1, mart_name

        # All caught up → live, zero lag.
        assert body["lag_seconds"] == 0
        assert body["health"] == "live"


# ── health transitions ───────────────────────────────────────────────────────


class TestHealthTransitions:
    def test_zero_lag_is_live(self, app_client):
        client, store_db = app_client
        _, eid = _seed_one_event(store_db)
        # Watermarks pinned at the max — fully caught up.
        conn = db.connect(store_db)
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            _set_watermark(conn, mart_name=name, last_event_id=eid)
        conn.commit()
        conn.close()
        body = client.get("/api/etl/status").json()
        assert body["health"] == "live"
        assert body["lag_seconds"] == 0

    def test_small_lag_with_no_recent_refresh_stays_live(self, app_client):
        """50 events behind, no recent refresh — under the stale threshold (100)."""
        client, store_db = app_client
        # Create one project + 60 events; pin watermarks at event_id - 50.
        conn = db.connect(store_db)
        pid = _insert_project(conn, provider="claude", slug="-a")
        sfk = _insert_session(conn, project_id=pid, session_id="s1")
        ids: list[int] = []
        for _ in range(60):
            mid = _insert_message(conn, session_fk=sfk)
            eid = _insert_event(conn, source_message_fk=mid, project_id=pid)
            ids.append(eid)
        max_id = ids[-1]
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            _set_watermark(conn, mart_name=name, last_event_id=max_id - 50)
        conn.commit()
        conn.close()
        body = client.get("/api/etl/status").json()
        assert body["lag_seconds"] == 50
        assert body["health"] == "live"

    def test_large_lag_with_running_watcher_is_stale(self, app_client, monkeypatch):
        """200 events behind threshold, watcher running → stale (not error)."""
        client, store_db = app_client
        conn = db.connect(store_db)
        pid = _insert_project(conn, provider="claude", slug="-a")
        sfk = _insert_session(conn, project_id=pid, session_id="s1")
        ids: list[int] = []
        for _ in range(250):
            mid = _insert_message(conn, session_fk=sfk)
            eid = _insert_event(conn, source_message_fk=mid, project_id=pid)
            ids.append(eid)
        max_id = ids[-1]
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            _set_watermark(conn, mart_name=name, last_event_id=max_id - 200)
        conn.commit()
        conn.close()

        # Fake a running watcher handle.
        class _RunningHandle:
            class _T:
                def is_alive(self) -> bool:
                    return True
            thread = _T()
        monkeypatch.setattr(deps, "watcher_handle", _RunningHandle(), raising=False)

        body = client.get("/api/etl/status").json()
        assert body["lag_seconds"] == 200
        assert body["watcher"]["running"] is True
        assert body["health"] == "stale"

    def test_large_lag_with_dead_watcher_is_error(self, app_client, monkeypatch):
        """Lag over threshold + watcher reports running=False → error."""
        client, store_db = app_client
        conn = db.connect(store_db)
        pid = _insert_project(conn, provider="claude", slug="-a")
        sfk = _insert_session(conn, project_id=pid, session_id="s1")
        ids: list[int] = []
        for _ in range(250):
            mid = _insert_message(conn, session_fk=sfk)
            eid = _insert_event(conn, source_message_fk=mid, project_id=pid)
            ids.append(eid)
        max_id = ids[-1]
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            _set_watermark(conn, mart_name=name, last_event_id=max_id - 200)
        conn.commit()
        conn.close()

        class _DeadHandle:
            class _T:
                def is_alive(self) -> bool:
                    return False
            thread = _T()
        monkeypatch.setattr(deps, "watcher_handle", _DeadHandle(), raising=False)

        body = client.get("/api/etl/status").json()
        assert body["watcher"]["running"] is False
        assert body["lag_seconds"] == 200
        assert body["health"] == "error"

    def test_small_lag_with_recent_refresh_is_syncing(self, app_client, monkeypatch):
        """Lag below threshold + last refresh in the last 10s → syncing."""
        client, store_db = app_client
        conn = db.connect(store_db)
        pid = _insert_project(conn, provider="claude", slug="-a")
        sfk = _insert_session(conn, project_id=pid, session_id="s1")
        ids: list[int] = []
        for _ in range(10):
            mid = _insert_message(conn, session_fk=sfk)
            eid = _insert_event(conn, source_message_fk=mid, project_id=pid)
            ids.append(eid)
        max_id = ids[-1]
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            _set_watermark(conn, mart_name=name, last_event_id=max_id - 5)
        conn.commit()
        conn.close()

        # Watcher running with a fresh-as-of-just-now refresh timestamp.
        recent_ts = (datetime.now(UTC) - timedelta(seconds=2)).isoformat()

        class _SyncingHandle:
            class _T:
                def is_alive(self) -> bool:
                    return True
            thread = _T()
            last_refresh_ts = recent_ts
            events_in_last_cycle = 5
        monkeypatch.setattr(deps, "watcher_handle", _SyncingHandle(), raising=False)

        body = client.get("/api/etl/status").json()
        assert body["lag_seconds"] == 5
        assert body["health"] == "syncing"
        assert body["watcher"]["events_in_last_cycle"] == 5


# ── watcher graceful degrade ─────────────────────────────────────────────────


class TestWatcherDegrade:
    def test_no_handle_reports_unknown(self, app_client):
        """CLI / pre-server / no-watcher mode → running='unknown', no crash."""
        client, _ = app_client
        body = client.get("/api/etl/status").json()
        assert body["watcher"]["running"] == "unknown"
        assert body["watcher"]["last_refresh_ts"] is None
        assert body["watcher"]["seconds_since_refresh"] is None
        assert body["watcher"]["events_in_last_cycle"] is None
        # enabled is True unless STACKUNDERFLOW_DISABLE_WATCHER is set.
        assert body["watcher"]["enabled"] is True

    def test_env_disable_flag_reports_disabled(self, app_client, monkeypatch):
        client, _ = app_client
        monkeypatch.setenv("STACKUNDERFLOW_DISABLE_WATCHER", "1")
        body = client.get("/api/etl/status").json()
        assert body["watcher"]["enabled"] is False

    def test_handle_introspection_failure_reports_unknown(self, app_client, monkeypatch):
        """Handle present but ``thread.is_alive()`` raises → running='unknown'."""
        client, _ = app_client

        class _BrokenHandle:
            @property
            def thread(self) -> Any:
                raise RuntimeError("boom")
        monkeypatch.setattr(deps, "watcher_handle", _BrokenHandle(), raising=False)

        body = client.get("/api/etl/status").json()
        assert body["watcher"]["running"] == "unknown"
