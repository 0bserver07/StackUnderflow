"""End-to-end tests for ``stackunderflow compare``.

Exercises text + json output, and every supported flag combination
(`--period`, `--provider`, `--project`).
"""

from __future__ import annotations

import json

import pytest
from click.testing import CliRunner

from stackunderflow.cli import cli
from stackunderflow.store import db, schema

# ── shared seeding (mirrors the service test fixture) ───────────────────────


def _seed(store_db, *, projects, messages):
    conn = db.connect(store_db)
    schema.apply(conn)

    project_pk: dict[tuple[str, str], int] = {}
    for prov, slug in projects:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            (prov, slug, slug, 0.0, 0.0),
        )
        project_pk[(prov, slug)] = cur.lastrowid

    sess_pk: dict[tuple[int, str], int] = {}
    seq_counter: dict[int, int] = {}
    for m in messages:
        prov = m.get("provider", "claude")
        slug = m["project_slug"]
        ppk = project_pk[(prov, slug)]
        sk = (ppk, m["session_id"])
        if sk not in sess_pk:
            cur = conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, ?, ?, ?)",
                (ppk, m["session_id"], m["timestamp"], m["timestamp"], 0),
            )
            sess_pk[sk] = cur.lastrowid
        sfk = sess_pk[sk]
        seq = seq_counter.get(sfk, 0)
        seq_counter[sfk] = seq + 1
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                sfk, seq, m["timestamp"], m["role"], m.get("model"),
                m.get("in_tok", 0), m.get("out_tok", 0),
                m.get("cache_w", 0), m.get("cache_r", 0),
                "", "[]", "{}", 0, None, None,
            ),
        )
    conn.commit()
    conn.close()


def _fixture_store(tmp_path):
    """Two-model claude store + one codex model so filter flags have something to drop."""
    db_path = tmp_path / "store.db"
    _seed(
        db_path,
        projects=[
            ("claude", "alpha"),
            ("claude", "beta"),
            ("codex", "gamma"),
        ],
        messages=[
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A1",
             "timestamp": "2026-04-01T10:00:01Z", "role": "assistant",
             "model": "claude-A", "in_tok": 100, "out_tok": 50},
            {"project_slug": "beta", "session_id": "B1",
             "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {"project_slug": "beta", "session_id": "B1",
             "timestamp": "2026-04-02T10:00:01Z", "role": "assistant",
             "model": "claude-B", "in_tok": 200, "out_tok": 100},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-03T10:00:00Z", "role": "user"},
            {"project_slug": "gamma", "provider": "codex",
             "session_id": "C1",
             "timestamp": "2026-04-03T10:00:01Z", "role": "assistant",
             "model": "gpt-X", "in_tok": 50, "out_tok": 25},
        ],
    )
    return db_path


def _invoke(runner, args, store_db, monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    return runner.invoke(cli, args)


# ── format: text ─────────────────────────────────────────────────────────────


class TestTextFormat:
    def test_default_period_is_text_table(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(runner, ["compare", "-p", "all"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        # Header columns
        for col in ("Model", "Sessions", "Calls", "1-shot%", "Cache%", "Total$"):
            assert col in r.output, f"missing column header {col!r} in:\n{r.output}"
        # All three model ids should render
        for model in ("claude-A", "claude-B", "gpt-X"):
            assert model in r.output

    def test_empty_store_message(self, tmp_path, monkeypatch):
        store_db = tmp_path / "empty.db"
        conn = db.connect(store_db)
        schema.apply(conn)
        conn.close()
        runner = CliRunner()
        r = _invoke(runner, ["compare", "-p", "all"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "No model activity" in r.output


# ── format: json ─────────────────────────────────────────────────────────────


class TestJsonFormat:
    def test_json_shape(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(runner, ["compare", "-p", "all", "--format", "json"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        data = json.loads(r.output)
        assert data["period"] == "all"
        assert isinstance(data["models"], list)
        assert isinstance(data["generated"], float)
        models = {row["model"] for row in data["models"]}
        assert models == {"claude-A", "claude-B", "gpt-X"}
        # Each row has every documented field
        expected = {
            "model", "provider", "sessions", "calls",
            "one_shot_pct", "retry_rate", "cache_hit_rate",
            "cost_per_call", "cost_per_session",
            "total_cost", "total_tokens",
        }
        assert expected.issubset(set(data["models"][0].keys()))

    def test_json_sorted_by_total_cost(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(runner, ["compare", "-p", "all", "--format", "json"], store_db, monkeypatch)
        data = json.loads(r.output)
        costs = [m["total_cost"] for m in data["models"]]
        assert costs == sorted(costs, reverse=True)


# ── filter flags ─────────────────────────────────────────────────────────────


class TestFilters:
    def test_provider_filter_drops_codex(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["compare", "-p", "all", "--format", "json", "--provider", "claude"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        data = json.loads(r.output)
        models = {m["model"] for m in data["models"]}
        assert "gpt-X" not in models
        assert "claude-A" in models

    def test_project_filter_keeps_only_alpha(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["compare", "-p", "all", "--format", "json", "--project", "alpha"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        data = json.loads(r.output)
        models = {m["model"] for m in data["models"]}
        assert models == {"claude-A"}

    def test_project_filter_repeatable(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["compare", "-p", "all", "--format", "json",
             "--project", "alpha", "--project", "gamma"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        data = json.loads(r.output)
        models = {m["model"] for m in data["models"]}
        assert models == {"claude-A", "gpt-X"}


# ── period flag ──────────────────────────────────────────────────────────────


class TestPeriodFlag:
    @pytest.mark.parametrize("period", ["today", "week", "month", "all"])
    def test_valid_periods_succeed(self, tmp_path, monkeypatch, period):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["compare", "-p", period, "--format", "json"],
            store_db, monkeypatch,
        )
        assert r.exit_code == 0, r.output
        data = json.loads(r.output)
        assert data["period"] == period

    def test_invalid_period_rejected(self, tmp_path, monkeypatch):
        store_db = _fixture_store(tmp_path)
        runner = CliRunner()
        r = _invoke(
            runner,
            ["compare", "-p", "yesterday"],
            store_db, monkeypatch,
        )
        assert r.exit_code != 0


# ── default period ───────────────────────────────────────────────────────────


def test_default_period_is_month(tmp_path, monkeypatch):
    store_db = _fixture_store(tmp_path)
    runner = CliRunner()
    r = _invoke(runner, ["compare", "--format", "json"], store_db, monkeypatch)
    assert r.exit_code == 0, r.output
    data = json.loads(r.output)
    assert data["period"] == "month"
