"""Tests for ``stackunderflow.services.risk.file_risk_summary``.

Spec 16 — file-risk recommender. Mart-fixture seeds the three outcome
shapes per file (reverted, failed, worked) plus a touched-but-uncertain
session, then asserts the bucket counts. Path resolution and ``since``
plumb through to the underlying outcome heuristic; we verify the full
shape locks in the keys the MCP / CLI / meta-agent surfaces consume.
"""

from __future__ import annotations

import json
import sqlite3
from datetime import UTC, datetime, timedelta

import pytest

from stackunderflow.services.risk import file_risk_summary
from stackunderflow.store import db, schema

# ── seeding helpers (mirrors tests/stackunderflow/services/test_discovery.py) ──


def _make_conn(tmp_path) -> sqlite3.Connection:
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _seed_project(
    conn: sqlite3.Connection,
    *,
    slug: str = "-Users-yad-dev-foo",
    path: str | None = None,
) -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES ('claude', ?, ?, ?, 0.0, 0.0)",
        (slug, path, slug),
    )
    return int(cur.lastrowid)


def _edit_blob(file_path: str = "/x/cost.py") -> str:
    return json.dumps([{"name": "Edit", "input": {"file_path": file_path}}])


def _bash_blob(cmd: str) -> str:
    return json.dumps([{"name": "Bash", "input": {"command": cmd}}])


def _seed_outcome_session(
    conn: sqlite3.Connection,
    *,
    session_id: str,
    project_id: int | None = None,
    turns: list[tuple[str, str, str]] | list[tuple[str, str]],
    last_ts: str = "2026-04-01T00:00:00+00:00",
) -> int:
    """Insert a session + a chain of messages.

    ``turns`` is a list of ``(role, content_text)`` or
    ``(role, content_text, tools_json)`` triples. Mirrors the fixture
    helper in ``tests/stackunderflow/services/test_discovery.py``.
    """
    if project_id is None:
        # Re-use an existing default-slug project so callers can chain
        # several sessions without violating the (provider, slug) unique
        # constraint.
        row = conn.execute(
            "SELECT id FROM projects WHERE provider='claude' AND slug=?",
            ("-Users-yad-dev-foo",),
        ).fetchone()
        if row is None:
            project_id = _seed_project(conn)
        else:
            project_id = int(row["id"] if isinstance(row, sqlite3.Row) else row[0])
    sfk_cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, last_ts, last_ts, len(turns)),
    )
    sfk = int(sfk_cur.lastrowid)
    for seq, turn in enumerate(turns):
        if len(turn) == 2:
            role, content_text = turn
            tools_json = "[]"
        else:
            role, content_text, tools_json = turn
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, "
            " cache_read_tokens, content_text, tools_json, raw_json, "
            " is_sidechain) VALUES "
            "(?, ?, ?, ?, 'claude-sonnet-4-5', 0, 0, 0, 0, ?, ?, '{}', 0)",
            (sfk, seq, last_ts, role, content_text, tools_json),
        )
    return sfk


# ── shape lock-in ───────────────────────────────────────────────────────────


class TestSummaryShape:
    def test_empty_store_returns_zero_buckets(self, tmp_path):
        conn = _make_conn(tmp_path)
        out = file_risk_summary(conn, "/x/cost.py")
        assert out == {
            "path": "/x/cost.py",
            "since": None,
            "total_sessions": 0,
            "reverted": 0,
            "failed": 0,
            "worked": 0,
            "recent_session_ids": [],
        }

    def test_all_keys_present(self, tmp_path):
        """The MCP / meta-agent contract depends on this exact key set."""
        conn = _make_conn(tmp_path)
        out = file_risk_summary(conn, "/x/cost.py")
        assert set(out) == {
            "path", "since", "total_sessions",
            "reverted", "failed", "worked", "recent_session_ids",
        }


# ── three-outcome bucketing (Spec 16 mart-fixture requirement) ──────────────


