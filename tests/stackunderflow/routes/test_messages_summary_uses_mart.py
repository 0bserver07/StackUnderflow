"""``/api/messages/summary`` — mart-backed body, and its internal consistency.

The route used to serve ``total`` from ``project_mart.total_messages`` while
building ``by_type`` from a full ``get_project_messages`` pipeline pass. Those
two disagree: ``total_messages`` counts BILLABLE EVENTS, ``by_type`` counts
records — so ``sum(by_type) != total`` on the great majority of real projects
(median 58 apart, up to 5,846). The old fixture here only inserted assistant
messages, which made it blind to that.

Contract now:

* Every project row for the slug materialised in ``project_mart`` → the whole
  body comes from the store: ``total`` = summed ``total_records``, ``by_type``
  = the ``{user, assistant}`` pair from the summed per-type columns (they
  partition the record set, so ``sum(by_type) == total``), ``by_model`` /
  ``total_tokens`` from one scoped ``GROUP BY``, and ``total_sessions`` as a
  bonus. No pipeline pass at all.
* Multi-provider slugs take the same path — every column read is additive.
* Any provider row missing from ``project_mart`` → legacy
  ``get_project_messages`` fallback, ``total_sessions`` absent.
"""

from __future__ import annotations

import time

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


def _insert_message(conn, *, session_fk, ts, role, model=None, tokens=10):
    """Insert one message; per-session ``seq`` autoincrements so the
    ``UNIQUE(session_fk, seq)`` index is honoured. ``model=None`` mirrors a
    real user row (the column is only populated for assistant turns)."""
    seq = _seq_state.get(session_fk, 0)
    _seq_state[session_fk] = seq + 1
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, 0, 0, 0, '', '[]', '{}', 0, NULL, NULL)",
        (session_fk, seq, ts, role, model, tokens),
    )


def _insert_assistant_message(conn, *, session_fk, ts, model, tokens=10):
    _insert_message(
        conn, session_fk=session_fk, ts=ts, role="assistant", model=model, tokens=tokens,
    )


def _insert_project_mart(
    conn,
    *,
    project_id,
    provider,
    slug,
    total_messages,
    total_sessions,
    total_records=0,
    total_user_messages=0,
    total_assistant_messages=0,
):
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, "
        " total_output_tokens, total_cache_read, total_cache_create, "
        " total_cost_usd, total_records, total_user_messages, "
        " total_assistant_messages) "
        "VALUES (?, ?, ?, ?, '2026-04-01', '2026-04-30', ?, ?, 0, 0, 0, 0, 0.0, ?, ?, ?)",
        (
            project_id, provider, slug, slug, total_messages, total_sessions,
            total_records, total_user_messages, total_assistant_messages,
        ),
    )


# ── the total must agree with its own by_type ──────────────────────────────


def test_messages_summary_total_matches_by_type(tmp_path, monkeypatch):
    """``total`` is the record count and ``by_type`` partitions it.

    The mart row deliberately carries a ``total_messages`` (billable events)
    that differs from ``total_records`` — the route must ignore the former.
    """
    store_db = tmp_path / "summary-mart.db"
    slug = "-mart-summary"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z")
    _insert_message(conn, session_fk=sfk, ts="2026-04-01T10:00:01Z", role="user")
    _insert_message(conn, session_fk=sfk, ts="2026-04-01T10:00:02Z", role="user")
    _insert_message(
        conn, session_fk=sfk, ts="2026-04-01T10:00:03Z", role="assistant", model="claude-A",
    )
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug=slug,
        # Billable-event count — the value the route used to return as `total`.
        total_messages=4242, total_sessions=99,
        total_records=3, total_user_messages=2, total_assistant_messages=1,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    summary = data_route.get_messages_summary_endpoint()

    assert summary["total"] == 3, "total must be the record count, not total_messages"
    assert summary["total"] != 4242
    assert summary["by_type"] == {"user": 2, "assistant": 1}
    assert sum(summary["by_type"].values()) == summary["total"]
    # tool_use / tool_result are overlapping flags in the legacy classifier,
    # not a partition — surfacing them would break the invariant above.
    assert set(summary["by_type"]) == {"user", "assistant"}
    assert summary["total_sessions"] == 99
    # by_model comes from a scoped GROUP BY over `messages`. Rows without a
    # model key as "N/A" — the `Record.model` default the stats enricher
    # stamps, and therefore the key the legacy pass produced for them.
    assert summary["by_model"] == {"N/A": 2, "claude-A": 1}
    assert summary["total_tokens"] == 30  # 3 rows × 10 input tokens


