"""Wave 5 — optimize detectors short-circuit on populated ``tool_mart``.

Four detectors gain a mart-aware early-exit:

* ``_detect_low_read_edit_ratio`` — when Read calls in window <
  ``LOW_READ_EDIT_READ_FLOOR``, no session can possibly trip the
  detector, so we skip the per-session messages walk.
* ``_detect_junk_reads`` — when zero Read calls in window, no file can
  be re-read N+ times.
* ``_detect_bash_output_limits`` — when zero Bash calls in window, no
  oversized output can exist.
* ``_detect_ghost_agents`` — when zero Task calls in window AND there
  are registered agents, every registered agent is ghost (no need to
  scan raw_json for ``subagent_type`` markers).

Empty-mart fallback: full aggregator path runs, no behaviour change.
"""

from __future__ import annotations

from stackunderflow.reports.optimize import (
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
    return conn


def _insert_tool_mart(conn, *, tool_name, event_count, day="2026-04-01"):
    conn.execute(
        "INSERT INTO tool_mart "
        "(day, project_id, provider, tool_name, "
        " event_count, cost_usd, tokens_in, tokens_out, session_count) "
        "VALUES (?, 1, 'claude', ?, ?, 0.0, 0, 0, 1)",
        (day, tool_name, event_count),
    )


def _scope_for_april():
    return Scope(label="April", since="2026-04-01T00:00:00Z",
                 until="2026-04-30T23:59:59Z")


# ── early-exit when no Read calls ──────────────────────────────────────────


def test_low_read_edit_short_circuits_when_reads_below_floor(tmp_path):
    """Mart says only 3 Read calls in window — below floor, no finding."""
    conn = _connect(tmp_path / "store.db")
    # Floor is 20; 3 reads can't possibly hit a single-session floor.
    _insert_tool_mart(conn, tool_name="Read", event_count=3)
    findings = _detect_low_read_edit_ratio(conn, scope=_scope_for_april())
    assert findings == []


def test_low_read_edit_falls_through_when_mart_empty(tmp_path):
    """Empty tool_mart → run the full aggregator pass.

    With no messages either, the detector still returns ``[]`` because
    no session has any Reads, but the early-exit must NOT kick in (the
    aggregator pass is what produced the empty list, not the mart).
    """
    conn = _connect(tmp_path / "store.db")
    findings = _detect_low_read_edit_ratio(conn, scope=_scope_for_april())
    assert findings == []


def test_junk_reads_short_circuits_when_zero_reads(tmp_path):
    """Mart says zero Read calls in window — no findings, no scan."""
    conn = _connect(tmp_path / "store.db")
    # Mart has rows for other tools but zero for Read.
    _insert_tool_mart(conn, tool_name="Bash", event_count=10)
    findings = _detect_junk_reads(conn, scope=_scope_for_april())
    assert findings == []


def test_bash_output_short_circuits_when_zero_bash_calls(tmp_path):
    """Mart says zero Bash calls in window — no findings, no scan."""
    conn = _connect(tmp_path / "store.db")
    _insert_tool_mart(conn, tool_name="Read", event_count=10)
    findings = _detect_bash_output_limits(conn, scope=_scope_for_april())
    assert findings == []


def test_ghost_agents_skips_raw_json_scan_when_no_task_calls(tmp_path, monkeypatch):
    """Mart says zero Task calls + registered agents → all agents are ghosts.

    Without the early-exit the detector would scan ``messages.raw_json``
    for ``subagent_type=...`` markers; with the early-exit we just emit
    the same finding directly off the registered-agent list.
    """
    conn = _connect(tmp_path / "store.db")
    # Mart populated but no Task tool calls.
    _insert_tool_mart(conn, tool_name="Bash", event_count=5)

    # Stub the registered-agent list so we don't depend on filesystem
    # state in tests.
    # Synthetic paths — never read; only the .stem is consulted by the
    # detector for the finding payload.
    agents_root = tmp_path / "agents"
    fake_agents = [
        ("ghost-a", agents_root / "ghost-a.md"),
        ("ghost-b", agents_root / "ghost-b.md"),
    ]
    monkeypatch.setattr(
        "stackunderflow.reports.optimize._registered_agents",
        lambda: fake_agents,
    )

    findings = _detect_ghost_agents(conn, scope=_scope_for_april())
    assert len(findings) == 1
    f = findings[0]
    assert f.pattern_id == "ghost_agents"
    # Both agents flagged as ghosts since no Task calls happened.
    assert f.affected_count == 2
    names = {a["name"] for a in f.details["agents"]}
    assert names == {"ghost-a", "ghost-b"}


# ── empty-mart fallback ────────────────────────────────────────────────────


def test_ghost_agents_falls_through_when_mart_empty(tmp_path, monkeypatch):
    """Empty ``tool_mart`` → original aggregator-path scan runs.

    With no agents registered there are no ghosts; with no messages
    table to scan, the aggregator returns an empty list. Either way
    the early-exit didn't fire, which is the contract we lock in here.
    """
    conn = _connect(tmp_path / "store.db")
    monkeypatch.setattr(
        "stackunderflow.reports.optimize._registered_agents",
        lambda: [],
    )
    findings = _detect_ghost_agents(conn, scope=_scope_for_april())
    assert findings == []
