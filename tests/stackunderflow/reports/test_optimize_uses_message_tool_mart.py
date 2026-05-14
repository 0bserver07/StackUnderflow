"""v011 — optimize detectors read from ``message_tool_mart`` when populated.

Four detectors gain a per-message-mart fast path that *replaces* the
raw ``messages`` scan entirely (not just an early-exit like the Wave 5
``tool_mart`` filter):

* ``_detect_junk_reads`` — per-(session, file) Read counts via
  ``GROUP BY ... HAVING``.
* ``_detect_bash_output_limits`` — Bash mart rows whose ``byte_count``
  exceeds the threshold (the result size is already paired into the mart).
* ``_detect_low_read_edit_ratio`` — per-session (Read, Edit) counts.
* ``_detect_ghost_agents`` — invoked-agent set from the ``Task`` rows'
  ``file_path`` (which holds the ``subagent_type``).

Empty mart → the original raw-scan / Wave 5 short-circuit path runs
unchanged. The parity tests below assert the mart path and the raw-scan
path produce the same findings on the same logical data.
"""

from __future__ import annotations

import json

from stackunderflow.reports import optimize as optimize_mod
from stackunderflow.reports.optimize import (
    JUNK_READ_REPEAT_THRESHOLD,
    LOW_READ_EDIT_READ_FLOOR,
    _detect_bash_output_limits,
    _detect_ghost_agents,
    _detect_junk_reads,
    _detect_low_read_edit_ratio,
)
from stackunderflow.reports.scope import Scope
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (1, 'claude', 'fixture-proj', 'fixture-proj', 0, 0)"
    )
    return conn


def _insert_mt(
    conn, *, message_id, tool_name, call_index=0, session_id="sess-A",
    project_id=1, file_path=None, byte_count=None, day="2026-04-10",
    ts="2026-04-10T00:00:00Z",
):
    conn.execute(
        "INSERT INTO message_tool_mart "
        "(message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (message_id, project_id, session_id, ts, day, tool_name, file_path, byte_count, call_index),
    )


def _april_scope():
    return Scope(label="April", since="2026-04-01T00:00:00Z", until="2026-04-30T23:59:59Z")


# ── junk_reads from mart ───────────────────────────────────────────────


def test_junk_reads_from_mart_emits_finding(tmp_path):
    conn = _connect(tmp_path / "store.db")
    # Same file Read JUNK_READ_REPEAT_THRESHOLD times in one session.
    for i in range(JUNK_READ_REPEAT_THRESHOLD):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path="/repo/foo.py")
    # A different file Read once — below threshold, must not show up.
    _insert_mt(conn, message_id=200, tool_name="Read", file_path="/repo/bar.py")
    findings = _detect_junk_reads(conn, scope=_april_scope())
    assert len(findings) == 1
    f = findings[0]
    assert f.pattern_id == "junk_reads"
    assert f.affected_count == 1
    files = {ff["path"] for s in f.details["sessions"] for ff in s["files"]}
    assert files == {"/repo/foo.py"}


def test_junk_reads_from_mart_below_threshold_no_finding(tmp_path):
    conn = _connect(tmp_path / "store.db")
    for i in range(JUNK_READ_REPEAT_THRESHOLD - 1):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path="/repo/foo.py")
    assert _detect_junk_reads(conn, scope=_april_scope()) == []


def test_junk_reads_from_mart_ignores_null_file_path(tmp_path):
    conn = _connect(tmp_path / "store.db")
    for i in range(JUNK_READ_REPEAT_THRESHOLD + 3):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path=None)
    assert _detect_junk_reads(conn, scope=_april_scope()) == []


def test_junk_reads_from_mart_respects_project_filter(tmp_path):
    conn = _connect(tmp_path / "store.db")
    conn.execute(
        "INSERT INTO projects (id, provider, slug, display_name, first_seen, last_modified) "
        "VALUES (2, 'claude', 'other-proj', 'other-proj', 0, 0)"
    )
    for i in range(JUNK_READ_REPEAT_THRESHOLD + 1):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path="/repo/foo.py", project_id=2)
    # Filtering to a project with no junk reads → nothing.
    assert _detect_junk_reads(conn, scope=_april_scope(), project_filter=["fixture-proj"]) == []
    # Filtering to the project that has them → finding.
    assert len(_detect_junk_reads(conn, scope=_april_scope(), project_filter=["other-proj"])) == 1


# ── bash_output from mart ──────────────────────────────────────────────


