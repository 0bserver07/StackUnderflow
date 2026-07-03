"""``stackunderflow sync {init,push,status}`` CLI surface.

Store + state dir are redirected to ``tmp_path`` (as in ``cli/test_doctor.py``),
so nothing touches ``~/.stackunderflow``. Crypto-dependent flows are gated on
``pyrage``; the deps-missing hint and the ``status``/``not-configured`` paths run
without any optional dependency (``_sync_missing_deps`` is monkeypatched so the
result is independent of what's actually installed).
"""

from __future__ import annotations

import json
import os
import stat

import pytest
from click.testing import CliRunner

import stackunderflow.cli as cli_mod
import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.store import db, schema


def _prep(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    conn = db.connect(store)
    schema.apply(conn)
    conn.close()
    state = tmp_path / "state"
    state.mkdir()
    monkeypatch.setattr(deps, "store_path", store)
    monkeypatch.setattr(cli_mod, "_STATE_DIR", state)
    return store, state


def _fingerprint(store):
    conn = db.connect(store)
    try:
        row = conn.execute("SELECT key_fingerprint FROM sync_identity WHERE id = 1").fetchone()
        return row["key_fingerprint"] if row else None
    finally:
        conn.close()


def test_status_off_text(tmp_path, monkeypatch):
    _prep(tmp_path, monkeypatch)
    r = CliRunner().invoke(cli, ["sync", "status"])
    assert r.exit_code == 0, r.output
    assert "Sync: off" in r.output


def test_status_off_json(tmp_path, monkeypatch):
    _prep(tmp_path, monkeypatch)
    r = CliRunner().invoke(cli, ["sync", "status", "--json"])
    assert r.exit_code == 0, r.output
    payload = json.loads(r.output)
    assert payload["enabled"] is False
    assert payload["pending_count"] == 0


def test_init_missing_deps_prints_hint(tmp_path, monkeypatch):
    _prep(tmp_path, monkeypatch)
    monkeypatch.setattr(cli_mod, "_sync_missing_deps", lambda **_: ["pyrage"])
    r = CliRunner().invoke(cli, ["sync", "init", "--bucket", "s3://b"])
    assert r.exit_code == 1
    assert "pip install 'stackunderflow[sync]'" in r.output


def test_push_not_configured_exits_nonzero(tmp_path, monkeypatch):
    _prep(tmp_path, monkeypatch)
    monkeypatch.setattr(cli_mod, "_sync_missing_deps", lambda **_: [])  # pretend deps present
    r = CliRunner().invoke(cli, ["sync", "push"])
    assert r.exit_code == 1
    assert "not configured" in r.output


def test_init_generates_key_and_configures(tmp_path, monkeypatch):
    pytest.importorskip("pyrage")
    store, state = _prep(tmp_path, monkeypatch)

    r = CliRunner().invoke(cli, ["sync", "init", "--bucket", "s3://my-bucket"])
    assert r.exit_code == 0, r.output
    # Loud zero-knowledge / key-loss banner, and the key shown once.
    assert "UNRECOVERABLE" in r.output
    assert "AGE-SECRET-KEY-1" in r.output

    keyfile = state / "sync-identity"
    assert keyfile.exists()
    assert stat.S_IMODE(os.stat(keyfile).st_mode) == 0o600

    conn = db.connect(store)
    row = conn.execute(
        "SELECT device_uuid, key_fingerprint, bucket_url FROM sync_identity WHERE id = 1"
    ).fetchone()
    conn.close()
    assert row["bucket_url"] == "s3://my-bucket"
    # Only the fingerprint is persisted — never the secret.
    assert row["key_fingerprint"]
    assert "AGE-SECRET-KEY" not in row["key_fingerprint"]

    r2 = CliRunner().invoke(cli, ["sync", "status"])
    assert "Sync: on" in r2.output
    assert row["device_uuid"] in r2.output


def test_init_refuses_second_without_force_then_rotates_with_force(tmp_path, monkeypatch):
    pytest.importorskip("pyrage")
    store, _ = _prep(tmp_path, monkeypatch)

    assert CliRunner().invoke(cli, ["sync", "init", "--bucket", "s3://b"]).exit_code == 0
    first_fp = _fingerprint(store)

    r = CliRunner().invoke(cli, ["sync", "init", "--bucket", "s3://b"])
    assert r.exit_code == 1
    assert "already configured" in r.output
    assert _fingerprint(store) == first_fp  # unchanged

    r2 = CliRunner().invoke(cli, ["sync", "init", "--bucket", "s3://b", "--force"])
    assert r2.exit_code == 0, r2.output
    assert _fingerprint(store) != first_fp  # key rotated


def test_push_end_to_end_with_fake_bucket(tmp_path, monkeypatch, seed_marts):
    pytest.importorskip("pyrage")
    store, state = _prep(tmp_path, monkeypatch)
    conn = db.connect(store)
    seed_marts(conn)
    conn.close()

    assert CliRunner().invoke(cli, ["sync", "init", "--bucket", "s3://b"]).exit_code == 0

    from stackunderflow.sync import bucket as bkt

    fake = bkt.InMemoryObjectStore()
    monkeypatch.setattr("stackunderflow.sync.bucket.s3_store_from_url", lambda *a, **k: fake)
    monkeypatch.setattr("stackunderflow.sync.keys._read_keychain", lambda service=None: None)
    monkeypatch.setattr(cli_mod, "_sync_missing_deps", lambda **_: [])

    r = CliRunner().invoke(cli, ["sync", "push"])
    assert r.exit_code == 0, r.output
    assert "Pushed" in r.output
    assert len(fake) > 0
    assert any(k.endswith("manifest.age") for k in fake.list(""))

    # Idempotent second push — nothing changed.
    r2 = CliRunner().invoke(cli, ["sync", "push"])
    assert r2.exit_code == 0, r2.output
    assert "Up to date" in r2.output

    # status now shows no pending.
    r3 = CliRunner().invoke(cli, ["sync", "status"])
    assert "pending upload:  0" in r3.output
