"""Tests for ``stackunderflow ingest *`` — Spec 20 CLI surface.

Covers:

* ``stackunderflow ingest github --repo OWNER/REPO`` happy path, with
  the HTTP layer mocked via the service's ``client_factory`` hook (so
  we never make a real network call).
* Token resolution: ``--token`` flag wins, then ``$STACKUNDERFLOW_GITHUB_TOKEN``,
  then ``$GITHUB_TOKEN``.
* ``stackunderflow ingest webhook --help`` lists the ``serve`` subcommand
  — we don't actually boot uvicorn in a unit test.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.services import github_ingest
from stackunderflow.store import db, schema


@pytest.fixture()
def isolated_store(tmp_path: Path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_db)
    return store_db


def _mock_backfill(monkeypatch, captured: dict | None = None):
    """Replace ``backfill_repo`` with a recorder."""
    if captured is None:
        captured = {}

    def _fake(conn, repo_slug, **kwargs):
        captured.update({"repo_slug": repo_slug, **kwargs})
        return github_ingest.BackfillReport(
            repo_slug=repo_slug,
            pr_inserted=2,
            pr_updated=1,
            pr_pages_fetched=1,
            ci_inserted=3,
            ci_updated=0,
            ci_pages_fetched=1,
            duration_seconds=0.123,
        )

    monkeypatch.setattr(github_ingest, "backfill_repo", _fake)
    return captured


def test_ingest_github_invokes_backfill(isolated_store, monkeypatch) -> None:
    captured = _mock_backfill(monkeypatch)
    runner = CliRunner()
    result = runner.invoke(
        cli,
        ["ingest", "github", "--repo", "octo/widgets", "--token", "xyz"],
    )
    assert result.exit_code == 0, result.output
    assert captured["repo_slug"] == "octo/widgets"
    assert captured["token"] == "xyz"
    assert "Backfill complete for octo/widgets" in result.output
    assert "inserted=2" in result.output
    assert "inserted=3" in result.output


def test_ingest_github_json_format(isolated_store, monkeypatch) -> None:
    _mock_backfill(monkeypatch)
    runner = CliRunner()
    result = runner.invoke(
        cli,
        ["ingest", "github", "--repo", "octo/widgets",
         "--token", "xyz", "--format", "json"],
    )
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output.strip())
    assert payload["repo_slug"] == "octo/widgets"
    assert payload["pr_inserted"] == 2
    assert payload["ci_inserted"] == 3


def test_ingest_github_token_from_env(isolated_store, monkeypatch) -> None:
    captured = _mock_backfill(monkeypatch)
    monkeypatch.setenv("STACKUNDERFLOW_GITHUB_TOKEN", "env-token")
    monkeypatch.delenv("GITHUB_TOKEN", raising=False)
    runner = CliRunner()
    result = runner.invoke(cli, ["ingest", "github", "--repo", "octo/widgets"])
    assert result.exit_code == 0, result.output
    assert captured["token"] == "env-token"


def test_ingest_github_token_flag_wins(isolated_store, monkeypatch) -> None:
    captured = _mock_backfill(monkeypatch)
    monkeypatch.setenv("STACKUNDERFLOW_GITHUB_TOKEN", "env-token")
    runner = CliRunner()
    result = runner.invoke(
        cli, ["ingest", "github", "--repo", "octo/widgets", "--token", "flag-token"]
    )
    assert result.exit_code == 0, result.output
    assert captured["token"] == "flag-token"


def test_ingest_github_no_token_warning(isolated_store, monkeypatch) -> None:
    _mock_backfill(monkeypatch)
    monkeypatch.delenv("STACKUNDERFLOW_GITHUB_TOKEN", raising=False)
    monkeypatch.delenv("GITHUB_TOKEN", raising=False)
    runner = CliRunner()
    result = runner.invoke(cli, ["ingest", "github", "--repo", "octo/widgets"])
    assert result.exit_code == 0, result.output
    assert "no GitHub token provided" in result.output


def test_ingest_github_no_ci_flag_propagates(isolated_store, monkeypatch) -> None:
    captured = _mock_backfill(monkeypatch)
    runner = CliRunner()
    result = runner.invoke(
        cli,
        ["ingest", "github", "--repo", "octo/widgets", "--token", "xyz", "--no-ci"],
    )
    assert result.exit_code == 0, result.output
    assert captured["include_ci"] is False


def test_ingest_github_state_filter_propagates(isolated_store, monkeypatch) -> None:
    captured = _mock_backfill(monkeypatch)
    runner = CliRunner()
    result = runner.invoke(
        cli,
        ["ingest", "github", "--repo", "octo/widgets",
         "--token", "xyz", "--state", "open"],
    )
    assert result.exit_code == 0, result.output
    assert captured["state"] == "open"


def test_ingest_github_rate_limit_raises_click_exception(isolated_store, monkeypatch) -> None:
    def _raise(*args, **kwargs):
        raise github_ingest.RateLimitedError("limit exhausted")

    monkeypatch.setattr(github_ingest, "backfill_repo", _raise)
    runner = CliRunner()
    result = runner.invoke(
        cli, ["ingest", "github", "--repo", "octo/widgets", "--token", "xyz"]
    )
    assert result.exit_code != 0
    assert "limit exhausted" in result.output


def test_ingest_webhook_serve_help_lists_subcommand() -> None:
    runner = CliRunner()
    result = runner.invoke(cli, ["ingest", "webhook", "--help"])
    assert result.exit_code == 0
    assert "serve" in result.output


def test_ingest_help_lists_github_and_webhook() -> None:
    runner = CliRunner()
    result = runner.invoke(cli, ["ingest", "--help"])
    assert result.exit_code == 0
    assert "github" in result.output
    assert "webhook" in result.output