def test_bash_output_from_mart_emits_finding(tmp_path):
    conn = _connect(tmp_path / "store.db")
    _insert_mt(conn, message_id=100, tool_name="Bash", byte_count=optimize_mod.BASH_OUTPUT_BYTES_THRESHOLD + 1)
    _insert_mt(conn, message_id=101, tool_name="Bash", byte_count=10)  # small — ignored
    _insert_mt(conn, message_id=102, tool_name="Read", byte_count=999_999)  # not Bash — ignored
    findings = _detect_bash_output_limits(conn, scope=_april_scope())
    assert len(findings) == 1
    assert findings[0].pattern_id == "bash_output_limits"
    assert findings[0].affected_count == 1


def test_bash_output_from_mart_no_oversized_no_finding(tmp_path):
    conn = _connect(tmp_path / "store.db")
    _insert_mt(conn, message_id=100, tool_name="Bash", byte_count=10)
    # A Bash row with NULL byte_count (no result paired) must not trip it either.
    _insert_mt(conn, message_id=101, tool_name="Bash", byte_count=None)
    assert _detect_bash_output_limits(conn, scope=_april_scope()) == []


# ── low_read_edit from mart ────────────────────────────────────────────


def test_low_read_edit_from_mart_emits_finding(tmp_path):
    conn = _connect(tmp_path / "store.db")
    # Session with floor+2 Reads and zero Edits.
    for i in range(LOW_READ_EDIT_READ_FLOOR + 2):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path=f"/f{i}", session_id="sess-explore")
    # Another session with many Reads but also an Edit — must NOT flag.
    for i in range(LOW_READ_EDIT_READ_FLOOR + 5):
        _insert_mt(conn, message_id=300 + i, tool_name="Read", file_path=f"/g{i}", session_id="sess-mixed")
    _insert_mt(conn, message_id=400, tool_name="Edit", file_path="/g0", byte_count=5, session_id="sess-mixed")
    findings = _detect_low_read_edit_ratio(conn, scope=_april_scope())
    assert len(findings) == 1
    f = findings[0]
    assert f.pattern_id == "low_read_edit_ratio"
    assert f.affected_count == 1
    assert {s["session_fk"] for s in f.details["sessions"]} == {"sess-explore"}


def test_low_read_edit_from_mart_below_floor_no_finding(tmp_path):
    conn = _connect(tmp_path / "store.db")
    for i in range(LOW_READ_EDIT_READ_FLOOR - 1):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path=f"/f{i}", session_id="s")
    assert _detect_low_read_edit_ratio(conn, scope=_april_scope()) == []


def test_low_read_edit_from_mart_counts_multiedit_as_edit(tmp_path):
    conn = _connect(tmp_path / "store.db")
    for i in range(LOW_READ_EDIT_READ_FLOOR + 2):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path=f"/f{i}", session_id="s")
    _insert_mt(conn, message_id=500, tool_name="MultiEdit", file_path="/f0", byte_count=3, session_id="s")
    assert _detect_low_read_edit_ratio(conn, scope=_april_scope()) == []


# ── ghost_agents from mart ─────────────────────────────────────────────


def test_ghost_agents_from_mart(tmp_path, monkeypatch):
    conn = _connect(tmp_path / "store.db")
    agents_root = tmp_path / "agents"
    monkeypatch.setattr(
        "stackunderflow.reports.optimize._registered_agents",
        lambda: [
            ("explorer", agents_root / "explorer.md"),
            ("reviewer", agents_root / "reviewer.md"),
            ("ghostly", agents_root / "ghostly.md"),
        ],
    )
    # explorer + reviewer were invoked via Task; ghostly never was.
    _insert_mt(conn, message_id=100, tool_name="Task", file_path="explorer")
    _insert_mt(conn, message_id=101, tool_name="Task", file_path="reviewer")
    # A Task with NULL subagent_type doesn't rescue any named agent.
    _insert_mt(conn, message_id=102, tool_name="Task", file_path=None)
    findings = _detect_ghost_agents(conn, scope=_april_scope())
    assert len(findings) == 1
    f = findings[0]
    assert f.pattern_id == "ghost_agents"
    assert f.affected_count == 1
    assert {a["name"] for a in f.details["agents"]} == {"ghostly"}


def test_ghost_agents_from_mart_all_ghost_when_no_task_rows(tmp_path, monkeypatch):
    """Mart populated (Read rows) but zero Task rows → every agent is ghost."""
    conn = _connect(tmp_path / "store.db")
    agents_root = tmp_path / "agents"
    monkeypatch.setattr(
        "stackunderflow.reports.optimize._registered_agents",
        lambda: [("a", agents_root / "a.md"), ("b", agents_root / "b.md")],
    )
    _insert_mt(conn, message_id=100, tool_name="Read", file_path="/f")
    findings = _detect_ghost_agents(conn, scope=_april_scope())
    assert len(findings) == 1
    assert findings[0].affected_count == 2


