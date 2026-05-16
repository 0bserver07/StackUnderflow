"""Tests for ``routes/live`` — Spec 13.

Two surfaces:

* ``GET /api/live/stats`` — snapshot. Locks the response shape and the
  ``watcher.running`` introspection (mirrors the etl/status pattern).
* ``GET /api/live/stream`` — SSE. We exercise the inner ``_stream_loop``
  generator directly with a stub ``Request`` so the test doesn't depend
  on Starlette's TCP plumbing. The contract: one ``ready`` event up
  front, then one ``event`` per ``usage_events.id`` advancement, plus
  periodic ``burn_tick`` events.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from typing import AsyncIterator

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.live import (
    _format_sse,
    _stream_loop,
    router as live_router,
)
from stackunderflow.store import db, schema


# ── seed helpers (mirrors test_live's shape) ───────────────────────────


def _project(conn, *, slug: str = "-alpha") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', ?, ?, 0.0, 0.0)",
        (slug, slug),
    )
    return int(cur.lastrowid)


def _session(conn, *, project_id: int, sid: str = "s1") -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, sid, "2026-04-01T00:00:00Z", "2026-04-01T00:00:00Z", 1),
    )
    return int(cur.lastrowid)


_seq: dict[int, int] = {}


def _message(conn, *, session_fk: int, ts: str = "2026-05-15T12:00:00Z") -> int:
    s = _seq.get(session_fk, 0)
    _seq[session_fk] = s + 1
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, 'assistant', 'claude-sonnet-4-5', "
        " 0, 0, 0, 0, '', '[]', '{}', 0)",
        (session_fk, s, ts),
    )
    return int(conn.execute(
        "SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1"
    ).fetchone()[0])


def _event(conn, *, source_message_fk: int, project_id: int,
           ts: str = "2026-05-15T12:00:00Z", cost_usd: float = 0.0) -> int:
    cur = conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, account, project_id, session_id, ts, day, "
        " model, speed, input_tokens, output_tokens, cache_read_tokens, "
        " cache_create_tokens, cost_usd, cost_source, role, raw_extras) "
        "VALUES (?, 'claude', 'default', ?, 's1', ?, ?, "
        " 'claude-sonnet-4-5', 'standard', 0, 0, 0, 0, ?, 'rate_card', 'assistant', NULL)",
        (source_message_fk, project_id, ts, ts[:10], cost_usd),
    )
    return int(cur.lastrowid)


def _tool_call(conn, *, message_id: int, project_id: int,
               ts: str = "2026-05-15T12:00:00Z",
               tool_name: str = "Read", call_index: int = 0) -> int:
    cur = conn.execute(
        "INSERT INTO message_tool_mart "
        "(message_id, project_id, session_id, ts, day, "
        " tool_name, file_path, byte_count, call_index) "
        "VALUES (?, ?, 's1', ?, ?, ?, NULL, NULL, ?)",
        (message_id, project_id, ts, ts[:10], tool_name, call_index),
    )
    return int(cur.lastrowid)


@pytest.fixture(autouse=True)
def _reset_seq():
    _seq.clear()
    yield
    _seq.clear()


# ── /api/live/stats ────────────────────────────────────────────────────


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    """Mount only the live router with a fresh, schema-applied store."""
    store = tmp_path / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    conn.close()

    monkeypatch.setattr(deps, "store_path", store)
    monkeypatch.setattr(deps, "watcher_handle", None, raising=False)

    app = FastAPI()
    app.include_router(live_router)
    return TestClient(app), store


class TestStatsRoute:
    def test_empty_store_returns_zero_shape(self, app_client):
        client, _ = app_client
        r = client.get("/api/live/stats")
        assert r.status_code == 200
        body = r.json()
        assert set(body.keys()) == {"burn", "tool_latency", "watermarks", "watcher"}
        assert body["watermarks"] == {"event_id": 0, "tool_call_id": 0}
        assert body["tool_latency"] == []
        # No watcher handle → "unknown" (not boolean) per the contract.
        assert body["watcher"]["running"] == "unknown"

    def test_populated_store_returns_burn_and_watermarks(self, app_client):
        client, store = app_client
        conn = db.connect(store)
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        m1 = _message(conn, session_fk=sfk)
        eid = _event(conn, source_message_fk=m1, project_id=pid, cost_usd=0.05)
        tid = _tool_call(conn, message_id=m1, project_id=pid)
        conn.commit()
        conn.close()

        r = client.get("/api/live/stats")
        assert r.status_code == 200
        body = r.json()
        assert body["watermarks"]["event_id"] == eid
        assert body["watermarks"]["tool_call_id"] == tid
        assert body["burn"]["window_minutes"] == 5

    def test_watcher_running_reflects_handle_state(self, app_client, monkeypatch):
        client, _ = app_client

        class _Thread:
            def is_alive(self) -> bool:
                return True

        class _Handle:
            thread = _Thread()

        monkeypatch.setattr(deps, "watcher_handle", _Handle(), raising=False)
        body = client.get("/api/live/stats").json()
        assert body["watcher"]["running"] is True


# ── /api/live/stream — SSE format ─────────────────────────────────────


class TestSseFormat:
    def test_format_sse_emits_event_and_data_lines(self):
        out = _format_sse("burn_tick", {"type": "burn_tick", "x": 1})
        # Standard SSE shape: ``event:`` line, ``data:`` line, blank-line terminator.
        assert out.startswith("event: burn_tick\n")
        assert "\ndata: " in out
        assert out.endswith("\n\n")
        body = json.loads(out.split("data: ", 1)[1].rstrip())
        assert body == {"type": "burn_tick", "x": 1}


# ── /api/live/stream — _stream_loop generator ──────────────────────────


class _FakeRequest:
    """Stub Starlette ``Request`` exposing only ``is_disconnected``."""

    def __init__(self, *, disconnect_after_iters: int = 1) -> None:
        self._iters = 0
        self._limit = disconnect_after_iters

    async def is_disconnected(self) -> bool:
        # First call returns False (let the loop run); after a few it
        # returns True so the generator winds down cleanly.
        self._iters += 1
        return self._iters > self._limit


async def _drain(gen: AsyncIterator[str]) -> list[str]:
    out: list[str] = []
    async for chunk in gen:
        out.append(chunk)
    return out


def _parse_sse(chunks: list[str]) -> list[tuple[str, dict]]:
    """Parse a list of SSE-formatted strings into ``(event, payload)`` pairs."""
    out: list[tuple[str, dict]] = []
    for c in chunks:
        ev = None
        data = None
        for line in c.splitlines():
            if line.startswith("event: "):
                ev = line[len("event: "):]
            elif line.startswith("data: "):
                data = json.loads(line[len("data: "):])
        if ev is not None and data is not None:
            out.append((ev, data))
    return out


@pytest.fixture()
def store(tmp_path, monkeypatch):
    s = tmp_path / "store.db"
    conn = db.connect(s)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", s)
    monkeypatch.setattr(deps, "watcher_handle", None, raising=False)
    return s


class TestStreamLoop:
    @pytest.mark.asyncio
    async def test_emits_ready_event_first(self, store):
        req = _FakeRequest(disconnect_after_iters=0)
        chunks = await _drain(_stream_loop(
            req, poll_interval=0.01, burn_interval=999, max_iterations=1
        ))
        events = _parse_sse(chunks)
        assert events[0][0] == "ready"
        assert events[0][1]["type"] == "ready"
        assert "watermarks" in events[0][1]["payload"]
        assert "watcher" in events[0][1]["payload"]

    @pytest.mark.asyncio
    async def test_one_event_per_usage_event_advancement(self, store):
        # Seed two events BEFORE opening the loop. The seed watermark
        # should sit at max(id) so neither original event re-emits;
        # then we add one more between iterations and confirm a single
        # "event" SSE message is emitted.
        conn = db.connect(store)
        pid = _project(conn)
        sfk = _session(conn, project_id=pid)
        m1 = _message(conn, session_fk=sfk)
        m2 = _message(conn, session_fk=sfk)
        _event(conn, source_message_fk=m1, project_id=pid)
        _event(conn, source_message_fk=m2, project_id=pid)
        conn.commit()
        conn.close()

        # Custom request that lets us insert a row mid-loop.
        class _AdvancingRequest:
            def __init__(self) -> None:
                self.calls = 0

            async def is_disconnected(self) -> bool:
                self.calls += 1
                if self.calls == 2:
                    # After the first iteration, append a new event so
                    # the next cycle has something to emit.
                    c2 = db.connect(store)
                    pid2 = c2.execute(
                        "SELECT id FROM projects LIMIT 1"
                    ).fetchone()[0]
                    sfk2 = c2.execute(
                        "SELECT id FROM sessions LIMIT 1"
                    ).fetchone()[0]
                    m3 = _message(c2, session_fk=sfk2)
                    _event(c2, source_message_fk=m3, project_id=pid2,
                           cost_usd=0.99)
                    c2.commit()
                    c2.close()
                # Stop after two iterations so the test terminates.
                return self.calls > 5

        req = _AdvancingRequest()
        chunks = await _drain(_stream_loop(
            req, poll_interval=0.01, burn_interval=999, max_iterations=4
        ))
        events = _parse_sse(chunks)
        # ready + exactly one new event (the seeded ones are below the seed watermark).
        event_payloads = [p for ev, p in events if ev == "event"]
        assert len(event_payloads) == 1
        assert event_payloads[0]["payload"]["cost_usd"] == 0.99

    @pytest.mark.asyncio
    async def test_burn_tick_emitted_on_first_cycle(self, store):
        # last_burn_at starts at 0.0 → first cycle should emit one tick
        # regardless of the burn_interval value.
        req = _FakeRequest(disconnect_after_iters=1)
        chunks = await _drain(_stream_loop(
            req, poll_interval=0.01, burn_interval=999, max_iterations=2
        ))
        events = _parse_sse(chunks)
        burn_ticks = [p for ev, p in events if ev == "burn_tick"]
        assert len(burn_ticks) == 1
        assert "window_cost" in burn_ticks[0]["payload"]
        assert "projected_month_end" in burn_ticks[0]["payload"]

    @pytest.mark.asyncio
    async def test_disconnect_stops_loop_cleanly(self, store):
        # Disconnect on the very first check after the ready event.
        req = _FakeRequest(disconnect_after_iters=0)
        chunks = await _drain(_stream_loop(
            req, poll_interval=0.01, burn_interval=999, max_iterations=10
        ))
        # We should still get the ready event and then exit.
        events = _parse_sse(chunks)
        ready_count = sum(1 for ev, _ in events if ev == "ready")
        assert ready_count == 1
        # Loop should exit immediately — no event/tool_call/burn_tick.
        non_ready = [ev for ev, _ in events if ev != "ready"]
        assert non_ready == []