class TestBucketing:
    def test_reverted_session_counted(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="rev-1", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("assistant", "", _bash_blob("git checkout -- x/cost.py")),
        ])
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py")
        assert out["reverted"] == 1
        assert out["failed"] == 0
        assert out["worked"] == 0
        assert out["total_sessions"] == 1
        assert out["recent_session_ids"] == ["rev-1"]

    def test_failed_session_counted(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="fail-1", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "no, that broke the cost endpoint"),
        ])
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py")
        assert out["failed"] == 1
        assert out["reverted"] == 0
        assert out["worked"] == 0
        assert out["recent_session_ids"] == ["fail-1"]

    def test_worked_session_counted(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="ok-1", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "thanks, that worked"),
        ])
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py")
        assert out["worked"] == 1
        assert out["failed"] == 0
        assert out["reverted"] == 0
        assert out["total_sessions"] == 1
        # No failure-mode rows ⇒ no recent ids surfaced.
        assert out["recent_session_ids"] == []

    def test_three_outcome_shapes_per_file(self, tmp_path):
        """Mart-fixture from the spec — three different shapes on one path."""
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="rev-1", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("assistant", "", _bash_blob("git checkout -- x/cost.py")),
        ], last_ts="2026-04-01T00:00:00+00:00")
        _seed_outcome_session(conn, session_id="fail-1", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "no, that broke the cost endpoint"),
        ], last_ts="2026-04-02T00:00:00+00:00")
        _seed_outcome_session(conn, session_id="ok-1", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "thanks, that worked"),
        ], last_ts="2026-04-03T00:00:00+00:00")
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py")
        assert out["reverted"] == 1
        assert out["failed"] == 1
        assert out["worked"] == 1
        assert out["total_sessions"] == 3
        # Failure-mode sessions, newest first ⇒ fail-1 then rev-1.
        assert out["recent_session_ids"] == ["fail-1", "rev-1"]

    def test_uncertain_outcome_counted_in_total_only(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="quiet", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
        ])
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py")
        # Single edit with no follow-up ⇒ uncertain ⇒ filtered from
        # all three outcome buckets, but the touching aggregator still
        # counts it because the file appears in tools_json.
        assert out["total_sessions"] == 1
        assert out["reverted"] == 0
        assert out["failed"] == 0
        assert out["worked"] == 0
        assert out["recent_session_ids"] == []


# ── since cutoff plumb-through ──────────────────────────────────────────────


class TestSinceCutoff:
    def test_since_filters_total_and_failure_modes(self, tmp_path):
        conn = _make_conn(tmp_path)
        # An old failure-mode session (outside the 7d window) and a
        # recent one (inside).
        old_ts = "2025-01-01T00:00:00+00:00"
        recent_ts = (datetime.now(UTC) - timedelta(hours=1)).isoformat()
        _seed_outcome_session(conn, session_id="old-fail", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "no, that broke the cost endpoint"),
        ], last_ts=old_ts)
        _seed_outcome_session(conn, session_id="recent-fail", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "no, that broke the cost endpoint"),
        ], last_ts=recent_ts)
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py", since="7d")
        assert out["since"] == "7d"
        assert out["failed"] == 1
        assert out["recent_session_ids"] == ["recent-fail"]

    def test_invalid_since_raises(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError):
            file_risk_summary(conn, "/x/cost.py", since="yesterday")


# ── recent_session_ids cap ──────────────────────────────────────────────────


class TestRecentSessionIdsCap:
    def test_cap_at_recent_limit(self, tmp_path):
        conn = _make_conn(tmp_path)
        # Six failure-mode sessions; the default ``recent_limit=5`` keeps
        # the five newest.
        for i in range(6):
            _seed_outcome_session(conn, session_id=f"fail-{i}", turns=[
                ("assistant", "", _edit_blob("/x/cost.py")),
                ("user", "no, that broke the cost endpoint"),
            ], last_ts=f"2026-04-{(i + 1):02d}T00:00:00+00:00")
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py")
        assert out["failed"] == 6
        # Newest first, capped at 5.
        assert out["recent_session_ids"] == [
            "fail-5", "fail-4", "fail-3", "fail-2", "fail-1",
        ]

    def test_explicit_recent_limit_zero_returns_all(self, tmp_path):
        """``recent_limit <= 0`` removes the cap."""
        conn = _make_conn(tmp_path)
        for i in range(3):
            _seed_outcome_session(conn, session_id=f"f-{i}", turns=[
                ("assistant", "", _edit_blob("/x/cost.py")),
                ("user", "no, that broke things"),
            ], last_ts=f"2026-04-{(i + 1):02d}T00:00:00+00:00")
        conn.commit()
        out = file_risk_summary(conn, "/x/cost.py", recent_limit=0)
        assert len(out["recent_session_ids"]) == 3
