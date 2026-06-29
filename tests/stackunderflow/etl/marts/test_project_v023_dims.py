"""Equivalence: ``project_mart`` v023 Overview rate dims == full pipeline.

The North Star for v023 (ui-perf-audit #20 + the cache / interruption / errors
blocks that read 0 on the mart fast-path): the numerators
``ProjectMartBuilder`` writes onto ``project_mart`` must EXACTLY equal what
``get_project_stats`` (the full classifier → enricher → aggregator pass)
computes for the same project, AND the rates ``routes/data._stats_from_marts``
derives from them must match the aggregator's own rate fields.

We build a synthetic store with real ``messages.raw_json`` engineered so EVERY
new dim is non-zero, run the mart refresh, and assert:

* the raw mart COLUMNS against the pipeline's own internals
  (``cache.messages_with_cache_read``, ``errors.total`` / ``by_category``,
  ``user_interactions.commands_followed_by_interruption`` /
  ``total_tools_used`` / ``total_assistant_steps``), and
* the mart-path RATES (``cache.hit_rate``, ``interruption_rate``,
  ``avg_tools_per_command``, ``avg_steps_per_command``, ``errors.rate``)
  against the same fields from ``get_project_stats``.
"""

from __future__ import annotations

import json
from pathlib import Path

from stackunderflow.etl.marts.project import ProjectMartBuilder
from stackunderflow.routes import data as data_route
from stackunderflow.store import db, queries, schema

# ── store builders ───────────────────────────────────────────────────────────


def _connect(tmp_path: Path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _insert_project(conn, *, pid: int, provider: str, slug: str) -> None:
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, "
        "first_seen, last_modified) VALUES (?, ?, ?, ?, 0, 0)",
        (pid, provider, slug, slug),
    )


def _insert_session(conn, *, sid: int, pid: int, session_id: str) -> None:
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id) VALUES (?, ?, ?)",
        (sid, pid, session_id),
    )


def _insert_message(conn, *, mid, session_fk, seq, ts, role, payload) -> None:
    conn.execute(
        "INSERT INTO messages (id, session_fk, seq, timestamp, role, raw_json) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        (mid, session_fk, seq, ts, role, json.dumps(payload)),
    )


def _insert_assistant_event(conn, *, eid, mid, pid, provider, session_id, ts) -> None:
    """Mirror the normalizer: only billable assistant rows become events.

    The project_mart row only exists when usage_events has rows for the
    project; the v023 dims themselves are sourced from ``messages.raw_json``.
    """
    conn.execute(
        "INSERT INTO usage_events (id, source_message_fk, provider, project_id, "
        "session_id, ts, day, model, speed, input_tokens, output_tokens, "
        "cache_read_tokens, cache_create_tokens, cost_usd, cost_source, role) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, 'm', 'standard', 10, 5, 0, 0, 0.0, "
        "'rate_card', 'assistant')",
        (eid, mid, provider, pid, session_id, ts, ts[:10]),
    )


def _user(content) -> dict:
    return {"type": "user", "message": {"role": "user", "content": content}}


def _assistant(content, *, cache_read: int = 0) -> dict:
    return {
        "type": "assistant",
        "message": {
            "role": "assistant", "model": "m", "content": content,
            "usage": {
                "input_tokens": 10, "output_tokens": 5,
                "cache_read_input_tokens": cache_read,
            },
        },
    }


def _seed(conn, *, pid: int, provider: str, slug: str, sid: int, session_id: str) -> None:
    """Seed one session engineered so every v023 dim is non-zero.

    By construction:
      * 8 records total (overview.total_messages == 8)
      * 3 assistant rows, 2 of them carrying cache_read   -> w_read == 2
      * 1 error (a tool_result with is_error, "permission denied")
      * 2 real commands ("do X", "do Y"); "do X" is followed by an
        interruption          -> commands_followed_by_interruption == 1
      * tools per command: Read (do X) + Bash (do Y)      -> total_tools == 2
      * assistant steps: 2 (do X) + 1 (do Y)              -> total_steps == 3
    """
    _insert_project(conn, pid=pid, provider=provider, slug=slug)
    _insert_session(conn, sid=sid, pid=pid, session_id=session_id)
    rows = [
        ("user", _user("do X")),                                            # 1 command
        ("assistant", _assistant([                                          # 2 step+tool+cache
            {"type": "text", "text": "ok"},
            {"type": "tool_use", "name": "Read", "id": f"t{pid}a", "input": {"file_path": "/a"}},
        ], cache_read=100)),
        ("user", _user([                                                    # 3 tool_result ERROR
            {"type": "tool_result", "tool_use_id": f"t{pid}a",
             "content": "permission denied", "is_error": True},
        ])),
        ("assistant", _assistant([{"type": "text", "text": "done"}])),      # 4 step, no cache
        ("user", _user("[Request interrupted by user for tool use]")),      # 5 interruption
        ("user", _user("do Y")),                                            # 6 command
        ("assistant", _assistant([                                          # 7 step+tool+cache
            {"type": "text", "text": "go"},
            {"type": "tool_use", "name": "Bash", "id": f"t{pid}b", "input": {"command": "ls"}},
        ], cache_read=50)),
        ("summary", {"type": "summary", "summary": "recap"}),               # 8 summary
    ]
    eid = pid * 1000
    for i, (role, payload) in enumerate(rows):
        mid = pid * 100 + i
        ts = f"2026-04-0{i + 1}T00:00:00+00:00"
        _insert_message(conn, mid=mid, session_fk=sid, seq=i, ts=ts, role=role, payload=payload)
        if role == "assistant":
            _insert_assistant_event(conn, eid=eid, mid=mid, pid=pid,
                                    provider=provider, session_id=session_id, ts=ts)
            eid += 1


