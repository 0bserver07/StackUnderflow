"""Tests for ``stackunderflow doctor`` — read-only store health check.

Proves the three things the spec pins: it reports ``ok`` on a healthy store,
turns every failure mode (missing / corrupt / watermark-ahead / orphan / FK)
into a finding rather than a crash, and — critically — never writes to the
store it inspects.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import _run_store_health_checks, cli
from stackunderflow.store import db, schema


# ── fixtures ──────────────────────────────────────────────────────────────────


@pytest.fixture(autouse=True)
def _no_real_adapters(monkeypatch):
    """Doctor's delivery section enumerates every registered adapter; these
    health-check tests must not walk the real home directories (hermeticity —
    the delivery section has its own tests with fake adapters)."""
    import stackunderflow.adapters as adapters_pkg

    monkeypatch.setattr(adapters_pkg, "registered", lambda: [])


def _healthy_store(path: Path) -> None:
    conn = db.connect(path)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', '-a', '-a', 0.0, 0.0)"
    )
    conn.close()


def _invoke(runner: CliRunner, args, store_db: Path, monkeypatch):
    monkeypatch.setattr(deps, "store_path", store_db)
    return runner.invoke(cli, args)


# ── healthy ───────────────────────────────────────────────────────────────────


def test_healthy_store_reports_ok(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _healthy_store(store)
    r = _invoke(CliRunner(), ["doctor"], store, monkeypatch)
    assert r.exit_code == 0, r.output
    assert r.output.splitlines()[0] == "ok"
    assert "delivery (" in r.output  # the scoreboard renders alongside health


def test_healthy_store_json(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _healthy_store(store)
    r = _invoke(CliRunner(), ["doctor", "--json"], store, monkeypatch)
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert payload["ok"] is True
    assert payload["findings"] == []
    assert payload["store_path"] == str(store)
    # The delivery scoreboard ships in the same envelope (empty registry here).
    assert payload["delivery"] == {"ok": True, "providers": [], "gaps": []}


# ── failure modes are findings, not crashes ───────────────────────────────────


def test_missing_store_is_a_finding_not_a_crash(tmp_path, monkeypatch):
    store = tmp_path / "nope.db"
    r = _invoke(CliRunner(), ["doctor"], store, monkeypatch)
    assert r.exit_code == 1
    assert r.exception is None or isinstance(r.exception, SystemExit)
    assert "store not found" in r.output
    # doctor must NOT have created the store it failed to find.
    assert not store.exists()


def test_missing_store_json(tmp_path, monkeypatch):
    store = tmp_path / "nope.db"
    r = _invoke(CliRunner(), ["doctor", "--json"], store, monkeypatch)
    assert r.exit_code == 1
    payload = json.loads(r.output)
    assert payload["ok"] is False
    assert payload["findings"] and payload["findings"][0]["check"] == "store"


def test_corrupt_store_is_a_finding(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    store.write_bytes(b"definitely not a sqlite database" * 64)
    r = _invoke(CliRunner(), ["doctor"], store, monkeypatch)
    assert r.exit_code == 1
    checks = [f["check"] for f in _run_store_health_checks(store)["findings"]]
    assert "integrity" in checks
    # The garbage file is untouched.
    assert store.read_bytes().startswith(b"definitely not a sqlite")


def test_watermark_ahead_of_events(tmp_path):
    store = tmp_path / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO mart_watermark (mart_name, last_event_id, last_refresh_ts) "
        "VALUES ('daily', 999, '2026-01-01T00:00:00Z')"
    )
    conn.close()
    result = _run_store_health_checks(store)
    assert result["ok"] is False
    assert any(f["check"] == "watermark" for f in result["findings"])


def test_orphan_mart_row(tmp_path):
    store = tmp_path / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', '-a', '-a', 0.0, 0.0)"
    )
    # project_id 777 does not exist — session_mart has no FK to enforce it.
    conn.execute(
        "INSERT INTO session_mart (session_id, project_id, provider, first_ts, last_ts) "
        "VALUES ('s1', 777, 'claude', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"
    )
    conn.close()
    result = _run_store_health_checks(store)
    assert result["ok"] is False
    orphan = [f for f in result["findings"] if f["check"] == "orphan"]
    assert orphan and "session_mart" in orphan[0]["message"]


def test_foreign_key_violation(tmp_path):
    store = tmp_path / "store.db"
    _healthy_store(store)
    # Inject a dangling session with foreign keys OFF (raw connection), so the
    # row lands and PRAGMA foreign_key_check has something to catch.
    raw = sqlite3.connect(store)
    raw.execute("PRAGMA foreign_keys = OFF")
    raw.execute(
        "INSERT INTO sessions (project_id, session_id, message_count) "
        "VALUES (424242, 'ghost', 0)"
    )
    raw.commit()
    raw.close()
    result = _run_store_health_checks(store)
    assert result["ok"] is False
    assert any(f["check"] == "foreign_key" for f in result["findings"])


# ── read-only guarantee ───────────────────────────────────────────────────────


def test_doctor_never_writes_the_store(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _healthy_store(store)
    before_bytes = store.read_bytes()
    before_mtime = store.stat().st_mtime_ns

    r = _invoke(CliRunner(), ["doctor"], store, monkeypatch)
    assert r.exit_code == 0, r.output

    assert store.read_bytes() == before_bytes
    assert store.stat().st_mtime_ns == before_mtime


def test_doctor_does_not_migrate_an_old_schema(tmp_path):
    """A store below CURRENT_VERSION stays below it — doctor never migrates."""
    store = tmp_path / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    conn.execute("PRAGMA user_version = 6")  # pretend it's an older store
    conn.close()

    result = _run_store_health_checks(store)

    check = sqlite3.connect(f"file:{store}?mode=ro", uri=True)
    try:
        assert check.execute("PRAGMA user_version").fetchone()[0] == 6
    finally:
        check.close()
    # An old-but-valid store is healthy; behind-schema is not a finding.
    assert result["ok"] is True
