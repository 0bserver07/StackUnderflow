"""Tests for ``stackunderflow.services.playback``.

Builds a synthetic store with seeded ``messages`` rows whose ``raw_json``
mirrors the Anthropic transcript shape (assistant ``tool_use`` blocks +
following user ``tool_result`` blocks) and locks the contract:

* a session's tool calls become an ordered ``PlaybackEvent`` stream;
* ``tool_filter`` subsets the stream while preserving each event's
  ``seq`` from the *full* numbering;
* ``summary`` formatting is table-driven and stable;
* ``success`` derives from the transcript ``is_error`` flag, is overlaid
  from the optional spec-05 ``captured_events`` table when present, and
  is ``None`` otherwise (works without hooks installed);
* ``limit`` caps the output with a ``truncated`` signal;
* the cross-session ``project_timeline`` interleaves sessions in ts order.

See ``.notes/specs/10-playback-timeline.md``.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.services import playback
from stackunderflow.store import db, schema

_T0 = "2026-05-01T00:00:00Z"
_T1 = "2026-05-01T01:00:00Z"


# ── seed helpers ─────────────────────────────────────────────────────────────


def _seed_project(conn, *, slug: str = "demo") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0.0, 1.0)",
        ("claude", slug, slug),
    )
    return int(cur.lastrowid)


def _seed_session(conn, *, project_id, session_id, first_ts=_T0, last_ts=_T1) -> int:
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, 0)",
        (project_id, session_id, first_ts, last_ts),
    )
    return int(
        conn.execute(
            "SELECT id FROM sessions WHERE project_id = ? AND session_id = ?",
            (project_id, session_id),
        ).fetchone()["id"]
    )


def _seed_message(conn, *, session_fk, seq, role, raw, ts) -> int:
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, input_tokens, output_tokens, "
        " cache_create_tokens, cache_read_tokens, content_text, tools_json, raw_json, "
        " is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, '', '[]', ?, 0, ?, NULL)",
        (session_fk, seq, ts, role, json.dumps(raw), f"u{session_fk}-{seq}"),
    )
    return int(
        conn.execute(
            "SELECT id FROM messages WHERE session_fk = ? AND seq = ?", (session_fk, seq)
        ).fetchone()["id"]
    )


def _assistant(*tool_calls: tuple[str, str, dict]) -> dict:
    """``tool_calls`` = ``(tool_use_id, tool_name, input_dict)`` triples."""
    return {
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": tuid, "name": name, "input": inp}
                for tuid, name, inp in tool_calls
            ],
        },
    }


def _result(*results: tuple[str, str, bool]) -> dict:
    """``results`` = ``(tool_use_id, content_text, is_error)`` triples."""
    return {
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": tuid, "content": text, "is_error": err}
                for tuid, text, err in results
            ],
        },
    }


def _reads(n: int) -> list[tuple[str, dict]]:
    return [("Read", {"file_path": f"f{i}.py"}) for i in range(n)]


@pytest.fixture()
def conn(tmp_path):
    store_path = tmp_path / "store.db"
    c = db.connect(store_path)
    schema.apply(c)
    yield c
    c.close()


def _seed_linear(conn, *, project_id, session_id, pairs, start_minute=0) -> int:
    """Seed a session as ``pairs`` of (assistant tool_use → user tool_result)
    messages. ``pairs`` is a list of ``(tool_name, input_dict)``. Timestamps
    tick by one minute per message, starting at ``start_minute``.
    """
    m0 = start_minute
    sfk = _seed_session(
        conn,
        project_id=project_id,
        session_id=session_id,
        first_ts=f"2026-05-01T{m0 // 60:02d}:{m0 % 60:02d}:00Z",
    )
    seq = 0
    minute = start_minute
    for i, (tname, tinp) in enumerate(pairs):
        tuid = f"tu-{session_id}-{i}"
        ts_a = f"2026-05-01T{minute // 60:02d}:{minute % 60:02d}:00Z"
        minute += 1
        ts_r = f"2026-05-01T{minute // 60:02d}:{minute % 60:02d}:00Z"
        minute += 1
        _seed_message(conn, session_fk=sfk, seq=seq, role="assistant",
                      raw=_assistant((tuid, tname, tinp)), ts=ts_a)
        seq += 1
        _seed_message(conn, session_fk=sfk, seq=seq, role="user",
                      raw=_result((tuid, f"result {i}", False)), ts=ts_r)
        seq += 1
    conn.commit()
    return sfk


# ── basic stream ─────────────────────────────────────────────────────────────


def test_empty_session_returns_empty_list(conn):
    pid = _seed_project(conn)
    _seed_session(conn, project_id=pid, session_id="s0")
    assert playback.session_playback(conn, "s0") == []


def test_unknown_session_returns_empty_and_page_none(conn):
    assert playback.session_playback(conn, "nope") == []
    assert playback.session_playback_page(conn, "nope") is None


def test_fifty_mixed_tool_calls_yield_fifty_events_in_ts_order(conn):
    pid = _seed_project(conn)
    tools = ["Read", "Edit", "Bash", "Glob", "Grep"]
    pairs = []
    for i in range(50):
        name = tools[i % len(tools)]
        if name in ("Read", "Edit"):
            pairs.append((name, {"file_path": f"src/mod_{i}.py"}))
        elif name == "Bash":
            pairs.append((name, {"command": f"echo {i}"}))
        else:
            pairs.append((name, {"pattern": f"*.{i}"}))
    _seed_linear(conn, project_id=pid, session_id="big", pairs=pairs)

    events = playback.session_playback(conn, "big")
    assert len(events) == 50
    # seq is the 0-based index over all tool calls...
    assert [e.seq for e in events] == list(range(50))
    # ...and the events come back in chronological (== seq) order.
    assert events == sorted(events, key=lambda e: (e.ts, e.seq))
    assert all(e.session_id == "big" for e in events)
    # message_id round-trips to a real row.
    ids = {int(r["id"]) for r in conn.execute("SELECT id FROM messages").fetchall()}
    assert all(e.message_id in ids for e in events)


def test_parallel_tool_calls_in_one_message_each_get_an_event(conn):
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="par")
    mid = _seed_message(
        conn, session_fk=sfk, seq=0, role="assistant",
        raw=_assistant(("a", "Read", {"file_path": "a.py"}), ("b", "Read", {"file_path": "b.py"})),
        ts="2026-05-01T00:00:01Z",
    )
    _seed_message(conn, session_fk=sfk, seq=1, role="user",
                  raw=_result(("a", "x", False), ("b", "yy", False)), ts="2026-05-01T00:00:02Z")
    conn.commit()
    events = playback.session_playback(conn, "par")
    assert [(e.seq, e.tool_name, e.target_path, e.message_id) for e in events] == [
        (0, "Read", "a.py", mid),
        (1, "Read", "b.py", mid),
    ]
    assert {e.ts for e in events} == {"2026-05-01T00:00:01Z"}


# ── tool_filter ──────────────────────────────────────────────────────────────


def test_tool_filter_subsets_but_preserves_seq(conn):
    pid = _seed_project(conn)
    # Order: Read, Edit, Bash, Edit, Read → Edits are at global seq 1 and 3.
    pairs = [
        ("Read", {"file_path": "a.py"}),
        ("Edit", {"file_path": "routes/cost.py", "old_string": "x", "new_string": "y"}),
        ("Bash", {"command": "pytest"}),
        ("Edit", {"file_path": "b.py", "old_string": "1", "new_string": "2"}),
        ("Read", {"file_path": "c.py"}),
    ]
    _seed_linear(conn, project_id=pid, session_id="mix", pairs=pairs)

    edits = playback.session_playback(conn, "mix", tool_filter=["Edit"])
    assert [e.seq for e in edits] == [1, 3]
    assert all(e.tool_name == "Edit" for e in edits)

    pair = playback.session_playback(conn, "mix", tool_filter=["Edit", "Bash"])
    assert [(e.seq, e.tool_name) for e in pair] == [(1, "Edit"), (2, "Bash"), (3, "Edit")]

    assert playback.session_playback(conn, "mix", tool_filter=["WebFetch"]) == []
    # An empty filter list is treated as "no filter".
    assert len(playback.session_playback(conn, "mix", tool_filter=[])) == len(pairs)


# ── summary formatting ───────────────────────────────────────────────────────


def test_summary_formatting_table():
    s = playback.summarize_tool_call
    assert s("Edit", {"file_path": "routes/cost.py"}) == "Edit routes/cost.py"
    assert s("Edit", {"file_path": "/Users/me/repo/routes/cost.py"}) == "Edit routes/cost.py"
    assert s("Read", {"file_path": "stackunderflow/store/mart_queries.py"}) == "Read store/mart_queries.py"
    assert s("Bash", {"command": "pytest tests/ -q"}) == "Bash: pytest"
    assert s("Bash", {"command": "cd /tmp && pytest tests/"}) == "Bash: pytest"
    assert s("Bash", {"command": "FOO=bar pytest"}) == "Bash: pytest"
    assert s("Glob", {"pattern": "**/*.py"}) == "Glob **/*.py"
    assert s("Grep", {"pattern": "TODO"}) == "Grep TODO"
    assert s("LS", {"path": "/Users/me/repo/src"}) == "LS repo/src"
    assert s("TodoWrite", {"todos": [1, 2, 3]}) == "TodoWrite (3 todos)"
    assert s("TodoWrite", {"todos": [1]}) == "TodoWrite (1 todo)"
    assert s("Task", {"description": "Refactor the parser", "subagent_type": "general"}).startswith("Task: Refactor")
    assert s("WebFetch", {"url": "https://example.com"}) == "WebFetch https://example.com"
    # MCP tools collapse server__tool → server.tool
    assert s("mcp__github__create_pr", {}) == "github.create_pr"
    # Unknown tool with a path-ish arg still reads sensibly.
    assert s("FancyNewTool", {"file_path": "x/y.txt"}) == "FancyNewTool x/y.txt"
    assert s("FancyNewTool", {"query": "hello world"}) == "FancyNewTool: hello world"
    # Degenerate input never crashes.
    assert s("", None) == "(unparseable)"
    assert s("Bash", None) == "Bash"


def test_summary_in_event_stream(conn):
    pid = _seed_project(conn)
    pairs = [
        ("Edit", {"file_path": "routes/cost.py", "old_string": "x", "new_string": "y"}),
        ("Bash", {"command": "pytest tests/ -q"}),
    ]
    _seed_linear(conn, project_id=pid, session_id="sum", pairs=pairs)
    events = playback.session_playback(conn, "sum")
    assert [e.summary for e in events] == ["Edit routes/cost.py", "Bash: pytest"]


# ── success flag ─────────────────────────────────────────────────────────────


def test_success_from_transcript_is_error(conn):
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="err")
    _seed_message(conn, session_fk=sfk, seq=0, role="assistant",
                  raw=_assistant(("ok", "Bash", {"command": "true"})), ts="2026-05-01T00:00:01Z")
    _seed_message(conn, session_fk=sfk, seq=1, role="user",
                  raw=_result(("ok", "done", False)), ts="2026-05-01T00:00:02Z")
    _seed_message(conn, session_fk=sfk, seq=2, role="assistant",
                  raw=_assistant(("bad", "Bash", {"command": "false"})), ts="2026-05-01T00:00:03Z")
    _seed_message(conn, session_fk=sfk, seq=3, role="user",
                  raw=_result(("bad", "boom", True)), ts="2026-05-01T00:00:04Z")
    # A tool call with no matching result → success unknown (None).
    _seed_message(conn, session_fk=sfk, seq=4, role="assistant",
                  raw=_assistant(("dangling", "Read", {"file_path": "z.py"})), ts="2026-05-01T00:00:05Z")
    conn.commit()
    events = playback.session_playback(conn, "err")
    assert [e.success for e in events] == [True, False, None]


def test_success_overlay_from_captured_events_when_present(conn):
    """When the spec-05 ``captured_events`` table exists, a 'failure' event
    near a tool-call message marks that call ``success=False`` even if the
    transcript didn't carry an ``is_error`` flag."""
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="hooked")
    _seed_message(conn, session_fk=sfk, seq=0, role="assistant",
                  raw=_assistant(("t0", "Bash", {"command": "deploy"})), ts="2026-05-01T00:10:00Z")
    # Transcript result has no is_error — but a hook recorded a failure.
    _seed_message(conn, session_fk=sfk, seq=1, role="user",
                  raw=_result(("t0", "exit 1", False)), ts="2026-05-01T00:10:01Z")
    # Mimic the spec-05 schema (session_id text, ts, event_kind).
    conn.execute(
        "CREATE TABLE captured_events ("
        " id INTEGER PRIMARY KEY, ts TEXT NOT NULL, project_id INTEGER, session_id TEXT,"
        " hook_id TEXT NOT NULL, event_kind TEXT NOT NULL, payload_json TEXT NOT NULL)"
    )
    conn.execute(
        "INSERT INTO captured_events (ts, project_id, session_id, hook_id, event_kind, payload_json) "
        "VALUES (?, ?, ?, 'posttooluse', 'failure', '{}')",
        ("2026-05-01T00:10:02Z", pid, "hooked"),
    )
    conn.commit()
    events = playback.session_playback(conn, "hooked")
    assert len(events) == 1
    assert events[0].success is False


