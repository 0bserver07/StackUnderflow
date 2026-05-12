"""MessageToolMartBuilder — per-(message, tool, call_index) detail rows.

Locks in the v011 contract: each ``usage_events`` row fans out to one
mart row per ``tool_use`` block in its source message's ``raw_json``,
with ``file_path`` / ``byte_count`` / per-tool ``call_index``;
``INSERT OR IGNORE`` on ``UNIQUE(message_id, tool_name, call_index)``
keeps refresh idempotent; the watermark advances to ``max(events.id)``.
"""

from __future__ import annotations

import json
from typing import Any

from stackunderflow.etl.marts.message_tool import (
    MessageToolMartBuilder,
    _parse_tool_calls,
)
from stackunderflow.etl.watermark import get_watermark, refresh_all_marts

from .conftest import insert_event, insert_message

# ── raw_json fixtures ──────────────────────────────────────────────────


def _assistant_raw(*blocks: dict[str, Any]) -> str:
    """A Claude-style assistant ``raw_json`` carrying ``blocks`` as content."""
    return json.dumps({"message": {"role": "assistant", "content": list(blocks)}})


def _tool_use(name: str, inp: dict[str, Any] | None = None, *, tuid: str | None = None) -> dict:
    block: dict[str, Any] = {"type": "tool_use", "name": name, "input": inp or {}}
    if tuid is not None:
        block["id"] = tuid
    return block


def _user_raw(*results: dict[str, Any]) -> str:
    """A Claude-style user ``raw_json`` carrying ``tool_result`` blocks."""
    return json.dumps({"message": {"role": "user", "content": list(results)}})


def _tool_result(tool_use_id: str, content: Any) -> dict:
    return {"type": "tool_result", "tool_use_id": tool_use_id, "content": content}


def _rows(conn) -> list[dict]:
    return [dict(r) for r in conn.execute(
        "SELECT message_id, project_id, session_id, ts, day, "
        "tool_name, file_path, byte_count, call_index "
        "FROM message_tool_mart ORDER BY message_id, tool_name, call_index"
    ).fetchall()]


def _snapshot(conn) -> list[str]:
    """Stable, comparison-safe snapshot of the mart (rows JSON-serialised).

    JSON-encoding sidesteps the ``None < int`` problem you hit sorting
    tuples that mix nullable ``file_path`` / ``byte_count`` with the
    non-null columns.
    """
    return sorted(json.dumps(r, sort_keys=True) for r in _rows(conn))


# ── _parse_tool_calls (pure) ───────────────────────────────────────────


def test_parse_empty_or_garbage_returns_empty() -> None:
    assert _parse_tool_calls(None) == []
    assert _parse_tool_calls("") == []
    assert _parse_tool_calls("not json{") == []
    assert _parse_tool_calls("{}") == []
    assert _parse_tool_calls(json.dumps({"message": {"content": "nope"}})) == []
    assert _parse_tool_calls(json.dumps({"message": {"content": [1, 2, 3]}})) == []
    # text-only assistant turn → no tool calls
    assert _parse_tool_calls(_assistant_raw({"type": "text", "text": "hi"})) == []


def test_parse_single_read_has_file_path() -> None:
    calls = _parse_tool_calls(_assistant_raw(_tool_use("Read", {"file_path": "/a/b.py"})))
    assert len(calls) == 1
    assert calls[0].tool_name == "Read"
    assert calls[0].file_path == "/a/b.py"
    assert calls[0].call_index == 0
    # Read has no input payload and no result paired → byte_count None
    assert calls[0].byte_count is None


def test_parse_path_keys_priority() -> None:
    assert _parse_tool_calls(_assistant_raw(_tool_use("Grep", {"path": "/p"})))[0].file_path == "/p"
    assert (
        _parse_tool_calls(_assistant_raw(_tool_use("NotebookEdit", {"notebook_path": "/n.ipynb"})))[0].file_path
        == "/n.ipynb"
    )
    # Bash with no path-bearing input → file_path NULL
    assert _parse_tool_calls(_assistant_raw(_tool_use("Bash", {"command": "ls"})))[0].file_path is None


