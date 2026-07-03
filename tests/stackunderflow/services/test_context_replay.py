"""Tests for ``stackunderflow.services.context_replay`` — context replay (#96).

Locks the MVP reconstruction contract:

* the reconstruction at ``at_seq=K`` contains exactly the messages with
  ``seq <= K``, in ``seq`` order;
* ``at_seq=None`` returns the whole session;
* the running ``cumulative_tokens`` total is monotonic non-decreasing;
* each event carries ``role`` / ``content_preview`` / ``tokens`` /
  ``cumulative_tokens`` / ``tool_calls``;
* tool calls are surfaced (from ``raw_json``, with a ``tools_json`` fallback);
* the content preview is capped;
* an unknown session is empty-but-valid (never raises);
* an empty session is empty-but-valid;
* ``slice_context_timeline`` re-slices + retotals a full build;
* ``empty_context`` is the canonical empty shape.
"""

from __future__ import annotations

import json

from stackunderflow.services import context_replay
from stackunderflow.store import db, schema


# ── seed helpers ────────────────────────────────────────────────────────────


def _seed_project(conn, *, slug: str = "demo") -> int:
    return int(
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, "
            " last_modified) VALUES ('claude', ?, ?, 0.0, 1.0)",
            (slug, slug),
        ).lastrowid
    )


def _seed_session(conn, *, project_id: int, session_id: str) -> int:
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, '2026-05-01T00:00:00Z', "
        "'2026-05-01T01:00:00Z', 0)",
        (project_id, session_id),
    )
    return int(
        conn.execute(
            "SELECT id FROM sessions WHERE session_id = ?", (session_id,)
        ).fetchone()["id"]
    )


def _seed_msg(conn, *, sfk, seq, role, content_text="", raw=None, tools_json="[]"):
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, ?, ?, ?, 0, ?, NULL)",
        (
            sfk, seq, f"2026-05-01T00:{seq:02d}:00Z", role, content_text,
            tools_json, json.dumps(raw or {}), f"u{sfk}-{seq}",
        ),
    )


def _assistant_tool(tool_use_id, name, tool_input):
    return {
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": tool_use_id, "name": name, "input": tool_input}
            ],
        },
    }


def _text(role, text):
    return {"type": role, "message": {"role": role, "content": [{"type": "text", "text": text}]}}


def _store(tmp_path):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _seed_basic(conn, *, session_id="s1", slug="demo"):
    """user → assistant(Edit tool) → user, three turns in seq order."""
    pid = _seed_project(conn, slug=slug)
    sfk = _seed_session(conn, project_id=pid, session_id=session_id)
    _seed_msg(conn, sfk=sfk, seq=0, role="user",
              content_text="implement the feature", raw=_text("user", "implement the feature"))
    _seed_msg(conn, sfk=sfk, seq=1, role="assistant", content_text="",
              raw=_assistant_tool("t1", "Edit", {"file_path": "a.py", "old_string": "x", "new_string": "y"}))
    _seed_msg(conn, sfk=sfk, seq=2, role="user",
              content_text="thanks that worked", raw=_text("user", "thanks that worked"))
    conn.commit()
    return sfk


# ── the K-prefix contract ───────────────────────────────────────────────────


