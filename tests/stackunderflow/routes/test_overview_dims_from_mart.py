"""Read-site wiring (v022): Overview message-type + command dims off the mart.

Proves ``routes/data._stats_from_marts`` surfaces the materialised
``overview.message_types`` and ``user_interactions.user_commands_analyzed``,
and ``routes/projects._mart_row_to_stats`` surfaces ``total_commands`` — the
0/None the audit (#7/#26) flagged is now the real value. These insert a
``project_mart`` row with the dim columns directly (the builder's own
materialisation is covered by the equivalence tests), so they exercise the
read path in isolation.
"""

from __future__ import annotations

import json

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.routes.projects import get_projects
from stackunderflow.store import db, schema

_PM_COLS = (
    "project_id, provider, slug, display_name, first_ts, last_ts, "
    "total_messages, total_sessions, total_input_tokens, total_output_tokens, "
    "total_cache_read, total_cache_create, total_cost_usd, "
    "total_user_messages, total_assistant_messages, total_tool_use_messages, "
    "total_tool_result_messages, total_commands"
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


def _insert_project_mart_with_dims(conn, *, pid, provider, slug, **dims):
    conn.execute(
        f"INSERT INTO project_mart ({_PM_COLS}) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            pid, provider, slug, slug,
            "2026-04-01T00:00:00Z", "2026-04-30T00:00:00Z",
            dims.get("total_messages", 0), dims.get("total_sessions", 0),
            1000, 500, 0, 0, 1.25,
            dims.get("total_user_messages", 0),
            dims.get("total_assistant_messages", 0),
            dims.get("total_tool_use_messages", 0),
            dims.get("total_tool_result_messages", 0),
            dims.get("total_commands", 0),
        ),
    )


@pytest.mark.asyncio
async def test_dashboard_overview_dims_from_mart(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-dims-proj"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 's1', '2026-04-25T00:00:00Z', '2026-04-25T00:00:00Z', 42)",
        (pid,),
    )
    _insert_project_mart_with_dims(
        conn, pid=pid, provider="claude", slug=slug,
        total_messages=42, total_sessions=1,
        total_user_messages=20, total_assistant_messages=18,
        total_tool_use_messages=12, total_tool_result_messages=9,
        total_commands=7,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    payload = await data_route.get_dashboard_data()
    stats = payload["statistics"]

    mt = stats["overview"]["message_types"]
    assert mt["user"] == 20
    assert mt["assistant"] == 18
    assert mt["tool_use"] == 12
    assert mt["tool_result"] == 9
    assert stats["user_interactions"]["user_commands_analyzed"] == 7


@pytest.mark.asyncio
async def test_dashboard_overview_dims_summed_multi_provider(tmp_path, monkeypatch):
    """A slug under two providers sums its dims across the merged mart rows."""
    store_db = tmp_path / "store.db"
    slug = "-multi-dims"
    conn = _connect(store_db)
    p1 = _insert_project(conn, "claude", slug)
    p2 = _insert_project(conn, "codex", slug)
    for pid in (p1, p2):
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, '2026-04-25T00:00:00Z', '2026-04-25T00:00:00Z', 10)",
            (pid, f"s{pid}"),
        )
    _insert_project_mart_with_dims(
        conn, pid=p1, provider="claude", slug=slug,
        total_user_messages=20, total_assistant_messages=18, total_commands=7,
    )
    _insert_project_mart_with_dims(
        conn, pid=p2, provider="codex", slug=slug,
        total_user_messages=5, total_assistant_messages=4, total_commands=3,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    data_route.invalidate_dashboard_cache()

    payload = await data_route.get_dashboard_data()
    stats = payload["statistics"]
    assert stats["overview"]["message_types"]["user"] == 25
    assert stats["overview"]["message_types"]["assistant"] == 22
    assert stats["user_interactions"]["user_commands_analyzed"] == 10


@pytest.mark.asyncio
async def test_projects_list_surfaces_total_commands(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", "alpha")
    _insert_project_mart_with_dims(
        conn, pid=pid, provider="claude", slug="alpha", total_commands=13,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    response = await get_projects(include_stats=True)
    body = json.loads(response.body.decode("utf-8"))
    stats = body["projects"][0]["stats"]
    assert stats["total_commands"] == 13
