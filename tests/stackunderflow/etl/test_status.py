"""Unit tests for :mod:`stackunderflow.etl.status` internals.

The route-level shape/health contract lives in
``tests/stackunderflow/routes/test_etl_status.py``; this module pins the
``_events_summary`` aggregation directly — both its values and its query
*shape*, since #43 folded the per-poll event breakdowns into a single
``GROUP BY provider, cost_source`` pass (no separate unindexed
``cost_source`` full scan) — and the ``_coverage_summary`` gap counter.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from stackunderflow.etl.status import (
    COVERAGE_SAMPLE_LIMIT,
    _coverage_summary,
    _events_summary,
)
from stackunderflow.store import db, schema


@pytest.fixture()
def conn(tmp_path: Path):
    store_db = tmp_path / "store.db"
    c = db.connect(store_db)
    schema.apply(c)
    # The breakdowns only read provider/cost_source/id off ``usage_events``;
    # drop FK enforcement so the test can seed events directly without the
    # full project→session→message chain.
    c.execute("PRAGMA foreign_keys = OFF")
    yield c
    c.close()


def _seed_events(c: sqlite3.Connection, rows: list[tuple[int, str, str]]) -> None:
    """Insert ``(source_message_fk, provider, cost_source)`` triples."""
    c.executemany(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, account, project_id, session_id, ts, day, "
        " model, speed, input_tokens, output_tokens, cache_read_tokens, "
        " cache_create_tokens, cost_usd, cost_source, role, raw_extras) "
        "VALUES (?, ?, 'default', 1, 's1', '2026-04-01T00:00:00Z', '2026-04-01', "
        "'m', 'standard', 0, 0, 0, 0, 0.0, ?, 'assistant', NULL)",
        rows,
    )
    c.commit()


class _RecordingConn:
    """Transparent proxy that records every SQL string ``execute``'d."""

    def __init__(self, real: sqlite3.Connection):
        self._real = real
        self.sql: list[str] = []

    def execute(self, sql, *args, **kwargs):
        self.sql.append(sql)
        return self._real.execute(sql, *args, **kwargs)

    def __getattr__(self, name):
        return getattr(self._real, name)


def test_empty_store_returns_zeros(conn):
    assert _events_summary(conn) == {
        "total": 0,
        "max_id": 0,
        "by_provider": {},
        "by_cost_source": {},
    }


def test_breakdowns_match_and_total_counts_blank_rows(conn):
    # 4 events: two providers + one blank-provider row (which ``total`` still
    # counts but ``by_provider`` skips, exactly like the old COUNT(*)).
    _seed_events(
        conn,
        [
            (1, "claude", "rate_card"),
            (2, "claude", "estimated"),
            (3, "codex", "rate_card"),
            (4, "", "rate_card"),
        ],
    )

    events = _events_summary(conn)

    assert events["total"] == 4
    assert events["max_id"] == 4
    assert events["by_provider"] == {"claude": 2, "codex": 1}
    assert events["by_cost_source"] == {"rate_card": 3, "estimated": 1}


def test_events_summary_is_a_single_grouped_pass(conn):
    """#43: one combined GROUP BY scan + one O(1) MAX(id) lookup — no
    standalone unindexed ``cost_source`` scan, no separate COUNT(*) scan."""
    _seed_events(
        conn,
        [
            (1, "claude", "rate_card"),
            (2, "codex", "estimated"),
        ],
    )

    rec = _RecordingConn(conn)
    events = _events_summary(rec)  # type: ignore[arg-type]  # duck-typed proxy
    # Correctness still holds through the proxy.
    assert events["by_cost_source"] == {"rate_card": 1, "estimated": 1}

    norm = [" ".join(s.split()).lower() for s in rec.sql]

    # Exactly one GROUP BY touches usage_events, and it groups by BOTH columns.
    group_bys = [s for s in norm if "group by" in s and "usage_events" in s]
    assert len(group_bys) == 1, group_bys
    assert "group by provider, cost_source" in group_bys[0]

    # Only two reads of usage_events total: the grouped pass + the MAX(id)
    # primary-key lookup. (The old code did three: COUNT+MAX, GROUP BY
    # provider, GROUP BY cost_source.)
    ue_reads = [s for s in norm if "from usage_events" in s]
    assert len(ue_reads) == 2, ue_reads
    assert any("max(id)" in s for s in ue_reads)


# ── coverage ─────────────────────────────────────────────────────────────────
#
# A ``projects`` row with no ``project_mart`` row is invisible to every
# mart-backed read AND to ``lag_seconds`` (which only compares watermarks).
# The gap ran unnoticed because nothing counted it; these pin the counter.


def _seed_projects(c: sqlite3.Connection, n: int) -> list[int]:
    ids = []
    for i in range(n):
        cur = c.execute(
            "INSERT INTO projects (provider, slug, display_name, "
            "first_seen, last_modified) VALUES ('claude', ?, ?, 0.0, 0.0)",
            (f"-p{i}", f"-p{i}"),
        )
        ids.append(int(cur.lastrowid))
    return ids


def _cover(c: sqlite3.Connection, project_ids: list[int]) -> None:
    c.executemany(
        "INSERT INTO project_mart (project_id, provider, slug, display_name) "
        "VALUES (?, 'claude', 'x', 'x')",
        [(pid,) for pid in project_ids],
    )


def test_coverage_empty_store_is_all_zeros(conn):
    assert _coverage_summary(conn) == {
        "projects": 0,
        "projects_with_mart": 0,
        "projects_without_mart": 0,
        "projects_without_mart_sample": [],
    }


def test_coverage_reports_the_gap_with_ids(conn):
    ids = _seed_projects(conn, 5)
    _cover(conn, ids[:3])

    cov = _coverage_summary(conn)

    assert cov["projects"] == 5
    assert cov["projects_with_mart"] == 3
    assert cov["projects_without_mart"] == 2
    assert cov["projects_without_mart_sample"] == ids[3:]


def test_coverage_is_zero_when_every_project_has_a_row(conn):
    ids = _seed_projects(conn, 4)
    _cover(conn, ids)

    cov = _coverage_summary(conn)

    assert cov["projects"] == 4
    assert cov["projects_with_mart"] == 4
    assert cov["projects_without_mart"] == 0
    assert cov["projects_without_mart_sample"] == []


def test_coverage_sample_is_capped_but_count_is_not(conn):
    """The count is the signal; the id sample is a fixed-size convenience."""
    n = COVERAGE_SAMPLE_LIMIT + 7
    _seed_projects(conn, n)

    cov = _coverage_summary(conn)

    assert cov["projects_without_mart"] == n
    assert len(cov["projects_without_mart_sample"]) == COVERAGE_SAMPLE_LIMIT


def test_coverage_degrades_when_mart_table_is_missing(conn):
    """Pre-Wave-1 store: no ``project_mart`` at all → everything uncovered."""
    ids = _seed_projects(conn, 2)
    conn.execute("DROP TABLE project_mart")

    cov = _coverage_summary(conn)

    assert cov["projects"] == 2
    assert cov["projects_with_mart"] == 0
    assert cov["projects_without_mart"] == 2
    assert cov["projects_without_mart_sample"] == ids
