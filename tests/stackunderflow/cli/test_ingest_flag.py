"""Tests for the ``--ingest`` / ``--auto-ingest`` flag wired onto every
read-only data command (``status``, ``today``, ``month``, ``report``,
``compare``, ``yield``, ``optimize``, ``export``).

The fresh-data path is verified in two layers:

* :mod:`stackunderflow.cli_helpers.ingest` — pure helper, exercised
  directly so we can assert exactly when the ingest pass runs.
* The CLI dispatch — invoked through :class:`click.testing.CliRunner`
  with the underlying ``run_ingest`` + ``backfill`` mocked, so we can
  assert the helper was called with the right ``force``/``auto`` shape
  for each command without paying for an actual ingest run.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from unittest.mock import MagicMock, patch

import pytest
from click.testing import CliRunner

from stackunderflow.cli import cli
from stackunderflow.cli_helpers.ingest import (
    STALENESS_THRESHOLD_HOURS,
    ensure_fresh,
    is_stale,
)

# ── direct unit tests for ensure_fresh / is_stale ────────────────────────────


class _RowMock:
    """Behaves like an ``sqlite3.Row`` for the columns we read."""

    def __init__(self, max_ts):
        self._max_ts = max_ts

    def __getitem__(self, key):
        if key in ("max_ts", 0):
            return self._max_ts
        raise IndexError(key)


def _conn_with_max_ts(max_ts):
    """A MagicMock conn whose ``execute(...).fetchone()`` returns _RowMock(max_ts)."""
    conn = MagicMock(name="conn")
    cur = MagicMock(name="cursor")
    cur.fetchone.return_value = _RowMock(max_ts)
    conn.execute.return_value = cur
    return conn


class TestIsStale:
    def test_empty_store_is_not_stale(self):
        # An empty store has nothing to threshold against. The deliberate
        # choice is *not* to auto-trigger a real adapter walk on the first
        # ``status`` after install — the user can still force the pass
        # with ``--ingest``.
        conn = _conn_with_max_ts(None)
        assert is_stale(conn) is False

    def test_recent_event_is_fresh(self):
        # 1 hour old → well within the 6h threshold.
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=1)).isoformat()
        conn = _conn_with_max_ts(ts)
        assert is_stale(conn, now=now) is False

    def test_old_event_is_stale(self):
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=STALENESS_THRESHOLD_HOURS + 1)).isoformat()
        conn = _conn_with_max_ts(ts)
        assert is_stale(conn, now=now) is True

    def test_unparseable_ts_is_treated_fresh(self):
        # A corrupt timestamp must never trigger a refresh loop — we'd
        # rather under-trigger than thrash the store.
        conn = _conn_with_max_ts("not-a-timestamp")
        assert is_stale(conn) is False

    def test_naive_timestamp_handled(self):
        # Some sqlite stores have naive ISO strings; the helper should
        # not blow up on a TZ-aware/naive comparison.
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=1)).replace(tzinfo=None).isoformat()
        conn = _conn_with_max_ts(ts)
        assert is_stale(conn, now=now) is False


class TestEnsureFresh:
    """``ensure_fresh`` is the public surface used by the CLI."""

    def test_force_runs_ingest_unconditionally(self):
        # Even a fresh store should ingest when ``force=True``.
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=1)).isoformat()
        conn = _conn_with_max_ts(ts)
        with patch(
            "stackunderflow.cli_helpers.ingest._run_ingest_pass",
        ) as mock_run:
            ran = ensure_fresh(conn, force=True, auto=True)
        assert ran is True
        mock_run.assert_called_once_with(conn)

    def test_stale_store_with_auto_runs_ingest(self):
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=STALENESS_THRESHOLD_HOURS + 0.5)).isoformat()
        conn = _conn_with_max_ts(ts)
        with patch(
            "stackunderflow.cli_helpers.ingest._run_ingest_pass",
        ) as mock_run:
            ran = ensure_fresh(conn, force=False, auto=True)
        assert ran is True
        mock_run.assert_called_once_with(conn)

    def test_fresh_store_with_auto_skips_ingest(self):
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=1)).isoformat()
        conn = _conn_with_max_ts(ts)
        with patch(
            "stackunderflow.cli_helpers.ingest._run_ingest_pass",
        ) as mock_run:
            ran = ensure_fresh(conn, force=False, auto=True)
        assert ran is False
        mock_run.assert_not_called()

    def test_no_auto_ingest_never_runs_on_staleness(self):
        # Even an ancient store stays untouched when auto=False and
        # force=False — that's the ``--no-auto-ingest`` contract.
        now = datetime.now(UTC)
        ts = (now - timedelta(days=30)).isoformat()
        conn = _conn_with_max_ts(ts)
        with patch(
            "stackunderflow.cli_helpers.ingest._run_ingest_pass",
        ) as mock_run:
            ran = ensure_fresh(conn, force=False, auto=False)
        assert ran is False
        mock_run.assert_not_called()

    def test_no_auto_ingest_still_obeys_force(self):
        # ``--no-auto-ingest --ingest`` is unusual but valid: the user
        # explicitly asks for one pass, no automatic future passes.
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=1)).isoformat()
        conn = _conn_with_max_ts(ts)
        with patch(
            "stackunderflow.cli_helpers.ingest._run_ingest_pass",
        ) as mock_run:
            ran = ensure_fresh(conn, force=True, auto=False)
        assert ran is True
        mock_run.assert_called_once_with(conn)


# ── CLI-level tests — each command receives the flag ─────────────────────────


def _fake_report():
    return {
        "scope_label": "today",
        "total_cost": 0.0,
        "total_messages": 0,
        "total_sessions": 0,
        "by_project": [],
    }


# Commands that build their report via ``build_report`` directly. We
# stub the heavy machinery and only assert on the helper call.
_REPORT_COMMANDS = (
    ("report", ["report"]),
    ("today", ["today"]),
    ("month", ["month"]),
    ("status", ["status"]),
)


@pytest.mark.parametrize(("name", "argv"), _REPORT_COMMANDS)
class TestReportFamilyFlagWiring:
    """All four ``build_report``-backed commands share the helper path."""

    def test_no_ingest_flag_skips_helper_run_on_fresh_store(self, name, argv):
        # MagicMock store + a fresh max(ts) → the staleness check
        # short-circuits and the ingest pass never runs.
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=1)).isoformat()
        conn = _conn_with_max_ts(ts)
        runner = CliRunner()
        with patch("stackunderflow.cli._open_store", return_value=conn), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()), \
             patch(
                 "stackunderflow.cli_helpers.ingest._run_ingest_pass",
             ) as mock_run:
            r = runner.invoke(cli, argv)
        assert r.exit_code == 0, r.output
        mock_run.assert_not_called()

    def test_explicit_ingest_flag_forces_run(self, name, argv):
        conn = _conn_with_max_ts(None)
        runner = CliRunner()
        with patch("stackunderflow.cli._open_store", return_value=conn), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()), \
             patch(
                 "stackunderflow.cli_helpers.ingest._run_ingest_pass",
             ) as mock_run:
            r = runner.invoke(cli, [*argv, "--ingest"])
        assert r.exit_code == 0, r.output
        mock_run.assert_called_once_with(conn)

    def test_no_auto_ingest_skips_on_stale_store(self, name, argv):
        # Old timestamp → would be stale, but ``--no-auto-ingest`` wins.
        now = datetime.now(UTC)
        ts = (now - timedelta(days=30)).isoformat()
        conn = _conn_with_max_ts(ts)
        runner = CliRunner()
        with patch("stackunderflow.cli._open_store", return_value=conn), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()), \
             patch(
                 "stackunderflow.cli_helpers.ingest._run_ingest_pass",
             ) as mock_run:
            r = runner.invoke(cli, [*argv, "--no-auto-ingest"])
        assert r.exit_code == 0, r.output
        mock_run.assert_not_called()

    def test_auto_ingest_on_stale_store_runs(self, name, argv):
        # Old enough timestamp that auto-ingest kicks in. Notice goes
        # to stderr by default (``click.echo(..., err=True)``).
        now = datetime.now(UTC)
        ts = (now - timedelta(hours=STALENESS_THRESHOLD_HOURS + 1)).isoformat()
        conn = _conn_with_max_ts(ts)
        runner = CliRunner()
        with patch("stackunderflow.cli._open_store", return_value=conn), \
             patch("stackunderflow.cli.build_report", return_value=_fake_report()), \
             patch(
                 "stackunderflow.cli_helpers.ingest._run_ingest_pass",
             ) as mock_run:
            r = runner.invoke(cli, argv)
        assert r.exit_code == 0, r.output
        mock_run.assert_called_once_with(conn)
        # The notice is echoed via ``click.echo(..., err=True)``. In
        # Click 8.3, CliRunner captures stderr separately on ``.stderr``.
        assert "stale data" in r.stderr


# ── compare / yield / optimize / export — wired to the same helper ───────────


def test_compare_flag_wires_through(monkeypatch):
    conn = _conn_with_max_ts(None)
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=conn), \
         patch(
             "stackunderflow.services.compare.build_compare_payload",
             return_value={"period": "month", "models": []},
         ), \
         patch(
             "stackunderflow.cli_helpers.ingest._run_ingest_pass",
         ) as mock_run:
        r = runner.invoke(cli, ["compare", "--ingest"])
    assert r.exit_code == 0, r.output
    mock_run.assert_called_once_with(conn)


def test_yield_flag_wires_through():
    conn = _conn_with_max_ts(None)
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=conn), \
         patch(
             "stackunderflow.services.yield_tracker.compute_yield",
             return_value=[],
         ), \
         patch(
             "stackunderflow.cli_helpers.ingest._run_ingest_pass",
         ) as mock_run:
        r = runner.invoke(cli, ["yield", "--ingest"])
    assert r.exit_code == 0, r.output
    mock_run.assert_called_once_with(conn)


def test_optimize_flag_wires_through():
    conn = _conn_with_max_ts(None)
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=conn), \
         patch("stackunderflow.cli.find_waste", return_value=[]), \
         patch("stackunderflow.cli.find_patterns", return_value=[]), \
         patch(
             "stackunderflow.cli_helpers.ingest._run_ingest_pass",
         ) as mock_run:
        r = runner.invoke(cli, ["optimize", "--ingest"])
    assert r.exit_code == 0, r.output
    mock_run.assert_called_once_with(conn)


def test_export_flag_wires_through(tmp_path):
    conn = _conn_with_max_ts(None)
    out_path = tmp_path / "out.json"
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=conn), \
         patch(
             "stackunderflow.cli.run_export",
             return_value=("{}", "application/json", "x.json"),
         ), \
         patch("stackunderflow.cli.safe_write_text"), \
         patch(
             "stackunderflow.cli_helpers.ingest._run_ingest_pass",
         ) as mock_run:
        r = runner.invoke(cli, [
            "export", "-f", "json", "-o", str(out_path), "--ingest",
        ])
    assert r.exit_code == 0, r.output
    mock_run.assert_called_once_with(conn)


def test_export_no_auto_ingest_skips_even_on_stale_store(tmp_path):
    now = datetime.now(UTC)
    ts = (now - timedelta(days=30)).isoformat()
    conn = _conn_with_max_ts(ts)
    out_path = tmp_path / "out.json"
    runner = CliRunner()
    with patch("stackunderflow.cli._open_store", return_value=conn), \
         patch(
             "stackunderflow.cli.run_export",
             return_value=("{}", "application/json", "x.json"),
         ), \
         patch("stackunderflow.cli.safe_write_text"), \
         patch(
             "stackunderflow.cli_helpers.ingest._run_ingest_pass",
         ) as mock_run:
        r = runner.invoke(cli, [
            "export", "-f", "json", "-o", str(out_path), "--no-auto-ingest",
        ])
    assert r.exit_code == 0, r.output
    mock_run.assert_not_called()