def test_works_without_captured_events_table(conn):
    """No ``captured_events`` table (the common, hook-less case) → ``success``
    is purely transcript-driven; nothing crashes."""
    pid = _seed_project(conn)
    _seed_linear(conn, project_id=pid, session_id="nohooks", pairs=_reads(3))
    # The seeder writes is_error=False results, so success is True for each.
    assert [e.success for e in playback.session_playback(conn, "nohooks")] == [True, True, True]


# ── byte_count / duration / payload_excerpt ──────────────────────────────────


def test_byte_count_duration_and_excerpt(conn):
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="meta")
    _seed_message(conn, session_fk=sfk, seq=0, role="assistant",
                  raw=_assistant(("r", "Read", {"file_path": "a.py"})), ts="2026-05-01T00:00:00Z")
    # 6 utf-8 bytes for "héllo".
    _seed_message(conn, session_fk=sfk, seq=1, role="user",
                  raw=_result(("r", "héllo", False)), ts="2026-05-01T00:00:03Z")
    # A Write with no result yet → byte_count from the written content.
    _seed_message(conn, session_fk=sfk, seq=2, role="assistant",
                  raw=_assistant(("w", "Write", {"file_path": "b.py", "content": "abcd"})),
                  ts="2026-05-01T00:00:05Z")
    conn.commit()
    by_tool = {e.tool_name: e for e in playback.session_playback(conn, "meta")}
    assert by_tool["Read"].byte_count == len("héllo".encode())
    assert by_tool["Read"].duration_ms == 3000
    assert by_tool["Write"].byte_count == 4
    assert by_tool["Write"].duration_ms is None  # no result message → not computable
    assert by_tool["Read"].payload_excerpt
    assert all(e.payload_excerpt == "" for e in playback.session_playback(conn, "meta", include_payload=False))


