"""End-to-end tests for the external history-source import path (spec #12 +
#16), driven by the committed fake ``amp-export`` fixture script."""

from __future__ import annotations

import importlib.util
import json
import sqlite3
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters import custom_import
from stackunderflow.adapters.base import content_hash_id
from stackunderflow.adapters.custom_jsonl import (
    ExportCommandError,
    ManifestError,
    StreamValidationError,
)
from stackunderflow.store import db, schema

# ── load the fixture as the single source of truth for its constants ─────────

_FIXTURE = (
    Path(__file__).resolve().parents[2]
    / "fixtures" / "history_source" / "fake_amp_export.py"
)
_spec = importlib.util.spec_from_file_location("fake_amp_export", _FIXTURE)
assert _spec and _spec.loader
_fake = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_fake)
TOUCHED_PATH: str = _fake.TOUCHED_PATH
FINAL_CURSOR: str = _fake.FINAL_CURSOR


# ── fixtures / helpers ───────────────────────────────────────────────────────


@pytest.fixture
def conn(tmp_path: Path):
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


@pytest.fixture
def state_dir(tmp_path: Path) -> Path:
    return tmp_path / "state"


def write_manifest(
    tmp_path: Path,
    *,
    mode: str = "ok",
    log: Path | None = None,
    source_id: str = "amp",
    env_passthrough: list[str] | None = None,
    cursor: str | None = None,
) -> Path:
    command = [sys.executable, str(_FIXTURE), "--mode", mode]
    if log is not None:
        command += ["--log", str(log)]
    data: dict = {
        "schema": "stackunderflow-history-jsonl-v1",
        "source_id": source_id,
        "command": command,
        "timeout_seconds": 30,
    }
    if env_passthrough is not None:
        data["env_passthrough"] = env_passthrough
    if cursor is not None:
        data["cursor"] = cursor
    p = tmp_path / f"{source_id}-manifest.json"
    p.write_text(json.dumps(data))
    return p


def run_import(manifest_path, conn, state_dir, **kw):
    return custom_import.import_history_source(
        manifest_path=manifest_path, conn=conn, state_dir=state_dir, **kw
    )


def _messages(conn: sqlite3.Connection) -> list[sqlite3.Row]:
    return conn.execute(
        "SELECT m.id, m.seq, m.uuid, m.role, m.content_text, m.tools_json, "
        "       s.session_id, p.provider, p.slug "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id "
        "ORDER BY s.session_id, m.seq"
    ).fetchall()


# ── happy path ───────────────────────────────────────────────────────────────


def test_import_lands_under_custom_provider(conn, state_dir, tmp_path):
    manifest = write_manifest(tmp_path)
    result = run_import(manifest, conn, state_dir)

    assert result.provider == "custom"
    assert result.source_id == "amp"
    assert result.sessions_seen == 1
    # 2 messages + 1 file_touch (synthesised into a message row).
    assert result.messages_ingested == 3
    assert result.file_touches_seen == 1

    rows = _messages(conn)
    assert len(rows) == 3
    assert all(r["provider"] == "custom" for r in rows)
    # project namespaced under source_id + the export's project name.
    assert rows[0]["slug"] == "amp--billing-service"
    # store session id is namespaced by source_id.
    assert rows[0]["session_id"] == "amp:amp-sess-1"
    # content-addressed uuids.
    assert all(r["uuid"].startswith("c-") for r in rows)


def test_cursor_is_stored_after_success(conn, state_dir, tmp_path):
    manifest = write_manifest(tmp_path)
    result = run_import(manifest, conn, state_dir)
    assert result.cursor_after == FINAL_CURSOR
    assert result.cursor_advanced is True
    assert custom_import.load_cursor(state_dir, "amp") == FINAL_CURSOR


def test_project_and_session_rows(conn, state_dir, tmp_path):
    run_import(write_manifest(tmp_path), conn, state_dir)
    projects = conn.execute("SELECT provider, slug FROM projects").fetchall()
    assert [(r["provider"], r["slug"]) for r in projects] == [("custom", "amp--billing-service")]
    sessions = conn.execute("SELECT session_id FROM sessions").fetchall()
    assert [r["session_id"] for r in sessions] == ["amp:amp-sess-1"]


