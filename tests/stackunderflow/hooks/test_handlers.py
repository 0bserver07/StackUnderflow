"""``stackunderflow hooks run <id>`` handler dispatch — the capture path.

Locked here (the spec's handler tier):

* ``PostToolUse`` with a non-zero Bash exit → one ``captured_events`` row,
  ``event_kind='failure'``, payload carries the tool name + exit code (and a
  short error line) — never full stdout/stderr.
* a successful tool call → no row at all.
* ``UserPromptSubmit`` matching the correction heuristic → row,
  ``event_kind='correction'``, the prompt text is *redacted* (only its length
  and the matched keyword are stored). An ordinary prompt → no row.
* ``Stop`` → ``boundary`` row with a session-totals snapshot.
* ``PreCompact`` → ``snapshot`` row.
* ``--capture-content`` stores the full (unsanitised) payload instead.
* unknown hook id / malformed payload → no-op, exit 0; the handler never
  raises out (a recorder must not disturb Claude Code).
* re-firing the same hook (same ts+hook+session) is idempotent.
* ``project_id`` is best-effort resolved from the payload ``cwd``.
* the handler is cheap — p99 well under the 50 ms budget.
"""

from __future__ import annotations

import json
import sqlite3
import time
from pathlib import Path

import pytest

import stackunderflow.deps as deps
from stackunderflow.hooks import handlers
from stackunderflow.hooks.handlers import ensure_captured_events_table, run
from stackunderflow.store import db, schema


@pytest.fixture
def store(tmp_path: Path, monkeypatch) -> Path:
    """A real-schema store at a tmp path, wired in as ``deps.store_path``."""
    p = tmp_path / "store.db"
    conn = db.connect(p)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", p)
    return p


@pytest.fixture
def bare_store(tmp_path: Path, monkeypatch) -> Path:
    """A store path that does NOT exist yet — exercises the handler's self-heal."""
    p = tmp_path / "store.db"
    monkeypatch.setattr(deps, "store_path", p)
    return p


def _rows(store_path: Path) -> list[dict]:
    conn = sqlite3.connect(store_path)
    conn.row_factory = sqlite3.Row
    try:
        return [dict(r) for r in conn.execute(
            "SELECT id, ts, project_id, session_id, hook_id, event_kind, payload_json "
            "FROM captured_events ORDER BY id"
        ).fetchall()]
    finally:
        conn.close()


def _seed_project_and_session(store_path: Path, *, slug: str, session_id: str) -> tuple[int, int]:
    conn = db.connect(store_path)
    try:
        pid = int(conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, 0.0, 0.0)", (slug, slug),
        ).lastrowid)
        sfk = int(conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, ?, '2026-05-01T00:00:00Z', '2026-05-01T01:00:00Z', 2)", (pid, session_id),
        ).lastrowid)
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, output_tokens, "
            "cache_create_tokens, cache_read_tokens, content_text, tools_json, raw_json) "
            "VALUES (?, 0, '2026-05-01T00:00:00Z', 'user', NULL, 0, 0, 0, 0, 'hi', '[]', '{}'),"
            "       (?, 1, '2026-05-01T00:30:00Z', 'assistant', 'claude-x', 100, 50, 10, 200, 'ok', '[]', '{}')",
            (sfk, sfk),
        )
        return pid, sfk
    finally:
        conn.close()


# ── store bootstrap ─────────────────────────────────────────────────────────


class TestEnsureTable:
    def test_creates_table_and_indexes(self, tmp_path: Path) -> None:
        conn = db.connect(tmp_path / "s.db")
        try:
            ensure_captured_events_table(conn)
            cols = [r[1] for r in conn.execute("PRAGMA table_info(captured_events)").fetchall()]
            assert cols == ["id", "ts", "project_id", "session_id", "hook_id", "event_kind", "payload_json"]
            idx = {r[1] for r in conn.execute("PRAGMA index_list(captured_events)").fetchall()}
            assert "idx_captured_events_session" in idx
            assert "idx_captured_events_kind" in idx
            # idempotent
            ensure_captured_events_table(conn)
        finally:
            conn.close()

    def test_does_not_bump_user_version(self, tmp_path: Path) -> None:
        conn = db.connect(tmp_path / "s.db")
        try:
            assert conn.execute("PRAGMA user_version").fetchone()[0] == 0
            ensure_captured_events_table(conn)
            assert conn.execute("PRAGMA user_version").fetchone()[0] == 0  # untouched
        finally:
            conn.close()

    def test_handler_self_heals_on_missing_store(self, bare_store: Path) -> None:
        # No schema.apply ran; the handler must create the table itself.
        rc = run("stackunderflow-post-tool-use",
                 {"tool_name": "Bash", "session_id": "s1", "tool_response": {"exit_code": 1}})
        assert rc == 0
        assert len(_rows(bare_store)) == 1


