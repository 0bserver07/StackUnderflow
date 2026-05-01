"""Tests for ``stackunderflow.services.compare``.

The fixture builds a tiny three-model store so we can verify every
metric by hand. Sessions are deliberately shaped to exercise:

* a clean one-shot (model A: 1 user / 1 assistant)
* a retry session (model A: 1 user / 3 assistant — 2 retries)
* a multi-user session (model B)
* a different-provider session (model C)
"""

from __future__ import annotations

import pytest

from stackunderflow.services.compare import (
    ModelStats,
    build_compare_payload,
    compare_models,
)
from stackunderflow.store import db, schema

# ── store seeding helpers ────────────────────────────────────────────────────


def _seed(store_db, *, projects, messages):
    conn = db.connect(store_db)
    schema.apply(conn)

    project_pk: dict[tuple[str, str], int] = {}
    for prov, slug in projects:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            (prov, slug, slug, 0.0, 0.0),
        )
        project_pk[(prov, slug)] = cur.lastrowid

    sess_pk: dict[tuple[int, str], int] = {}
    seq_counter: dict[int, int] = {}
    for m in messages:
        prov = m.get("provider", "claude")
        slug = m["project_slug"]
        ppk = project_pk[(prov, slug)]
        sk = (ppk, m["session_id"])
        if sk not in sess_pk:
            cur = conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, ?, ?, ?)",
                (ppk, m["session_id"], m["timestamp"], m["timestamp"], 0),
            )
            sess_pk[sk] = cur.lastrowid
        sfk = sess_pk[sk]
        seq = seq_counter.get(sfk, 0)
        seq_counter[sfk] = seq + 1
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                sfk, seq, m["timestamp"], m["role"], m.get("model"),
                m.get("in_tok", 0), m.get("out_tok", 0),
                m.get("cache_w", 0), m.get("cache_r", 0),
                "", "[]", "{}", 0, None, None,
            ),
        )
    conn.commit()
    conn.close()


def _all_period(monkeypatch=None):
    """Helper — period 'all' has no time filter so seeded data always counts."""
    return "all"


# ── fixture ──────────────────────────────────────────────────────────────────


@pytest.fixture
def store_db(tmp_path):
    """Three-model fixture covering happy/retry/cross-provider cases."""
    db_path = tmp_path / "compare.db"
    _seed(
        db_path,
        projects=[
            ("claude", "alpha"),
            ("claude", "beta"),
            ("codex", "gamma"),
        ],
        messages=[
            # Session A1 — model claude-A, clean one-shot (1u + 1a)
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:01Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50,
             "cache_r": 200, "cache_w": 100},
            # Session A2 — model claude-A, retried (1u + 3a)
            {"project_slug": "alpha", "session_id": "A2",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A2",
             "timestamp": "2026-04-02T10:00:01Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50,
             "cache_r": 0, "cache_w": 0},
            {"project_slug": "alpha", "session_id": "A2",
             "timestamp": "2026-04-02T10:00:02Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50,
             "cache_r": 0, "cache_w": 0},
            {"project_slug": "alpha", "session_id": "A2",
             "timestamp": "2026-04-02T10:00:03Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50,
             "cache_r": 0, "cache_w": 0},
            # Session B1 — model claude-B, 2u + 2a (not one-shot)
            {"project_slug": "beta", "session_id": "B1",
             "timestamp": "2026-04-03T10:00:00Z", "role": "user"},
            {"project_slug": "beta", "session_id": "B1",
             "timestamp": "2026-04-03T10:00:01Z", "role": "assistant",
             "model": "claude-B", "in_tok": 200, "out_tok": 100,
             "cache_r": 50, "cache_w": 50},
            {"project_slug": "beta", "session_id": "B1",
             "timestamp": "2026-04-03T10:00:02Z", "role": "user"},
            {"project_slug": "beta", "session_id": "B1",
             "timestamp": "2026-04-03T10:00:03Z", "role": "assistant",
             "model": "claude-B", "in_tok": 200, "out_tok": 100,
             "cache_r": 50, "cache_w": 50},
            # Session C1 — codex / gpt-X, clean one-shot
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-04T10:00:00Z", "role": "user"},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-04T10:00:01Z", "role": "assistant",
             "model": "gpt-X", "in_tok": 50, "out_tok": 25,
             "cache_r": 0, "cache_w": 0},
        ],
    )
    return db_path


# ── core math ────────────────────────────────────────────────────────────────


def test_three_models_present(store_db):
    conn = db.connect(store_db)
    try:
        results = compare_models(conn, period="all")
    finally:
        conn.close()
    models = {r.model for r in results}
    assert models == {"claude-A", "claude-B", "gpt-X"}


def test_session_and_call_counts(store_db):
    conn = db.connect(store_db)
    try:
        results = {r.model: r for r in compare_models(conn, period="all")}
    finally:
        conn.close()

    a = results["claude-A"]
    assert a.sessions == 2          # A1 + A2
    assert a.calls == 4             # 1 + 3 assistant messages

    b = results["claude-B"]
    assert b.sessions == 1          # B1
    assert b.calls == 2

    c = results["gpt-X"]
    assert c.sessions == 1          # C1
    assert c.calls == 1


def test_one_shot_pct(store_db):
    """A: 1 of 2 sessions is one-shot. B: 0 of 1. C: 1 of 1."""
    conn = db.connect(store_db)
    try:
        results = {r.model: r for r in compare_models(conn, period="all")}
    finally:
        conn.close()

    assert results["claude-A"].one_shot_pct == pytest.approx(0.5)
    assert results["claude-B"].one_shot_pct == pytest.approx(0.0)
    assert results["gpt-X"].one_shot_pct == pytest.approx(1.0)