def test_payload_excerpt_is_capped(conn):
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="long")
    _seed_message(conn, session_fk=sfk, seq=0, role="assistant",
                  raw=_assistant(("x", "Bash", {"command": "echo hi"})), ts="2026-05-01T00:00:00Z")
    _seed_message(conn, session_fk=sfk, seq=1, role="user",
                  raw=_result(("x", "Z" * 5000, False)), ts="2026-05-01T00:00:01Z")
    conn.commit()
    e = playback.session_playback(conn, "long")[0]
    assert len(e.payload_excerpt) <= 200


# ── malformed input ──────────────────────────────────────────────────────────


def test_malformed_raw_json_never_crashes(conn):
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="bad")
    # Row 0: raw_json is not valid JSON at all.
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
        " output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
        " raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, 0, '2026-05-01T00:00:00Z', 'assistant', 'm', 0, 0, 0, 0, '', '[]', "
        " 'NOT JSON{', 0, 'u0', NULL)",
        (sfk,),
    )
    # Row 1: valid envelope, tool_use block with a non-string name.
    bad_block = {"type": "tool_use", "id": "z", "name": None, "input": {}}
    _seed_message(conn, session_fk=sfk, seq=1, role="assistant",
                  raw={"type": "assistant", "message": {"role": "assistant", "content": [bad_block]}},
                  ts="2026-05-01T00:00:01Z")
    # Row 2: a normal one so we know parsing continued past the bad rows.
    _seed_message(conn, session_fk=sfk, seq=2, role="assistant",
                  raw=_assistant(("ok", "Read", {"file_path": "a.py"})), ts="2026-05-01T00:00:02Z")
    conn.commit()
    events = playback.session_playback(conn, "bad")
    summaries = [e.summary for e in events]
    assert "(unparseable)" in summaries
    assert "Read a.py" in summaries
    # When filtering, the marker rows are skipped (no real tool name to match).
    assert [e.summary for e in playback.session_playback(conn, "bad", tool_filter=["Read"])] == ["Read a.py"]