# ── PostToolUse → failure ───────────────────────────────────────────────────


class TestFailureCapture:
    def test_nonzero_exit_records_failure_without_stdout(self, store: Path) -> None:
        rc = run("stackunderflow-post-tool-use", {
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "abc-123",
            "cwd": "/Users/dev/project",
            "tool_input": {"command": "pytest"},
            "tool_response": {
                "exit_code": 1,
                "stdout": "x" * 5000,                       # large — must NOT be stored
                "stderr": "AssertionError: nope\n  at line 9\n  ...50 more lines...",
            },
        })
        assert rc == 0
        rows = _rows(store)
        assert len(rows) == 1
        row = rows[0]
        assert row["hook_id"] == "stackunderflow-post-tool-use"
        assert row["event_kind"] == "failure"
        assert row["session_id"] == "abc-123"
        payload = json.loads(row["payload_json"])
        assert payload["tool_name"] == "Bash"
        assert payload["exit_code"] == 1
        # The error summary is the *first line only*, truncated.
        assert payload["error_summary"] == "AssertionError: nope"
        # Conservative: no command, no stdout, no full stderr anywhere.
        blob = row["payload_json"]
        assert "pytest" not in blob
        assert "x" * 100 not in blob
        assert "50 more lines" not in blob

    def test_successful_call_records_nothing(self, store: Path) -> None:
        assert run("stackunderflow-post-tool-use",
                   {"tool_name": "Bash", "session_id": "s", "tool_response": {"exit_code": 0}}) == 0
        assert _rows(store) == []

    def test_error_flag_shapes_count_as_failure(self, store: Path) -> None:
        # Different Claude Code versions shape tool_response differently — probe a few.
        for i, resp in enumerate([
            {"is_error": True, "content": "boom"},
            {"error": "permission denied"},
            {"success": False},
            {"returncode": 127},
            {"code": "2"},
        ]):
            assert run("stackunderflow-post-tool-use",
                       {"tool_name": "Bash", "session_id": f"s{i}", "tool_response": resp}) == 0
        assert len(_rows(store)) == 5

    def test_no_recognisable_failure_signal_records_nothing(self, store: Path) -> None:
        # No exit code, no error flag → we do NOT guess (that's the heuristic
        # this spec replaces). Better a missed row than a false positive.
        assert run("stackunderflow-post-tool-use",
                   {"tool_name": "Read", "session_id": "s", "tool_response": {"content": "file contents"}}) == 0
        assert _rows(store) == []

    def test_project_id_resolved_from_cwd(self, store: Path) -> None:
        pid, _ = _seed_project_and_session(store, slug="-Users-dev-proj", session_id="sess-1")
        run("stackunderflow-post-tool-use", {
            "tool_name": "Bash", "session_id": "sess-1", "cwd": "/Users/dev/proj",
            "tool_response": {"exit_code": 2},
        })
        rows = _rows(store)
        assert rows[0]["project_id"] == pid

    def test_unknown_cwd_leaves_project_id_null(self, store: Path) -> None:
        run("stackunderflow-post-tool-use", {
            "tool_name": "Bash", "session_id": "s", "cwd": "/nowhere/in/the/store",
            "tool_response": {"exit_code": 1},
        })
        assert _rows(store)[0]["project_id"] is None


# ── UserPromptSubmit → correction (redacted) ────────────────────────────────


