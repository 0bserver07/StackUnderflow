"""Fork / sidechain economics — :mod:`stackunderflow.reports.forks`.

Fixtures build a real message DAG in the store (sidechain flags + parent_uuid
branches) and assert the priced sidechain share and the abandoned-branch math.
A deterministic injected ``compute_cost`` (``$0.001`` per token) makes every
dollar figure exact and independent of the live pricing tables.
"""

from __future__ import annotations

from typing import Any

import pytest

from stackunderflow.reports.forks import (
    MIN_BRANCH_COST_USD,
    ForkReport,
    analyze_forks,
)
from stackunderflow.store import db, schema


# ── deterministic pricer ─────────────────────────────────────────────────────


def _fake_cost(tokens: dict[str, int], model: str, provider: str = "anthropic", *, speed: str = "standard") -> dict[str, float]:
    """$0.001 per token, summed across all token buckets."""
    total = sum(int(v or 0) for v in tokens.values())
    return {"total_cost": total * 0.001}


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


def _add_msg(
    conn,
    session_fk: int,
    *,
    seq: int,
    ts: str,
    role: str,
    uuid: str,
    parent_uuid: str | None,
    model: str = "claude-opus-4-6",
    is_sidechain: bool = False,
    input_tokens: int = 0,
    output_tokens: int = 0,
    cache_create_tokens: int = 0,
    cache_read_tokens: int = 0,
) -> None:
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, speed) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '', '[]', '{}', ?, ?, ?, 'standard')",
        (
            session_fk, seq, ts, role, model,
            input_tokens, output_tokens, cache_create_tokens, cache_read_tokens,
            int(is_sidechain), uuid, parent_uuid,
        ),
    )


def _seed_branched_session(conn) -> int:
    """Build one session with a clear fork + abandoned sidechain branch.

    DAG (times ascending → the live path is the one that reaches the latest ts):

        U0 (user, root)
        └─ A0 (assistant)                      <- FORK POINT (2 distinct children)
           ├─ U1 (user)  ─ A1 (assistant)      <- LIVE branch, latest activity
           └─ B0 (assistant, SIDECHAIN)        <- ABANDONED branch head
              └─ B1 (assistant, SIDECHAIN)     <- subtree ends early

    Tokens (→ $0.001/token via the fake pricer):
        A0: 100 tok  = $0.10   (assistant, main path, not sidechain)
        A1: 200 tok  = $0.20   (assistant, main path)
        B0: 300 tok  = $0.30   (assistant, SIDECHAIN, abandoned)
        B1: 400 tok  = $0.40   (assistant, SIDECHAIN, abandoned)
      user turns U0/U1: 0 cost (not assistant)

    Expected:
        total_cost       = 0.10 + 0.20 + 0.30 + 0.40 = 1.00
        sidechain_cost   = 0.30 + 0.40 = 0.70
        abandoned subtree(B0) cost = 0.30 + 0.40 = 0.70  (B0 + B1)
    """
    pid = _add_project(conn)
    sid = _add_session(conn, pid, "sess-fork")

    _add_msg(conn, sid, seq=0, ts="2026-05-01T10:00:00+00:00", role="user",
             uuid="U0", parent_uuid=None, model="")
    _add_msg(conn, sid, seq=1, ts="2026-05-01T10:00:10+00:00", role="assistant",
             uuid="A0", parent_uuid="U0", input_tokens=100)
    # Live branch — reaches the latest timestamp in the session.
    _add_msg(conn, sid, seq=2, ts="2026-05-01T10:05:00+00:00", role="user",
             uuid="U1", parent_uuid="A0", model="")
    _add_msg(conn, sid, seq=3, ts="2026-05-01T10:06:00+00:00", role="assistant",
             uuid="A1", parent_uuid="U1", output_tokens=200)
    # Abandoned sidechain branch — stops at 10:01, well before 10:06.
    _add_msg(conn, sid, seq=4, ts="2026-05-01T10:00:30+00:00", role="assistant",
             uuid="B0", parent_uuid="A0", is_sidechain=True, input_tokens=300)
    _add_msg(conn, sid, seq=5, ts="2026-05-01T10:01:00+00:00", role="assistant",
             uuid="B1", parent_uuid="B0", is_sidechain=True, output_tokens=400)

    conn.commit()
    return pid


# ── tests: empty / degenerate ────────────────────────────────────────────────