# ── parity: mart path == raw-scan path ─────────────────────────────────


def _seed_msg(conn, *, session_fk, seq, role, raw, timestamp="2026-04-10T10:00:00Z", content_text=""):
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, 'claude-sonnet-4-6', 0, 0, 0, 0, ?, '[]', ?, 0, ?, NULL)",
        (session_fk, seq, timestamp, role, content_text, json.dumps(raw), f"u{session_fk}-{seq}"),
    )


def _assistant_tool_use_raw(name, inp):
    return {"message": {"role": "assistant", "content": [{"type": "tool_use", "name": name, "input": inp}]}}


def test_junk_reads_parity_mart_vs_raw_scan(tmp_path):
    """Same junk-read pattern → identical finding via mart and via raw scan."""
    n_junk = JUNK_READ_REPEAT_THRESHOLD + 1
    path = "/repo/hot.py"

    # ── A) raw-scan path: seed messages, leave message_tool_mart empty ──
    raw_conn = _connect(tmp_path / "raw.db")
    raw_conn.execute("INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 'sess-A')")
    for i in range(n_junk):
        _seed_msg(
            raw_conn, session_fk=1, seq=i, role="assistant",
            raw=_assistant_tool_use_raw("Read", {"file_path": path}),
        )
    raw_findings = _detect_junk_reads(raw_conn, scope=_april_scope())

    # ── B) mart path: seed message_tool_mart rows mirroring the same data ──
    mart_conn = _connect(tmp_path / "mart.db")
    for i in range(n_junk):
        _insert_mt(mart_conn, message_id=10 + i, tool_name="Read", file_path=path, session_id="sess-A")
    mart_findings = _detect_junk_reads(mart_conn, scope=_april_scope())

    assert len(raw_findings) == len(mart_findings) == 1
    assert raw_findings[0].pattern_id == mart_findings[0].pattern_id
    assert raw_findings[0].affected_count == mart_findings[0].affected_count
    assert raw_findings[0].severity == mart_findings[0].severity
    raw_files = {ff["path"] for s in raw_findings[0].details["sessions"] for ff in s["files"]}
    mart_files = {ff["path"] for s in mart_findings[0].details["sessions"] for ff in s["files"]}
    assert raw_files == mart_files == {path}


def test_empty_mart_falls_through_to_raw_scan(tmp_path):
    """With ``message_tool_mart`` empty, the detectors run the raw-scan path.

    Seed only ``messages`` (no mart rows) and confirm the finding still
    fires — proving the empty-mart gate handed off correctly.
    """
    conn = _connect(tmp_path / "store.db")
    conn.execute("INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 'sess-A')")
    for i in range(JUNK_READ_REPEAT_THRESHOLD + 1):
        _seed_msg(conn, session_fk=1, seq=i, role="assistant",
                  raw=_assistant_tool_use_raw("Read", {"file_path": "/repo/x.py"}))
    # Mart is empty → gate is False → raw scan runs.
    assert conn.execute("SELECT COUNT(*) AS n FROM message_tool_mart").fetchone()["n"] == 0
    findings = _detect_junk_reads(conn, scope=_april_scope())
    assert len(findings) == 1 and findings[0].pattern_id == "junk_reads"


def test_low_read_edit_parity_mart_vs_raw_scan(tmp_path):
    """Same exploration-only session → identical finding via mart and via raw scan.

    Raw-scan counts Read names off ``tools_json`` (one increment per
    name occurrence); the mart counts one row per call. The seed below
    keeps the two equivalent — one ``tools_json: ["Read"]`` per
    assistant message, one mart row per message — so the per-session
    Read total matches across paths.
    """
    n_reads = LOW_READ_EDIT_READ_FLOOR + 3

    # ── A) raw-scan path ──
    raw_conn = _connect(tmp_path / "raw.db")
    raw_conn.execute("INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 'sess-explore')")
    for i in range(n_reads):
        # Raw-scan path keys on `tools_json` — list every Read by name.
        raw_conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (1, ?, '2026-04-10T10:00:00Z', 'assistant', 'claude-sonnet-4-6', "
            " 0, 0, 0, 0, '', '[\"Read\"]', '{}', 0, ?, NULL)",
            (i, f"u{i}"),
        )
    raw_findings = _detect_low_read_edit_ratio(raw_conn, scope=_april_scope())

    # ── B) mart path ──
    mart_conn = _connect(tmp_path / "mart.db")
    for i in range(n_reads):
        _insert_mt(mart_conn, message_id=10 + i, tool_name="Read",
                   file_path=f"/f{i}", session_id="sess-explore")
    mart_findings = _detect_low_read_edit_ratio(mart_conn, scope=_april_scope())

    assert len(raw_findings) == len(mart_findings) == 1
    assert raw_findings[0].pattern_id == mart_findings[0].pattern_id == "low_read_edit_ratio"
    assert raw_findings[0].affected_count == mart_findings[0].affected_count == 1
    assert raw_findings[0].severity == mart_findings[0].severity
    # Both report exactly the same Read total — n_reads — even though
    # ``session_fk`` differs (int vs string). The waste estimate is a
    # pure function of that total.
    assert raw_findings[0].estimated_waste_tokens == mart_findings[0].estimated_waste_tokens