def test_parse_task_uses_subagent_type_as_file_path() -> None:
    calls = _parse_tool_calls(_assistant_raw(_tool_use("Task", {"subagent_type": "code-reviewer", "prompt": "x"})))
    assert calls[0].tool_name == "Task"
    assert calls[0].file_path == "code-reviewer"
    # legacy "agent" key
    calls2 = _parse_tool_calls(_assistant_raw(_tool_use("Task", {"agent": "explorer"})))
    assert calls2[0].file_path == "explorer"


def test_parse_call_index_is_per_tool_name() -> None:
    raw = _assistant_raw(
        _tool_use("Read", {"file_path": "/1"}),
        _tool_use("Edit", {"file_path": "/1", "new_string": "x"}),
        _tool_use("Read", {"file_path": "/2"}),
        _tool_use("Read", {"file_path": "/3"}),
        _tool_use("Edit", {"file_path": "/2", "new_string": "yy"}),
    )
    calls = _parse_tool_calls(raw)
    by_pair = {(c.tool_name, c.call_index): c for c in calls}
    assert set(by_pair) == {("Read", 0), ("Read", 1), ("Read", 2), ("Edit", 0), ("Edit", 1)}
    assert by_pair[("Read", 0)].file_path == "/1"
    assert by_pair[("Read", 2)].file_path == "/3"


def test_parse_byte_count_from_write_family_input() -> None:
    # Write → len(content)
    c = _parse_tool_calls(_assistant_raw(_tool_use("Write", {"file_path": "/w", "content": "héllo"})))[0]
    assert c.byte_count == len("héllo".encode())  # 6 bytes (é is 2)
    # Edit → len(new_string)
    edit_inp = {"file_path": "/e", "old_string": "aaaa", "new_string": "bb"}
    c = _parse_tool_calls(_assistant_raw(_tool_use("Edit", edit_inp)))[0]
    assert c.byte_count == 2
    # NotebookEdit → len(new_source)
    c = _parse_tool_calls(_assistant_raw(_tool_use("NotebookEdit", {"notebook_path": "/n", "new_source": "abc"})))[0]
    assert c.byte_count == 3
    # MultiEdit → sum of new_string sizes
    c = _parse_tool_calls(_assistant_raw(_tool_use("MultiEdit", {
        "file_path": "/m",
        "edits": [{"old_string": "x", "new_string": "12"}, {"old_string": "y", "new_string": "345"}],
    })))[0]
    assert c.byte_count == 5
    # MultiEdit with no usable edits → None
    c = _parse_tool_calls(_assistant_raw(_tool_use("MultiEdit", {"file_path": "/m", "edits": []})))[0]
    assert c.byte_count is None


def test_parse_byte_count_from_paired_tool_result() -> None:
    raw = _assistant_raw(_tool_use("Bash", {"command": "ls"}, tuid="toolu_1"))
    # result content as a plain string
    sizes = {"toolu_1": 1234}
    c = _parse_tool_calls(raw, result_sizes=sizes)[0]
    assert c.byte_count == 1234
    # no match → None
    assert _parse_tool_calls(raw, result_sizes={"other": 5})[0].byte_count is None
    # block without an id can't be paired
    raw_noid = _assistant_raw(_tool_use("Bash", {"command": "ls"}))
    assert _parse_tool_calls(raw_noid, result_sizes={"toolu_1": 9})[0].byte_count is None


# ── builder: refresh / watermark / idempotency ─────────────────────────


def test_empty_events_returns_zero(conn) -> None:
    new = MessageToolMartBuilder().refresh(conn, since_event_id=0)
    assert new == 0
    assert _rows(conn) == []


def test_no_tool_use_blocks_creates_no_rows(conn) -> None:
    # default raw_json is '{}' — no tool_use blocks
    insert_event(conn, event_id=1, cost_usd=0.1)
    MessageToolMartBuilder().refresh(conn, since_event_id=0)
    assert _rows(conn) == []