def test_message_tokens_and_tools_preserved(conn, state_dir, tmp_path):
    run_import(write_manifest(tmp_path), conn, state_dir)
    asst = conn.execute(
        "SELECT input_tokens, output_tokens, cache_read_tokens, model, tools_json "
        "FROM messages WHERE role='assistant' AND model='amp-large'"
    ).fetchone()
    assert asst["input_tokens"] == 1200
    assert asst["output_tokens"] == 340
    assert asst["cache_read_tokens"] == 100
    assert "Edit" in asst["tools_json"]


# ── idempotency (#16) ────────────────────────────────────────────────────────


def test_reimport_is_idempotent(conn, state_dir, tmp_path):
    manifest = write_manifest(tmp_path)

    r1 = run_import(manifest, conn, state_dir)
    before = {(r["session_id"], r["seq"]): (r["id"], r["uuid"]) for r in _messages(conn)}

    r2 = run_import(manifest, conn, state_dir)
    after = {(r["session_id"], r["seq"]): (r["id"], r["uuid"]) for r in _messages(conn)}

    # No new rows; every id + uuid identical across the two runs.
    assert r1.messages_ingested == 3
    assert r2.messages_ingested == 0
    assert before == after
    # Cursor unchanged on the second run (same stream, same cursor).
    assert r2.cursor_advanced is False
    assert r2.cursor_after == FINAL_CURSOR


def test_uuid_is_reproducible_content_hash(conn, state_dir, tmp_path):
    run_import(write_manifest(tmp_path), conn, state_dir)
    row = conn.execute(
        "SELECT uuid FROM messages WHERE role='user' AND seq=0"
    ).fetchone()
    # Recompute the same content hash a foreign consumer would — proving the id
    # is content-addressed and cross-machine reproducible, not machine-local.
    expected = content_hash_id(
        "custom", "amp", "amp:amp-sess-1", 0, "message", "user",
        "2026-06-01T10:00:00+00:00", "",
        "The retry loop in service.py hammers the billing API.",
        prefix="c-",
    )
    assert row["uuid"] == expected


# ── fail-closed ──────────────────────────────────────────────────────────────


def test_nonzero_exit_fails_closed(conn, state_dir, tmp_path):
    manifest = write_manifest(tmp_path, mode="fail")
    with pytest.raises(ExportCommandError):
        run_import(manifest, conn, state_dir)
    assert conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == 0
    # Cursor never stored — the sidecar must not exist.
    assert custom_import.load_cursor(state_dir, "amp") is None


def test_malformed_line_aborts_whole_import(conn, state_dir, tmp_path):
    manifest = write_manifest(tmp_path, mode="malformed")
    with pytest.raises(StreamValidationError):
        run_import(manifest, conn, state_dir)
    # The malformed stream had one valid message before the broken line; because
    # validation runs over the whole stream first, NOTHING is written.
    assert conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == 0
    assert custom_import.load_cursor(state_dir, "amp") is None


def test_failure_after_prior_success_leaves_prior_cursor(conn, state_dir, tmp_path):
    ok = write_manifest(tmp_path, mode="ok", source_id="amp")
    run_import(ok, conn, state_dir)
    assert custom_import.load_cursor(state_dir, "amp") == FINAL_CURSOR

    fail = write_manifest(tmp_path, mode="fail", source_id="amp")
    with pytest.raises(ExportCommandError):
        run_import(fail, conn, state_dir)
    # The prior good cursor is untouched by the failed run.
    assert custom_import.load_cursor(state_dir, "amp") == FINAL_CURSOR


# ── cursor replay + env allowlist ────────────────────────────────────────────


def test_cursor_is_replayed_to_export(conn, state_dir, tmp_path):
    log = tmp_path / "invocations.log"
    manifest = write_manifest(tmp_path, log=log)

    run_import(manifest, conn, state_dir)  # first run stores FINAL_CURSOR
    run_import(manifest, conn, state_dir)  # second run should replay it

    entries = [json.loads(line) for line in log.read_text().splitlines()]
    assert len(entries) == 2
    assert entries[0]["cursor_in"] == ""            # empty seed on first run
    assert entries[1]["cursor_in"] == FINAL_CURSOR  # replayed on the second


