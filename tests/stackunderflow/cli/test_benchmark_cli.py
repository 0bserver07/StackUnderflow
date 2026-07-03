"""CLI tests for the ``stackunderflow benchmark`` namespace (spec 26 §6.2).

Mirrors ``test_memory_cli.py``: monkeypatch ``deps.store_path`` to a tmp store,
seed a fixture, drive via Click's ``CliRunner``. The ``--json`` path must emit
the shared ``stackunderflow.memory/1`` agent-output envelope.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema
from tests.stackunderflow.reports.test_benchmark import (
    _seed_project,
    _seed_winner_fixture,
)

_CORE_ENVELOPE_FIELDS = {
    "schema", "command", "query", "results",
    "result_count", "token_estimate", "budget", "truncated",
}


def _seed_winner(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    pid = _seed_project(conn)
    _seed_winner_fixture(conn, pid)
    conn.commit()
    conn.close()


def _seed_empty(store_db: Path) -> None:
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()


def _invoke(runner: CliRunner, args: list[str], store_db: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    return runner.invoke(cli, args)


class TestBenchmarkShow:
    def test_text_names_the_winner(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_winner(store_db)
        r = _invoke(CliRunner(), ["benchmark", "show"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "Verdict:" in r.output
        assert "sonnet" in r.output

    def test_json_envelope_shape(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_winner(store_db)
        r = _invoke(CliRunner(), ["benchmark", "show", "--json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert _CORE_ENVELOPE_FIELDS <= set(body)
        assert body["schema"] == "stackunderflow.memory/1"
        assert body["command"] == "benchmark"
        assert body["verdict"]["winning_model"] == "sonnet"
        assert isinstance(body["results"], list) and body["results"]
        assert body["weights"] == {"success": 0.45, "cost": 0.35, "effort": 0.20}

    def test_empty_store_emits_valid_envelope(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        r = _invoke(CliRunner(), ["benchmark", "show", "--json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["verdict"]["headline"] == "insufficient evidence"
        assert body["results"] == []

    def test_bad_period_is_click_error(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        r = _invoke(CliRunner(), ["benchmark", "show", "--period", "decade"],
                    store_db, monkeypatch)
        assert r.exit_code != 0


class TestBenchmarkRecommend:
    def test_json_recommends_winner(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_winner(store_db)
        r = _invoke(
            CliRunner(),
            ["benchmark", "recommend", "--intent", "fix", "--size", "small", "--json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["command"] == "benchmark-recommend"
        assert body["results"][0]["recommended_model"] == "sonnet"

    def test_text_insufficient_evidence(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        r = _invoke(CliRunner(), ["benchmark", "recommend", "--intent", "refactor"],
                    store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "insufficient evidence" in r.output

    def test_intent_is_required(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed_empty(store_db)
        r = _invoke(CliRunner(), ["benchmark", "recommend"], store_db, monkeypatch)
        assert r.exit_code != 0  # click flags the missing required option
