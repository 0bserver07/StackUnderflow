"""Shared fixtures for the sync tests.

``make_store`` returns freshly-migrated stores (so a test can build two
independent "devices"); ``seed_marts`` populates the Overview/Cost-core marts
with a small, deterministic dataset. Both are dependency-free — the crypto-path
tests layer ``pyrage`` on top via ``importorskip``.
"""

from __future__ import annotations

import sqlite3

import pytest

from stackunderflow.store import db, schema

_DAILY = (
    # day, project_id, provider, model, speed, in, out, cr, cc, msg, sess, cost
    ("2026-07-01", "opus", "standard", 100, 50, 10, 5, 3, 1, 1.5),
    ("2026-07-02", "opus", "standard", 200, 80, 20, 8, 4, 1, 2.5),
)
_BETA_DAILY = ("2026-06-30", "sonnet", "standard", 40, 20, 4, 2, 2, 1, 0.4)


def _seed(conn: sqlite3.Connection, *, alpha_id: int = 1, beta_id: int = 2,
          session_id: str = "sess-a", scale: int = 1) -> None:
    """Insert a deterministic mart dataset. ``scale`` multiplies token/cost measures.

    ``alpha_id`` / ``beta_id`` are the machine-local ``project_id`` values — vary
    them across "devices" to exercise re-keying; keep the slugs (``alpha`` /
    ``beta``) fixed so the stable ``(provider, slug)`` identity matches.
    """
    conn.execute(
        "INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, 'claude', 'alpha', '/Users/x/alpha', 'Alpha', 0, 0)",
        (alpha_id,),
    )
    conn.execute(
        "INSERT INTO projects (id, provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, 'claude', 'beta', '/Users/x/beta', 'Beta', 0, 0)",
        (beta_id,),
    )

    for day, model, speed, tin, tout, cr, cc, msg, sess, cost in _DAILY:
        conn.execute(
            "INSERT INTO daily_mart (day, project_id, provider, model, speed, "
            "input_tokens, output_tokens, cache_read, cache_create, message_count, "
            "session_count, cost_usd) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            (day, alpha_id, "claude", model, speed, tin * scale, tout * scale,
             cr * scale, cc * scale, msg, sess, cost * scale),
        )
    day, model, speed, tin, tout, cr, cc, msg, sess, cost = _BETA_DAILY
    conn.execute(
        "INSERT INTO daily_mart (day, project_id, provider, model, speed, "
        "input_tokens, output_tokens, cache_read, cache_create, message_count, "
        "session_count, cost_usd) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        (day, beta_id, "claude", model, speed, tin * scale, tout * scale,
         cr * scale, cc * scale, msg, sess, cost * scale),
    )

    conn.execute(
        "INSERT INTO project_mart (project_id, provider, slug, display_name, first_ts, "
        "last_ts, total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        "total_cache_read, total_cache_create, total_cost_usd) "
        "VALUES (?, 'claude', 'alpha', 'Alpha', '2026-07-01T10:00:00', '2026-07-02T12:00:00', "
        "7, 2, ?, ?, ?, ?, ?)",
        (alpha_id, 300 * scale, 130 * scale, 30 * scale, 13 * scale, 4.0 * scale),
    )
    conn.execute(
        "INSERT INTO project_mart (project_id, provider, slug, display_name, first_ts, "
        "last_ts, total_messages, total_sessions, total_input_tokens, total_output_tokens, "
        "total_cache_read, total_cache_create, total_cost_usd) "
        "VALUES (?, 'claude', 'beta', 'Beta', '2026-06-30T09:00:00', '2026-06-30T10:00:00', "
        "2, 1, ?, ?, ?, ?, ?)",
        (beta_id, 40 * scale, 20 * scale, 4 * scale, 2 * scale, 0.4 * scale),
    )

    conn.executemany(
        "INSERT INTO provider_day_mart (day, provider, cost_usd, message_count, "
        "session_count, project_count) VALUES (?,?,?,?,?,?)",
        [
            ("2026-07-01", "claude", 1.5 * scale, 3, 1, 1),
            ("2026-06-30", "claude", 0.4 * scale, 2, 1, 1),
        ],
    )
    conn.executemany(
        "INSERT INTO model_day_mart (day, model, speed, cost_usd, input_tokens, "
        "output_tokens, cache_read, cache_create, message_count, session_count) "
        "VALUES (?,?,?,?,?,?,?,?,?,?)",
        [
            ("2026-07-01", "opus", "standard", 1.5 * scale, 100 * scale, 50 * scale,
             10 * scale, 5 * scale, 3, 1),
            ("2026-06-30", "sonnet", "standard", 0.4 * scale, 40 * scale, 20 * scale,
             4 * scale, 2 * scale, 2, 1),
        ],
    )

    conn.execute(
        "INSERT INTO session_mart (session_id, project_id, provider, primary_model, "
        "first_ts, last_ts, message_count, user_message_count, assistant_message_count, "
        "input_tokens, output_tokens, cache_read, cache_create, cost_usd, is_one_shot, cwd) "
        "VALUES (?, ?, 'claude', 'opus', '2026-07-01T10:00:00', '2026-07-01T11:00:00', "
        "5, 2, 3, ?, ?, ?, ?, ?, 0, '/Users/x/alpha')",
        (session_id, alpha_id, 300 * scale, 130 * scale, 30 * scale, 13 * scale, 4.0 * scale),
    )


@pytest.fixture
def make_store(tmp_path):
    """Factory returning fresh, migrated stores (call once per simulated device)."""
    counter = {"n": 0}

    def _make() -> sqlite3.Connection:
        counter["n"] += 1
        conn = db.connect(tmp_path / f"store{counter['n']}.db")
        schema.apply(conn)
        return conn

    return _make


@pytest.fixture
def store_conn(make_store):
    return make_store()


@pytest.fixture
def seed_marts():
    """Return the ``_seed`` helper (so tests can seed several stores)."""
    return _seed
