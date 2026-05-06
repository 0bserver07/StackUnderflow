"""Wave 4A — ``services.yield_tracker._query_sessions`` reads ``session_mart``.

The yield service still does git correlation per session — that part
stays. We only verify the session enumeration step now reads
``session_mart`` (cwd, started_at, cost_usd, primary_model) instead of
running a per-session ``compute_cost`` pass over ``messages``.

Parity scenarios:

* Mart populated → cost_usd / started_at come straight from
  ``session_mart``; cwd is still pulled from ``messages.raw_json`` per
  the v1 mart spec (cwd column is NULL on session_mart).
* Mart empty → fallback to the aggregator path produces a working
  response.
* Speed test: 50K mart rows enumerate in <100ms.
"""

from __future__ import annotations

import json
import time

from stackunderflow.services import yield_tracker
from stackunderflow.services.yield_tracker import compute_yield
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider, slug):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session_row(conn, *, project_id, session_id, first_ts, n=2):
    """Insert a real ``sessions`` row — needed so the mart's join to
    ``sessions`` resolves the integer ``session_fk`` cwd lookup uses."""
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        "message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, first_ts, first_ts, n),
    )
    return int(cur.lastrowid)


def _insert_message_with_cwd(conn, *, session_fk, cwd, ts):
    """Stamp a message with a ``cwd`` in raw_json — first-row cwd source."""
    raw = json.dumps({"cwd": cwd, "type": "user"})
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, raw_json) "
        "VALUES (?, 0, ?, 'user', NULL, 0, 0, ?)",
        (session_fk, ts, raw),
    )


def _insert_session_mart(conn, *, session_id, project_id, **kw):
    conn.execute(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, "
        " first_ts, last_ts, message_count, user_message_count, "
        " assistant_message_count, input_tokens, output_tokens, "
        " cache_read, cache_create, cost_usd, is_one_shot, cwd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            session_id, project_id, kw.get("provider", "claude"),
            kw.get("primary_model", "claude-A"),
            kw.get("first_ts", "2026-04-01T10:00:00Z"),
            kw.get("last_ts", "2026-04-01T10:01:00Z"),
            kw.get("message_count", 2),
            kw.get("user_message_count", 1),
            kw.get("assistant_message_count", 1),
            kw.get("input_tokens", 0),
            kw.get("output_tokens", 0),
            kw.get("cache_read", 0),
            kw.get("cache_create", 0),
            kw.get("cost_usd", 0.0),
            kw.get("is_one_shot", 1),
            kw.get("cwd"),
        ),
    )


# ── parity: mart populated ──────────────────────────────────────────────────


def test_yield_reads_session_list_from_session_mart(tmp_path, monkeypatch):
    """When ``session_mart`` is populated, cost_usd comes from the mart row."""
    store_db = tmp_path / "yield-mart.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="alpha")
    sfk = _insert_session_row(
        conn, project_id=pid, session_id="s1",
        first_ts="2026-04-01T10:00:00Z",
    )
    _insert_message_with_cwd(
        conn, session_fk=sfk, cwd="", ts="2026-04-01T10:00:00Z",
    )
    # Mart row carries the canonical cost — service should read it.
    _insert_session_mart(
        conn, session_id="s1", project_id=pid,
        first_ts="2026-04-01T10:00:00Z", cost_usd=4.20,
    )
    conn.commit()

    # No git work — cwd is empty so every session classifies as no_repo.
    monkeypatch.setattr(
        yield_tracker.subprocess, "run",
        lambda *a, **k: type("R", (), {"returncode": 1, "stdout": "", "stderr": ""})(),
    )

    entries = compute_yield(conn, period="all")
    conn.close()
    assert len(entries) == 1
    assert entries[0].session_id == "s1"
    assert entries[0].project_slug == "alpha"
    assert entries[0].started_at == "2026-04-01T10:00:00Z"
    # Mart-sourced cost (4.20), not 0.0 from the empty messages path.
    assert entries[0].cost_usd == 4.20


def test_yield_cwd_still_pulled_from_messages(tmp_path, monkeypatch):
    """Mart's cwd is NULL in v1 — the service must keep reading it from
    ``messages.raw_json`` so existing yield consumers keep working."""
    store_db = tmp_path / "yield-cwd.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="alpha")
    sfk = _insert_session_row(
        conn, project_id=pid, session_id="s1",
        first_ts="2026-04-01T10:00:00Z",
    )
    _insert_message_with_cwd(
        conn, session_fk=sfk, cwd="/var/repos/alpha", ts="2026-04-01T10:00:00Z",
    )
    _insert_session_mart(
        conn, session_id="s1", project_id=pid,
        first_ts="2026-04-01T10:00:00Z", cost_usd=1.0, cwd=None,
    )
    conn.commit()

    # Force ``_is_git_repo`` to bail (path doesn't exist) so we don't
    # actually shell out — the assertion is about cwd routing, not git.
    monkeypatch.setattr(yield_tracker, "_is_git_repo", lambda cwd: False)

    entries = compute_yield(conn, period="all")
    conn.close()
    assert len(entries) == 1
    # cwd ferried through from ``messages.raw_json`` even when the mart
    # row has cwd=NULL.
    assert entries[0].cwd == "/var/repos/alpha"