def test_ghost_agents_parity_mart_vs_raw_scan(tmp_path, monkeypatch):
    """Same registered-agent set + spawn pattern → identical ghost set.

    Raw-scan substring-matches ``"subagent_type":"<name>"`` against
    every ``raw_json`` whose ``tools_json`` mentions ``Task``; the mart
    SELECTs DISTINCT ``file_path`` from ``Task`` rows. Both should
    bucket the *same* agent as ghosted.
    """
    agents_root = tmp_path / "agents"
    monkeypatch.setattr(
        "stackunderflow.reports.optimize._registered_agents",
        lambda: [
            ("alpha", agents_root / "alpha.md"),
            ("beta", agents_root / "beta.md"),
            ("gamma", agents_root / "gamma.md"),
        ],
    )

    # ── A) raw-scan path: ``alpha`` and ``beta`` invoked; ``gamma`` ghost ──
    raw_conn = _connect(tmp_path / "raw.db")
    raw_conn.execute("INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 'sess-A')")
    raw_conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (1, 0, '2026-04-10T10:00:00Z', 'assistant', 'claude-sonnet-4-6', "
        " 0, 0, 0, 0, '', '[\"Task\"]', ?, 0, 'u1', NULL)",
        (json.dumps({"message": {"role": "assistant", "content": [
            {"type": "tool_use", "name": "Task", "input": {"subagent_type": "alpha"}},
            {"type": "tool_use", "name": "Task", "input": {"subagent_type": "beta"}},
        ]}}),),
    )
    raw_findings = _detect_ghost_agents(raw_conn, scope=_april_scope())

    # ── B) mart path: same Task spawns recorded in message_tool_mart ──
    mart_conn = _connect(tmp_path / "mart.db")
    _insert_mt(mart_conn, message_id=100, tool_name="Task", file_path="alpha", call_index=0)
    _insert_mt(mart_conn, message_id=100, tool_name="Task", file_path="beta", call_index=1)
    mart_findings = _detect_ghost_agents(mart_conn, scope=_april_scope())

    assert len(raw_findings) == len(mart_findings) == 1
    raw_names = {a["name"] for a in raw_findings[0].details["agents"]}
    mart_names = {a["name"] for a in mart_findings[0].details["agents"]}
    assert raw_names == mart_names == {"gamma"}
    assert raw_findings[0].affected_count == mart_findings[0].affected_count == 1
    assert raw_findings[0].severity == mart_findings[0].severity


