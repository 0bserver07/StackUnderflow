"""mart_queries readers for ``command_day_mart`` (v025, #25).

Unit-level coverage of the windowed-command readers the dashboard/Overview
paths consume: ``mart_has_command_day_rows``, ``command_count_in_window``
(scalar windowed sum) and ``command_day_series`` (per-day rows for the
frontend's window sum). Rows are inserted directly so the readers are exercised
in isolation from the builder (covered in ``etl/marts/test_command_day_mart``).
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.store import db, mart_queries, schema


def _connect(tmp_path: Path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed(conn, rows: list[tuple[str, int, int]]) -> None:
    """rows = [(day, project_id, command_count), ...]."""
    conn.executemany(
        "INSERT INTO command_day_mart (day, project_id, command_count) VALUES (?, ?, ?)",
        rows,
    )
    conn.commit()


def test_mart_has_command_day_rows(tmp_path):
    conn = _connect(tmp_path)
    assert mart_queries.mart_has_command_day_rows(conn) is False
    _seed(conn, [("2026-04-01", 1, 3)])
    assert mart_queries.mart_has_command_day_rows(conn) is True
    conn.close()


def test_command_count_in_window_bounds_and_projects(tmp_path):
    conn = _connect(tmp_path)
    _seed(conn, [
        ("2026-04-01", 1, 3), ("2026-04-10", 1, 5), ("2026-04-20", 1, 2),
        ("2026-04-10", 2, 7),  # different project
    ])
    # Whole window, single project.
    assert mart_queries.command_count_in_window(conn, project_ids=[1]) == 10
    # Bounded window keeps only the 04-10 bucket.
    assert mart_queries.command_count_in_window(
        conn, project_ids=[1], day_from="2026-04-05", day_to="2026-04-15"
    ) == 5
    # Multi-project sum within window.
    assert mart_queries.command_count_in_window(
        conn, project_ids=[1, 2], day_from="2026-04-05", day_to="2026-04-15"
    ) == 12
    # Empty project list → 0.
    assert mart_queries.command_count_in_window(conn, project_ids=[]) == 0
    conn.close()


def test_command_count_in_window_no_table(tmp_path):
    """Missing table (fresh store, no migration applied) → 0, no error."""
    conn = db.connect(tmp_path / "store.db")  # NOTE: no schema.apply
    assert mart_queries.command_count_in_window(conn, project_ids=[1]) == 0
    conn.close()


def test_command_day_series_global_sums_across_projects(tmp_path):
    conn = _connect(tmp_path)
    _seed(conn, [
        ("2026-04-01", 1, 3), ("2026-04-01", 2, 4), ("2026-04-02", 1, 5),
    ])
    series = mart_queries.command_day_series(conn)  # global
    assert series == [
        {"date": "2026-04-01", "commands": 7},
        {"date": "2026-04-02", "commands": 5},
    ]
    conn.close()


def test_command_day_series_project_scoped_and_windowed(tmp_path):
    conn = _connect(tmp_path)
    _seed(conn, [
        ("2026-04-01", 1, 3), ("2026-04-10", 1, 5), ("2026-04-01", 2, 99),
    ])
    # Project 1 only, no window.
    assert mart_queries.command_day_series(conn, project_ids=[1]) == [
        {"date": "2026-04-01", "commands": 3},
        {"date": "2026-04-10", "commands": 5},
    ]
    # Windowed.
    assert mart_queries.command_day_series(
        conn, project_ids=[1], day_from="2026-04-05"
    ) == [{"date": "2026-04-10", "commands": 5}]
    # Empty project list → [].
    assert mart_queries.command_day_series(conn, project_ids=[]) == []
    conn.close()