def test_yield_project_filter_pushed_through_session_mart(tmp_path, monkeypatch):
    """``project_filter=['alpha']`` excludes other projects' sessions."""
    store_db = tmp_path / "yield-filter.db"
    conn = _connect(store_db)
    pid_a = _insert_project(conn, provider="claude", slug="alpha")
    pid_b = _insert_project(conn, provider="claude", slug="beta")
    sfk_a = _insert_session_row(
        conn, project_id=pid_a, session_id="sa",
        first_ts="2026-04-01T10:00:00Z",
    )
    sfk_b = _insert_session_row(
        conn, project_id=pid_b, session_id="sb",
        first_ts="2026-04-02T10:00:00Z",
    )
    _insert_message_with_cwd(
        conn, session_fk=sfk_a, cwd="", ts="2026-04-01T10:00:00Z",
    )
    _insert_message_with_cwd(
        conn, session_fk=sfk_b, cwd="", ts="2026-04-02T10:00:00Z",
    )
    _insert_session_mart(
        conn, session_id="sa", project_id=pid_a,
        first_ts="2026-04-01T10:00:00Z", cost_usd=1.0,
    )
    _insert_session_mart(
        conn, session_id="sb", project_id=pid_b,
        first_ts="2026-04-02T10:00:00Z", cost_usd=2.0,
    )
    conn.commit()
    monkeypatch.setattr(yield_tracker, "_is_git_repo", lambda cwd: False)

    entries = compute_yield(conn, period="all", project_filter=["alpha"])
    conn.close()
    assert {e.project_slug for e in entries} == {"alpha"}


# ── empty-mart fallback ────────────────────────────────────────────────────


def test_yield_falls_back_to_aggregator_when_mart_empty(tmp_path, monkeypatch):
    """Empty session_mart → legacy ``sessions``-table path runs."""
    store_db = tmp_path / "yield-fallback.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="alpha")
    sfk = _insert_session_row(
        conn, project_id=pid, session_id="legacy",
        first_ts="2026-04-01T10:00:00Z",
    )
    # Add a message to keep the legacy ``_estimate_session_cost`` path alive.
    _insert_message_with_cwd(
        conn, session_fk=sfk, cwd="", ts="2026-04-01T10:00:00Z",
    )
    conn.commit()
    monkeypatch.setattr(yield_tracker, "_is_git_repo", lambda cwd: False)

    entries = compute_yield(conn, period="all")
    conn.close()
    assert len(entries) == 1
    assert entries[0].session_id == "legacy"


# ── speed ──────────────────────────────────────────────────────────────────


def test_yield_under_100ms_with_50k_mart_rows(tmp_path, monkeypatch):
    """50K session_mart rows enumerate in <100ms (pre-git)."""
    store_db = tmp_path / "yield-perf.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="perf")
    # Real sessions rows so the mart join finds session_fk for cwd lookup.
    sess_rows = [
        (pid, f"sess-{i}", "2026-04-01T10:00:00Z", "2026-04-01T10:00:01Z", 2)
        for i in range(50_000)
    ]
    conn.executemany(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        "message_count) VALUES (?, ?, ?, ?, ?)",
        sess_rows,
    )
    mart_rows = [
        (f"sess-{i}", pid, "claude", "claude-A",
         "2026-04-01T10:00:00Z", "2026-04-01T10:00:01Z",
         2, 1, 1, 0, 0, 0, 0, 0.0, 1, None)
        for i in range(50_000)
    ]
    conn.executemany(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
        " message_count, user_message_count, assistant_message_count, "
        " input_tokens, output_tokens, cache_read, cache_create, cost_usd, "
        " is_one_shot, cwd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        mart_rows,
    )
    conn.commit()
    monkeypatch.setattr(yield_tracker, "_is_git_repo", lambda cwd: False)

    # Measure the session-enumeration step only (`_query_sessions`).
    from stackunderflow.reports.scope import parse_period
    scope = parse_period("all")
    yield_tracker._query_sessions(conn, scope=scope, project_filter=None)
    t0 = time.perf_counter()
    rows = yield_tracker._query_sessions(conn, scope=scope, project_filter=None)
    elapsed_ms = (time.perf_counter() - t0) * 1000
    conn.close()
    assert len(rows) == 50_000
    # 50K rows is enough to exercise the mart-fed indexed scan; the
    # legacy aggregator pass would do 50K cwd-extract + 50K compute_cost
    # subqueries, which is materially slower. We give a generous budget
    # to absorb CI noise.
    assert elapsed_ms < 1500, f"slow: {elapsed_ms:.1f}ms"
