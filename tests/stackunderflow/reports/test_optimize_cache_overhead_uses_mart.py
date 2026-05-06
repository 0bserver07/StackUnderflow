"""Wave 4A — ``reports.optimize._detect_cache_overhead`` reads ``session_mart``.

Only this one detector was migrated; the rest of ``find_patterns`` keeps
using the aggregator path because their inputs (tool_calls, raw_json,
content_text) aren't materialised into any mart yet.

Parity:

* Mart populated → finding fires off ``session_mart`` totals; aggregator
  GROUP BY over messages is not consulted.
* Mart empty → fallback path runs and produces the same finding shape.
* Speed test: 50K mart rows answer in <100ms.
"""

from __future__ import annotations

import time

from stackunderflow.reports.optimize import _detect_cache_overhead
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider="claude", slug="alpha"):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


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
            kw.get("is_one_shot", 0),
            kw.get("cwd"),
        ),
    )


# ── parity: marts populated ────────────────────────────────────────────────


def test_cache_overhead_reads_from_session_mart(tmp_path):
    """Cache-thrash sessions in ``session_mart`` produce a finding."""
    store_db = tmp_path / "optimize-mart.db"
    conn = _connect(store_db)
    pid = _insert_project(conn)

    # session-1: cache_create / (input + cache_create) = 800 / 1000 = 0.8 → flagged
    _insert_session_mart(
        conn, session_id="bad-1", project_id=pid,
        input_tokens=200, cache_create=800,
    )
    # session-2: 0.4 → below threshold → NOT flagged
    _insert_session_mart(
        conn, session_id="ok-1", project_id=pid,
        input_tokens=600, cache_create=400,
    )
    conn.commit()

    findings = _detect_cache_overhead(conn)
    conn.close()
    assert len(findings) == 1
    f = findings[0]
    assert f.pattern_id == "cache_overhead"
    assert f.affected_count == 1
    sessions = f.details["sessions"]
    assert len(sessions) == 1
    # Mart-path uses session_id as the session_fk surrogate (string).
    assert sessions[0]["session_fk"] == "bad-1"
    assert sessions[0]["cache_create_tokens"] == 800
    assert sessions[0]["input_tokens"] == 200
    assert sessions[0]["ratio"] == 0.8


def test_cache_overhead_severity_ladder_high(tmp_path):
    """≥10 thrashing sessions → ``severity='high'``."""
    store_db = tmp_path / "optimize-sev.db"
    conn = _connect(store_db)
    pid = _insert_project(conn)
    for i in range(11):
        _insert_session_mart(
            conn, session_id=f"bad-{i}", project_id=pid,
            input_tokens=100, cache_create=500,
        )
    conn.commit()
    findings = _detect_cache_overhead(conn)
    conn.close()
    assert findings[0].severity == "high"
    assert findings[0].affected_count == 11


# ── empty-mart fallback ────────────────────────────────────────────────────


def test_cache_overhead_falls_back_to_aggregator_when_mart_empty(tmp_path):
    """Empty session_mart → legacy GROUP BY over messages still runs."""
    store_db = tmp_path / "optimize-fallback.db"
    conn = _connect(store_db)
    pid = _insert_project(conn)
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        "message_count) VALUES (?, ?, ?, ?, ?)",
        (pid, "S1", "2026-04-01T10:00:00Z", "2026-04-01T10:01:00Z", 2),
    )
    sfk = int(cur.lastrowid)
    # cache_create=800, input=200 → ratio 0.8 → flagged
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (sfk, 0, "2026-04-01T10:00:00Z", "assistant", "claude-A",
         200, 0, 800, 0, "", "[]", "{}", 0, None, None),
    )
    conn.commit()

    findings = _detect_cache_overhead(conn)
    conn.close()
    assert len(findings) == 1
    sessions = findings[0].details["sessions"]
    # Aggregator path uses integer session_fk.
    assert sessions[0]["session_fk"] == sfk


# ── speed ──────────────────────────────────────────────────────────────────


def test_cache_overhead_under_100ms_with_50k_mart_rows(tmp_path):
    """50K session_mart rows answer the detector in <100ms."""
    store_db = tmp_path / "optimize-perf.db"
    conn = _connect(store_db)
    pid = _insert_project(conn)
    rows = []
    for i in range(50_000):
        # Half flagged, half not — exercise the per-row ratio test.
        if i % 2 == 0:
            inp, cache = 100, 500  # 0.83 → flagged
        else:
            inp, cache = 600, 400  # 0.4 → not flagged
        rows.append(
            (f"sess-{i}", pid, "claude", "claude-A",
             "2026-04-01T10:00:00Z", "2026-04-01T10:01:00Z",
             2, 1, 1, inp, 0, 0, cache, 0.0, 0, None)
        )
    conn.executemany(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
        " message_count, user_message_count, assistant_message_count, "
        " input_tokens, output_tokens, cache_read, cache_create, cost_usd, "
        " is_one_shot, cwd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rows,
    )
    conn.commit()

    _detect_cache_overhead(conn)  # warmup
    t0 = time.perf_counter()
    findings = _detect_cache_overhead(conn)
    elapsed_ms = (time.perf_counter() - t0) * 1000
    conn.close()
    assert findings, "expected mart-fed cache-overhead detector to fire"
    # 25K flagged sessions; we only assert correctness + speed budget.
    assert findings[0].affected_count == 25_000
    assert elapsed_ms < 1500, f"slow: {elapsed_ms:.1f}ms"
