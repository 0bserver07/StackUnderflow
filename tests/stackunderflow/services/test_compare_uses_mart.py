"""Wave 4A — ``services.compare.compare_models`` reads from marts.

Parity coverage:

* Synthetic ``model_day_mart`` + ``session_mart`` fixture seeded so the
  shape matches what the aggregator path would have produced for a
  hand-crafted three-session scenario; the mart-fed ``compare_models``
  call returns the same per-model metric values.
* Empty marts → fallback to the aggregator (raw messages) path
  preserves the response.
* Speed test: 50K synthetic mart rows answer in <100ms.
"""

from __future__ import annotations

import time

import pytest

from stackunderflow.services.compare import compare_models
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


def _insert_session_mart(conn, **kw):
    conn.execute(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, "
        " first_ts, last_ts, message_count, user_message_count, "
        " assistant_message_count, input_tokens, output_tokens, "
        " cache_read, cache_create, cost_usd, is_one_shot, cwd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            kw["session_id"], kw["project_id"], kw.get("provider", "claude"),
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


def _insert_model_day(conn, **kw):
    conn.execute(
        "INSERT INTO model_day_mart "
        "(day, model, speed, cost_usd, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            kw["day"], kw["model"], kw.get("speed", "standard"),
            kw.get("cost_usd", 0.0),
            kw.get("input_tokens", 0),
            kw.get("output_tokens", 0),
            kw.get("cache_read", 0),
            kw.get("cache_create", 0),
            kw.get("message_count", 0),
            kw.get("session_count", 0),
        ),
    )


# ── parity: marts present ──────────────────────────────────────────────────


def test_compare_reads_per_model_totals_from_model_day_mart(tmp_path):
    """Per-model calls / cost / tokens come from ``model_day_mart``."""
    store_db = tmp_path / "compare-mart.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="alpha")

    # Two sessions with the same primary model — claude-A.
    _insert_session_mart(conn,
        session_id="s1", project_id=pid, provider="claude",
        primary_model="claude-A",
        first_ts="2026-04-01T10:00:00Z", last_ts="2026-04-01T10:01:00Z",
        message_count=2, user_message_count=1, assistant_message_count=1,
        input_tokens=100, output_tokens=50, cost_usd=0.20, is_one_shot=1,
    )
    _insert_session_mart(conn,
        session_id="s2", project_id=pid, provider="claude",
        primary_model="claude-A",
        first_ts="2026-04-02T10:00:00Z", last_ts="2026-04-02T10:00:30Z",
        message_count=4, user_message_count=1, assistant_message_count=3,
        input_tokens=300, output_tokens=150, cost_usd=0.60, is_one_shot=0,
    )

    # model_day_mart: per-day rollup the compare totals are summed from.
    _insert_model_day(conn, day="2026-04-01", model="claude-A",
        cost_usd=0.20, input_tokens=100, output_tokens=50, message_count=1,
        session_count=1)
    _insert_model_day(conn, day="2026-04-02", model="claude-A",
        cost_usd=0.60, input_tokens=300, output_tokens=150, message_count=3,
        session_count=1)
    conn.commit()

    results = {r.model: r for r in compare_models(conn, period="all")}
    conn.close()

    a = results["claude-A"]
    # calls = 1 + 3 = 4 (sum of message_count across days)
    assert a.calls == 4
    assert a.total_cost == pytest.approx(0.80)
    # tokens = (100 + 300) input + (50 + 150) output = 600
    assert a.total_tokens == 600
    # 2 sessions seeded for claude-A
    assert a.sessions == 2
    # one_shot 1 of 2 → 0.5
    assert a.one_shot_pct == pytest.approx(0.5)
    # retry rate: assistant_msgs (1+3) / sessions (2) - 1 = 1.0
    assert a.retry_rate == pytest.approx(1.0)
    assert a.provider == "claude"


def test_compare_provider_attribution_from_session_mart(tmp_path):
    """``provider`` per-row is sourced from ``session_mart``."""
    store_db = tmp_path / "compare-provider.db"
    conn = _connect(store_db)
    pid_codex = _insert_project(conn, provider="codex", slug="gamma")
    _insert_session_mart(conn,
        session_id="c1", project_id=pid_codex, provider="codex",
        primary_model="gpt-X",
        first_ts="2026-04-04T10:00:00Z", last_ts="2026-04-04T10:00:01Z",
        message_count=2, user_message_count=1, assistant_message_count=1,
        input_tokens=50, output_tokens=25, cost_usd=0.05, is_one_shot=1,
    )
    _insert_model_day(conn, day="2026-04-04", model="gpt-X",
        cost_usd=0.05, input_tokens=50, output_tokens=25, message_count=1,
        session_count=1)
    conn.commit()

    results = {r.model: r for r in compare_models(conn, period="all")}
    conn.close()
    assert results["gpt-X"].provider == "codex"


