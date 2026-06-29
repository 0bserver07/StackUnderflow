"""ToolMartBuilder — v023 cache-token attribution (ui-perf #20).

``tool_mart`` gained ``cache_read`` / ``cache_create`` columns so the ToolCost
block can attribute per-tool prompt-cache tokens instead of reading 0. They
follow the SAME 1/N split as ``tokens_in`` / ``tokens_out`` and are summed
additively across refresh windows. ``mart_queries.tool_mart_for_project``
surfaces them under the aggregator's field names (``cache_read_tokens`` /
``cache_creation_tokens``).
"""

from __future__ import annotations

import json

from stackunderflow.etl.marts.tool import ToolMartBuilder
from stackunderflow.store import mart_queries

from .conftest import insert_event


def test_single_tool_full_cache_attribution(conn) -> None:
    """One tool in tools_json gets 100% of the event's cache tokens."""
    insert_event(
        conn, event_id=1, cost_usd=0.20,
        cache_read=1000, cache_create=200,
        tools_json=json.dumps(["Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    r = conn.execute("SELECT * FROM tool_mart").fetchone()
    assert r["tool_name"] == "Read"
    assert r["cache_read"] == 1000
    assert r["cache_create"] == 200

    # mart_queries exposes them under the aggregator's field names, non-zero.
    out = mart_queries.tool_mart_for_project(conn, project_id=1)
    assert out["Read"]["cache_read_tokens"] == 1000
    assert out["Read"]["cache_creation_tokens"] == 200


def test_multi_tool_one_over_n_cache_split(conn) -> None:
    """Two distinct tools in one event split cache tokens 50/50."""
    insert_event(
        conn, event_id=1, cost_usd=0.30,
        cache_read=1000, cache_create=200,
        tools_json=json.dumps(["Read", "Edit"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    rows = {r["tool_name"]: r for r in conn.execute("SELECT * FROM tool_mart").fetchall()}
    assert set(rows) == {"Read", "Edit"}
    for name in ("Read", "Edit"):
        assert rows[name]["cache_read"] == 500
        assert rows[name]["cache_create"] == 100


def test_cache_tokens_additive_across_windows(conn) -> None:
    """Two events on the same (day, project, tool) sum their cache tokens."""
    insert_event(
        conn, event_id=1, cache_read=300, cache_create=50,
        tools_json=json.dumps(["Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=0)
    insert_event(
        conn, event_id=2, cache_read=700, cache_create=150,
        tools_json=json.dumps(["Read"]),
    )
    ToolMartBuilder().refresh(conn, since_event_id=1)  # incremental window

    r = conn.execute("SELECT * FROM tool_mart WHERE tool_name = 'Read'").fetchone()
    assert r["cache_read"] == 1000   # 300 + 700
    assert r["cache_create"] == 200  # 50 + 150


def test_rebuild_matches_incremental_cache_tokens(conn) -> None:
    """A full rebuild reproduces the incrementally-summed cache totals."""
    insert_event(conn, event_id=1, cache_read=300, cache_create=50, tools_json=json.dumps(["Read"]))
    insert_event(conn, event_id=2, cache_read=700, cache_create=150, tools_json=json.dumps(["Read"]))
    ToolMartBuilder().rebuild_from_scratch(conn)
    r = conn.execute("SELECT cache_read, cache_create FROM tool_mart WHERE tool_name = 'Read'").fetchone()
    assert r["cache_read"] == 1000
    assert r["cache_create"] == 200
