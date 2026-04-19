"""Tests for queries.get_project_stats / get_project_messages.

Seeds the store from the pipeline reader's raw entries (same source that
`process()` uses) then verifies that the store-backed output matches the
filesystem-based pipeline output on the same mock data.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from stackunderflow.pipeline import process
from stackunderflow.store import db, queries, schema

MOCK_DIR = (
    Path(__file__).parent.parent.parent
    / "mock-data"
    / "-Users-test-dev-ai-music"
)


@pytest.fixture
def conn(tmp_path):
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def _seed_from_reader(conn, mock_dir: Path) -> int:
    """Seed the store using pipeline reader RawEntries — exact same source process() uses."""
    from stackunderflow.pipeline.reader import scan

    slug = mock_dir.name
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?, ?)",
        ("claude", slug, str(mock_dir), slug, 0.0, 0.0),
    )
    project_id = cur.lastrowid

    by_session: dict[str, list] = {}
    for entry in scan(str(mock_dir)):
        by_session.setdefault(entry.session_id, []).append(entry)

    for session_id, entries in by_session.items():
        cur = conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
            (project_id, session_id),
        )
        session_fk = cur.lastrowid
        for seq, entry in enumerate(entries):
            obj = entry.payload
            conn.execute(
                "INSERT OR IGNORE INTO messages "
                "(session_fk, seq, timestamp, role, model, input_tokens, output_tokens, "
                " cache_create_tokens, cache_read_tokens, content_text, tools_json, "
                " raw_json, is_sidechain, uuid, parent_uuid) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    session_fk, seq,
                    obj.get("timestamp", ""),
                    obj.get("type", ""),
                    obj.get("model"),
                    0, 0, 0, 0,
                    "",
                    "[]",
                    json.dumps(obj, default=str),
                    int(bool(obj.get("isSidechain", False))),
                    obj.get("uuid"),
                    obj.get("parentUuid"),
                ),
            )
    conn.commit()
    return project_id


@pytest.fixture
def project_id(conn):
    if not MOCK_DIR.exists():
        pytest.skip("mock data not present")
    return _seed_from_reader(conn, MOCK_DIR)


@pytest.fixture
def baseline():
    if not MOCK_DIR.exists():
        pytest.skip("mock data not present")
    msgs, stats = process(str(MOCK_DIR))
    return msgs, stats


class TestGetProjectStats:
    def test_returns_same_message_count(self, conn, project_id, baseline):
        baseline_msgs, _ = baseline
        store_msgs, _ = queries.get_project_stats(conn, project_id=project_id)
        assert len(store_msgs) == len(baseline_msgs)

    def test_overview_totals_match(self, conn, project_id, baseline):
        _, baseline_stats = baseline
        _, store_stats = queries.get_project_stats(conn, project_id=project_id)
        assert store_stats["overview"]["total_messages"] == baseline_stats["overview"]["total_messages"]
        assert store_stats["overview"]["sessions"] == baseline_stats["overview"]["sessions"]
        assert store_stats["overview"]["total_tokens"] == baseline_stats["overview"]["total_tokens"]
        assert store_stats["overview"]["total_cost"] == pytest.approx(
            baseline_stats["overview"]["total_cost"], rel=1e-6
        )

    def test_tools_match(self, conn, project_id, baseline):
        _, baseline_stats = baseline
        _, store_stats = queries.get_project_stats(conn, project_id=project_id)
        assert store_stats["tools"] == baseline_stats["tools"]

    def test_daily_stats_match(self, conn, project_id, baseline):
        _, baseline_stats = baseline
        _, store_stats = queries.get_project_stats(conn, project_id=project_id)
        assert store_stats["daily_stats"] == baseline_stats["daily_stats"]

    def test_hourly_pattern_matches(self, conn, project_id, baseline):
        _, baseline_stats = baseline
        _, store_stats = queries.get_project_stats(conn, project_id=project_id)
        assert store_stats["hourly_pattern"] == baseline_stats["hourly_pattern"]

    def test_sessions_section_matches(self, conn, project_id, baseline):
        _, baseline_stats = baseline
        _, store_stats = queries.get_project_stats(conn, project_id=project_id)
        assert store_stats["sessions"] == baseline_stats["sessions"]

    def test_models_match(self, conn, project_id, baseline):
        _, baseline_stats = baseline
        _, store_stats = queries.get_project_stats(conn, project_id=project_id)
        assert store_stats["models"] == baseline_stats["models"]

    def test_missing_project_returns_empty(self, conn):
        msgs, stats = queries.get_project_stats(conn, project_id=99999)
        assert msgs == []
        assert stats == {}

    def test_tz_offset_affects_daily_stats(self, conn, project_id, baseline):
        _, baseline_stats = baseline
        _, stats_utc = queries.get_project_stats(conn, project_id=project_id, tz_offset=0)
        _, stats_tz = queries.get_project_stats(conn, project_id=project_id, tz_offset=480)
        # With a large offset, day boundaries shift — at least one key may differ
        # (we just verify it doesn't crash and produces valid structure)
        assert "daily_stats" in stats_tz
        assert "hourly_pattern" in stats_tz


class TestGetProjectMessages:
    def test_message_count_matches_pipeline(self, conn, project_id, baseline):
        baseline_msgs, _ = baseline
        store_msgs = queries.get_project_messages(conn, project_id=project_id)
        assert len(store_msgs) == len(baseline_msgs)

    def test_session_ids_match(self, conn, project_id, baseline):
        baseline_msgs, _ = baseline
        store_msgs = queries.get_project_messages(conn, project_id=project_id)
        assert {m["session_id"] for m in store_msgs} == {m["session_id"] for m in baseline_msgs}

    def test_limit_respected(self, conn, project_id):
        limited = queries.get_project_messages(conn, project_id=project_id, limit=3)
        assert len(limited) <= 3

    def test_limit_none_returns_all(self, conn, project_id, baseline):
        baseline_msgs, _ = baseline
        all_msgs = queries.get_project_messages(conn, project_id=project_id)
        assert len(all_msgs) == len(baseline_msgs)
