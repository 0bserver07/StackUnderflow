"""Tests for GET /api/sessions/compare — analytics-expansion spec §1.10.

These lock both the *contract* (``a``/``b``/``diff``/``currency`` shape, 400/404
behaviour) and the *performance fix*: compare must build each side's
``SessionCost`` from a per-session path over ONLY the two requested sessions'
messages — never the whole-project ``get_project_stats`` pipeline, which
materialised + enriched + aggregated every message just to diff two rows
(~3.4s on a large project).
"""
from __future__ import annotations

import json

import pytest
from fastapi import HTTPException

from stackunderflow.infra.costs import compute_cost
from stackunderflow.routes.sessions import compare_sessions
from stackunderflow.store import db, queries, schema

_MODEL = "claude-sonnet-4-20250514"


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, *, provider="claude", slug="-cmp"):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        (provider, slug, slug, 0.0, 0.0),
    )
    return int(cur.lastrowid)


def _insert_session(conn, *, project_id, session_id, ts="2026-02-01T00:00:00Z"):
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, 0)",
        (project_id, session_id, ts, ts),
    )
    return int(cur.lastrowid)


def _insert_message(
    conn,
    *,
    session_fk,
    seq,
    ts,
    role,
    model=None,
    input_tokens=0,
    output_tokens=0,
    cache_read=0,
    cache_create=0,
    content="",
    is_error=False,
):
    """Insert a message with ``raw_json`` the pipeline can classify/enrich.

    ``role`` is the logical kind: ``user`` (a command), ``assistant`` (carries
    usage + model), or ``user`` with ``is_error=True`` (a ``tool_result`` error
    block — counts as an error but NOT a command).
    """
    if is_error:
        col_role = "user"
        msg = {
            "role": "user",
            "content": [{"type": "tool_result", "is_error": True, "content": content or "boom"}],
        }
        col_content = ""
        col_model = None
    elif role == "assistant":
        col_role = "assistant"
        msg = {
            "role": "assistant",
            "model": model,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_create,
            },
            "content": [{"type": "text", "text": content}] if content else [],
        }
        col_content = content
        col_model = model
    else:
        col_role = "user"
        msg = {"role": "user", "content": content}
        col_content = content
        col_model = None

    raw = {"type": col_role, "timestamp": ts, "uuid": f"u{session_fk}-{seq}", "message": msg}
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '[]', ?, 0, ?, NULL)",
        (
            session_fk, seq, ts, col_role, col_model,
            input_tokens, output_tokens, cache_create, cache_read,
            col_content, json.dumps(raw), f"u{session_fk}-{seq}",
        ),
    )


@pytest.fixture
def seeded(tmp_path, monkeypatch):
    """Two real sessions (a small, b larger) plus a big decoy session c.

    - a: 1 command, 0 errors, 1000/500 tokens, 60s.
    - b: 2 commands, 1 error, 4000/1700 tokens (+200 cache_read), 480s.
    - c: 100 messages — the decoy that proves compare(a, b) never touches the
      whole project.
    """
    store_db = tmp_path / "cmp.db"
    slug = "-cmp"
    conn = _connect(store_db)
    pid = _insert_project(conn, slug=slug)

    sa = _insert_session(conn, project_id=pid, session_id="sess-a")
    _insert_message(conn, session_fk=sa, seq=0, ts="2026-02-01T00:00:00Z", role="user", content="start of session A")
    _insert_message(conn, session_fk=sa, seq=1, ts="2026-02-01T00:01:00Z", role="assistant",
                    model=_MODEL, input_tokens=1000, output_tokens=500, content="ok a")

    sb = _insert_session(conn, project_id=pid, session_id="sess-b")
    _insert_message(conn, session_fk=sb, seq=0, ts="2026-02-02T00:00:00Z", role="user", content="start of session B")
    _insert_message(conn, session_fk=sb, seq=1, ts="2026-02-02T00:05:00Z", role="assistant",
                    model=_MODEL, input_tokens=3000, output_tokens=1500, content="ok b1")
    _insert_message(conn, session_fk=sb, seq=2, ts="2026-02-02T00:06:00Z", role="user", content="more b")
    _insert_message(conn, session_fk=sb, seq=3, ts="2026-02-02T00:07:00Z", role="assistant",
                    model=_MODEL, input_tokens=1000, output_tokens=200, cache_read=200, content="ok b2")
    _insert_message(conn, session_fk=sb, seq=4, ts="2026-02-02T00:08:00Z", role="user", is_error=True)

    sc = _insert_session(conn, project_id=pid, session_id="sess-c")
    for i in range(100):
        _insert_message(conn, session_fk=sc, seq=i, ts=f"2026-02-03T00:{i // 60:02d}:{i % 60:02d}Z",
                        role="assistant", model=_MODEL, input_tokens=10, output_tokens=10, content=f"c{i}")

    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    return slug


