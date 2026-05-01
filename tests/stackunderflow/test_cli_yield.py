"""End-to-end CLI tests for ``stackunderflow yield``."""

from __future__ import annotations

import json
from unittest.mock import MagicMock, patch

from click.testing import CliRunner

from stackunderflow.cli import cli
from stackunderflow.services.yield_tracker import YieldEntry


def _fake_entries() -> list[YieldEntry]:
    return [
        YieldEntry(
            session_id="sess-1",
            project_slug="alpha",
            cwd="/repo/alpha",
            started_at="2026-04-01T10:00:00+00:00",
            cost_usd=4.50,
            classification="productive",
            follow_commit_sha="aaaaaaa1",
            follow_commit_msg="feat: ship",
            follow_commit_age_hours=2.0,
        ),
        YieldEntry(
            session_id="sess-2",
            project_slug="alpha",
            cwd="/repo/alpha",
            started_at="2026-04-02T10:00:00+00:00",
            cost_usd=1.00,
            classification="reverted",
            follow_commit_sha="bbbbbbb2",
            follow_commit_msg='Revert "feat: thing"',
            follow_commit_age_hours=4.0,
        ),
        YieldEntry(
            session_id="sess-3",
            project_slug="beta",
            cwd="/repo/beta",
            started_at="2026-04-03T10:00:00+00:00",
            cost_usd=0.25,
            classification="abandoned",
        ),
        YieldEntry(
            session_id="sess-4",
            project_slug="gamma",
            cwd="",
            started_at="2026-04-04T10:00:00+00:00",
            cost_usd=0.10,
            classification="no_repo",
        ),
    ]


def test_yield_text_format_default_month():
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=MagicMock()), \
         patch(
             "stackunderflow.services.yield_tracker.compute_yield",
             return_value=_fake_entries(),
         ):
        result = runner.invoke(cli, ["yield"])
    assert result.exit_code == 0, result.output
    # Summary line — counts plus formatted costs.
    assert "productive:" in result.output
    assert "reverted:" in result.output
    assert "abandoned:" in result.output
    assert "no_repo:" in result.output
    # Sorted by cost desc — ``sess-1`` ($4.50) shows up before ``sess-2`` ($1.00).
    pos1 = result.output.find("sess-1")
    pos2 = result.output.find("sess-2")
    assert 0 < pos1 < pos2
    # Heuristic warning is always rendered.
    assert "correlated by time" in result.output


def test_yield_json_format_carries_summary_and_entries():
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=MagicMock()), \
         patch(
             "stackunderflow.services.yield_tracker.compute_yield",
             return_value=_fake_entries(),
         ):
        result = runner.invoke(cli, ["yield", "--format", "json"])
    assert result.exit_code == 0, result.output
    body = json.loads(result.output)
    assert body["period"] == "month"
    assert body["summary"]["productive"] == 1
    assert body["summary"]["reverted"] == 1
    assert body["summary"]["abandoned"] == 1
    assert body["summary"]["no_repo"] == 1
    # Entries are sorted by cost desc.
    assert [e["session_id"] for e in body["entries"]] == [
        "sess-1", "sess-2", "sess-3", "sess-4",
    ]


def test_yield_period_passed_through_to_service():
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=MagicMock()), \
         patch(
             "stackunderflow.services.yield_tracker.compute_yield",
             return_value=[],
         ) as mock_compute:
        result = runner.invoke(cli, ["yield", "-p", "week"])
    assert result.exit_code == 0
    _args, kwargs = mock_compute.call_args
    assert kwargs.get("period") == "week"


def test_yield_project_filter_passed_through():
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=MagicMock()), \
         patch(
             "stackunderflow.services.yield_tracker.compute_yield",
             return_value=[],
         ) as mock_compute:
        result = runner.invoke(cli, ["yield", "--project", "alpha", "--project", "beta"])
    assert result.exit_code == 0
    _args, kwargs = mock_compute.call_args
    assert kwargs.get("project_filter") == ["alpha", "beta"]


def test_yield_empty_message_when_no_sessions():
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=MagicMock()), \
         patch(
             "stackunderflow.services.yield_tracker.compute_yield",
             return_value=[],
         ):
        result = runner.invoke(cli, ["yield"])
    assert result.exit_code == 0
    assert "No sessions" in result.output


def test_yield_invalid_period_rejected():
    runner = CliRunner()
    result = runner.invoke(cli, ["yield", "-p", "bogus"])
    assert result.exit_code != 0