def _mart_row(conn, pid: int) -> dict:
    return dict(conn.execute(
        "SELECT * FROM project_mart WHERE project_id = ?", (pid,)
    ).fetchone())


# ── tests ────────────────────────────────────────────────────────────────────


def test_v023_columns_match_full_pipeline(tmp_path):
    """The materialised numerators equal the aggregator's own internals."""
    conn = _connect(tmp_path)
    _seed(conn, pid=1, provider="claude", slug="alpha", sid=1, session_id="s1")
    conn.commit()

    ProjectMartBuilder().rebuild_from_scratch(conn)
    _, stats = queries.get_project_stats(conn, project_id=1)
    row = _mart_row(conn, 1)

    cache = stats["cache"]
    ui = stats["user_interactions"]
    errors = stats["errors"]

    # Sanity: the fixture must keep every dim non-zero (no vacuous pass).
    assert cache["messages_with_cache_read"] == 2
    assert errors["total"] == 1
    assert ui["commands_followed_by_interruption"] == 1
    assert ui["total_tools_used"] == 2
    assert ui["total_assistant_steps"] == 3

    # Equivalence: mart columns == pipeline internals.
    assert row["total_records"] == stats["overview"]["total_messages"]
    assert row["total_cache_read_messages"] == cache["messages_with_cache_read"]
    assert row["total_errors"] == errors["total"]
    assert json.loads(row["errors_by_category"]) == errors["by_category"]
    assert json.loads(row["errors_by_category"]) == {"Permission Error": 1}
    assert row["total_commands_followed_by_interruption"] == ui["commands_followed_by_interruption"]
    assert row["total_command_tools"] == ui["total_tools_used"]
    assert row["total_command_steps"] == ui["total_assistant_steps"]
    conn.close()


def test_v023_mart_path_rates_match_pipeline(tmp_path):
    """``_stats_from_marts`` derives the SAME rates the aggregator emits."""
    conn = _connect(tmp_path)
    _seed(conn, pid=1, provider="claude", slug="alpha", sid=1, session_id="s1")
    conn.commit()

    ProjectMartBuilder().rebuild_from_scratch(conn)
    _, pipe = queries.get_project_stats(conn, project_id=1)
    mart = data_route._stats_from_marts(conn, project_ids=[1])
    conn.close()

    # Rates are non-zero so the assertions can't pass vacuously.
    assert pipe["cache"]["hit_rate"] > 0
    assert pipe["user_interactions"]["interruption_rate"] > 0

    assert mart["cache"]["hit_rate"] == pipe["cache"]["hit_rate"]
    m_ui, p_ui = mart["user_interactions"], pipe["user_interactions"]
    assert m_ui["user_commands_analyzed"] == p_ui["user_commands_analyzed"]
    assert m_ui["interruption_rate"] == p_ui["interruption_rate"]
    assert m_ui["avg_tools_per_command"] == p_ui["avg_tools_per_command"]
    assert m_ui["avg_steps_per_command"] == p_ui["avg_steps_per_command"]
    assert mart["errors"]["total"] == pipe["errors"]["total"]
    assert mart["errors"]["rate"] == pipe["errors"]["rate"]
    assert mart["errors"]["by_category"] == pipe["errors"]["by_category"]


def test_v023_incremental_refresh_matches_rebuild(tmp_path):
    """A watermarked incremental refresh produces the same v023 dims as rebuild."""
    conn = _connect(tmp_path)
    _seed(conn, pid=1, provider="claude", slug="alpha", sid=1, session_id="s1")
    conn.commit()

    b = ProjectMartBuilder()
    b.refresh(conn, since_event_id=0)
    inc = _mart_row(conn, 1)
    b.rebuild_from_scratch(conn)
    full = _mart_row(conn, 1)

    cols = (
        "total_records", "total_errors", "errors_by_category",
        "total_cache_read_messages", "total_commands_followed_by_interruption",
        "total_command_tools", "total_command_steps",
    )
    assert {c: inc[c] for c in cols} == {c: full[c] for c in cols}
    conn.close()
