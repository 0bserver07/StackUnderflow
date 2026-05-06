"""Tests for ``stackunderflow etl status`` — Wave 4C CLI subcommand.

Locks the text + json render contracts and confirms the command works
without a server (the assembler is the canonical data source for both
CLI and HTTP, so as long as the SQL passes, the output is consistent).
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema


# ── helpers ─────────────────────────────────────────────────────────────────


def _seed_empty(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()


def _seed_with_events(store_db: Path, n_events: int = 3) -> int:
    """Seed a store with *n_events* events and return the max event id."""
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', '-a', '-a', 0.0, 0.0)"
    )
    pid = int(cur.lastrowid)
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, 's1', '2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z', 1)",
        (pid,),
    )
    sfk = int(cur.lastrowid)
    last_eid = 0
    for i in range(n_events):
        # v008: ``messages`` is a UNION-ALL view; INSERT routes through
        # an INSTEAD OF trigger that allocates ids from
        # ``_messages_id_seq``. ``cur.lastrowid`` does not propagate
        # the trigger's nested INSERT id, so we query the sequence
        # directly. ``next_id - 1`` is the id the trigger just assigned.
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain) "
            "VALUES (?, ?, '2026-04-01T00:00:00Z', 'assistant', 'claude-sonnet-4-5',"
            " 0, 0, 0, 0, '', '[]', '{}', 0)",
            (sfk, i),
        )
        mid = int(conn.execute(
            "SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()[0])
        cur = conn.execute(
            "INSERT INTO usage_events "
            "(source_message_fk, provider, account, project_id, session_id, ts, day, "
            " model, speed, input_tokens, output_tokens, cache_read_tokens, "
            " cache_create_tokens, cost_usd, cost_source, role, raw_extras) "
            "VALUES (?, 'claude', 'default', ?, 's1', '2026-04-01T00:00:00Z', "
            "'2026-04-01', 'claude-sonnet-4-5', 'standard', 0, 0, 0, 0, 0.0, "
            "'rate_card', 'assistant', NULL)",
            (mid, pid),
        )
        last_eid = int(cur.lastrowid)
    # All five marts caught up at the max event id.
    for name in ("daily", "session", "project", "provider_day", "model_day"):
        conn.execute(
            "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts) "
            "VALUES (?, ?, '2026-05-01T00:00:00Z') "
            "ON CONFLICT(mart_name) DO UPDATE SET last_event_id = excluded.last_event_id",
            (name, last_eid),
        )
    conn.commit()
    conn.close()
    return last_eid


def _invoke(runner, args, store_db, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    monkeypatch.setattr(deps, "watcher_handle", None, raising=False)
    return runner.invoke(cli, args)


# ── text format ──────────────────────────────────────────────────────────────


class TestTextFormat:
    def test_empty_store_renders_live(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        runner = CliRunner()
        r = _invoke(runner, ["etl", "status"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "ETL pipeline" in r.output
        assert "live" in r.output
        # Every mart name appears, even on an empty store.
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            assert name in r.output
        # Watcher state visible.
        assert "Watcher" in r.output

    def test_populated_store_renders_event_counts(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_events(store_db, n_events=3)
        runner = CliRunner()
        r = _invoke(runner, ["etl", "status"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "3 total" in r.output
        assert "claude=3" in r.output
        assert "rate_card=3" in r.output

    def test_default_format_is_text(self, tmp_path, monkeypatch):
        """No --format flag should produce the human-readable output, not JSON."""
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        runner = CliRunner()
        r = _invoke(runner, ["etl", "status"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        # Text output starts with the header phrase, not a `{`.
        assert not r.output.lstrip().startswith("{")
        assert "ETL pipeline" in r.output


# ── json format ──────────────────────────────────────────────────────────────


class TestJsonFormat:
    def test_json_shape_matches_route(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_with_events(store_db, n_events=2)
        runner = CliRunner()
        r = _invoke(runner, ["etl", "status", "--format", "json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert set(body.keys()) == {
            "watcher", "marts", "events", "lag_seconds", "health", "current_job",
        }
        assert body["events"]["total"] == 2
        assert body["events"]["by_provider"] == {"claude": 2}
        assert set(body["marts"].keys()) == {
            "daily", "session", "project", "provider_day", "model_day",
        }
        assert body["health"] == "live"
        # Watcher graceful degrade — CLI never has a live handle.
        assert body["watcher"]["running"] == "unknown"

    def test_json_empty_store_is_valid(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        runner = CliRunner()
        r = _invoke(runner, ["etl", "status", "--format", "json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["events"]["total"] == 0
        assert body["events"]["by_provider"] == {}
        assert body["health"] == "live"


# ── degradation ──────────────────────────────────────────────────────────────


class TestGracefulDegrade:
    def test_works_without_server_running(self, tmp_path, monkeypatch):
        """The CLI never brings up the FastAPI lifespan, so deps.watcher_handle
        is always None. The command must still succeed and report
        ``running='unknown'`` rather than raising.
        """
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        runner = CliRunner()
        # Confirm the handle stays None throughout the run.
        monkeypatch.setattr(deps, "watcher_handle", None, raising=False)
        r = _invoke(runner, ["etl", "status", "--format", "json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["watcher"]["running"] == "unknown"
        assert body["watcher"]["enabled"] is True

    def test_invalid_format_rejected(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        runner = CliRunner()
        r = _invoke(runner, ["etl", "status", "--format", "yaml"], store_db, monkeypatch)
        assert r.exit_code != 0
