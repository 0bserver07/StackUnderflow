"""Tests for ``stackunderflow pricing doctor`` — read-only pricing health CLI.

Locks the text + json render contracts, the ``--strict`` exit code, and
the ``--stale-days`` passthrough. The command shares its assembler with
``GET /api/pricing/doctor`` (see ``routes/pricing.py``), so the data
contract is covered there and in ``test_pricing_invariants.py``; this
file owns the CLI surface.
"""

from __future__ import annotations

import itertools
import json

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema
from tests.conftest import set_home_env

_SEQ = itertools.count()


# ── seeding ───────────────────────────────────────────────────────────────────


def _seed(store_db, *, events=()):
    """Create a schema-applied store; insert each event spec.

    ``events`` is an iterable of dicts with keys ``model`` / ``cost_usd`` /
    ``cost_source`` (+ optional tokens). Returns nothing — the caller points
    ``deps.store_path`` at ``store_db``.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    if events:
        pid = int(
            conn.execute(
                "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
                "VALUES ('claude', '-a', '-a', 0.0, 0.0)"
            ).lastrowid
        )
        sfk = int(
            conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, 's1', '2026-04-01T00:00:00Z', '2026-04-01T00:00:00Z', 1)",
                (pid,),
            ).lastrowid
        )
        for ev in events:
            _insert_event(conn, project_id=pid, session_fk=sfk, **ev)
    conn.commit()
    conn.close()


def _insert_event(
    conn,
    *,
    project_id,
    session_fk,
    model,
    cost_usd,
    cost_source="rate_card",
    provider="claude",
    input_tokens=0,
    output_tokens=0,
    cache_read=0,
    cache_create=0,
):
    seq = next(_SEQ)
    ts = "2026-04-01T00:00:00Z"
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, 'assistant', ?, ?, ?, ?, ?, '', '[]', '{}', 0)",
        (session_fk, seq, ts, model, input_tokens, output_tokens, cache_create, cache_read),
    )
    mid = int(
        conn.execute(
            "SELECT next_id - 1 FROM _messages_id_seq WHERE rowid_kind = 1"
        ).fetchone()[0]
    )
    conn.execute(
        "INSERT INTO usage_events "
        "(source_message_fk, provider, account, project_id, session_id, ts, day, "
        " model, speed, input_tokens, output_tokens, cache_read_tokens, "
        " cache_create_tokens, cost_usd, cost_source, role) "
        "VALUES (?, ?, 'default', ?, 's1', ?, '2026-04-01', ?, 'standard', "
        " ?, ?, ?, ?, ?, ?, 'assistant')",
        (
            mid, provider, project_id, ts, model,
            input_tokens, output_tokens, cache_read, cache_create, cost_usd, cost_source,
        ),
    )


def _invoke(runner, args, store_db, monkeypatch):
    set_home_env(monkeypatch, store_db.parent / "home")
    monkeypatch.setattr(deps, "store_path", store_db)
    return runner.invoke(cli, args)


# ── text format ────────────────────────────────────────────────────────────────


class TestTextFormat:
    def test_empty_store_renders_ok(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(CliRunner(), ["pricing", "doctor"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "Pricing health" in r.output
        assert "OK" in r.output

    def test_populated_store_lists_unpriced_and_unknown(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(
            store_db,
            events=[
                {"model": "claude-opus-4-8", "cost_usd": 0.5,
                 "input_tokens": 1000, "output_tokens": 500},
                {"model": "exotic-model-x", "cost_usd": 0.0, "cost_source": "unknown",
                 "input_tokens": 2000, "output_tokens": 1000},
            ],
        )
        r = _invoke(CliRunner(), ["pricing", "doctor"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert "Unpriced models" in r.output
        assert "Unknown cost_source models" in r.output
        assert "exotic-model-x" in r.output

    def test_default_format_is_text(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(CliRunner(), ["pricing", "doctor"], store_db, monkeypatch)
        assert r.exit_code == 0, r.output
        assert not r.output.lstrip().startswith("{")
        assert "Pricing health" in r.output


# ── json format ─────────────────────────────────────────────────────────────────


class TestJsonFormat:
    def test_json_shape(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(
            store_db,
            events=[
                {"model": "exotic-model-x", "cost_usd": 0.0, "cost_source": "unknown",
                 "input_tokens": 2000, "output_tokens": 1000},
            ],
        )
        r = _invoke(
            CliRunner(), ["pricing", "doctor", "--format", "json"], store_db, monkeypatch
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert set(body.keys()) == {
            "stale_days", "ok", "summary", "unpriced_models",
            "unknown_cost_source", "rate_freshness",
        }
        assert body["summary"]["unpriced_model_count"] == 1
        assert body["summary"]["unknown_cost_source_model_count"] == 1
        assert body["unpriced_models"][0]["model"] == "exotic-model-x"
        # No network in tests → overlay reads as absent.
        assert body["rate_freshness"]["source"] == "none"

    def test_stale_days_passthrough(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(
            CliRunner(),
            ["pricing", "doctor", "--format", "json", "--stale-days", "30"],
            store_db,
            monkeypatch,
        )
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert body["stale_days"] == 30
        assert body["rate_freshness"]["stale_days_threshold"] == 30


# ── strict gating ───────────────────────────────────────────────────────────────


class TestStrict:
    def test_strict_exits_nonzero_on_violation(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(
            store_db,
            events=[
                # contract violation: unknown source with nonzero cost
                {"model": "exotic-model-x", "cost_usd": 3.0, "cost_source": "unknown",
                 "input_tokens": 1000},
            ],
        )
        r = _invoke(
            CliRunner(), ["pricing", "doctor", "--strict"], store_db, monkeypatch
        )
        assert r.exit_code == 1, r.output

    def test_strict_exits_zero_when_healthy(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(
            store_db,
            events=[
                {"model": "claude-opus-4-8", "cost_usd": 0.5,
                 "input_tokens": 1000, "output_tokens": 500},
                # an expected unknown row (cost 0) is NOT a hard defect
                {"model": "exotic-model-x", "cost_usd": 0.0, "cost_source": "unknown",
                 "input_tokens": 100},
            ],
        )
        r = _invoke(
            CliRunner(), ["pricing", "doctor", "--strict"], store_db, monkeypatch
        )
        assert r.exit_code == 0, r.output

    def test_invalid_format_rejected(self, tmp_path, monkeypatch):
        store_db = tmp_path / "store.db"
        _seed(store_db)
        r = _invoke(
            CliRunner(), ["pricing", "doctor", "--format", "yaml"], store_db, monkeypatch
        )
        assert r.exit_code != 0