def test_single_message_fans_out_per_call(conn) -> None:
    raw = _assistant_raw(
        _tool_use("Read", {"file_path": "/a"}),
        _tool_use("Read", {"file_path": "/b"}),
        _tool_use("Edit", {"file_path": "/a", "new_string": "patched"}),
    )
    insert_event(conn, event_id=1, msg_id=10, raw_json=raw, day="2024-01-01", session_id="sess-1")
    new = MessageToolMartBuilder().refresh(conn, since_event_id=0)
    assert new == 1
    rows = _rows(conn)
    assert len(rows) == 3
    reads = sorted((r for r in rows if r["tool_name"] == "Read"), key=lambda r: r["call_index"])
    assert [r["call_index"] for r in reads] == [0, 1]
    assert [r["file_path"] for r in reads] == ["/a", "/b"]
    assert all(r["message_id"] == 10 and r["session_id"] == "sess-1" and r["day"] == "2024-01-01" for r in rows)
    edit = next(r for r in rows if r["tool_name"] == "Edit")
    assert edit["call_index"] == 0
    assert edit["byte_count"] == len("patched")


def test_bash_byte_count_from_following_tool_result(conn) -> None:
    # Assistant message (seq 5) runs Bash; the next message (seq 6) carries
    # the tool_result the builder sizes byte_count from.
    raw = _assistant_raw(_tool_use("Bash", {"command": "find /"}, tuid="toolu_bash"))
    insert_event(conn, event_id=1, msg_id=10, seq=5, session_fk=1, raw_json=raw)
    big = "x" * 60_000
    insert_message(
        conn, msg_id=11, session_fk=1, seq=6, role="user",
        raw_json=_user_raw(_tool_result("toolu_bash", big)),
    )
    MessageToolMartBuilder().refresh(conn, since_event_id=0)
    row = conn.execute("SELECT tool_name, byte_count FROM message_tool_mart").fetchone()
    assert row["tool_name"] == "Bash"
    assert row["byte_count"] == 60_000


def test_result_content_as_text_block_list(conn) -> None:
    raw = _assistant_raw(_tool_use("Read", {"file_path": "/f"}, tuid="t1"))
    insert_event(conn, event_id=1, msg_id=10, seq=5, session_fk=1, raw_json=raw)
    insert_message(
        conn, msg_id=11, session_fk=1, seq=6, role="user",
        raw_json=_user_raw(_tool_result("t1", [{"type": "text", "text": "abc"}, {"type": "text", "text": "de"}])),
    )
    MessageToolMartBuilder().refresh(conn, since_event_id=0)
    assert conn.execute("SELECT byte_count FROM message_tool_mart").fetchone()["byte_count"] == 5


def test_idempotency_re_running_with_watermark(conn) -> None:
    raw = _assistant_raw(_tool_use("Read", {"file_path": "/a"}), _tool_use("Bash", {"command": "ls"}))
    insert_event(conn, event_id=1, msg_id=10, raw_json=raw)
    b = MessageToolMartBuilder()
    w = b.refresh(conn, since_event_id=0)
    before = _rows(conn)
    # Re-running with the persisted watermark must be a no-op.
    b.refresh(conn, since_event_id=w)
    assert _rows(conn) == before
    # Re-running from 0 must also be a no-op (UNIQUE dedup).
    b.refresh(conn, since_event_id=0)
    assert _rows(conn) == before


def test_watermark_advances_and_incremental_picks_up(conn) -> None:
    insert_event(conn, event_id=1, msg_id=10, raw_json=_assistant_raw(_tool_use("Read", {"file_path": "/a"})))
    b = MessageToolMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    assert w1 == 1
    assert len(_rows(conn)) == 1
    # Second event after the watermark.
    raw2 = _assistant_raw(_tool_use("Edit", {"file_path": "/b", "new_string": "z"}))
    insert_event(conn, event_id=2, msg_id=11, raw_json=raw2)
    w2 = b.refresh(conn, since_event_id=w1)
    assert w2 == 2
    rows = _rows(conn)
    assert len(rows) == 2
    assert {r["tool_name"] for r in rows} == {"Read", "Edit"}


