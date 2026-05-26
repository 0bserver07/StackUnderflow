"""Unit tests for LLM-graded session quality grading service, routes, and CLI."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from click.testing import CliRunner
from fastapi.testclient import TestClient

from stackunderflow import deps
from stackunderflow.cli import cli
from stackunderflow.routes.quality import router
from stackunderflow.server import app
from stackunderflow.services.grading import get_stored_grade, grade_session
from stackunderflow.store import db, schema


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def test_grade_session_caches_in_sqlite(conn: sqlite3.Connection) -> None:
    # 1. Insert dummy session
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'widget-repo', 'Widgets', 1.0, 1.0)"
    )
    conn.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (1, 1, 'sess_xyz', '2026-05-01T12:00:00Z', '2026-05-01T13:00:00Z', 1)"
    )
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, content_text, raw_json) "
        "VALUES (1, 0, '2026-05-01T12:05:00Z', 'user', 'implement something', '{}')"
    )
    conn.commit()

    # Setup mocked response for Ollama tags and chat
    mock_tags_resp = MagicMock()
    mock_tags_resp.status_code = 200
    mock_tags_resp.json.return_value = {
        "models": [{"name": "qwen2.5-coder:7b"}]
    }

    mock_chat_content = {
        "overall_score": 8.5,
        "grades": {
            "goal_clarity": 9.0,
            "execution_efficiency": 8.0,
            "success": 8.5,
        },
        "rationale": "High quality session.",
        "suggestions": ["Add some comments"],
    }
    mock_chat_resp = MagicMock()
    mock_chat_resp.status_code = 200
    mock_chat_resp.json.return_value = {
        "message": {"content": json.dumps(mock_chat_content)}
    }

    # Patch httpx requests
    with patch("httpx.get", return_value=mock_tags_resp) as mock_get, \
         patch("httpx.post", return_value=mock_chat_resp) as mock_post:
        
        grade = grade_session(conn, "sess_xyz")

        assert grade["overall_score"] == 8.5
        assert grade["grades"]["goal_clarity"] == 9.0
        assert grade["rationale"] == "High quality session."
        assert grade["suggestions"] == ["Add some comments"]

        mock_get.assert_called_once()
        mock_post.assert_called_once()

    # Now verify it's persisted in DB
    db_grade = get_stored_grade(conn, "sess_xyz")
    assert db_grade is not None
    assert db_grade["overall_score"] == 8.5
    assert db_grade["rationale"] == "High quality session."

    # Verify that calling grade_session again retrieves it from DB without calling httpx!
    with patch("httpx.get") as mock_get_2, patch("httpx.post") as mock_post_2:
        cached_grade = grade_session(conn, "sess_xyz")
        assert cached_grade["overall_score"] == 8.5
        mock_get_2.assert_not_called()
        mock_post_2.assert_not_called()

    # Verify that force=True re-triggers the LLM query
    with patch("httpx.get", return_value=mock_tags_resp) as mock_get_3, \
         patch("httpx.post", return_value=mock_chat_resp) as mock_post_3:
        
        forced_grade = grade_session(conn, "sess_xyz", force=True)
        assert forced_grade["overall_score"] == 8.5
        mock_get_3.assert_called_once()
        mock_post_3.assert_called_once()


def test_quality_endpoints(tmp_path: Path, monkeypatch) -> None:
    store_db = tmp_path / "store.db"
    c = db.connect(store_db)
    schema.apply(c)
    # Seed
    c.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'widget-repo', 'Widgets', 1.0, 1.0)"
    )
    c.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (1, 1, 'sess_xyz', '2026-05-01T12:00:00Z', '2026-05-01T13:00:00Z', 1)"
    )
    c.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, content_text, raw_json) "
        "VALUES (1, 0, '2026-05-01T12:05:00Z', 'user', 'implement something', '{}')"
    )
    c.commit()
    c.close()

    monkeypatch.setattr(deps, "store_path", store_db)
    client = TestClient(app)

    # Mock Ollama HTTP calls
    mock_tags_resp = MagicMock(status_code=200)
    mock_tags_resp.json.return_value = {"models": [{"name": "qwen2.5-coder:7b"}]}
    mock_chat_resp = MagicMock(status_code=200)
    mock_chat_resp.json.return_value = {
        "message": {"content": json.dumps({
            "overall_score": 7.0,
            "grades": {"goal_clarity": 7.0, "execution_efficiency": 7.0, "success": 7.0},
            "rationale": "Good.",
            "suggestions": ["Comment code"]
        })}
    }

    with patch("httpx.get", return_value=mock_tags_resp), \
         patch("httpx.post", return_value=mock_chat_resp):
        
        # Test lazy grading GET endpoint
        r = client.get("/api/static-analysis/session/sess_xyz/quality")
        assert r.status_code == 200, r.text
        data = r.json()
        assert data["overall_score"] == 7.0
        assert data["rationale"] == "Good."

        # Test POST grade endpoint (forces re-grading)
        r2 = client.post("/api/static-analysis/session/sess_xyz/grade")
        assert r2.status_code == 200, r2.text
        assert r2.json()["overall_score"] == 7.0

        # Test 404
        r_404 = client.get("/api/static-analysis/session/non_existent_id/quality")
        assert r_404.status_code == 404


def test_cli_analyze_quality(tmp_path: Path, monkeypatch) -> None:
    store_db = tmp_path / "store.db"
    c = db.connect(store_db)
    schema.apply(c)
    # Seed
    c.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'widget-repo', 'Widgets', 1.0, 1.0)"
    )
    c.execute(
        "INSERT INTO sessions (id, project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (1, 1, 'sess_xyz', '2026-05-01T12:00:00Z', '2026-05-01T13:00:00Z', 1)"
    )
    c.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, content_text, raw_json) "
        "VALUES (1, 0, '2026-05-01T12:05:00Z', 'user', 'implement something', '{}')"
    )
    c.commit()
    c.close()

    monkeypatch.setattr(deps, "store_path", store_db)

    # Mock Ollama HTTP calls
    mock_tags_resp = MagicMock(status_code=200)
    mock_tags_resp.json.return_value = {"models": [{"name": "qwen2.5-coder:7b"}]}
    mock_chat_resp = MagicMock(status_code=200)
    mock_chat_resp.json.return_value = {
        "message": {"content": json.dumps({
            "overall_score": 9.2,
            "grades": {"goal_clarity": 9.5, "execution_efficiency": 9.0, "success": 9.2},
            "rationale": "Outstanding effort.",
            "suggestions": ["Excellent job"]
        })}
    }

    with patch("httpx.get", return_value=mock_tags_resp), \
         patch("httpx.post", return_value=mock_chat_resp):
        
        runner = CliRunner()
        # Test CLI text format
        result = runner.invoke(cli, ["analyze", "quality", "sess_xyz"])
        assert result.exit_code == 0, result.output
        assert "Overall Score: 9.2/10.0" in result.output
        assert "Outstanding effort." in result.output

        # Test CLI JSON format
        result_json = runner.invoke(cli, ["analyze", "quality", "sess_xyz", "--format", "json"])
        assert result_json.exit_code == 0, result_json.output
        data = json.loads(result_json.output)
        assert data["overall_score"] == 9.2
        assert data["rationale"] == "Outstanding effort."

        # Test CLI backfiller (--all)
        # Clear database session_quality_metrics to make it ungraded again
        c = db.connect(store_db)
        c.execute("DELETE FROM session_quality_metrics")
        c.commit()
        c.close()

        result_all = runner.invoke(cli, ["analyze", "quality", "--all"])
        assert result_all.exit_code == 0, result_all.output
        assert "Graded session sess_xyz: score=9.2" in result_all.output