# ── limit / truncation ───────────────────────────────────────────────────────


def test_limit_caps_with_truncated_flag(conn):
    pid = _seed_project(conn)
    _seed_linear(conn, project_id=pid, session_id="cap", pairs=_reads(15))
    page = playback.session_playback_page(conn, "cap", limit=10)
    assert page is not None
    events, truncated = page
    assert len(events) == 10
    assert truncated is True
    assert [e.seq for e in events] == list(range(10))

    page = playback.session_playback_page(conn, "cap", limit=100)
    assert page is not None
    events, truncated = page
    assert len(events) == 15
    assert truncated is False


# ── project_timeline ─────────────────────────────────────────────────────────


def test_project_timeline_interleaves_sessions_in_ts_order(conn):
    pid = _seed_project(conn)
    # Session A: assistant msgs at minutes 0, 2. Session B: at minutes 1, 3.
    _seed_linear(conn, project_id=pid, session_id="A",
                 pairs=[("Read", {"file_path": "a0.py"}), ("Read", {"file_path": "a1.py"})], start_minute=0)
    _seed_linear(conn, project_id=pid, session_id="B",
                 pairs=[("Read", {"file_path": "b0.py"}), ("Read", {"file_path": "b1.py"})], start_minute=1)
    events = playback.project_timeline(conn, pid)
    assert [(e.session_id, e.target_path) for e in events] == [
        ("A", "a0.py"), ("B", "b0.py"), ("A", "a1.py"), ("B", "b1.py"),
    ]
    assert [e.seq for e in events] == [0, 1, 2, 3]
    # Cross-session payload excerpts are opt-out by default on this surface.
    assert all(e.payload_excerpt == "" for e in events)
    assert any(e.payload_excerpt for e in playback.project_timeline(conn, pid, include_payload=True))


