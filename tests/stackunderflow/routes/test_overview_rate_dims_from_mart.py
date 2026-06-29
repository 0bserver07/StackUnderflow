"""Read-site wiring (v023): Overview cache / interruption / errors rates.

Proves ``routes/data._stats_from_marts`` surfaces the v023 rate dims off the
mart — ``cache.hit_rate``, ``user_interactions.interruption_rate`` /
``avg_tools_per_command`` / ``avg_steps_per_command``, and ``errors`` total /
rate / by_category — and that a multi-provider slug sums the numerators across
its merged rows before dividing. These insert ``project_mart`` rows with the
dim columns directly (the builder's own materialisation is covered by the
``etl/marts`` equivalence test), so they exercise the read path in isolation.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema

# Full project_mart column list (v006 + v022 + v023), in table order.
_PM_COLS = (
    "project_id, provider, slug, display_name, first_ts, last_ts, "
    "total_messages, total_sessions, total_input_tokens, total_output_tokens, "
    "total_cache_read, total_cache_create, total_cost_usd, "
    "total_user_messages, total_assistant_messages, total_tool_use_messages, "
    "total_tool_result_messages, total_commands, "
    "total_records, total_errors, errors_by_category, total_cache_read_messages, "
    "total_commands_followed_by_interruption, total_command_tools, total_command_steps"
)


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, provider, slug):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0.0, 0.0)",
        (provider, slug, slug),
    )
    return int(cur.lastrowid)


def _insert_mart(conn, *, pid, provider, slug, **d):
    conn.execute(
        f"INSERT INTO project_mart ({_PM_COLS}) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            pid, provider, slug, slug,
            "2026-04-01T00:00:00Z", "2026-04-30T00:00:00Z",
            d.get("total_messages", 0), d.get("total_sessions", 1),
            1000, 500, 0, 0, 1.25,
            d.get("total_user_messages", 0), d.get("total_assistant_messages", 0),
            d.get("total_tool_use_messages", 0), d.get("total_tool_result_messages", 0),
            d.get("total_commands", 0),
            d.get("total_records", 0), d.get("total_errors", 0),
            json.dumps(d.get("errors_by_category", {})),
            d.get("total_cache_read_messages", 0),
            d.get("total_commands_followed_by_interruption", 0),
            d.get("total_command_tools", 0), d.get("total_command_steps", 0),
        ),
    )


def _add_session(conn, pid):
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, '2026-04-25T00:00:00Z', '2026-04-25T00:00:00Z', 42)",
        (pid, f"s{pid}"),
    )


@pytest.mark.asyncio
async def test_dashboard_rate_dims_single_provider(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-rate-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _add_session(conn, pid)
    _insert_mart(
        conn, pid=pid, provider="claude", slug=slug,
        total_messages=42, total_records=50,
        total_assistant_messages=20, total_commands=10,
        total_cache_read_messages=15,             # hit_rate = 15/20*100 = 75.0
        total_commands_followed_by_interruption=3,  # ir = 3/10*100 = 30.0
        total_command_tools=25,                   # avg_tools = 25/10 = 2.5
        total_command_steps=40,                   # avg_steps = 40/10 = 4.0
        total_errors=5,                           # rate = 5/50 = 0.1
        errors_by_category={"Permission Error": 3, "Syntax Error": 2},
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    stats = (await data_route.get_dashboard_data())["statistics"]

    assert stats["cache"]["hit_rate"] == 75.0
    ui = stats["user_interactions"]
    assert ui["interruption_rate"] == 30.0
    assert ui["avg_tools_per_command"] == 2.5
    assert ui["avg_steps_per_command"] == 4.0
    assert ui["user_commands_analyzed"] == 10
    errors = stats["errors"]
    assert errors["total"] == 5
    assert errors["rate"] == pytest.approx(0.1)
    assert errors["by_category"] == {"Permission Error": 3, "Syntax Error": 2}


@pytest.mark.asyncio
async def test_dashboard_rate_dims_summed_multi_provider(tmp_path, monkeypatch):
    """A slug under two providers sums the numerators before dividing."""
    store_db = tmp_path / "store.db"
    slug = "-rate-multi"
    conn = _connect(store_db)
    p1 = _insert_project(conn, "claude", slug)
    p2 = _insert_project(conn, "codex", slug)
    _add_session(conn, p1)
    _add_session(conn, p2)
    _insert_mart(
        conn, pid=p1, provider="claude", slug=slug,
        total_assistant_messages=20, total_commands=10, total_records=50,
        total_cache_read_messages=15, total_commands_followed_by_interruption=3,
        total_command_tools=25, total_command_steps=40, total_errors=5,
        errors_by_category={"Permission Error": 3, "Syntax Error": 2},
    )
    _insert_mart(
        conn, pid=p2, provider="codex", slug=slug,
        total_assistant_messages=10, total_commands=4, total_records=20,
        total_cache_read_messages=5, total_commands_followed_by_interruption=1,
        total_command_tools=8, total_command_steps=12, total_errors=2,
        errors_by_category={"Permission Error": 1, "Tool Not Found": 2},
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    stats = (await data_route.get_dashboard_data())["statistics"]

    # hit_rate = (15+5)/(20+10)*100 = 66.7
    assert stats["cache"]["hit_rate"] == 66.7
    ui = stats["user_interactions"]
    assert ui["user_commands_analyzed"] == 14
    assert ui["interruption_rate"] == 28.6   # (3+1)/14*100
    assert ui["avg_tools_per_command"] == 2.36  # (25+8)/14
    assert ui["avg_steps_per_command"] == 3.71  # (40+12)/14
    errors = stats["errors"]
    assert errors["total"] == 7
    assert errors["rate"] == pytest.approx(7 / 70)
    assert errors["by_category"] == {
        "Permission Error": 4, "Syntax Error": 2, "Tool Not Found": 2,
    }