class TestCorrectionCapture:
    @pytest.mark.parametrize("prompt", [
        "no, do it the other way",
        "STOP — that's not what I asked for",
        "undo that last change",
        "revert the migration please",
        "wait, hold on",
        "actually that's wrong, use the other module",
        "go back to the previous approach",
        "don't touch the config file",
    ])
    def test_correction_keywords_record_redacted_row(self, store: Path, prompt: str) -> None:
        leak_canary = "DO-NOT-LEAK-THIS-12345"
        full_prompt = f"{prompt} — context: {leak_canary}"
        assert run("stackunderflow-user-prompt",
                   {"hook_event_name": "UserPromptSubmit", "prompt": full_prompt, "session_id": "s"}) == 0
        rows = _rows(store)
        assert len(rows) == 1
        assert rows[0]["event_kind"] == "correction"
        payload = json.loads(rows[0]["payload_json"])
        assert payload["matched_keyword"]  # something matched
        assert payload["prompt_length"] == len(full_prompt)
        # The prompt content — and especially the canary — is NOT stored.
        assert leak_canary not in rows[0]["payload_json"]
        assert prompt not in rows[0]["payload_json"]

    @pytest.mark.parametrize("prompt", [
        "I have no idea how this works",
        "now let's add the feature",
        "can you explain the noqa comment?",
        "the build is green, ship it",
        "nobody uses that anymore, remove it",   # "nobody" must not match "no"
        "",
        "implement the parser",
    ])
    def test_non_correction_prompts_record_nothing(self, store: Path, prompt: str) -> None:
        assert run("stackunderflow-user-prompt", {"prompt": prompt, "session_id": "s"}) == 0
        assert _rows(store) == []

    def test_missing_prompt_field_is_noop(self, store: Path) -> None:
        assert run("stackunderflow-user-prompt", {"session_id": "s"}) == 0
        assert _rows(store) == []


# ── Stop / PreCompact → boundary / snapshot ─────────────────────────────────


class TestBoundaryAndSnapshot:
    def test_stop_records_boundary_with_session_totals(self, store: Path) -> None:
        _seed_project_and_session(store, slug="-Users-dev-app", session_id="sess-x")
        assert run("stackunderflow-stop",
                   {"hook_event_name": "Stop", "session_id": "sess-x", "cwd": "/Users/dev/app",
                    "stop_hook_active": False}) == 0
        rows = _rows(store)
        assert len(rows) == 1
        assert rows[0]["event_kind"] == "boundary"
        payload = json.loads(rows[0]["payload_json"])
        totals = payload["session_totals"]
        assert totals["available"] is True
        assert totals["message_count"] == 2
        assert totals["input_tokens"] == 100
        assert totals["output_tokens"] == 50
        assert totals["cache_read_tokens"] == 200

    def test_stop_for_unknown_session_still_records_zero_totals(self, store: Path) -> None:
        # Session not in the store (JSONL hasn't landed yet): we still record the
        # boundary; totals are an available-but-empty rollup.
        assert run("stackunderflow-stop", {"session_id": "ghost", "cwd": "/x"}) == 0
        rows = _rows(store)
        assert len(rows) == 1
        assert rows[0]["event_kind"] == "boundary"
        totals = json.loads(rows[0]["payload_json"])["session_totals"]
        assert totals["available"] is True
        assert totals["message_count"] == 0
        assert totals["input_tokens"] == 0

    def test_stop_records_unavailable_totals_on_bare_store(self, bare_store: Path) -> None:
        # No schema applied: the sessions/messages tables don't exist, so the
        # totals query can't run — recorded as available=False, never an error.
        assert run("stackunderflow-stop", {"session_id": "s", "cwd": "/x"}) == 0
        rows = _rows(bare_store)
        assert len(rows) == 1
        assert json.loads(rows[0]["payload_json"])["session_totals"] == {"available": False}

    def test_pre_compact_records_snapshot_with_trigger(self, store: Path) -> None:
        assert run("stackunderflow-pre-compact",
                   {"hook_event_name": "PreCompact", "trigger": "auto", "session_id": "s"}) == 0
        rows = _rows(store)
        assert len(rows) == 1
        assert rows[0]["event_kind"] == "snapshot"
        assert json.loads(rows[0]["payload_json"])["trigger"] == "auto"


# ── capture-content opt-in ──────────────────────────────────────────────────


class TestCaptureContent:
    def test_full_payload_stored_when_opted_in(self, store: Path) -> None:
        payload_in = {
            "hook_event_name": "UserPromptSubmit",
            "prompt": "no, revert that — and here's a token: SECRET123",
            "session_id": "s",
        }
        assert run("stackunderflow-user-prompt", payload_in, capture_content=True) == 0
        rows = _rows(store)
        assert len(rows) == 1
        stored = json.loads(rows[0]["payload_json"])
        # The whole payload — prompt text included — is kept verbatim.
        assert stored == payload_in
        assert "SECRET123" in rows[0]["payload_json"]

    def test_failure_full_payload_when_opted_in(self, store: Path) -> None:
        payload_in = {"tool_name": "Bash", "session_id": "s",
                      "tool_input": {"command": "rm -rf /tmp/x"},
                      "tool_response": {"exit_code": 1, "stdout": "lots of output here"}}
        assert run("stackunderflow-post-tool-use", payload_in, capture_content=True) == 0
        stored = json.loads(_rows(store)[0]["payload_json"])
        assert stored == payload_in  # nothing dropped


