"""v017 migration: ``pr_outcomes`` + ``ci_runs`` — Spec 20.

See ``stackunderflow/store/migrations/v017_pr_ci_outcomes.sql`` for the
full design. Both tables are additive, ``IF NOT EXISTS``-guarded, with
a ``UNIQUE`` constraint that lets the upsert helpers in
``stackunderflow.services.github_ingest`` route between insert + update.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.store import db, schema

_PR_COLUMNS = (
    "id", "provider", "repo_slug", "pr_number", "title", "state",
    "merged_at", "reverted_at", "author", "raw_json",
)
_CI_COLUMNS = (
    "id", "provider", "repo_slug", "run_id", "commit_sha", "status",
    "workflow_name", "started_ts", "completed_ts", "raw_json",
)


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


class TestV017:
    def test_current_version_is_at_least_17(self) -> None:
        assert schema.CURRENT_VERSION >= 17

    def test_apply_lands_on_current_version(self, conn: sqlite3.Connection) -> None:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION

    def test_pr_outcomes_table_shape(self, conn: sqlite3.Connection) -> None:
        cols = [
            r["name"] for r in conn.execute("PRAGMA table_info(pr_outcomes)").fetchall()
        ]
        assert tuple(cols) == _PR_COLUMNS

    def test_ci_runs_table_shape(self, conn: sqlite3.Connection) -> None:
        cols = [
            r["name"] for r in conn.execute("PRAGMA table_info(ci_runs)").fetchall()
        ]
        assert tuple(cols) == _CI_COLUMNS

    def test_pr_outcomes_required_columns_not_null(self, conn: sqlite3.Connection) -> None:
        info = {
            r["name"]: r
            for r in conn.execute("PRAGMA table_info(pr_outcomes)").fetchall()
        }
        for col in ("provider", "repo_slug", "pr_number", "state", "raw_json"):
            assert info[col]["notnull"] == 1, f"{col} should be NOT NULL"
        # Nullable: title, merged_at, reverted_at, author
        for col in ("title", "merged_at", "reverted_at", "author"):
            assert info[col]["notnull"] == 0, f"{col} should be nullable"

    def test_ci_runs_required_columns_not_null(self, conn: sqlite3.Connection) -> None:
        info = {
            r["name"]: r
            for r in conn.execute("PRAGMA table_info(ci_runs)").fetchall()
        }
        for col in ("provider", "repo_slug", "run_id", "commit_sha", "status", "raw_json"):
            assert info[col]["notnull"] == 1, f"{col} should be NOT NULL"
        for col in ("workflow_name", "started_ts", "completed_ts"):
            assert info[col]["notnull"] == 0, f"{col} should be nullable"

    def test_indexes_present(self, conn: sqlite3.Connection) -> None:
        pr_idx = {
            r["name"]
            for r in conn.execute("PRAGMA index_list(pr_outcomes)").fetchall()
        }
        ci_idx = {
            r["name"]
            for r in conn.execute("PRAGMA index_list(ci_runs)").fetchall()
        }
        assert "idx_pr_outcomes_repo" in pr_idx
        assert "idx_ci_runs_commit" in ci_idx

    def test_pr_unique_constraint(self, conn: sqlite3.Connection) -> None:
        # UNIQUE (provider, repo_slug, pr_number).
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, "
            " state, raw_json) VALUES ('github', 'octocat/hello', 1, 'open', '{}')"
        )
        with pytest.raises(sqlite3.IntegrityError):
            conn.execute(
                "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, "
                " state, raw_json) VALUES ('github', 'octocat/hello', 1, 'merged', '{}')"
            )
        # Different pr_number is fine.
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, "
            " state, raw_json) VALUES ('github', 'octocat/hello', 2, 'open', '{}')"
        )
        # Different provider on the same repo+pr is fine.
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, "
            " state, raw_json) VALUES ('gitlab', 'octocat/hello', 1, 'open', '{}')"
        )

    def test_ci_unique_constraint(self, conn: sqlite3.Connection) -> None:
        # UNIQUE (provider, run_id).
        conn.execute(
            "INSERT INTO ci_runs (provider, repo_slug, run_id, "
            " commit_sha, status, raw_json) "
            "VALUES ('github-actions', 'octocat/hello', 'r1', 'abc', 'success', '{}')"
        )
        with pytest.raises(sqlite3.IntegrityError):
            conn.execute(
                "INSERT INTO ci_runs (provider, repo_slug, run_id, "
                " commit_sha, status, raw_json) "
                "VALUES ('github-actions', 'octocat/hello', 'r1', 'def', 'failure', '{}')"
            )
        # Different run_id allowed.
        conn.execute(
            "INSERT INTO ci_runs (provider, repo_slug, run_id, "
            " commit_sha, status, raw_json) "
            "VALUES ('github-actions', 'octocat/hello', 'r2', 'abc', 'success', '{}')"
        )

    def test_raw_json_round_trips(self, conn: sqlite3.Connection) -> None:
        payload = {"id": 99, "title": "hello", "extra": [1, 2, {"k": "v"}]}
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, "
            " state, raw_json) VALUES (?, ?, ?, ?, ?)",
            ("github", "octocat/hello", 99, "open", json.dumps(payload)),
        )
        row = conn.execute(
            "SELECT raw_json FROM pr_outcomes WHERE pr_number = 99"
        ).fetchone()
        assert json.loads(row["raw_json"]) == payload

    def test_reapply_is_idempotent(self, conn: sqlite3.Connection) -> None:
        conn.execute(
            "INSERT INTO pr_outcomes (provider, repo_slug, pr_number, "
            " state, raw_json) VALUES ('github', 'octocat/hello', 1, 'open', '{}')"
        )
        before_count = conn.execute(
            "SELECT COUNT(*) FROM pr_outcomes"
        ).fetchone()[0]
        before_ver = conn.execute("PRAGMA user_version").fetchone()[0]

        schema.apply(conn)  # second apply must be a no-op
        assert conn.execute("PRAGMA user_version").fetchone()[0] == before_ver
        assert conn.execute(
            "SELECT COUNT(*) FROM pr_outcomes"
        ).fetchone()[0] == before_count

    def test_additive_does_not_disturb_existing_tables(self, conn: sqlite3.Connection) -> None:
        names = {
            r["name"]
            for r in conn.execute(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
            ).fetchall()
        }
        for table in (
            "projects", "sessions", "messages", "usage_events",
            "session_mart", "discovery_telemetry", "captured_events",
            "discovery_embeddings", "mode_recommendations",
        ):
            assert table in names, f"{table} missing after v017"

    def test_recovers_from_partial_apply(self, tmp_path: Path) -> None:
        """Operator hand-creates the tables without bumping user_version
        — the next ``schema.apply`` must not choke on the existing tables
        and must finish at CURRENT_VERSION.
        """
        c = db.connect(tmp_path / "store.db")
        try:
            schema.apply(c)
            c.execute("PRAGMA user_version = 16")
            c.execute("DROP TABLE pr_outcomes")
            c.execute("DROP TABLE ci_runs")
            # Re-create by hand to simulate the partial state.
            c.execute("""
                CREATE TABLE pr_outcomes (
                    id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
                    repo_slug TEXT NOT NULL, pr_number INTEGER NOT NULL,
                    title TEXT, state TEXT NOT NULL,
                    merged_at TEXT, reverted_at TEXT, author TEXT,
                    raw_json TEXT NOT NULL,
                    UNIQUE (provider, repo_slug, pr_number)
                )
            """)
            c.execute("""
                CREATE TABLE ci_runs (
                    id INTEGER PRIMARY KEY, provider TEXT NOT NULL,
                    repo_slug TEXT NOT NULL, run_id TEXT NOT NULL,
                    commit_sha TEXT NOT NULL, status TEXT NOT NULL,
                    workflow_name TEXT, started_ts TEXT, completed_ts TEXT,
                    raw_json TEXT NOT NULL,
                    UNIQUE (provider, run_id)
                )
            """)
            assert c.execute("PRAGMA user_version").fetchone()[0] == 16
            schema.apply(c)
            assert c.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        finally:
            c.close()