def test_retry_rate(store_db):
    """A: avg 4/2 - 1 = 1.0 retries. B: 2/1 - 1 = 1.0. C: 1/1 - 1 = 0.0."""
    conn = db.connect(store_db)
    try:
        results = {r.model: r for r in compare_models(conn, period="all")}
    finally:
        conn.close()

    assert results["claude-A"].retry_rate == pytest.approx(1.0)
    assert results["claude-B"].retry_rate == pytest.approx(1.0)
    assert results["gpt-X"].retry_rate == pytest.approx(0.0)


def test_cache_hit_rate(store_db):
    """claude-A: read=200 / (200+100) = 0.667 across all assistant rows."""
    conn = db.connect(store_db)
    try:
        results = {r.model: r for r in compare_models(conn, period="all")}
    finally:
        conn.close()

    a = results["claude-A"]
    # cache_r totals: 200; cache_w totals: 100 → 200/300
    assert a.cache_hit_rate == pytest.approx(200.0 / 300.0)

    b = results["claude-B"]
    # cache_r totals: 100; cache_w totals: 100 → 100/200 = 0.5
    assert b.cache_hit_rate == pytest.approx(0.5)

    c = results["gpt-X"]
    # No cache tokens → 0.0 (no division by zero)
    assert c.cache_hit_rate == 0.0


def test_total_cost_sorted_desc(store_db):
    conn = db.connect(store_db)
    try:
        results = compare_models(conn, period="all")
    finally:
        conn.close()
    costs = [r.total_cost for r in results]
    assert costs == sorted(costs, reverse=True)


def test_cost_per_call_and_session(store_db):
    """Self-consistency: total_cost / calls == cost_per_call (and same for sessions)."""
    conn = db.connect(store_db)
    try:
        results = compare_models(conn, period="all")
    finally:
        conn.close()
    for r in results:
        assert r.cost_per_call == pytest.approx(r.total_cost / r.calls)
        assert r.cost_per_session == pytest.approx(r.total_cost / r.sessions)


def test_total_tokens_sums_all_four_kinds(store_db):
    conn = db.connect(store_db)
    try:
        results = {r.model: r for r in compare_models(conn, period="all")}
    finally:
        conn.close()
    # claude-A: input=400, output=200, cache_r=200, cache_w=100 → 900
    assert results["claude-A"].total_tokens == 900


def test_provider_carried_through(store_db):
    conn = db.connect(store_db)
    try:
        results = {r.model: r for r in compare_models(conn, period="all")}
    finally:
        conn.close()
    assert results["claude-A"].provider == "claude"
    assert results["gpt-X"].provider == "codex"


# ── filters ──────────────────────────────────────────────────────────────────


def test_provider_filter_excludes_other_providers(store_db):
    conn = db.connect(store_db)
    try:
        results = compare_models(conn, period="all", provider_filter="claude")
    finally:
        conn.close()
    models = {r.model for r in results}
    assert models == {"claude-A", "claude-B"}


def test_project_filter_includes_only_listed_slugs(store_db):
    conn = db.connect(store_db)
    try:
        results = compare_models(conn, period="all", project_filter=["alpha"])
    finally:
        conn.close()
    models = {r.model for r in results}
    assert models == {"claude-A"}


def test_project_filter_with_multiple_slugs(store_db):
    conn = db.connect(store_db)
    try:
        results = compare_models(
            conn, period="all", project_filter=["alpha", "gamma"]
        )
    finally:
        conn.close()
    models = {r.model for r in results}
    assert models == {"claude-A", "gpt-X"}


# ── period scoping ───────────────────────────────────────────────────────────


def test_today_period_returns_empty_for_old_data(store_db):
    """The seeded data is from April 2026 — 'today' window won't match
    unless the test is run on the same day. The empty path is the
    normal case here."""
    conn = db.connect(store_db)
    try:
        results = compare_models(conn, period="today")
    finally:
        conn.close()
    # Either zero rows (most days) or some rows (extremely rare overlap) —
    # we just assert the call doesn't raise and returns a list.
    assert isinstance(results, list)


def test_unknown_period_raises():
    import sqlite3
    conn = sqlite3.connect(":memory:")
    try:
        with pytest.raises(ValueError):
            compare_models(conn, period="yesterday")
    finally:
        conn.close()


# ── empty store ──────────────────────────────────────────────────────────────


def test_empty_store_returns_empty_list(tmp_path):
    db_path = tmp_path / "empty.db"
    conn = db.connect(db_path)
    schema.apply(conn)
    try:
        results = compare_models(conn, period="all")
    finally:
        conn.close()
    assert results == []


# ── payload helper ───────────────────────────────────────────────────────────


def test_build_compare_payload_shape(store_db):
    conn = db.connect(store_db)
    try:
        payload = build_compare_payload(conn, period="all")
    finally:
        conn.close()
    assert payload["period"] == "all"
    assert isinstance(payload["models"], list)
    assert isinstance(payload["generated"], float)
    assert payload["models"], "expected at least one model row"
    # Each row is a dict (asdict) with the expected keys.
    expected_keys = {f.name for f in ModelStats.__dataclass_fields__.values()}
    assert set(payload["models"][0].keys()) == expected_keys