# ── robustness: never raise, idempotent ─────────────────────────────────────


class TestRobustness:
    def test_unknown_hook_id_is_noop(self, store: Path) -> None:
        assert run("stackunderflow-bogus", {"session_id": "s"}) == 0
        assert run("totally-unrelated", {"session_id": "s"}) == 0
        assert _rows(store) == []

    @pytest.mark.parametrize("bad", [None, "not a dict", 42, [], {"weird": object}])
    def test_malformed_payload_never_raises(self, store: Path, bad) -> None:
        # ``store`` is here for its side effect — it points ``deps.store_path`` at
        # tmp, so a stray write never reaches the real store.
        assert run("stackunderflow-stop", bad) == 0  # never raises; rc always 0
        # A None/garbage payload yields no usable session/cwd; at worst a boundary
        # row with available=False, never an exception.

    def test_refire_same_event_is_idempotent(self, store: Path, monkeypatch) -> None:
        # Pin the timestamp so both fires collide on UNIQUE(ts, hook_id, session_id).
        from stackunderflow.hooks import handlers as h
        fixed = "2026-05-12T12:00:00.000000+00:00"

        class _FixedDT:
            @staticmethod
            def now(tz=None):
                from datetime import datetime as _dt
                return _dt.fromisoformat(fixed)

        monkeypatch.setattr(h, "datetime", _FixedDT)
        p = {"tool_name": "Bash", "session_id": "s", "tool_response": {"exit_code": 1}}
        assert run("stackunderflow-post-tool-use", p) == 0
        assert run("stackunderflow-post-tool-use", p) == 0  # re-fire
        assert len(_rows(store)) == 1  # second insert ignored

    def test_handler_swallows_internal_errors(self, store: Path, monkeypatch) -> None:
        # If the write path blows up, run() must still return 0.
        from stackunderflow.hooks import handlers as h
        monkeypatch.setattr(h, "_write_event", lambda **kw: (_ for _ in ()).throw(RuntimeError("kaboom")))
        assert run("stackunderflow-post-tool-use",
                   {"tool_name": "Bash", "session_id": "s", "tool_response": {"exit_code": 1}}) == 0


# ── performance budget ──────────────────────────────────────────────────────


class TestLatencyBudget:
    def test_handler_p99_under_50ms(self, store: Path) -> None:
        # Warm up (table create / first connection), then measure steady state.
        run("stackunderflow-post-tool-use", {"tool_name": "Bash", "session_id": "warm",
                                             "tool_response": {"exit_code": 1}})
        samples: list[float] = []
        for i in range(60):
            payload = {"tool_name": "Bash", "session_id": f"s{i}", "cwd": "/Users/dev/p",
                       "tool_response": {"exit_code": 1, "stderr": "err"}}
            t0 = time.perf_counter()
            run("stackunderflow-post-tool-use", payload)
            samples.append((time.perf_counter() - t0) * 1000.0)
        samples.sort()
        p99 = samples[int(len(samples) * 0.99) - 1]
        assert p99 < 50.0, f"handler p99 {p99:.2f}ms exceeds the 50ms budget (samples: {samples[-5:]})"

    def test_boundary_handler_also_cheap(self, store: Path) -> None:
        _seed_project_and_session(store, slug="-Users-dev-q", session_id="sb")
        run("stackunderflow-stop", {"session_id": "sb", "cwd": "/Users/dev/q"})  # warm
        samples: list[float] = []
        for _ in range(40):
            t0 = time.perf_counter()
            run("stackunderflow-stop", {"session_id": "sb", "cwd": "/Users/dev/q"})
            samples.append((time.perf_counter() - t0) * 1000.0)
        samples.sort()
        assert samples[int(len(samples) * 0.99) - 1] < 50.0


# ── re-exports ──────────────────────────────────────────────────────────────


def test_hook_ids_reexported() -> None:
    assert handlers.HOOK_IDS == (
        "stackunderflow-post-tool-use",
        "stackunderflow-user-prompt",
        "stackunderflow-stop",
        "stackunderflow-pre-compact",
    )
