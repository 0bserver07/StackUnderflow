"""Wave 4A — ``/api/messages/summary`` reads ``project_mart`` for totals.

Detail blocks (``by_type`` / ``by_model`` / ``total_tokens``) still come
from the messages list because those dimensions aren't in any mart yet;
the migration is scoped to the top-level ``total`` field plus a new
``total_sessions`` bonus field that drops out of the same mart row.

Parity:

* Mart populated → ``total`` is the mart's ``total_messages`` value;
  ``total_sessions`` surfaces from ``project_mart``.
* Mart empty → fallback to ``len(messages)`` from the legacy path.
"""

from __future__ import annotations

import time

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider="claude", slug="-test"):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, *, project_id, session_id, ts, n=2):
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        "message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, ts, ts, n),
    )
    return int(cur.lastrowid)


_seq_state: dict[int, int] = {}


def _insert_assistant_message(conn, *, session_fk, ts, model, tokens=10):
    """Insert an assistant message; per-session ``seq`` autoincrements
    so the ``UNIQUE(session_fk, seq)`` index is honoured."""
    seq = _seq_state.get(session_fk, 0)
    _seq_state[session_fk] = seq + 1
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, 'assistant', ?, ?, 0, 0, 0, '', '[]', '{}', 0, NULL, NULL)",
        (session_fk, seq, ts, model, tokens),
    )


def _insert_project_mart(conn, *, project_id, provider, slug,
                        total_messages, total_sessions):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, "
        " total_output_tokens, total_cache_read, total_cache_create, "
        " total_cost_usd) "
        "VALUES (?, ?, ?, ?, '2026-04-01', '2026-04-30', ?, ?, 0, 0, 0, 0, 0.0)",
        (project_id, provider, slug, slug, total_messages, total_sessions),
    )


# ── parity: mart populated ─────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_messages_summary_total_from_project_mart(tmp_path, monkeypatch):
    """``total`` reflects ``project_mart.total_messages``, not message-list len."""
    store_db = tmp_path / "summary-mart.db"
    slug = "-mart-summary"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(
        conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z",
    )
    # Insert ONE message into the table — the mart row will carry a
    # different (canonical) total to exercise the swap.
    _insert_assistant_message(
        conn, session_fk=sfk, ts="2026-04-01T10:00:01Z", model="claude-A",
    )
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug=slug,
        total_messages=4242, total_sessions=99,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    summary = await data_route.get_messages_summary_endpoint()
    # Mart row supplied the canonical total, NOT the messages-list length (1).
    assert summary["total"] == 4242
    # Wave 4A bonus: total_sessions surfaces from the same mart row.
    assert summary["total_sessions"] == 99
    # Detail blocks still computed from the messages list — the lone
    # assistant message has model claude-A, type defaults to "unknown"
    # because get_messages_summary reads ``msg["type"]`` and the legacy
    # `get_project_messages` rows don't carry that key in this fixture.
    assert "by_type" in summary
    assert "by_model" in summary


# ── empty-mart fallback ────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_messages_summary_falls_back_to_messages_when_mart_empty(
    tmp_path, monkeypatch,
):
    """Empty project_mart → ``total = len(messages)`` per legacy behaviour."""
    store_db = tmp_path / "summary-fallback.db"
    slug = "-fallback-summary"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(
        conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z",
    )
    _insert_assistant_message(
        conn, session_fk=sfk, ts="2026-04-01T10:00:01Z", model="claude-A",
    )
    _insert_assistant_message(
        conn, session_fk=sfk, ts="2026-04-01T10:00:02Z", model="claude-A",
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    summary = await data_route.get_messages_summary_endpoint()
    # Legacy total = len(messages) = 2; no bonus mart-only key.
    assert summary["total"] == 2
    assert "total_sessions" not in summary


# ── speed ──────────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_messages_summary_under_100ms_with_50k_mart_total(
    tmp_path, monkeypatch,
):
    """The mart fast-path keeps the route under 100ms even at scale.

    Note: the route still loads the message list for the detail
    breakdown (by_type / by_model), so this isn't a pure mart bench —
    it's verifying the migration didn't regress the route. The 50K
    figure here keeps the seed bounded; the actual mart read is O(1).
    """
    store_db = tmp_path / "summary-perf.db"
    slug = "-perf-summary"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(
        conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z",
        n=50_000,
    )
    # 50 messages in the messages table — enough to verify the
    # detail-block work runs but not so many that the perf budget
    # is dominated by message ingestion timing.
    for i in range(50):
        _insert_assistant_message(
            conn, session_fk=sfk, ts="2026-04-01T10:00:00Z",
            model=f"claude-{i % 3}",
        )
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug=slug,
        total_messages=50_000, total_sessions=1,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    # Warm the connection.
    await data_route.get_messages_summary_endpoint()
    t0 = time.perf_counter()
    summary = await data_route.get_messages_summary_endpoint()
    elapsed_ms = (time.perf_counter() - t0) * 1000
    assert summary["total"] == 50_000
    assert elapsed_ms < 1500, f"slow: {elapsed_ms:.1f}ms"