def test_bash_output_parity_mart_vs_raw_scan(tmp_path, monkeypatch):
    """Same oversized Bash result → identical finding via mart and via raw scan.

    The raw-scan path sizes the *following* user message's
    ``content_text`` as a proxy for the tool result; the mart path uses
    the ``byte_count`` it pre-computed off the ``tool_result`` block. We
    lower the threshold to make the test cheap, but the underlying
    detection has to agree — both paths should fire.
    """
    monkeypatch.setattr(
        "stackunderflow.reports.optimize.BASH_OUTPUT_BYTES_THRESHOLD", 50,
    )
    big_text = "x" * 100  # 100 bytes — clears the 50-byte test threshold

    # ── A) raw-scan path ──
    raw_conn = _connect(tmp_path / "raw.db")
    raw_conn.execute("INSERT INTO sessions (id, project_id, session_id) VALUES (1, 1, 'sess-A')")
    raw_conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (1, 0, '2026-04-10T10:00:00Z', 'assistant', 'claude-sonnet-4-6', "
        " 0, 0, 0, 0, '', '[\"Bash\"]', ?, 0, 'u1', NULL)",
        (json.dumps({"message": {"role": "assistant", "content": [
            {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
        ]}}),),
    )
    raw_conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (1, 1, '2026-04-10T10:01:00Z', 'user', 'claude-sonnet-4-6', "
        " 0, 0, 0, 0, ?, '[]', '{}', 0, 'u2', NULL)",
        (big_text,),
    )
    raw_findings = _detect_bash_output_limits(raw_conn, scope=_april_scope())

    # ── B) mart path: one Bash row whose byte_count clears the threshold ──
    mart_conn = _connect(tmp_path / "mart.db")
    _insert_mt(mart_conn, message_id=100, tool_name="Bash", byte_count=100)
    mart_findings = _detect_bash_output_limits(mart_conn, scope=_april_scope())

    assert len(raw_findings) == len(mart_findings) == 1
    assert raw_findings[0].pattern_id == mart_findings[0].pattern_id == "bash_output_limits"
    assert raw_findings[0].affected_count == mart_findings[0].affected_count == 1
    assert raw_findings[0].severity == mart_findings[0].severity
    # Both paths read 100 bytes for the one oversized call → identical estimate.
    assert raw_findings[0].estimated_waste_tokens == mart_findings[0].estimated_waste_tokens


def test_all_detectors_no_crash_on_empty_store(tmp_path, monkeypatch):
    """Empty store (no messages, no mart) → every detector returns []."""
    conn = _connect(tmp_path / "store.db")
    monkeypatch.setattr("stackunderflow.reports.optimize._registered_agents", lambda: [])
    assert _detect_junk_reads(conn, scope=_april_scope()) == []
    assert _detect_bash_output_limits(conn, scope=_april_scope()) == []
    assert _detect_low_read_edit_ratio(conn, scope=_april_scope()) == []
    assert _detect_ghost_agents(conn, scope=_april_scope()) == []


# ── all 4 detectors bypass raw_json when mart is populated ──────────────


def test_all_detectors_skip_raw_json_when_mart_populated(tmp_path, monkeypatch):
    """When ``message_tool_mart`` has rows, none of the 4 migrated detectors
    fall back to the raw ``messages`` scan.

    Locks in the v011 migration contract: the mart fully *replaces* the
    raw-json parse for ``junk_reads``, ``bash_output_limits``,
    ``low_read_edit_ratio`` and ``ghost_agents``. We seed the mart with
    the exact rows each detector needs, leave ``messages`` empty, and
    confirm every detector still emits its finding — proving the mart
    path is self-sufficient (a raw-scan path that depended on
    ``messages`` would emit nothing here).
    """
    conn = _connect(tmp_path / "store.db")
    monkeypatch.setattr(
        "stackunderflow.reports.optimize._registered_agents",
        lambda: [
            ("registered", tmp_path / "registered.md"),
            ("ghosty", tmp_path / "ghosty.md"),
        ],
    )

    # junk_reads — same file Read JUNK_READ_REPEAT_THRESHOLD times.
    for i in range(JUNK_READ_REPEAT_THRESHOLD):
        _insert_mt(conn, message_id=100 + i, tool_name="Read", file_path="/repo/hot.py")

    # low_read_edit — sess-explore has many Reads, zero Edits.
    for i in range(LOW_READ_EDIT_READ_FLOOR + 2):
        _insert_mt(conn, message_id=200 + i, tool_name="Read",
                   file_path=f"/f{i}", session_id="sess-explore")

    # bash_output_limits — oversized Bash byte_count.
    _insert_mt(conn, message_id=300, tool_name="Bash",
               byte_count=optimize_mod.BASH_OUTPUT_BYTES_THRESHOLD + 1,
               session_id="sess-bash")

    # ghost_agents — only ``registered`` was invoked via Task; ``ghosty`` wasn't.
    _insert_mt(conn, message_id=400, tool_name="Task", file_path="registered",
               session_id="sess-ghost")

    # ── precondition: no messages rows ──
    assert conn.execute("SELECT COUNT(*) AS n FROM messages").fetchone()["n"] == 0

    # ── all 4 detectors still fire — proving they're mart-only when populated ──
    jr = _detect_junk_reads(conn, scope=_april_scope())
    le = _detect_low_read_edit_ratio(conn, scope=_april_scope())
    bo = _detect_bash_output_limits(conn, scope=_april_scope())
    ga = _detect_ghost_agents(conn, scope=_april_scope())

    assert len(jr) == 1 and jr[0].pattern_id == "junk_reads"
    assert len(le) == 1 and le[0].pattern_id == "low_read_edit_ratio"
    assert len(bo) == 1 and bo[0].pattern_id == "bash_output_limits"
    assert len(ga) == 1 and ga[0].pattern_id == "ghost_agents"
    # ``ghosty`` is the only un-invoked registered agent.
    assert {a["name"] for a in ga[0].details["agents"]} == {"ghosty"}