def test_reconstruction_at_k_contains_exactly_messages_le_k_in_order(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    for k in (0, 1, 2):
        result = context_replay.reconstruct_context(conn, session_id="s1", at_seq=k)
        seqs = [e["seq"] for e in result["events"]]
        assert seqs == list(range(k + 1)), (k, seqs)
        assert seqs == sorted(seqs)  # in order
        assert result["message_count"] == k + 1
        assert result["at_seq"] == k
    conn.close()


def test_at_seq_none_returns_whole_session(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    result = context_replay.reconstruct_context(conn, session_id="s1", at_seq=None)
    assert [e["seq"] for e in result["events"]] == [0, 1, 2]
    assert result["at_seq"] is None
    conn.close()


def test_at_seq_before_first_message_is_empty_but_valid(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    result = context_replay.reconstruct_context(conn, session_id="s1", at_seq=-1)
    assert result["events"] == []
    assert result["message_count"] == 0
    assert result["total_tokens"] == 0
    conn.close()


# ── running token total ─────────────────────────────────────────────────────


def test_cumulative_token_total_is_monotonic(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    result = context_replay.reconstruct_context(conn, session_id="s1")
    cum = [e["cumulative_tokens"] for e in result["events"]]
    assert cum == sorted(cum), cum  # monotonic non-decreasing
    assert all(c >= 0 for c in cum)
    # total_tokens == the last event's cumulative
    assert result["total_tokens"] == cum[-1]
    # each event's cumulative == prefix sum of the per-message estimates
    running = 0
    for e in result["events"]:
        running += e["tokens"]
        assert e["cumulative_tokens"] == running
    conn.close()


def test_sliced_total_matches_prefix(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    full = context_replay.build_context_timeline(conn, session_id="s1")
    at1 = context_replay.slice_context_timeline(full, at_seq=1)
    assert at1["total_tokens"] == full["events"][1]["cumulative_tokens"]
    assert at1["message_count"] == 2
    conn.close()


# ── per-event fields + tool calls ───────────────────────────────────────────


def test_events_carry_required_fields(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    result = context_replay.reconstruct_context(conn, session_id="s1")
    for e in result["events"]:
        assert set(e) >= {
            "seq", "role", "content_preview", "tokens",
            "cumulative_tokens", "tool_calls",
        }
        assert isinstance(e["tool_calls"], list)
    assert result["events"][0]["role"] == "user"
    assert result["events"][1]["role"] == "assistant"
    conn.close()


def test_tool_calls_surfaced_from_raw_json(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    result = context_replay.reconstruct_context(conn, session_id="s1")
    assert result["events"][1]["tool_calls"] == ["Edit a.py"]
    # A pure tool turn with empty text gets a bracketed tool preview.
    assert "Edit a.py" in result["events"][1]["content_preview"]
    conn.close()


def test_tool_calls_fallback_from_tools_json(tmp_path):
    """When raw_json has no tool_use blocks, fall back to the tools_json column
    (both the array-of-strings and array-of-objects shapes)."""
    conn = _store(tmp_path)
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="s-tj")
    # array-of-strings (the canonical writer shape)
    _seed_msg(conn, sfk=sfk, seq=0, role="assistant", content_text="",
              raw={"type": "assistant", "message": {"role": "assistant", "content": []}},
              tools_json=json.dumps(["Read"]))
    # array-of-objects (fixture / adapter shape)
    _seed_msg(conn, sfk=sfk, seq=1, role="assistant", content_text="",
              raw={"type": "assistant", "message": {"role": "assistant", "content": []}},
              tools_json=json.dumps([{"name": "Write", "input": {"file_path": "b.py"}}]))
    conn.commit()
    result = context_replay.reconstruct_context(conn, session_id="s-tj")
    assert result["events"][0]["tool_calls"] == ["Read"]
    assert result["events"][1]["tool_calls"] == ["Write b.py"]
    conn.close()


def test_content_preview_is_capped(tmp_path):
    conn = _store(tmp_path)
    pid = _seed_project(conn)
    sfk = _seed_session(conn, project_id=pid, session_id="s-long")
    long_text = "z" * 5000
    _seed_msg(conn, sfk=sfk, seq=0, role="user",
              content_text=long_text, raw=_text("user", long_text))
    conn.commit()
    result = context_replay.reconstruct_context(conn, session_id="s-long")
    preview = result["events"][0]["content_preview"]
    assert len(preview) <= context_replay._PREVIEW_CHARS
    assert preview.endswith("…")
    conn.close()


# ── advisory / never-raise ──────────────────────────────────────────────────


def test_missing_session_is_empty_not_raise(tmp_path):
    conn = _store(tmp_path)
    _seed_basic(conn)
    result = context_replay.reconstruct_context(conn, session_id="does-not-exist")
    assert result["events"] == []
    assert result["message_count"] == 0
    assert result["total_tokens"] == 0
    assert result["session_id"] == "does-not-exist"
    assert any("not found" in w for w in result["warnings"])
    conn.close()


def test_empty_session_is_empty_but_valid(tmp_path):
    conn = _store(tmp_path)
    pid = _seed_project(conn)
    _seed_session(conn, project_id=pid, session_id="empty")
    conn.commit()
    result = context_replay.reconstruct_context(conn, session_id="empty")
    assert result["session_id"] == "empty"
    assert result["events"] == []
    assert result["warnings"] == []  # session exists, just has no messages
    conn.close()


def test_empty_context_shape():
    e = context_replay.empty_context("sid", at_seq=7, warnings=["w"])
    assert e == {
        "session_id": "sid",
        "at_seq": 7,
        "message_count": 0,
        "total_tokens": 0,
        "events": [],
        "warnings": ["w"],
    }