def test_messages_summary_skips_the_pipeline_when_mart_is_complete(tmp_path, monkeypatch):
    """The mart path must not call ``get_project_messages`` at all.

    That call runs the whole pipeline — including an ``aggregator.summarise``
    whose result this route throws away (~5s / ~800MB on a 50K-msg project).
    """
    store_db = tmp_path / "summary-nopipeline.db"
    slug = "-nopipeline"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z")
    _insert_message(conn, session_fk=sfk, ts="2026-04-01T10:00:01Z", role="user")
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug=slug,
        total_messages=1, total_sessions=1,
        total_records=1, total_user_messages=1,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    called = {"n": 0}

    def boom(conn, *, project_id, limit=None):  # noqa: ARG001
        called["n"] += 1
        return []

    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_messages", boom,
    )
    summary = data_route.get_messages_summary_endpoint()
    assert called["n"] == 0, "mart path still ran the legacy pipeline pass"
    assert summary["total"] == 1


# ── multi-provider: the fast path covers it too ────────────────────────────


def test_messages_summary_merges_multi_provider_slug(tmp_path, monkeypatch):
    """One slug, two providers → summed mart columns, still self-consistent.

    The old ``len(project_ids) == 1`` guard sent every multi-provider slug
    down the full pipeline; the dims are additive, so they merge by summing.
    """
    store_db = tmp_path / "summary-multi.db"
    slug = "-multi-summary"
    conn = _connect(store_db)
    pid_claude = _insert_project(conn, provider="claude", slug=slug)
    pid_codex = _insert_project(conn, provider="codex", slug=slug)
    sfk_c = _insert_session(conn, project_id=pid_claude, session_id="c1", ts="2026-04-01T10:00:00Z")
    sfk_x = _insert_session(conn, project_id=pid_codex, session_id="x1", ts="2026-04-02T10:00:00Z")
    _insert_message(conn, session_fk=sfk_c, ts="2026-04-01T10:00:01Z", role="user")
    _insert_assistant_message(
        conn, session_fk=sfk_c, ts="2026-04-01T10:00:02Z", model="claude-A",
    )
    _insert_message(conn, session_fk=sfk_x, ts="2026-04-02T10:00:01Z", role="user")
    _insert_assistant_message(
        conn, session_fk=sfk_x, ts="2026-04-02T10:00:02Z", model="gpt-5",
    )
    _insert_assistant_message(
        conn, session_fk=sfk_x, ts="2026-04-02T10:00:03Z", model="gpt-5",
    )
    _insert_project_mart(
        conn, project_id=pid_claude, provider="claude", slug=slug,
        total_messages=11, total_sessions=1,
        total_records=2, total_user_messages=1, total_assistant_messages=1,
    )
    _insert_project_mart(
        conn, project_id=pid_codex, provider="codex", slug=slug,
        total_messages=22, total_sessions=1,
        total_records=3, total_user_messages=1, total_assistant_messages=2,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    called = {"n": 0}
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_messages",
        lambda conn, *, project_id, limit=None: called.__setitem__("n", called["n"] + 1) or [],
    )
    summary = data_route.get_messages_summary_endpoint()

    assert called["n"] == 0, "multi-provider slug fell through to the pipeline"
    assert summary["total"] == 5
    assert summary["by_type"] == {"user": 2, "assistant": 3}
    assert sum(summary["by_type"].values()) == summary["total"]
    assert summary["total_sessions"] == 2
    assert summary["by_model"] == {"N/A": 2, "claude-A": 1, "gpt-5": 2}
    assert summary["total_tokens"] == 50