def test_orphaned_event_contributes_nothing(conn) -> None:
    # An event whose source message doesn't exist (FK dropped in v008).
    conn.execute(
        "INSERT INTO usage_events (id, source_message_fk, provider, project_id, "
        "session_id, ts, day, model, speed, input_tokens, output_tokens, "
        "cache_read_tokens, cache_create_tokens, cost_usd, cost_source, role) "
        "VALUES (1, 99999, 'claude', 1, 'sess-1', '2024-01-01T00:00:00Z', "
        "'2024-01-01', 'sonnet', 'standard', 0, 0, 0, 0, 0.0, 'rate_card', 'assistant')"
    )
    new = MessageToolMartBuilder().refresh(conn, since_event_id=0)
    assert new == 1  # watermark still advances
    assert _rows(conn) == []


def test_malformed_raw_json_skipped_not_raised(conn) -> None:
    insert_event(conn, event_id=1, msg_id=10, raw_json="not json{")
    insert_event(conn, event_id=2, msg_id=11, raw_json=_assistant_raw(_tool_use("Read", {"file_path": "/ok"})))
    MessageToolMartBuilder().refresh(conn, since_event_id=0)
    rows = _rows(conn)
    assert len(rows) == 1
    assert rows[0]["tool_name"] == "Read" and rows[0]["file_path"] == "/ok"


def test_rebuild_from_scratch_matches_incremental(conn) -> None:
    insert_event(conn, event_id=1, msg_id=10, seq=1, raw_json=_assistant_raw(
        _tool_use("Read", {"file_path": "/a"}, tuid="r1"),
        _tool_use("Read", {"file_path": "/b"}),
    ))
    insert_message(
        conn, msg_id=11, session_fk=1, seq=2, role="user",
        raw_json=_user_raw(_tool_result("r1", "result-text")),
    )
    insert_event(
        conn, event_id=2, msg_id=12, seq=3,
        raw_json=_assistant_raw(_tool_use("Write", {"file_path": "/c", "content": "hello"})),
    )
    b = MessageToolMartBuilder()
    b.refresh(conn, since_event_id=0)
    incremental = _snapshot(conn)
    b.rebuild_from_scratch(conn)
    rebuilt = _snapshot(conn)
    assert incremental == rebuilt
    assert len(rebuilt) == 3


def test_two_window_incremental_matches_one_shot(conn) -> None:
    # Window 1: one event.
    insert_event(conn, event_id=1, msg_id=10, seq=1, raw_json=_assistant_raw(_tool_use("Read", {"file_path": "/a"})))
    b = MessageToolMartBuilder()
    w1 = b.refresh(conn, since_event_id=0)
    # Window 2: another event + a re-refresh that re-sees event 1 (idempotent).
    insert_event(conn, event_id=2, msg_id=11, seq=2, raw_json=_assistant_raw(
        _tool_use("Bash", {"command": "ls"}, tuid="b1"),
        _tool_use("Edit", {"file_path": "/a", "new_string": "patched"}),
    ))
    insert_message(conn, msg_id=12, session_fk=1, seq=3, role="user", raw_json=_user_raw(_tool_result("b1", "x" * 100)))
    b.refresh(conn, since_event_id=w1)
    incremental = _snapshot(conn)
    b.rebuild_from_scratch(conn)
    one_shot = _snapshot(conn)
    assert incremental == one_shot


def test_refresh_all_marts_includes_message_tool(conn) -> None:
    insert_event(conn, event_id=1, msg_id=10, raw_json=_assistant_raw(_tool_use("Read", {"file_path": "/a"})))
    processed = refresh_all_marts(conn)
    assert "message_tool" in processed
    assert get_watermark(conn, "message_tool") == 1
    assert conn.execute("SELECT COUNT(*) AS n FROM message_tool_mart").fetchone()["n"] == 1
