"""Cross-session pattern mining — :mod:`stackunderflow.reports.patterns`.

Fixtures build a real store (schema-applied, messages + ``message_tool_mart``
rows) with recurring failures ACROSS sessions and assert the mined per-file
failure rates, error-signature recurrence, resolution hints, command
clusters, window bounding, and the advisory never-raises contract. A pinned
``now`` makes every window deterministic.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from itertools import count

from stackunderflow.reports.patterns import (
    MIN_RECURRENCE_SESSIONS,
    PatternsReport,
    _normalise_command,
    _normalise_signature,
    file_risk,
    mine_patterns,
)
from stackunderflow.store import db, schema

# Pinned clock — every fixture timestamp is relative to this, so windows
# never depend on the wall clock.
NOW = datetime(2026, 6, 30, 12, 0, 0, tzinfo=UTC)

_MART_MSG_IDS = count(1_000_000)  # unique message_id per mart row


def _ts(days_ago: float, minutes: int = 0) -> str:
    return (NOW - timedelta(days=days_ago) + timedelta(minutes=minutes)).isoformat()


# ── store seeding helpers ────────────────────────────────────────────────────


def _fresh_store(tmp_path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _add_project(conn, *, provider="claude", slug="demo") -> int:
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0, 0)",
        (provider, slug, slug),
    )
    return int(cur.lastrowid)


def _add_session(conn, project_id: int, session_id: str) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, NULL, NULL, 0)",
        (project_id, session_id),
    )
    return int(cur.lastrowid)


def _add_message(
    conn,
    session_fk: int,
    *,
    seq: int,
    ts: str,
    role: str,
    content_text: str = "",
    tools_json: str = "[]",
    raw_json: str = "{}",
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, speed) "
        "VALUES (?, ?, ?, ?, '', 0, 0, 0, 0, ?, ?, ?, 0, '', NULL, 'standard')",
        (session_fk, seq, ts, role, content_text, tools_json, raw_json),
    )


def _add_tool_call_msg(conn, session_fk: int, *, seq: int, ts: str, calls: list[dict]) -> None:
    """Assistant message carrying ``tool_use`` blocks (writer-shaped JSON)."""
    raw = {
        "type": "assistant",
        "timestamp": ts,
        "message": {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": c["id"], "name": c["name"], "input": c["input"]}
                for c in calls
            ],
        },
    }
    _add_message(
        conn, session_fk, seq=seq, ts=ts, role="assistant",
        tools_json=json.dumps(calls), raw_json=json.dumps(raw),
    )


def _add_error_result_msg(
    conn, session_fk: int, *, seq: int, ts: str, tool_use_id: str, text: str,
) -> None:
    """User message carrying an errored ``tool_result`` (writer-shaped JSON:
    stdlib ``json.dumps`` → the ``"is_error": true`` spacing the SQL screen
    must catch)."""
    raw = {
        "type": "user",
        "timestamp": ts,
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "is_error": True,
                    "content": text,
                }
            ],
        },
    }
    _add_message(
        conn, session_fk, seq=seq, ts=ts, role="user",
        content_text=text, raw_json=json.dumps(raw),
    )


def _add_mart_touch(
    conn, project_id: int, session_id: str, *, ts: str, tool: str, path: str | None,
) -> None:
    conn.execute(
        "INSERT INTO message_tool_mart "
        "(message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, NULL, 0)",
        (next(_MART_MSG_IDS), project_id, session_id, ts, ts[:10], tool, path),
    )


def _seed_failing_file(conn, pid: int, *, path="/repo/auth_test.py") -> None:
    """5 sessions touch *path*; sessions f1 + f2 fail an Edit on it.

    Expected: touch sessions 5, failure sessions 2 → failure_rate 0.4.
    """
    for i in range(1, 6):
        sid_txt = f"file-s{i}"
        sfk = _add_session(conn, pid, sid_txt)
        _add_mart_touch(conn, pid, sid_txt, ts=_ts(10, i), tool="Edit", path=path)
        if i <= 2:  # the two failing sessions
            tu = f"tu-file-{i}"
            _add_tool_call_msg(
                conn, sfk, seq=10, ts=_ts(10, i),
                calls=[{"id": tu, "name": "Edit", "input": {"file_path": path}}],
            )
            _add_error_result_msg(
                conn, sfk, seq=11, ts=_ts(10, i + 1), tool_use_id=tu,
                text=f"String to replace not found in {path}.",
            )
    conn.commit()


# ── unit: normalisers ────────────────────────────────────────────────────────


def test_signature_normalisation_collapses_paths_and_numbers():
    a = _normalise_signature("String not found in /Users/x/repo/module_a.py at line 42")
    b = _normalise_signature("String not found in /home/y/other/deep/module_a.py at line 7")
    assert a == b
    assert "module_a.py" in a
    assert "42" not in a and "<n>" in a


def test_signature_normalisation_handles_empty_and_multiline():
    assert _normalise_signature("") == "<empty error body>"
    multi = _normalise_signature("\n\n  Error: boom  \nsecond line ignored")
    assert multi == "Error: boom"


def test_command_normalisation_clusters_heads_and_subcommands():
    assert _normalise_command("npm install --no-fund left-pad") == "npm install"
    assert _normalise_command("cd /repo && npm install") == "npm install"
    assert _normalise_command("FOO=1 pytest tests/ -q") == "pytest"
    assert _normalise_command("/usr/bin/python3 scripts/build.py --fast") == "python3 build.py"
    assert _normalise_command("git push origin main") == "git push"
    assert _normalise_command("") == "<empty>"


# ── empty / degenerate stores ────────────────────────────────────────────────


def test_empty_store_returns_wellformed_zero_report(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()
    assert out["file_risk"] == []
    assert out["error_signatures"] == []
    assert out["command_clusters"] == []
    assert out["totals"]["error_count"] == 0
    assert out["totals"]["session_count"] == 0
    assert out["window"]["days"] == 90
    assert out["window"]["since"] == (NOW - timedelta(days=90)).isoformat()
    assert out["sources"]["message_tool_mart"] is True  # table exists, just empty


def test_bare_db_without_schema_is_advisory(tmp_path):
    conn = db.connect(tmp_path / "bare.db")
    try:
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()
    empty = PatternsReport().to_dict()
    assert set(out.keys()) == set(empty.keys())
    assert out["file_risk"] == []
    assert out["sources"]["message_tool_mart"] is False


# ── per-file risk ────────────────────────────────────────────────────────────


def test_file_failure_rate_across_sessions(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        _seed_failing_file(conn, pid)
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()

    assert len(out["file_risk"]) == 1
    entry = out["file_risk"][0]
    assert entry["path"] == "/repo/auth_test.py"
    assert entry["touch_count"] == 5
    assert entry["edit_count"] == 5
    assert entry["touch_session_count"] == 5
    assert entry["failure_count"] == 2
    assert entry["failure_session_count"] == 2
    assert entry["failure_rate"] == 0.4
    assert entry["categories"] == {"Content Not Found": 2}
    # Last failure is the later of the two error timestamps.
    assert entry["last_failure_ts"] == _ts(10, 3)
    assert entry["last_touch_ts"] == _ts(10, 5)
    assert "2 of 5" in entry["reason"] and "40%" in entry["reason"]


def test_files_without_failures_stay_out_of_the_risk_table(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        sid = "clean-s1"
        _add_session(conn, pid, sid)
        _add_mart_touch(conn, pid, sid, ts=_ts(5), tool="Edit", path="/repo/fine.py")
        conn.commit()
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()
    assert out["file_risk"] == []
    assert out["totals"]["files_touched"] == 1


def test_missing_mart_reports_untracked_rate_not_100pct(tmp_path):
    """Failures with NO touch history must not fabricate a failure rate."""
    conn = _fresh_store(tmp_path)
    try:
        conn.execute("DROP TABLE message_tool_mart")
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "nomart-s1")
        _add_tool_call_msg(
            conn, sfk, seq=1, ts=_ts(3),
            calls=[{"id": "tu-1", "name": "Edit", "input": {"file_path": "/repo/a.py"}}],
        )
        _add_error_result_msg(
            conn, sfk, seq=2, ts=_ts(3, 1), tool_use_id="tu-1",
            text="String to replace not found in /repo/a.py.",
        )
        conn.commit()
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()

    assert out["sources"]["message_tool_mart"] is False
    entry = out["file_risk"][0]
    assert entry["failure_count"] == 1
    assert entry["touch_count"] == 0
    assert entry["failure_rate"] is None
    assert "untracked" in entry["reason"]


# ── error signatures ─────────────────────────────────────────────────────────


def _seed_recurring_signature(conn, pid: int) -> None:
    """One signature in 3 sessions (different paths/lines each time); a
    second error that occurs in only ONE session (below recurrence floor).

    Sessions sig-s1 and sig-s3 have a tool call AFTER the last occurrence
    (resolved); sig-s2 ends on the error (unresolved).
    """
    specs = [
        ("sig-s1", "/Users/a/repo/module_a.py", 42, True, ("Edit", "/repo/fixture.py")),
        ("sig-s2", "/Users/b/proj/module_a.py", 7, False, None),
        ("sig-s3", "/srv/code/deep/module_a.py", 130, True, ("Bash", None)),
    ]
    for i, (sid_txt, path, line, resolved, next_action) in enumerate(specs, start=1):
        sfk = _add_session(conn, pid, sid_txt)
        tu = f"tu-sig-{i}"
        _add_tool_call_msg(
            conn, sfk, seq=1, ts=_ts(20, i),
            calls=[{"id": tu, "name": "Edit", "input": {"file_path": path}}],
        )
        _add_error_result_msg(
            conn, sfk, seq=2, ts=_ts(20, i + 1), tool_use_id=tu,
            text=f"String not found in {path} at line {line}",
        )
        if resolved and next_action is not None:
            tool, hint_path = next_action
            _add_mart_touch(
                conn, pid, sid_txt, ts=_ts(20, i + 30), tool=tool, path=hint_path,
            )
    # One-session-only error — must be excluded by the recurrence floor.
    sfk = _add_session(conn, pid, "sig-lonely")
    _add_tool_call_msg(
        conn, sfk, seq=1, ts=_ts(19),
        calls=[{"id": "tu-lonely", "name": "Bash", "input": {"command": "make lint"}}],
    )
    _add_error_result_msg(
        conn, sfk, seq=2, ts=_ts(19, 1), tool_use_id="tu-lonely",
        text="Permission denied: /etc/hosts",
    )
    conn.commit()


def test_error_signature_recurs_across_sessions(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        _seed_recurring_signature(conn, pid)
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()

    assert MIN_RECURRENCE_SESSIONS == 2
    assert len(out["error_signatures"]) == 1  # the lonely one is excluded
    sig = out["error_signatures"][0]
    assert sig["category"] == "Content Not Found"
    assert sig["count"] == 3
    assert sig["session_count"] == 3
    assert "module_a.py" in sig["signature"]
    assert "<n>" in sig["signature"]          # line numbers normalised away
    assert sig["top_tools"] == ["Edit"]
    assert sig["first_ts"] == _ts(20, 2)
    assert sig["last_ts"] == _ts(20, 4)
    assert sig["example"].startswith("String not found in ")


def test_resolution_hints_from_sessions_that_moved_on(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        _seed_recurring_signature(conn, pid)
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()

    sig = out["error_signatures"][0]
    # sig-s1 (Edit fixture.py after) and sig-s3 (Bash after) moved past it;
    # sig-s2 ended on the error.
    assert sig["resolved_session_count"] == 2
    hints = {h["action"]: h["count"] for h in sig["resolution_hints"]}
    assert hints == {"Edit fixture.py": 1, "Bash": 1}
    assert "2 moved past it" in sig["reason"]


# ── command clusters ─────────────────────────────────────────────────────────


def test_bash_failures_cluster_on_normalised_command(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        for i, cmd in enumerate(
            ["npm install --no-fund", "cd /repo && npm install left-pad"], start=1
        ):
            sid_txt = f"cmd-s{i}"
            sfk = _add_session(conn, pid, sid_txt)
            tu = f"tu-cmd-{i}"
            _add_tool_call_msg(
                conn, sfk, seq=1, ts=_ts(8, i),
                calls=[{"id": tu, "name": "Bash", "input": {"command": cmd}}],
            )
            _add_error_result_msg(
                conn, sfk, seq=2, ts=_ts(8, i + 1), tool_use_id=tu,
                text="Command timed out after 2m 0.0s",
            )
        # A single 'git push' failure — below the 2-failure cluster floor.
        sfk = _add_session(conn, pid, "cmd-s3")
        _add_tool_call_msg(
            conn, sfk, seq=1, ts=_ts(8),
            calls=[{"id": "tu-cmd-3", "name": "Bash", "input": {"command": "git push"}}],
        )
        _add_error_result_msg(
            conn, sfk, seq=2, ts=_ts(8, 1), tool_use_id="tu-cmd-3",
            text="Command timed out after 2m 0.0s",
        )
        conn.commit()
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()

    assert len(out["command_clusters"]) == 1
    cluster = out["command_clusters"][0]
    assert cluster["command"] == "npm install"
    assert cluster["failure_count"] == 2
    assert cluster["session_count"] == 2
    assert cluster["categories"] == {"Command Timeout": 2}
    assert cluster["last_failure_ts"] == _ts(8, 3)
    assert cluster["example"].startswith("npm install")
    assert "Command Timeout" in cluster["reason"]


# ── window bounding ──────────────────────────────────────────────────────────


def test_since_window_excludes_old_data(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        _seed_failing_file(conn, pid)  # ~10 days ago
        # Ancient failing file, 120 days back — outside the default window.
        sfk = _add_session(conn, pid, "old-s1")
        _add_mart_touch(conn, pid, "old-s1", ts=_ts(120), tool="Edit", path="/repo/old.py")
        _add_tool_call_msg(
            conn, sfk, seq=1, ts=_ts(120),
            calls=[{"id": "tu-old", "name": "Edit", "input": {"file_path": "/repo/old.py"}}],
        )
        _add_error_result_msg(
            conn, sfk, seq=2, ts=_ts(120, 1), tool_use_id="tu-old",
            text="String to replace not found in /repo/old.py.",
        )
        conn.commit()
        recent = mine_patterns(conn, since_days=90, now=NOW)
        wide = mine_patterns(conn, since_days=365, now=NOW)
    finally:
        conn.close()

    assert [f["path"] for f in recent["file_risk"]] == ["/repo/auth_test.py"]
    assert {f["path"] for f in wide["file_risk"]} == {"/repo/auth_test.py", "/repo/old.py"}
    assert recent["totals"]["error_count"] == 2
    assert wide["totals"]["error_count"] == 3


def test_since_days_is_clamped(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        out_low = mine_patterns(conn, since_days=0, now=NOW)
        out_high = mine_patterns(conn, since_days=99_999, now=NOW)
        out_junk = mine_patterns(conn, since_days="nope", now=NOW)  # type: ignore[arg-type]
    finally:
        conn.close()
    assert out_low["window"]["days"] == 1
    assert out_high["window"]["days"] == 365
    assert out_junk["window"]["days"] == 90


# ── project scoping ──────────────────────────────────────────────────────────


def test_project_ids_scope_the_report(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid_a = _add_project(conn, slug="alpha")
        pid_b = _add_project(conn, slug="beta")
        _seed_failing_file(conn, pid_a, path="/repo/a.py")
        sid = "beta-s1"
        _add_session(conn, pid_b, sid)
        _add_mart_touch(conn, pid_b, sid, ts=_ts(2), tool="Edit", path="/repo/b.py")
        conn.commit()

        only_a = mine_patterns(conn, project_ids=[pid_a], now=NOW)
        only_b = mine_patterns(conn, project_ids=[pid_b], now=NOW)
        nothing = mine_patterns(conn, project_ids=[], now=NOW)
    finally:
        conn.close()

    assert [f["path"] for f in only_a["file_risk"]] == ["/repo/a.py"]
    assert only_b["file_risk"] == []
    assert only_b["totals"]["files_touched"] == 1
    # Empty filter = requested-but-matched-nothing → empty report, not whole store.
    assert nothing["totals"]["session_count"] == 0
    assert nothing["file_risk"] == []


# ── interruptions ────────────────────────────────────────────────────────────


def test_interruptions_counted_and_attributed(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        for i in (1, 2):  # recurrence floor needs 2 sessions
            sid_txt = f"int-s{i}"
            sfk = _add_session(conn, pid, sid_txt)
            tu = f"tu-int-{i}"
            _add_tool_call_msg(
                conn, sfk, seq=1, ts=_ts(4, i),
                calls=[{"id": tu, "name": "Edit", "input": {"file_path": "/repo/risky.py"}}],
            )
            _add_error_result_msg(
                conn, sfk, seq=2, ts=_ts(4, i + 1), tool_use_id=tu,
                text="[Request interrupted by user for tool use]",
            )
        conn.commit()
        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()

    # Marker text counts in the interruption totals (content_text prefix)…
    assert out["totals"]["interruption_count"] == 2
    assert out["totals"]["interruption_session_count"] == 2
    # …is categorised as User Interruption…
    sig = out["error_signatures"][0]
    assert sig["category"] == "User Interruption"
    # …and lands on the file the user kept rejecting.
    entry = out["file_risk"][0]
    assert entry["path"] == "/repo/risky.py"
    assert entry["interruption_count"] == 2


# ── robustness: weird data must never raise ──────────────────────────────────


def test_weird_rows_never_raise(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        sfk = _add_session(conn, pid, "weird-s1")
        # Malformed raw_json that still matches the LIKE screen.
        _add_message(
            conn, sfk, seq=1, ts=_ts(2), role="user",
            raw_json='{"is_error": true broken json',
        )
        # Well-formed JSON, but content is not a dict.
        _add_message(conn, sfk, seq=2, ts=_ts(2, 1), role="user", raw_json='"is_error": true')
        # Error result whose tool_use_id matches nothing.
        _add_error_result_msg(
            conn, sfk, seq=3, ts=_ts(2, 2), tool_use_id="tu-ghost",
            text="Command timed out after 2m 0.0s",
        )
        # LIKE false positive: the literal text inside a normal body.
        _add_message(
            conn, sfk, seq=4, ts=_ts(2, 3), role="assistant",
            raw_json=json.dumps({
                "type": "assistant",
                "message": {"role": "assistant",
                            "content": [{"type": "text", "text": 'discussing "is_error": true'}]},
            }),
        )
        # Assistant row with garbage tools_json (attribution walks over it).
        _add_message(
            conn, sfk, seq=0, ts=_ts(2, -1), role="assistant", tools_json="not-json",
        )
        # Mart rows with NULL file_path and an unknown tool.
        _add_mart_touch(conn, pid, "weird-s1", ts=_ts(2), tool="Bash", path=None)
        _add_mart_touch(conn, pid, "weird-s1", ts=_ts(2), tool="Mystery", path="/x.py")
        conn.commit()

        out = mine_patterns(conn, now=NOW)
    finally:
        conn.close()

    # The one parseable error survives, unattributed; nothing raises.
    assert out["totals"]["error_count"] == 1
    assert out["totals"]["attributed_error_count"] == 0
    assert out["file_risk"] == []            # unknown tool / NULL path filtered
    assert out["command_clusters"] == []
    assert out["totals"]["tool_call_count"] == 2


def test_report_is_deterministic(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        _seed_failing_file(conn, pid)
        _seed_recurring_signature(conn, pid)
        one = mine_patterns(conn, now=NOW)
        two = mine_patterns(conn, now=NOW)
    finally:
        conn.close()
    assert one == two


# ── file_risk(): the programmatic per-file lookup (feeds campaign #5) ────────


def test_file_risk_exact_and_suffix_lookup(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        _seed_failing_file(conn, pid, path="/repo/auth_test.py")
        conn.commit()

        exact = file_risk(conn, "/repo/auth_test.py", now=NOW)
        suffix = file_risk(conn, "auth_test.py", now=NOW)
        missing = file_risk(conn, "/nowhere/void.py", now=NOW)
    finally:
        conn.close()

    assert exact["failure_rate"] == 0.4
    assert exact["failure_session_count"] == 2
    # Repo-relative lookup resolves through the unique suffix match.
    assert suffix["path"] == "/repo/auth_test.py"
    assert suffix["failure_rate"] == 0.4
    # Unknown file → well-formed zero entry, never a raise.
    assert missing["path"] == "/nowhere/void.py"
    assert missing["failure_count"] == 0
    assert missing["failure_rate"] is None


def test_file_risk_on_bare_db_is_advisory(tmp_path):
    conn = db.connect(tmp_path / "bare.db")
    try:
        out = file_risk(conn, "anything.py", now=NOW)
    finally:
        conn.close()
    assert out["path"] == "anything.py"
    assert out["failure_count"] == 0