def test_empty_store_returns_wellformed_zero_report(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        out = analyze_forks(conn, compute_cost=_fake_cost)
    finally:
        conn.close()

    assert out == ForkReport().to_dict()
    assert out["sidechain_cost_usd"] == 0.0
    assert out["abandoned_branches"] == []
    assert out["fork_point_count"] == 0


def test_missing_messages_table_is_advisory(tmp_path):
    # A DB with no schema at all — the sqlite_master guard must return the
    # empty report rather than raising.
    conn = db.connect(tmp_path / "bare.db")
    try:
        out = analyze_forks(conn, compute_cost=_fake_cost)
    finally:
        conn.close()
    assert out == ForkReport().to_dict()


def test_linear_conversation_has_no_forks(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        sid = _add_session(conn, pid, "linear")
        _add_msg(conn, sid, seq=0, ts="2026-05-01T10:00:00+00:00", role="user",
                 uuid="X0", parent_uuid=None, model="")
        _add_msg(conn, sid, seq=1, ts="2026-05-01T10:00:10+00:00", role="assistant",
                 uuid="X1", parent_uuid="X0", input_tokens=50)
        _add_msg(conn, sid, seq=2, ts="2026-05-01T10:00:20+00:00", role="user",
                 uuid="X2", parent_uuid="X1", model="")
        conn.commit()
        out = analyze_forks(conn, compute_cost=_fake_cost)
    finally:
        conn.close()

    assert out["fork_point_count"] == 0
    assert out["abandoned_branch_count"] == 0
    assert out["total_cost_usd"] == pytest.approx(0.05)
    assert out["sidechain_cost_usd"] == 0.0


# ── tests: sidechain economics ───────────────────────────────────────────────


def test_sidechain_cost_and_token_share(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        _seed_branched_session(conn)
        out = analyze_forks(conn, compute_cost=_fake_cost)
    finally:
        conn.close()

    # total = A0+A1+B0+B1 = 0.10+0.20+0.30+0.40 = 1.00
    assert out["total_cost_usd"] == pytest.approx(1.00)
    # sidechain = B0+B1 = 0.70
    assert out["sidechain_cost_usd"] == pytest.approx(0.70)
    assert out["sidechain_message_count"] == 2
    # share = 0.70 / 1.00
    assert out["sidechain_cost_share"] == pytest.approx(0.70)

    # Tokens: total = 100+200+300+400 = 1000; sidechain = 300+400 = 700.
    assert out["total_token_total"] == 1000
    assert out["sidechain_token_total"] == 700
    assert out["sidechain_token_share"] == pytest.approx(0.70)


# ── tests: abandonment economics ─────────────────────────────────────────────


def test_abandoned_branch_math_and_ranking(tmp_path):
    conn = _fresh_store(tmp_path)
    try:
        _seed_branched_session(conn)
        out = analyze_forks(conn, compute_cost=_fake_cost)
    finally:
        conn.close()

    # Exactly one fork point (A0), one abandoned branch (head B0).
    assert out["fork_point_count"] == 1
    assert out["abandoned_branch_count"] == 1
    assert len(out["abandoned_branches"]) == 1

    branch = out["abandoned_branches"][0]
    assert branch["fork_uuid"] == "A0"
    assert branch["branch_head_uuid"] == "B0"
    # subtree(B0) = B0 + B1 = 0.30 + 0.40 = 0.70, over 2 messages.
    assert branch["cost_usd"] == pytest.approx(0.70)
    assert branch["message_count"] == 2
    assert branch["token_total"] == 700
    assert branch["sidechain"] is True
    # Abandoned branch's last activity precedes the session's last activity.
    assert branch["last_ts"] == "2026-05-01T10:01:00+00:00"
    assert branch["session_last_ts"] == "2026-05-01T10:06:00+00:00"
    assert branch["gap_seconds"] is not None and branch["gap_seconds"] > 0
    assert branch["reason"]

    # Total abandoned spend == the single branch's subtree cost.
    assert out["abandoned_cost_usd"] == pytest.approx(0.70)


def test_pursued_branch_is_not_flagged_abandoned(tmp_path):
    """The branch that reaches the session's latest message is never abandoned."""
    conn = _fresh_store(tmp_path)
    try:
        _seed_branched_session(conn)
        out = analyze_forks(conn, compute_cost=_fake_cost)
    finally:
        conn.close()
    heads = {b["branch_head_uuid"] for b in out["abandoned_branches"]}
    # U1 heads the live branch (leads to A1, the latest ts) → must be absent.
    assert "U1" not in heads


def test_top_n_caps_abandoned_branches(tmp_path):
    """Many small dropped branches respect the ``top_n`` cap while the count and
    total still reflect ALL abandoned branches."""
    conn = _fresh_store(tmp_path)
    try:
        pid = _add_project(conn)
        sid = _add_session(conn, pid, "many-forks")
        # A root assistant that many independent branches fork from, then a
        # single live branch that outlives them all.
        _add_msg(conn, sid, seq=0, ts="2026-05-01T09:00:00+00:00", role="user",
                 uuid="R", parent_uuid=None, model="")
        _add_msg(conn, sid, seq=1, ts="2026-05-01T09:00:01+00:00", role="assistant",
                 uuid="ROOT", parent_uuid="R", input_tokens=10)
        # Live branch reaching the latest timestamp.
        _add_msg(conn, sid, seq=2, ts="2026-05-01T12:00:00+00:00", role="assistant",
                 uuid="LIVE", parent_uuid="ROOT", input_tokens=10)
        # 15 abandoned branches, each one assistant message with 100 tokens
        # ($0.10 > MIN_BRANCH_COST_USD), all stopping well before 12:00.
        n = 15
        for i in range(n):
            _add_msg(
                conn, sid, seq=10 + i, ts=f"2026-05-01T09:{i:02d}:30+00:00",
                role="assistant", uuid=f"AB{i}", parent_uuid="ROOT",
                input_tokens=100,
            )
        conn.commit()
        out = analyze_forks(conn, compute_cost=_fake_cost, top_n=5)
    finally:
        conn.close()

    assert out["abandoned_branch_count"] == n          # count reflects ALL
    assert len(out["abandoned_branches"]) == 5         # list capped at top_n
    assert out["abandoned_cost_usd"] == pytest.approx(n * 0.10)
    # Every returned branch clears the min-cost floor.
    assert all(b["cost_usd"] >= MIN_BRANCH_COST_USD for b in out["abandoned_branches"])


def test_scope_window_excludes_out_of_range_messages(tmp_path):
    """A scope window narrows the message sweep — out-of-window spend drops."""
    from stackunderflow.reports.scope import Scope

    conn = _fresh_store(tmp_path)
    try:
        _seed_branched_session(conn)  # all in May 2026
        # Window entirely before the data → empty report.
        scope = Scope(
            since="2026-01-01T00:00:00+00:00",
            until="2026-01-31T23:59:59+00:00",
            label="jan",
        )
        out = analyze_forks(conn, scope=scope, compute_cost=_fake_cost)
    finally:
        conn.close()
    assert out["total_message_count"] == 0
    assert out["total_cost_usd"] == 0.0


def test_project_filter_scopes_to_one_project(tmp_path):
    """``project_ids`` narrows to a single project's sessions."""
    conn = _fresh_store(tmp_path)
    try:
        pid_a = _seed_branched_session(conn)  # project 'demo' with the fork
        # A second project with a linear, cheap session.
        pid_b = _add_project(conn, slug="other")
        sid_b = _add_session(conn, pid_b, "other-sess")
        _add_msg(conn, sid_b, seq=0, ts="2026-05-02T10:00:00+00:00", role="assistant",
                 uuid="O0", parent_uuid=None, input_tokens=999)
        conn.commit()

        only_a = analyze_forks(conn, project_ids=[pid_a], compute_cost=_fake_cost)
        only_b = analyze_forks(conn, project_ids=[pid_b], compute_cost=_fake_cost)
    finally:
        conn.close()

    # Project A carries the fork + sidechains; project B does not.
    assert only_a["fork_point_count"] == 1
    assert only_a["sidechain_cost_usd"] == pytest.approx(0.70)
    assert only_b["fork_point_count"] == 0
    assert only_b["sidechain_cost_usd"] == 0.0
    assert only_b["total_cost_usd"] == pytest.approx(0.999)


def test_never_raises_on_default_pricer(tmp_path):
    """Smoke: with the real ``compute_cost`` (no injection) the report still
    computes and never raises."""
    conn = _fresh_store(tmp_path)
    try:
        _seed_branched_session(conn)
        out = analyze_forks(conn)  # default pricer path
    finally:
        conn.close()
    assert out["fork_point_count"] == 1
    assert out["sidechain_message_count"] == 2
    # Real rates are non-zero for a known Anthropic model, so cost is positive.
    assert out["total_cost_usd"] > 0.0