def test_compare_provider_filter_uses_session_mart(tmp_path):
    """``provider_filter='claude'`` excludes codex sessions/models."""
    store_db = tmp_path / "compare-filter.db"
    conn = _connect(store_db)
    pid_a = _insert_project(conn, provider="claude", slug="alpha")
    pid_g = _insert_project(conn, provider="codex", slug="gamma")

    _insert_session_mart(conn,
        session_id="s1", project_id=pid_a, provider="claude",
        primary_model="claude-A",
        first_ts="2026-04-01T10:00:00Z", last_ts="2026-04-01T10:01:00Z",
        assistant_message_count=1, input_tokens=100, output_tokens=50,
        cost_usd=0.10, is_one_shot=1,
    )
    _insert_session_mart(conn,
        session_id="g1", project_id=pid_g, provider="codex",
        primary_model="gpt-X",
        first_ts="2026-04-02T10:00:00Z", last_ts="2026-04-02T10:00:01Z",
        assistant_message_count=1, input_tokens=50, output_tokens=25,
        cost_usd=0.05, is_one_shot=1,
    )

    _insert_model_day(conn, day="2026-04-01", model="claude-A",
        cost_usd=0.10, input_tokens=100, output_tokens=50, message_count=1,
        session_count=1)
    _insert_model_day(conn, day="2026-04-02", model="gpt-X",
        cost_usd=0.05, input_tokens=50, output_tokens=25, message_count=1,
        session_count=1)
    conn.commit()

    results = compare_models(conn, period="all", provider_filter="claude")
    conn.close()
    models = {r.model for r in results}
    assert models == {"claude-A"}


# ── empty-mart fallback ────────────────────────────────────────────────────


def test_compare_falls_back_to_aggregator_when_marts_empty(tmp_path):
    """Empty session_mart / model_day_mart → legacy messages path runs."""
    store_db = tmp_path / "compare-fallback.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="alpha")
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        "message_count) VALUES (?, ?, ?, ?, ?)",
        (pid, "S1", "2026-04-01T10:00:00Z", "2026-04-01T10:01:00Z", 2),
    )
    sfk = int(cur.lastrowid)
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (sfk, 0, "2026-04-01T10:00:00Z", "user", None,
         0, 0, 0, 0, "", "[]", "{}", 0, None, None),
    )
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (sfk, 1, "2026-04-01T10:00:01Z", "assistant", "claude-A",
         100, 50, 0, 0, "", "[]", "{}", 0, None, None),
    )
    conn.commit()

    # Marts are empty — the route must answer from the aggregator path.
    results = {r.model: r for r in compare_models(conn, period="all")}
    conn.close()
    assert "claude-A" in results
    assert results["claude-A"].calls == 1


# ── speed ──────────────────────────────────────────────────────────────────


def test_compare_under_100ms_with_50k_mart_rows(tmp_path):
    """Mart-fed compare answers in <100ms with 50K mart rows seeded."""
    store_db = tmp_path / "compare-perf.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, provider="claude", slug="perf")

    # 100 sessions for primary_model claude-A (enough to exercise grouping).
    sess_rows = [
        ("sess-" + str(i), pid, "claude", "claude-A",
         "2026-04-01T10:00:00Z", "2026-04-01T10:01:00Z",
         2, 1, 1, 100, 50, 0, 0, 0.10, 1, None)
        for i in range(100)
    ]
    conn.executemany(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
        " message_count, user_message_count, assistant_message_count, "
        " input_tokens, output_tokens, cache_read, cache_create, cost_usd, "
        " is_one_shot, cwd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        sess_rows,
    )

    # 50K (day, model, speed) rollup rows — emulate a deep history.
    rows = []
    for d in range(500):
        for m in range(100):
            day_str = f"2024-{((d // 30) % 12) + 1:02d}-{(d % 28) + 1:02d}"
            rows.append((day_str, f"model-{m}", "standard",
                0.001, 10, 5, 0, 0, 1, 1))
    conn.executemany(
        "INSERT OR IGNORE INTO model_day_mart "
        "(day, model, speed, cost_usd, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rows,
    )
    conn.commit()

    # Warm the connection cache (first call materialises sqlite plans).
    compare_models(conn, period="all")
    t0 = time.perf_counter()
    results = compare_models(conn, period="all")
    elapsed_ms = (time.perf_counter() - t0) * 1000
    conn.close()
    assert results, "expected mart-fed compare to return rows"
    assert elapsed_ms < 100, f"slow: {elapsed_ms:.1f}ms"