def test_project_timeline_since_filters(conn):
    pid = _seed_project(conn)
    _seed_linear(conn, project_id=pid, session_id="S", pairs=_reads(4), start_minute=0)
    # Pair i's assistant message is at minute 2*i; since="...:04:00Z" keeps i>=2.
    events = playback.project_timeline(conn, pid, since="2026-05-01T00:04:00Z")
    assert [e.target_path for e in events] == ["f2.py", "f3.py"]
    assert len(events) == 2


def test_project_timeline_unknown_project_is_empty(conn):
    assert playback.project_timeline(conn, 99999) == []
    assert playback.project_timeline_page(conn, 99999) == ([], False)


def test_project_timeline_pagination_tail_marker(conn):
    pid = _seed_project(conn)
    _seed_linear(conn, project_id=pid, session_id="big", pairs=_reads(20))
    events, truncated = playback.project_timeline_page(conn, pid, limit=5)
    assert len(events) == 5
    assert truncated is True


def test_project_timeline_tool_filter(conn):
    pid = _seed_project(conn)
    pairs = [
        ("Read", {"file_path": "a.py"}),
        ("Edit", {"file_path": "b.py", "old_string": "1", "new_string": "2"}),
        ("Bash", {"command": "ls"}),
    ]
    _seed_linear(conn, project_id=pid, session_id="S", pairs=pairs)
    assert [e.tool_name for e in playback.project_timeline(conn, pid, tool_filter=["Edit"])] == ["Edit"]


# ── serialisation ────────────────────────────────────────────────────────────


def test_playback_event_to_dict_round_trips_all_fields(conn):
    pid = _seed_project(conn)
    _seed_linear(conn, project_id=pid, session_id="ser", pairs=[("Read", {"file_path": "a.py"})])
    e = playback.session_playback(conn, "ser")[0]
    d = playback.playback_event_to_dict(e)
    assert set(d) == {
        "seq", "ts", "message_id", "tool_name", "summary", "target_path",
        "byte_count", "success", "duration_ms", "payload_excerpt", "session_id",
    }
    assert d["tool_name"] == "Read"
    assert d["session_id"] == "ser"