@pytest.mark.asyncio
async def test_compare_returns_a_b_and_diff(seeded):
    resp = await compare_sessions(a="sess-a", b="sess-b")
    body = json.loads(resp.body)

    assert body["a"]["session_id"] == "sess-a"
    assert body["b"]["session_id"] == "sess-b"

    # Deterministic structural diffs (b - a).
    assert body["diff"]["commands"] == 1          # 2 - 1
    assert body["diff"]["errors"] == 1            # 1 - 0
    assert body["diff"]["duration_s"] == pytest.approx(420.0)  # 480 - 60
    assert body["diff"]["tokens"]["input"] == 3000            # 4000 - 1000
    assert body["diff"]["tokens"]["output"] == 1200           # 1700 - 500
    assert body["diff"]["tokens"]["cache_read"] == 200        # 200 - 0

    # Per-side cost matches compute_cost on that side's (model, speed) bucket —
    # exact parity with the aggregator's _SessionCostCollector.
    exp_a = compute_cost({"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0}, _MODEL)["total_cost"]
    exp_b = compute_cost({"input": 4000, "output": 1700, "cache_creation": 0, "cache_read": 200}, _MODEL)["total_cost"]
    assert body["a"]["cost"] == pytest.approx(exp_a)
    assert body["b"]["cost"] == pytest.approx(exp_b)
    assert body["diff"]["cost"] == pytest.approx(exp_b - exp_a)
    assert body["diff"]["cost"] > 0

    # Contract: a/b carry the full SessionCost field set the frontend renders.
    for side in (body["a"], body["b"]):
        for key in ("session_id", "duration_s", "cost", "tokens", "messages", "commands", "errors"):
            assert key in side
    assert "currency" in body


@pytest.mark.asyncio
async def test_compare_does_not_run_full_project_pipeline(seeded, monkeypatch):
    """The whole-project ``get_project_stats`` path must not be on compare, and
    the dataset handed to the aggregator holds ONLY the two requested sessions'
    messages — never the 100-message decoy session c."""
    def _boom(*a, **k):
        raise AssertionError("get_project_stats (full project pipeline) must not be called")

    monkeypatch.setattr(queries, "get_project_stats", _boom)

    from stackunderflow.stats import aggregator

    seen: dict = {}
    real = aggregator.summarise_session_costs

    def _spy(ds, **kwargs):
        seen["records"] = len(ds.records)
        seen["sessions"] = {r.session_id for r in ds.records}
        return real(ds, **kwargs)

    monkeypatch.setattr(aggregator, "summarise_session_costs", _spy)

    resp = await compare_sessions(a="sess-a", b="sess-b")
    assert resp.status_code == 200
    # a has 2 messages, b has 5 → exactly 7 records aggregated; c's 100 untouched.
    assert seen["records"] == 7
    assert seen["sessions"] == {"sess-a", "sess-b"}


@pytest.mark.asyncio
async def test_compare_runs_only_the_session_cost_collector(seeded, monkeypatch):
    """Compare reads one section of ``summarise`` — so it must not call
    ``summarise`` at all, and must not compute any other section.

    ``aggregator.summarise`` used to run 12 collectors + overview/daily/hourly/
    trends here and throw 17 of the 18 sections away.
    """
    from stackunderflow.stats import aggregator

    def _boom(*a, **k):
        raise AssertionError("aggregator.summarise (all 18 sections) must not be called")

    monkeypatch.setattr(aggregator, "summarise", _boom)

    # A collector compare has no use for: if it is constructed, work leaked.
    monkeypatch.setattr(aggregator, "_CommandCostCollector", _boom)
    monkeypatch.setattr(aggregator, "_ToolCostCollector", _boom)
    monkeypatch.setattr(aggregator, "_ErrorCostCollector", _boom)

    resp = await compare_sessions(a="sess-a", b="sess-b")
    assert resp.status_code == 200
    body = json.loads(resp.body)
    assert body["a"]["session_id"] == "sess-a"


@pytest.mark.asyncio
async def test_session_cost_rows_match_full_summarise(seeded):
    """Parity oracle: the narrowed path returns exactly the rows the full
    ``summarise`` sweep would have put in ``session_costs``."""
    import stackunderflow.deps as deps
    from stackunderflow.routes.sessions import _session_costs_for_sessions
    from stackunderflow.stats import aggregator, classifier, enricher
    from stackunderflow.stats.classifier import RawEntry

    slug = seeded
    conn = db.connect(deps.store_path)
    try:
        project_rows = queries.get_projects_by_slug(conn, slug=slug)
        pids = [r.id for r in project_rows]
        ph = ",".join("?" for _ in pids)
        sess_rows = conn.execute(
            f"SELECT id, session_id, project_id FROM sessions "
            f"WHERE project_id IN ({ph}) AND session_id IN (?, ?)",
            (*pids, "sess-a", "sess-b"),
        ).fetchall()
        provider_map = {r.id: (r.provider or "anthropic") for r in project_rows}
        got = _session_costs_for_sessions(conn, sess_rows, provider_map, "/fake/-cmp")

        # Rebuild the same dataset and run the FULL sweep as the oracle.
        fk_to_sid = {r["id"]: r["session_id"] for r in sess_rows}
        fk_ph = ",".join("?" for _ in fk_to_sid)
        rows = conn.execute(
            f"SELECT session_fk, raw_json, timestamp FROM messages "
            f"WHERE session_fk IN ({fk_ph}) ORDER BY timestamp",
            list(fk_to_sid),
        ).fetchall()
    finally:
        conn.close()

    entries = []
    for r in rows:
        sid = fk_to_sid[r["session_fk"]]
        payload = json.loads(r["raw_json"])
        if r["timestamp"]:
            payload["timestamp"] = r["timestamp"]
        entries.append(RawEntry(payload=payload, session_id=sid, origin=sid, provider="anthropic"))
    ds = enricher.build(classifier.tag(entries), "/fake/-cmp")
    expected = aggregator.summarise(ds, "/fake/-cmp")["session_costs"]

    assert got == expected
    assert {s["session_id"] for s in got} == {"sess-a", "sess-b"}


@pytest.mark.asyncio
async def test_compare_carries_every_field_the_ui_renders(seeded):
    """SessionCompareView reads a fixed field set off a/b/diff — lock it, with
    the arithmetic, so a narrowing of the aggregator path can't silently drop
    a row from the comparison table."""
    resp = await compare_sessions(a="sess-a", b="sess-b")
    body = json.loads(resp.body)
    a, b, diff = body["a"], body["b"], body["diff"]

    # ── per-side scalars (pickValue) ────────────────────────────────────────
    for side in (a, b):
        for key in ("session_id", "cost", "commands", "messages", "errors", "duration_s"):
            assert key in side, f"missing {key}"
        for tok in ("input", "output", "cache_read", "cache_creation"):
            assert tok in side["tokens"], f"missing tokens.{tok}"

    assert a["session_id"] == "sess-a"
    assert a["messages"] == 2 and a["commands"] == 1 and a["errors"] == 0
    assert a["duration_s"] == pytest.approx(60.0)
    assert a["tokens"] == {"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0}

    assert b["session_id"] == "sess-b"
    assert b["messages"] == 5 and b["commands"] == 2 and b["errors"] == 1
    assert b["duration_s"] == pytest.approx(480.0)
    assert b["tokens"] == {"input": 4000, "output": 1700, "cache_creation": 0, "cache_read": 200}

    # ── diff arithmetic (pickDiff) — b minus a, every key ───────────────────
    assert diff["commands"] == b["commands"] - a["commands"] == 1
    assert diff["errors"] == b["errors"] - a["errors"] == 1
    assert diff["duration_s"] == pytest.approx(b["duration_s"] - a["duration_s"])
    assert diff["cost"] == pytest.approx(b["cost"] - a["cost"])
    for tok in ("input", "output", "cache_read", "cache_creation"):
        assert diff["tokens"][tok] == b["tokens"][tok] - a["tokens"][tok]

    # Cost is real, not a zeroed placeholder from a swallowed collector error.
    assert a["cost"] > 0 and b["cost"] > a["cost"]
    # The response envelope the UI destructures.
    assert set(body) == {"a", "b", "diff", "currency"}


@pytest.mark.asyncio
async def test_compare_404_when_session_missing(seeded):
    with pytest.raises(HTTPException) as exc_info:
        await compare_sessions(a="sess-a", b="missing-session")
    assert exc_info.value.status_code == 404
    assert "missing-session" in exc_info.value.detail


@pytest.mark.asyncio
async def test_compare_400_when_no_project(monkeypatch):
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    with pytest.raises(HTTPException) as exc_info:
        await compare_sessions(a="x", b="y")
    assert exc_info.value.status_code == 400


@pytest.mark.asyncio
async def test_compare_uses_log_path_query_over_current(seeded, monkeypatch):
    """Explicit log_path query wins over a bogus deps.current_log_path."""
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/not/real")
    resp = await compare_sessions(a="sess-a", b="sess-b", log_path="/whatever/-cmp")
    assert resp.status_code == 200
    body = json.loads(resp.body)
    assert body["a"]["session_id"] == "sess-a"
    assert body["b"]["session_id"] == "sess-b"
