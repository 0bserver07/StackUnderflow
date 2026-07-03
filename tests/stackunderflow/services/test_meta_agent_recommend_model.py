"""Tests for the ``recommend_model_for_task`` meta-agent tool (spec 26 §6.3)."""

from __future__ import annotations

from stackunderflow.services.meta_agent import TOOL_CATALOG, execute_tool, tool_names
from tests.stackunderflow.reports.test_benchmark import (
    _make_conn,
    _seed_project,
    _seed_winner_fixture,
)


class TestCatalog:
    def test_tool_in_catalog_with_intent_required(self):
        assert "recommend_model_for_task" in tool_names()
        spec = next(
            t for t in TOOL_CATALOG
            if t["function"]["name"] == "recommend_model_for_task"
        )
        assert "intent" in spec["function"]["parameters"]["required"]
        # ops is part of the canonical 6-label enum.
        assert "ops" in spec["function"]["parameters"]["properties"]["intent"]["enum"]


class TestExecutor:
    def test_recommends_winner_from_history(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn)
        _seed_winner_fixture(conn, pid)
        result = execute_tool(conn, "recommend_model_for_task", {"intent": "fix", "size": "small"})
        assert result.ok
        assert result.data["recommended_model"] == "sonnet"
        assert result.data["basis"] == "stratum"

    def test_empty_store_is_valid_insufficient_evidence(self, tmp_path):
        conn = _make_conn(tmp_path)
        result = execute_tool(conn, "recommend_model_for_task", {"intent": "fix"})
        assert result.ok  # a well-formed "no opinion" is still a valid result
        assert result.data["basis"] == "insufficient_evidence"
        assert result.data["recommended_model"] is None

    def test_missing_intent_is_an_error(self, tmp_path):
        conn = _make_conn(tmp_path)
        result = execute_tool(conn, "recommend_model_for_task", {})
        assert result.ok is False
        assert "error" in result.data

    def test_bad_intent_is_an_error(self, tmp_path):
        conn = _make_conn(tmp_path)
        result = execute_tool(conn, "recommend_model_for_task", {"intent": "bogus"})
        assert result.ok is False
        assert "error" in result.data