def test_messages_summary_falls_back_when_one_provider_lacks_a_mart_row(tmp_path, monkeypatch):
    """A partially-materialised slug must not serve an undercounted merge."""
    store_db = tmp_path / "summary-partial.db"
    slug = "-partial-summary"
    conn = _connect(store_db)
    pid_claude = _insert_project(conn, provider="claude", slug=slug)
    _insert_project(conn, provider="codex", slug=slug)
    sfk = _insert_session(conn, project_id=pid_claude, session_id="c1", ts="2026-04-01T10:00:00Z")
    _insert_assistant_message(
        conn, session_fk=sfk, ts="2026-04-01T10:00:01Z", model="claude-A",
    )
    # Only the claude row is materialised.
    _insert_project_mart(
        conn, project_id=pid_claude, provider="claude", slug=slug,
        total_messages=1, total_sessions=1, total_records=1,
        total_assistant_messages=1,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    called = {"n": 0}
    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_messages",
        lambda conn, *, project_id, limit=None: called.__setitem__("n", called["n"] + 1) or [],
    )
    summary = data_route.get_messages_summary_endpoint()
    assert called["n"] == 1, "incomplete mart coverage must use the legacy pass"
    assert "total_sessions" not in summary


# ── empty-mart fallback ────────────────────────────────────────────────────


def test_messages_summary_falls_back_to_messages_when_mart_empty(tmp_path, monkeypatch):
    """Empty project_mart → ``total = len(messages)`` per legacy behaviour."""
    store_db = tmp_path / "summary-fallback.db"
    slug = "-fallback-summary"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z")
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
    summary = data_route.get_messages_summary_endpoint()
    # Legacy total = len(messages) = 2; no bonus mart-only key.
    assert summary["total"] == 2
    assert "total_sessions" not in summary


def test_messages_summary_empty_project_keeps_its_shape(tmp_path, monkeypatch):
    """A materialised project with nothing in it keeps the empty envelope."""
    store_db = tmp_path / "summary-empty.db"
    slug = "-empty-summary"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug=slug,
        total_messages=0, total_sessions=0,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    summary = data_route.get_messages_summary_endpoint()
    assert summary["total"] == 0
    assert summary["by_type"] == {}
    assert summary["by_model"] == {}
    assert summary["total_tokens"] == 0


# ── speed ──────────────────────────────────────────────────────────────────


def test_messages_summary_under_budget_with_50k_mart_total(tmp_path, monkeypatch):
    """The mart fast-path keeps the route quick at scale.

    Nothing here loads the message list any more — the body is a handful of
    mart reads plus one indexed ``GROUP BY``.
    """
    store_db = tmp_path / "summary-perf.db"
    slug = "-perf-summary"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)
    sfk = _insert_session(
        conn, project_id=pid, session_id="s1", ts="2026-04-01T10:00:00Z", n=50_000,
    )
    for i in range(50):
        _insert_assistant_message(
            conn, session_fk=sfk, ts="2026-04-01T10:00:00Z", model=f"claude-{i % 3}",
        )
    _insert_project_mart(
        conn, project_id=pid, provider="claude", slug=slug,
        total_messages=50_000, total_sessions=1,
        total_records=50_000, total_user_messages=20_000,
        total_assistant_messages=30_000,
    )
    conn.commit()
    conn.close()

    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    # Warm the connection.
    data_route.get_messages_summary_endpoint()
    t0 = time.perf_counter()
    summary = data_route.get_messages_summary_endpoint()
    elapsed_ms = (time.perf_counter() - t0) * 1000
    assert summary["total"] == 50_000
    assert sum(summary["by_type"].values()) == 50_000
    assert elapsed_ms < 1500, f"slow: {elapsed_ms:.1f}ms"
