"""Tests for ``stackunderflow/routes/meta_agent.py`` + the dispatcher.

Two surfaces are exercised:

1. ``services.meta_agent.execute_tool`` against a seeded store — verifies
   that every tool in the catalogue dispatches to a real backend
   service, returns a JSON-safe ``ToolResult``, and that unknown tool
   names produce a clean error result (never raise).

2. ``routes.meta_agent`` HTTP boundary — bad bodies (no model, empty
   messages, invalid JSON) return 4xx, the catalogue endpoint returns
   the static list, and an Ollama-down request surfaces as a clean
   ``error`` event on the NDJSON stream (not a 500).

We don't spin up Ollama. The chat-stream test mocks the ``httpx.AsyncClient``
inside ``routes.meta_agent`` to deterministically replay the wire format
Ollama would emit, both for plain answers and for the tool-call loop.
"""

from __future__ import annotations

import json

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.routes.meta_agent import router as meta_router
from stackunderflow.services import meta_agent as meta_service
from stackunderflow.store import db, schema

# ── fixtures ─────────────────────────────────────────────────────────────────


@pytest.fixture()
def app_client(tmp_path, monkeypatch):
    """Mount only the meta-agent router against a fresh store."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)
    app = FastAPI()
    app.include_router(meta_router)
    return TestClient(app), store_db


def _seed_minimal(store_db, *, slug: str = "demo", session_id: str = "sess-1"):
    """Seed one project + one session + one message — enough for every tool."""
    conn = db.connect(store_db)
    try:
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, path, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, '/Users/test/dev/demo', 0.0, 1.0)",
            (slug, slug),
        )
        pid = int(
            conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchone()["id"]
        )
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, '2026-05-01T00:00:00Z', '2026-05-01T01:00:00Z', 1)",
            (pid, session_id),
        )
        sfk = int(
            conn.execute(
                "SELECT id FROM sessions WHERE session_id = ?", (session_id,)
            ).fetchone()["id"]
        )
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
            " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
            " raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (?, 0, '2026-05-01T00:30:00Z', 'user', 'claude-sonnet-4-5', 100, 50, 0, 0, "
            " 'how do I fix the broken pipeline?', '[]', '{}', 0, 'u1', NULL)",
            (sfk,),
        )
        conn.commit()
        return pid, sfk
    finally:
        conn.close()


# ── unit: execute_tool ──────────────────────────────────────────────────────


def test_execute_tool_search_past_decisions_returns_shape(app_client):
    _, store_db = app_client
    _seed_minimal(store_db)
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "search_past_decisions", {"query": "pipeline"})
    finally:
        conn.close()
    assert result.ok is True
    assert result.name == "search_past_decisions"
    assert result.data["query"] == "pipeline"
    assert isinstance(result.data["sessions"], list)
    assert result.duration_ms >= 0


def test_execute_tool_unknown_name_returns_error_result(app_client):
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "no_such_tool", {})
    finally:
        conn.close()
    assert result.ok is False
    assert "unknown tool" in result.data["error"]
    # The error message lists the known tools so the LLM can self-correct.
    for name in meta_service.tool_names():
        assert name in result.data["error"]


def test_execute_tool_search_requires_query(app_client):
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "search_past_decisions", {"query": ""})
    finally:
        conn.close()
    assert result.ok is False
    assert "query is required" in result.data["error"]


def test_execute_tool_get_project_summary_uses_slug(app_client):
    _, store_db = app_client
    _seed_minimal(store_db, slug="myproj")
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "get_project_summary", {"slug": "myproj"})
    finally:
        conn.close()
    assert result.ok is True
    assert result.data["slug"] == "myproj"
    assert result.data["sessions"] == 1
    assert result.data["messages"] == 1
    assert result.data["path"] == "/Users/test/dev/demo"


def test_execute_tool_get_project_summary_falls_back_to_current_slug(app_client):
    _, store_db = app_client
    _seed_minimal(store_db, slug="myproj")
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(
            conn, "get_project_summary", {}, current_slug="myproj"
        )
    finally:
        conn.close()
    assert result.ok is True
    assert result.data["slug"] == "myproj"


def test_execute_tool_get_project_summary_errors_without_slug(app_client):
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "get_project_summary", {})
    finally:
        conn.close()
    assert result.ok is False
    assert "slug is required" in result.data["error"]


def test_execute_tool_list_recent_sessions_filters_by_project(app_client):
    _, store_db = app_client
    _seed_minimal(store_db, slug="alpha", session_id="s-a")
    _seed_minimal(store_db, slug="beta", session_id="s-b")
    conn = db.connect(store_db)
    try:
        all_result = meta_service.execute_tool(conn, "list_recent_sessions", {})
        scoped = meta_service.execute_tool(conn, "list_recent_sessions", {"project": "alpha"})
    finally:
        conn.close()
    assert all_result.ok is True
    assert all_result.data["count"] == 2
    assert scoped.ok is True
    assert scoped.data["count"] == 1
    assert scoped.data["sessions"][0]["session_id"] == "s-a"


def test_execute_tool_get_cost_summary_returns_period_label(app_client):
    _, store_db = app_client
    _seed_minimal(store_db)
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "get_cost_summary", {"period": "30days"})
    finally:
        conn.close()
    assert result.ok is True
    assert result.data["period"] == "30days"
    assert "last 30 days" in result.data["label"]
    assert "top_projects" in result.data


def test_execute_tool_get_cost_summary_rejects_bad_period(app_client):
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "get_cost_summary", {"period": "bogus"})
    finally:
        conn.close()
    assert result.ok is False
    assert "Unknown period" in result.data["error"]


def test_execute_tool_session_playback_unknown_session(app_client):
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(
            conn, "get_session_playback", {"session_id": "nope"}
        )
    finally:
        conn.close()
    assert result.ok is False
    assert "session not found" in result.data["error"]


def test_execute_tool_get_file_risk_returns_zero_buckets(app_client):
    """Spec 16 — meta-agent tool should plumb through to the risk service."""
    _, store_db = app_client
    _seed_minimal(store_db)
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(
            conn, "get_file_risk", {"path": "/x/cost.py"}
        )
    finally:
        conn.close()
    assert result.ok is True
    assert result.data["total_sessions"] == 0
    assert result.data["reverted"] == 0
    assert result.data["recent_session_ids"] == []
    # The seven-key shape the catalogue contract advertises.
    assert set(result.data) == {
        "path", "since", "total_sessions",
        "reverted", "failed", "worked", "recent_session_ids",
    }


def test_execute_tool_get_file_risk_requires_path(app_client):
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(conn, "get_file_risk", {})
    finally:
        conn.close()
    assert result.ok is False
    assert "path is required" in result.data["error"]


def test_execute_tool_get_file_risk_rejects_bad_since(app_client):
    _, store_db = app_client
    _seed_minimal(store_db)
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(
            conn, "get_file_risk",
            {"path": "/x/cost.py", "since": "yesterday"},
        )
    finally:
        conn.close()
    assert result.ok is False
    assert "invalid since" in result.data["error"]


def test_get_file_risk_in_catalogue(app_client):
    """The new tool must appear in TOOL_CATALOG so the LLM can pick it."""
    assert "get_file_risk" in meta_service.tool_names()
    entry = next(
        t for t in meta_service.TOOL_CATALOG
        if t["function"]["name"] == "get_file_risk"
    )
    assert entry["function"]["parameters"]["required"] == ["path"]


def test_execute_tool_string_args_parsed_as_json(app_client):
    """Some Ollama models emit ``arguments`` as a JSON string. We accept either."""
    _, store_db = app_client
    _seed_minimal(store_db)
    conn = db.connect(store_db)
    try:
        result = meta_service.execute_tool(
            conn, "search_past_decisions", '{"query": "pipeline"}'  # type: ignore[arg-type]
        )
    finally:
        conn.close()
    assert result.ok is True
    assert result.data["query"] == "pipeline"


def test_execute_tool_get_burn_projection_no_plan(app_client, tmp_path, monkeypatch):
    """Without a configured plan the tool returns an actionable hint, not an error."""
    # Isolate settings so a stray real-user config doesn't leak in.
    from unittest.mock import patch as _patch

    app_dir = tmp_path / ".su"
    app_dir.mkdir()
    cfg = app_dir / "cfg.json"
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        with (
            _patch("stackunderflow.settings._APP_DIR", app_dir),
            _patch("stackunderflow.settings._CFG_FILE", cfg),
        ):
            result = meta_service.execute_tool(conn, "get_burn_projection", {})
    finally:
        conn.close()
    # No plan → ok=True (it's a successful call), ``plan_set=False``.
    assert result.ok is True
    assert result.data["plan_set"] is False
    assert "stackunderflow plan set" in result.data["hint"]


def test_execute_tool_get_burn_projection_with_plan(app_client, tmp_path):
    """With a plan set the tool returns the structured projection block."""
    from unittest.mock import patch as _patch

    app_dir = tmp_path / ".su"
    app_dir.mkdir()
    cfg = app_dir / "cfg.json"
    _, store_db = app_client
    conn = db.connect(store_db)
    try:
        with (
            _patch("stackunderflow.settings._APP_DIR", app_dir),
            _patch("stackunderflow.settings._CFG_FILE", cfg),
        ):
            from stackunderflow.services import plans as plans_mod
            plans_mod.set_plan("claude-pro")
            result = meta_service.execute_tool(conn, "get_burn_projection", {})
    finally:
        conn.close()
    assert result.ok is True
    assert result.data["plan_set"] is True
    assert result.data["plan"]["name"] == "claude-pro"
    assert result.data["plan"]["monthly_usd"] == 20.0
    # All projection keys present.
    for key in (
        "period_start", "period_end", "days_so_far", "days_in_period",
        "used_usd", "remaining_usd", "pct_used", "status",
        "projected_month_end_usd", "projection_method",
        "daily_burn_usd", "days_to_limit", "thresholds",
        "crossed_threshold", "alert",
    ):
        assert key in result.data
    assert result.data["projection_method"] in ("linear", "weighted-7d")
    assert result.data["thresholds"] == [50, 75, 90]


def test_burn_projection_in_tool_catalog():
    """The tool catalogue advertises the new tool under the OpenAI function shape."""
    names = {t["function"]["name"] for t in meta_service.TOOL_CATALOG}
    assert "get_burn_projection" in names
    entry = next(
        t for t in meta_service.TOOL_CATALOG
        if t["function"]["name"] == "get_burn_projection"
    )
    assert entry["function"]["parameters"]["type"] == "object"
    # No required args — the LLM can call it with `{}`.
    assert entry["function"]["parameters"]["required"] == []


def test_truncate_caps_large_payloads():
    """A tool result that exceeds the 4 KB budget gets trimmed but kept JSON-safe."""
    # Build a dict whose serialised form is well over the budget. The
    # truncator should slice the offending list, mark ``_truncated``,
    # and the result must still parse as JSON.
    huge = {
        "sessions": [
            {"session_id": f"s{i}", "snippet": "x" * 500} for i in range(50)
        ]
    }
    out = meta_service._truncate(huge)
    encoded = json.dumps(out, default=str)
    assert len(encoded) <= meta_service._RESULT_CHAR_BUDGET
    assert out.get("_truncated") is True


# ── route: /api/meta-agent/tools ────────────────────────────────────────────


def test_route_tools_endpoint_returns_catalog(app_client):
    client, _ = app_client
    resp = client.get("/api/meta-agent/tools")
    assert resp.status_code == 200
    body = resp.json()
    assert set(body) == {"tools", "names", "max_hops"}
    assert body["max_hops"] == meta_service.MAX_TOOL_HOPS
    assert set(body["names"]) == set(meta_service.tool_names())
    # Every entry obeys the OpenAI-style function shape.
    for entry in body["tools"]:
        assert entry["type"] == "function"
        assert "name" in entry["function"]
        assert "parameters" in entry["function"]
        assert entry["function"]["parameters"]["type"] == "object"


# ── route: /api/meta-agent/chat error handling ──────────────────────────────


def test_route_chat_rejects_invalid_json(app_client):
    client, _ = app_client
    resp = client.post(
        "/api/meta-agent/chat",
        content="not-json",
        headers={"content-type": "application/json"},
    )
    assert resp.status_code == 400
    assert resp.json()["error"] == "invalid JSON body"


def test_route_chat_rejects_missing_messages(app_client):
    client, _ = app_client
    resp = client.post("/api/meta-agent/chat", json={"model": "llama3.2"})
    assert resp.status_code == 400
    assert "messages" in resp.json()["error"]


def test_route_chat_rejects_missing_model(app_client):
    client, _ = app_client
    resp = client.post(
        "/api/meta-agent/chat",
        json={"messages": [{"role": "user", "content": "hi"}]},
    )
    assert resp.status_code == 400
    assert "model" in resp.json()["error"]


def test_route_chat_when_ollama_down_yields_error_event(app_client, monkeypatch):
    """No network mock — the real httpx call will fail to reach localhost:11434.

    The route catches ``httpx.RequestError`` and emits a single
    ``type: "error"`` line; the response status itself stays 200 (the
    stream opens before we know Ollama's up).
    """
    client, _ = app_client

    # We can't reliably guarantee no Ollama is running on this dev box, so
    # point the endpoint at an unrouteable port via the cloud-first config —
    # the route resolves STACKUNDERFLOW_OLLAMA_URL per request.
    monkeypatch.setenv("STACKUNDERFLOW_OLLAMA_URL", "http://localhost:1")

    resp = client.post(
        "/api/meta-agent/chat",
        json={
            "messages": [{"role": "user", "content": "hi"}],
            "model": "llama3.2",
            "tools_enabled": False,
        },
    )
    assert resp.status_code == 200
    assert resp.headers["content-type"].startswith("application/x-ndjson")
    lines = [line for line in resp.text.splitlines() if line.strip()]
    assert lines, "stream produced no events"
    first = json.loads(lines[0])
    assert first["type"] == "error"
    assert "Ollama" in first["message"]


# ── route: tool-call loop with mocked Ollama ────────────────────────────────


class _FakeOllamaResponse:
    """Mimic httpx.Response's streaming surface for our chat tests.

    We only need:
      * ``status_code`` — branch in the route.
      * ``aiter_lines()`` — async iteration over the NDJSON Ollama emits.
      * ``text`` — used in the error branch.
    """

    def __init__(self, *, status_code: int = 200, lines: list[dict] | None = None,
                 body_text: str = "") -> None:
        self.status_code = status_code
        self._lines = lines or []
        self.text = body_text

    async def aiter_lines(self):
        for line in self._lines:
            yield json.dumps(line)

    async def aread(self) -> None:
        return None

    async def __aenter__(self) -> _FakeOllamaResponse:
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        return None


class _FakeOllamaClient:
    """Stand-in for ``httpx.AsyncClient`` used inside the chat stream.

    Drives a scripted sequence of responses — one per ``post`` — so we
    can simulate "model answers directly" or "model emits a tool_call,
    then answers".
    """

    def __init__(self, scripted: list[_FakeOllamaResponse]) -> None:
        self._scripted = scripted
        self._post_count = 0

    async def __aenter__(self) -> _FakeOllamaClient:
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        return None

    def _next(self) -> _FakeOllamaResponse:
        if self._post_count >= len(self._scripted):
            return _FakeOllamaResponse(
                status_code=500, body_text="no more scripted responses"
            )
        resp = self._scripted[self._post_count]
        self._post_count += 1
        return resp

    async def post(self, url: str, json: dict | None = None) -> _FakeOllamaResponse:  # noqa: A002
        return self._next()

    def stream(self, method: str, url: str, json: dict | None = None, headers: dict | None = None):  # noqa: A002
        # httpx.AsyncClient.stream is sync and returns an async context
        # manager; our fake response IS one (__aenter__/__aexit__ below).
        return self._next()


def _parse_stream(text: str) -> list[dict]:
    """Split an NDJSON response body into parsed event dicts."""
    return [json.loads(line) for line in text.splitlines() if line.strip()]


def test_route_chat_streams_token_then_done_for_plain_answer(app_client, monkeypatch):
    client, _ = app_client

    scripted = [
        _FakeOllamaResponse(
            lines=[
                {"message": {"role": "assistant", "content": "hello "}},
                {"message": {"role": "assistant", "content": "world"}, "done": True},
            ]
        )
    ]

    import stackunderflow.routes.meta_agent as ma_route

    def _factory(*_a, **_kw):
        return _FakeOllamaClient(scripted)

    monkeypatch.setattr(ma_route.httpx, "AsyncClient", _factory)

    resp = client.post(
        "/api/meta-agent/chat",
        json={
            "messages": [{"role": "user", "content": "hi"}],
            "model": "llama3.2",
            "tools_enabled": True,
        },
    )
    assert resp.status_code == 200
    events = _parse_stream(resp.text)
    types = [e["type"] for e in events]
    # Two token events followed by a done.
    assert types == ["token", "token", "done"]
    assert events[0]["delta"] == "hello "
    assert events[1]["delta"] == "world"
    assert events[2]["hops"] == 1


def test_route_chat_tool_call_loop_executes_and_resumes(app_client, monkeypatch):
    """Full loop: model emits a tool call, route executes it, calls Ollama again."""
    client, store_db = app_client
    _seed_minimal(store_db, slug="alpha", session_id="alpha-1")

    # Hop 1: model emits a tool call (no content), then done.
    hop1 = _FakeOllamaResponse(
        lines=[
            {
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "list_recent_sessions",
                                "arguments": {"project": "alpha", "limit": 5},
                            },
                        }
                    ],
                },
                "done": True,
            }
        ]
    )
    # Hop 2: model summarises, no further tool calls.
    hop2 = _FakeOllamaResponse(
        lines=[
            {"message": {"role": "assistant", "content": "you have one alpha session"}},
            {"message": {"role": "assistant", "content": ""}, "done": True},
        ]
    )

    import stackunderflow.routes.meta_agent as ma_route

    def _factory(*_a, **_kw):
        return _FakeOllamaClient([hop1, hop2])

    monkeypatch.setattr(ma_route.httpx, "AsyncClient", _factory)

    resp = client.post(
        "/api/meta-agent/chat",
        json={
            "messages": [{"role": "user", "content": "list my sessions"}],
            "model": "qwen2.5-coder",
            "tools_enabled": True,
            "project_slug": "alpha",
        },
    )
    assert resp.status_code == 200
    events = _parse_stream(resp.text)
    types = [e["type"] for e in events]
    # Expected: tool_call → tool_result → token → done.
    assert types[0] == "tool_call"
    assert events[0]["name"] == "list_recent_sessions"
    assert events[0]["id"] == "call_1"
    assert types[1] == "tool_result"
    assert events[1]["ok"] is True
    assert events[1]["name"] == "list_recent_sessions"
    assert events[1]["id"] == "call_1"
    assert events[1]["data"]["count"] == 1
    assert types[2] == "token"
    assert types[-1] == "done"
    assert events[-1]["hops"] == 2


def test_route_chat_unknown_tool_returns_clean_result(app_client, monkeypatch):
    client, _ = app_client
    hop1 = _FakeOllamaResponse(
        lines=[
            {
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "x1",
                            "type": "function",
                            "function": {
                                "name": "ghost_tool",
                                "arguments": {},
                            },
                        }
                    ],
                },
                "done": True,
            }
        ]
    )
    hop2 = _FakeOllamaResponse(
        lines=[
            {"message": {"role": "assistant", "content": "sorry, no such tool"}, "done": True},
        ]
    )

    import stackunderflow.routes.meta_agent as ma_route

    def _factory(*_a, **_kw):
        return _FakeOllamaClient([hop1, hop2])

    monkeypatch.setattr(ma_route.httpx, "AsyncClient", _factory)

    resp = client.post(
        "/api/meta-agent/chat",
        json={"messages": [{"role": "user", "content": "?"}], "model": "x"},
    )
    events = _parse_stream(resp.text)
    tool_results = [e for e in events if e["type"] == "tool_result"]
    assert len(tool_results) == 1
    assert tool_results[0]["ok"] is False
    assert "unknown tool" in tool_results[0]["data"]["error"]


def test_route_chat_hop_cap_terminates_runaway_loop(app_client, monkeypatch):
    """A model that keeps emitting tool calls hits ``MAX_TOOL_HOPS`` and stops."""
    client, store_db = app_client
    _seed_minimal(store_db)

    # Always return a tool-call response → the route should bail after
    # MAX_TOOL_HOPS iterations rather than looping forever.

    def fresh() -> _FakeOllamaResponse:
        return _FakeOllamaResponse(
            lines=[
                {
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [
                            {
                                "id": "loop_id",
                                "type": "function",
                                "function": {
                                    "name": "list_recent_sessions",
                                    "arguments": {},
                                },
                            }
                        ],
                    },
                    "done": True,
                }
            ]
        )

    scripted = [fresh() for _ in range(meta_service.MAX_TOOL_HOPS + 2)]

    import stackunderflow.routes.meta_agent as ma_route

    def _factory(*_a, **_kw):
        return _FakeOllamaClient(scripted)

    monkeypatch.setattr(ma_route.httpx, "AsyncClient", _factory)

    resp = client.post(
        "/api/meta-agent/chat",
        json={"messages": [{"role": "user", "content": "loop"}], "model": "x"},
    )
    events = _parse_stream(resp.text)
    # Final event should be an error mentioning the hop cap. We don't
    # care about the exact event count, only that the stream terminated
    # without a runaway.
    assert events[-1]["type"] == "error"
    assert "hop cap" in events[-1]["message"]


def test_route_chat_ollama_5xx_propagates_as_error_event(app_client, monkeypatch):
    client, _ = app_client
    bad = _FakeOllamaResponse(status_code=500, body_text="server exploded")

    import stackunderflow.routes.meta_agent as ma_route

    def _factory(*_a, **_kw):
        return _FakeOllamaClient([bad])

    monkeypatch.setattr(ma_route.httpx, "AsyncClient", _factory)

    resp = client.post(
        "/api/meta-agent/chat",
        json={"messages": [{"role": "user", "content": "x"}], "model": "x"},
    )
    events = _parse_stream(resp.text)
    assert len(events) == 1
    assert events[0]["type"] == "error"
    assert "500" in events[0]["message"]
    assert "server exploded" in events[0]["message"]