def test_seed_cursor_from_manifest_used_on_first_run(conn, state_dir, tmp_path):
    log = tmp_path / "invocations.log"
    manifest = write_manifest(tmp_path, log=log, cursor="seed-42")
    run_import(manifest, conn, state_dir)
    entry = json.loads(log.read_text().splitlines()[0])
    assert entry["cursor_in"] == "seed-42"


def test_env_allowlist_passes_through_only_listed_vars(conn, state_dir, tmp_path):
    log = tmp_path / "invocations.log"
    manifest = write_manifest(
        tmp_path, log=log, env_passthrough=["FAKE_EXPORT_TOKEN"]
    )
    parent_env = {
        "PATH": __import__("os").environ.get("PATH", ""),
        "FAKE_EXPORT_TOKEN": "s3cret",
        "FAKE_EXPORT_SHOULD_BE_DROPPED": "leaked",
    }
    run_import(manifest, conn, state_dir, parent_env=parent_env)
    entry = json.loads(log.read_text().splitlines()[0])
    assert entry["passthrough_token"] == "s3cret"   # allowlisted → visible
    assert entry["dropped_var"] is None             # not listed → dropped


# ── cursor-optional + empty streams ──────────────────────────────────────────


def test_nocursor_mode_leaves_cursor_unchanged(conn, state_dir, tmp_path):
    manifest = write_manifest(tmp_path, mode="nocursor")
    result = run_import(manifest, conn, state_dir)
    assert result.messages_ingested == 3
    assert result.cursor_advanced is False
    assert result.cursor_after is None
    assert custom_import.load_cursor(state_dir, "amp") is None


def test_empty_stream_only_advances_cursor(conn, state_dir, tmp_path):
    manifest = write_manifest(tmp_path, mode="empty")
    result = run_import(manifest, conn, state_dir)
    assert result.messages_ingested == 0
    assert result.sessions_seen == 0
    assert result.cursor_after == FINAL_CURSOR


# ── file-touch discoverability ───────────────────────────────────────────────


def test_file_touch_is_discoverable(conn, state_dir, tmp_path):
    from stackunderflow.services.discovery import find_sessions_touching_file

    run_import(write_manifest(tmp_path), conn, state_dir)
    matches = find_sessions_touching_file(conn, TOUCHED_PATH, mode="any")
    assert [m.session_id for m in matches] == ["amp:amp-sess-1"]


# ── multi-source isolation ───────────────────────────────────────────────────


def test_two_sources_land_in_distinct_projects(conn, state_dir, tmp_path):
    run_import(write_manifest(tmp_path, source_id="amp"), conn, state_dir)
    run_import(write_manifest(tmp_path, source_id="ampTwo"), conn, state_dir)
    slugs = {r["slug"] for r in conn.execute("SELECT slug FROM projects").fetchall()}
    assert slugs == {"amp--billing-service", "ampTwo--billing-service"}
    # Cursors are tracked independently per source_id.
    assert custom_import.load_cursor(state_dir, "amp") == FINAL_CURSOR
    assert custom_import.load_cursor(state_dir, "ampTwo") == FINAL_CURSOR


# ── manifest resolution ──────────────────────────────────────────────────────


def test_resolve_manifest_path_file(tmp_path):
    p = write_manifest(tmp_path)
    assert custom_import.resolve_manifest_path(str(p), search_roots=[]) == p


def test_resolve_manifest_path_directory(tmp_path):
    from stackunderflow.adapters.custom_jsonl import MANIFEST_FILENAME
    d = tmp_path / "src"
    d.mkdir()
    (d / MANIFEST_FILENAME).write_text(
        json.dumps({"source_id": "amp", "command": ["x"]})
    )
    assert custom_import.resolve_manifest_path(str(d), search_roots=[]) == d / MANIFEST_FILENAME


def test_resolve_manifest_path_named_under_root(tmp_path):
    from stackunderflow.adapters.custom_jsonl import MANIFEST_FILENAME
    root = tmp_path / "plugins"
    (root / "amp").mkdir(parents=True)
    manifest = root / "amp" / MANIFEST_FILENAME
    manifest.write_text(json.dumps({"source_id": "amp", "command": ["x"]}))
    assert custom_import.resolve_manifest_path("amp", search_roots=[root]) == manifest


def test_resolve_manifest_path_missing_raises(tmp_path):
    with pytest.raises(ManifestError):
        custom_import.resolve_manifest_path("ghost", search_roots=[tmp_path])
