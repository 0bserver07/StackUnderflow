"""CLI tests for the ``stackunderflow discovery`` subgroup.

Two subcommands:

* ``discovery telemetry`` — introspect the citation-feedback table.
* ``discovery demote-uncited`` — the periodic "demote sessions nobody
  ever cites" sweep (``--dry-run`` reports without flagging).

Pattern mirrors ``test_discovery_cli.py``: monkeypatch ``deps.store_path``
to a tmp store, seed a tiny fixture, run via ``CliRunner``.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema


def _iso_days_ago(n: float) -> str:
    return (datetime.now(UTC) - timedelta(days=n)).isoformat()


def _seed(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    # A surfaced-and-cited session, a surfaced-uncited-but-fresh session,
    # and a surfaced-a-lot-old-uncited session (the demote candidate).
    conn.execute(
        "INSERT INTO discovery_telemetry "
        "(command, session_id, loaded_count, cited_count, first_loaded_ts, "
        " last_loaded_ts, last_cited_ts, demoted) VALUES "
        "('find_sessions_in_path', 'cited-one', 4, 2, ?, ?, ?, 0)",
        (_iso_days_ago(20), _iso_days_ago(1), _iso_days_ago(1)),
    )
    conn.execute(
        "INSERT INTO discovery_telemetry "
        "(command, session_id, loaded_count, cited_count, first_loaded_ts, "
        " last_loaded_ts, last_cited_ts, demoted) VALUES "
        "('search_past_decisions', 'fresh-one', 30, 0, ?, ?, NULL, 0)",
        (_iso_days_ago(1), _iso_days_ago(0)),
    )
    conn.execute(
        "INSERT INTO discovery_telemetry "
        "(command, session_id, loaded_count, cited_count, first_loaded_ts, "
        " last_loaded_ts, last_cited_ts, demoted) VALUES "
        "('find_sessions_touching_file', 'noise-one', 25, 0, ?, ?, NULL, 0)",
        (_iso_days_ago(14), _iso_days_ago(2)),
    )
    conn.commit()
    conn.close()


def _invoke(args: list[str], store_db: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    return CliRunner().invoke(cli, args)


# ── discovery telemetry ─────────────────────────────────────────────────────


class TestDiscoveryTelemetryCmd:
    def test_text_lists_rows(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(["discovery", "telemetry"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "cited-one" in r.output
        assert "noise-one" in r.output
        assert "cite_rate=0.500" in r.output  # 2/4

    def test_json_shape(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(["discovery", "telemetry", "--format", "json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert "rows" in body and isinstance(body["rows"], list)
        assert len(body["rows"]) == 3
        sample = body["rows"][0]
        assert set(sample) >= {
            "command", "session_id", "loaded_count", "cited_count",
            "cite_rate", "first_loaded_ts", "last_loaded_ts", "last_cited_ts",
            "demoted",
        }

    def test_command_filter(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(
            ["discovery", "telemetry", "--command", "search_past_decisions",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        rows = json.loads(r.output)["rows"]
        assert {x["command"] for x in rows} == {"search_past_decisions"}

    def test_session_filter(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(
            ["discovery", "telemetry", "--session", "cited-one", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        rows = json.loads(r.output)["rows"]
        assert {x["session_id"] for x in rows} == {"cited-one"}

    def test_empty_store(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        r = _invoke(["discovery", "telemetry", "--format", "json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert json.loads(r.output) == {"rows": []}
        r = _invoke(["discovery", "telemetry"], store_db, monkeypatch)
        assert r.exit_code == 0
        assert "no rows" in r.output.lower()


# ── discovery demote-uncited ────────────────────────────────────────────────


class TestDiscoveryDemoteUncitedCmd:
    def test_dry_run_lists_without_flagging(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(
            ["discovery", "demote-uncited", "--dry-run", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["dry_run"] is True
        assert body["demoted"] == 0
        assert [c["session_id"] for c in body["candidates"]] == ["noise-one"]
        # Nothing flagged on disk.
        conn = db.connect(store_db)
        demoted = conn.execute(
            "SELECT demoted FROM discovery_telemetry WHERE session_id = 'noise-one'"
        ).fetchone()["demoted"]
        conn.close()
        assert demoted == 0

    def test_apply_flags_candidates(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(
            ["discovery", "demote-uncited", "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["dry_run"] is False
        assert body["demoted"] == 1
        conn = db.connect(store_db)
        demoted = conn.execute(
            "SELECT demoted FROM discovery_telemetry WHERE session_id = 'noise-one'"
        ).fetchone()["demoted"]
        conn.close()
        assert demoted == 1

        # Re-running finds no candidates now.
        r2 = _invoke(
            ["discovery", "demote-uncited", "--format", "json"],
            store_db, monkeypatch,
        )
        assert json.loads(r2.output)["candidates"] == []

    def test_thresholds_via_flags(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        # Lower min_loads so 'fresh-one' (30 loads) would qualify on loads,
        # but it's only 1 day old → still excluded by min_age_days default 7.
        r = _invoke(
            ["discovery", "demote-uncited", "--dry-run", "--min-loads", "10",
             "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        sids = {c["session_id"] for c in json.loads(r.output)["candidates"]}
        assert sids == {"noise-one"}  # fresh-one excluded by age
        # Drop the age floor too → fresh-one joins.
        r2 = _invoke(
            ["discovery", "demote-uncited", "--dry-run", "--min-loads", "10",
             "--min-age-days", "0", "--format", "json"],
            store_db, monkeypatch,
        )
        sids2 = {c["session_id"] for c in json.loads(r2.output)["candidates"]}
        assert sids2 == {"noise-one", "fresh-one"}

    def test_text_no_candidates(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        r = _invoke(["discovery", "demote-uncited"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "no candidates" in r.output.lower()
