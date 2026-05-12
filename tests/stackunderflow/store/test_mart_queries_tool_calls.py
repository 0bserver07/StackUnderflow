"""v012 read helpers — ``calls_total`` surfaces in the tool_mart queries.

Covers the three read-side touch points for the new column:

* ``tool_mart_for_project`` exposes ``calls_total`` alongside ``calls``.
* ``tool_mart_calls_distribution`` returns the per-tool distribution
  ``{tool_name, distinct_messages, total_calls, cost_usd}``.
* ``tool_call_count_in_window(count_column="calls_total")`` sums the
  non-distinct count; the default still sums ``event_count``.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from stackunderflow.store import db, mart_queries, schema


@pytest.fixture()
def conn(tmp_path: Path):
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    c.execute(
        "INSERT INTO projects (id, provider, slug, display_name, "
        "first_seen, last_modified) VALUES (1, 'claude', 'alpha', 'Alpha', 0, 0)"
    )
    c.execute(
        "INSERT INTO projects (id, provider, slug, display_name, "
        "first_seen, last_modified) VALUES (2, 'claude', 'beta', 'Beta', 0, 0)"
    )
    yield c
    c.close()


def _insert_tool_row(conn, *, project_id, day, tool_name, event_count,
                     calls_total, cost_usd=0.0, provider="claude",
                     tokens_in=0, tokens_out=0, session_count=1):
    conn.execute(
        "INSERT INTO tool_mart "
        "(day, project_id, provider, tool_name, event_count, calls_total, "
        " cost_usd, tokens_in, tokens_out, session_count) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (day, project_id, provider, tool_name, event_count, calls_total,
         cost_usd, tokens_in, tokens_out, session_count),
    )


def test_tool_mart_for_project_includes_calls_total(conn) -> None:
    """``tool_mart_for_project`` sums ``calls_total`` across (day, provider)."""
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Read",
                     event_count=3, calls_total=11, cost_usd=0.05,
                     tokens_in=100, tokens_out=50)
    _insert_tool_row(conn, project_id=1, day="2026-04-02", tool_name="Read",
                     event_count=2, calls_total=4, cost_usd=0.02,
                     tokens_in=40, tokens_out=20)
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Edit",
                     event_count=1, calls_total=1, cost_usd=0.01)
    out = mart_queries.tool_mart_for_project(conn, project_id=1)
    assert out["Read"]["calls"] == 5          # 3 + 2 distinct (message, tool)
    assert out["Read"]["calls_total"] == 15   # 11 + 4 occurrences
    assert out["Edit"]["calls"] == 1
    assert out["Edit"]["calls_total"] == 1
    assert abs(out["Read"]["cost"] - 0.07) < 1e-9


def test_tool_mart_calls_distribution_shape_and_order(conn) -> None:
    """Distribution rows carry the 4 documented keys, sorted by total_calls desc."""
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Read",
                     event_count=4, calls_total=20, cost_usd=0.10)
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Bash",
                     event_count=6, calls_total=6, cost_usd=0.30)
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Edit",
                     event_count=2, calls_total=9, cost_usd=0.05)
    # Different project — must not bleed in.
    _insert_tool_row(conn, project_id=2, day="2026-04-01", tool_name="Read",
                     event_count=99, calls_total=999, cost_usd=99.0)

    rows = mart_queries.tool_mart_calls_distribution(conn, 1)
    assert [r["tool_name"] for r in rows] == ["Read", "Edit", "Bash"]  # 20 > 9 > 6
    read = rows[0]
    assert set(read) == {"tool_name", "distinct_messages", "total_calls", "cost_usd"}
    assert read["distinct_messages"] == 4
    assert read["total_calls"] == 20
    assert abs(read["cost_usd"] - 0.10) < 1e-9


def test_tool_mart_calls_distribution_since_filter(conn) -> None:
    """``since`` is an inclusive ``YYYY-MM-DD`` lower bound on ``day``."""
    _insert_tool_row(conn, project_id=1, day="2026-03-31", tool_name="Read",
                     event_count=5, calls_total=50)
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Read",
                     event_count=2, calls_total=7)
    rows = mart_queries.tool_mart_calls_distribution(conn, 1, since="2026-04-01")
    assert len(rows) == 1
    assert rows[0]["total_calls"] == 7


def test_tool_mart_calls_distribution_empty_table(tmp_path: Path) -> None:
    """No ``tool_mart`` table → ``[]`` (caller falls back to the aggregator)."""
    c = db.connect(tmp_path / "bare.db")
    try:
        # Bare DB — no schema applied, so tool_mart doesn't exist.
        assert mart_queries.tool_mart_calls_distribution(c, 1) == []
    finally:
        c.close()


def test_tool_call_count_in_window_count_column(conn) -> None:
    """``count_column`` picks ``event_count`` (default) or ``calls_total``."""
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Read",
                     event_count=5, calls_total=42)
    default = mart_queries.tool_call_count_in_window(conn, tool_names=("Read",))
    assert default == 5
    totals = mart_queries.tool_call_count_in_window(
        conn, tool_names=("Read",), count_column="calls_total"
    )
    assert totals == 42


def test_tool_call_count_in_window_rejects_bad_column(conn) -> None:
    """An off-whitelist ``count_column`` raises rather than reaching SQL."""
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Read",
                     event_count=1, calls_total=1)
    with pytest.raises(ValueError, match="count_column"):
        mart_queries.tool_call_count_in_window(
            conn, tool_names=("Read",), count_column="cost_usd"
        )


def test_tool_call_count_in_window_calls_total_with_project_filter(conn) -> None:
    """``count_column='calls_total'`` works through the project-slug JOIN path."""
    _insert_tool_row(conn, project_id=1, day="2026-04-01", tool_name="Read",
                     event_count=3, calls_total=12)
    _insert_tool_row(conn, project_id=2, day="2026-04-01", tool_name="Read",
                     event_count=10, calls_total=100)
    only_alpha = mart_queries.tool_call_count_in_window(
        conn, tool_names=("Read",), project_filter=["alpha"],
        count_column="calls_total",
    )
    assert only_alpha == 12
