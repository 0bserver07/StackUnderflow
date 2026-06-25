"""Tests for ``GET /api/cost-data/by-provider`` — v0.6.1 multi-provider polish.

The endpoint powers the Cost tab's `CostByProviderCard`. The card needs:

* one row per provider in the user's store, sorted by cost desc
* `cost_usd` already converted into the active currency (parity with
  `/api/cost-data`)
* `message_count` and `session_count` so the card can render
  "X sessions · Y msgs" alongside the dollar figure
* invalid `period` arguments fail loudly with 400
"""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.cost import get_cost_by_provider
from stackunderflow.store import db, schema


# ── seeding helper ──────────────────────────────────────────────────────────


def _seed(store_db, *, projects, messages):
    """Seed projects + messages directly via SQL.

    Mirrors the pattern in ``test_compare.py::_seed`` so the two endpoints
    stay testable from the same fixtures. Each ``messages[]`` entry is a
    dict with ``project_slug, session_id, timestamp, role`` plus optional
    ``provider``, ``model``, ``in_tok``, ``out_tok``, ``cache_w``, ``cache_r``.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    project_pk: dict[tuple[str, str], int] = {}
    for prov, slug in projects:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
            (prov, slug, slug, 0.0, 0.0),
        )
        project_pk[(prov, slug)] = cur.lastrowid
    sess_pk: dict = {}
    seq_counter: dict[int, int] = {}
    for m in messages:
        prov = m.get("provider", "claude")
        slug = m["project_slug"]
        ppk = project_pk[(prov, slug)]
        sk = (ppk, m["session_id"])
        if sk not in sess_pk:
            cur = conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, ?, ?, ?)",
                (ppk, m["session_id"], m["timestamp"], m["timestamp"], 0),
            )
            sess_pk[sk] = cur.lastrowid
        sfk = sess_pk[sk]
        seq = seq_counter.get(sfk, 0)
        seq_counter[sfk] = seq + 1
        conn.execute(
            "INSERT INTO messages "
            "(session_fk, seq, timestamp, role, model, "
            " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
            " content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                sfk,
                seq,
                m["timestamp"],
                m["role"],
                m.get("model"),
                m.get("in_tok", 0),
                m.get("out_tok", 0),
                m.get("cache_w", 0),
                m.get("cache_r", 0),
                "",
                "[]",
                "{}",
                0,
                None,
                None,
            ),
        )
    conn.commit()
    conn.close()


# ── happy path ──────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_returns_one_row_per_provider(tmp_path, monkeypatch):
    """Two providers in store → two rows out, sorted by cost desc."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("codex", "gamma")],
        messages=[
            # Claude session — bigger token counts, so it should outspend codex.
            {"project_slug": "alpha", "session_id": "A1", "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {
                "project_slug": "alpha",
                "session_id": "A1",
                "timestamp": "2026-04-01T10:00:01Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 10000,
                "out_tok": 5000,
            },
            # Codex session.
            {
                "project_slug": "gamma",
                "provider": "codex",
                "session_id": "C1",
                "timestamp": "2026-04-02T10:00:00Z",
                "role": "user",
            },
            {
                "project_slug": "gamma",
                "provider": "codex",
                "session_id": "C1",
                "timestamp": "2026-04-02T10:00:01Z",
                "role": "assistant",
                "model": "gpt-5",
                "in_tok": 100,
                "out_tok": 50,
            },
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_provider(period="all")

    assert payload["period"] == "all"
    rows = payload["rows"]
    assert len(rows) == 2

    # Expect claude before codex (more tokens → more cost). Sort assertion
    # also pins the public contract — cards shouldn't have to re-sort.
    assert rows[0]["provider"] == "claude"
    assert rows[1]["provider"] == "codex"
    assert rows[0]["cost_usd"] > rows[1]["cost_usd"]

    # Per-provider counts: 2 messages each (1 user + 1 assistant), 1 session each.
    assert rows[0]["message_count"] == 2
    assert rows[0]["session_count"] == 1
    assert rows[1]["message_count"] == 2
    assert rows[1]["session_count"] == 1

    # Currency block stamped.
    assert "currency" in payload
    assert "code" in payload["currency"]


@pytest.mark.asyncio
async def test_empty_store_returns_empty_rows(tmp_path, monkeypatch):
    """Fresh install / no ingest → empty list, not crash, currency still set."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_provider(period="all")
    assert payload["rows"] == []
    assert payload["period"] == "all"
    assert "currency" in payload


@pytest.mark.asyncio
async def test_invalid_period_400s(tmp_path, monkeypatch):
    """Unknown period → HTTPException 400 with helpful message."""
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    with pytest.raises(HTTPException) as exc:
        await get_cost_by_provider(period="bogus")
    assert exc.value.status_code == 400
    # Error string should list the valid options so the API feels self-documenting.
    assert "today" in exc.value.detail
    assert "week" in exc.value.detail
    assert "all" in exc.value.detail


@pytest.mark.asyncio
async def test_user_messages_dont_double_count_cost(tmp_path, monkeypatch):
    """User messages have no tokens — they shouldn't price out, only count.

    Pinned because the SQL pulls every role to compute message + session
    counts, but ``compute_cost`` should only be called on assistant rows.
    Without that filter, a session with 50 user prompts would inflate the
    per-message average even though every user row is $0.
    """
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha")],
        messages=[
            # Three user messages, one assistant message — same session.
            {"project_slug": "alpha", "session_id": "A1", "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A1", "timestamp": "2026-04-01T10:00:01Z", "role": "user"},
            {"project_slug": "alpha", "session_id": "A1", "timestamp": "2026-04-01T10:00:02Z", "role": "user"},
            {
                "project_slug": "alpha",
                "session_id": "A1",
                "timestamp": "2026-04-01T10:00:03Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 1000,
                "out_tok": 500,
            },
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_provider(period="all")

    rows = payload["rows"]
    assert len(rows) == 1
    row = rows[0]
    assert row["provider"] == "claude"
    assert row["message_count"] == 4  # all four messages count
    assert row["session_count"] == 1
    # Should be a small but non-zero figure (claude-sonnet-4-5 priced).
    assert row["cost_usd"] > 0


@pytest.mark.asyncio
async def test_period_filter_excludes_out_of_window_messages(tmp_path, monkeypatch):
    """``period=today`` should only see today's messages.

    Seed two messages — one from 2020 (always out-of-window for ``today``),
    one with a current-time stamp — and assert only the recent one
    contributes to the rollup.
    """
    import datetime

    now = datetime.datetime.now(datetime.UTC).isoformat()
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha")],
        messages=[
            # Old message — outside any reasonable today/week/month window.
            {
                "project_slug": "alpha",
                "session_id": "OLD",
                "timestamp": "2020-01-01T00:00:00Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 99999,
                "out_tok": 99999,
            },
            # Today's message — should be the only one in the rollup.
            {
                "project_slug": "alpha",
                "session_id": "NEW",
                "timestamp": now,
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 100,
                "out_tok": 50,
            },
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_provider(period="today")

    # Only one row (claude), only one message (the recent one), one session.
    assert len(payload["rows"]) == 1
    row = payload["rows"][0]
    assert row["message_count"] == 1
    assert row["session_count"] == 1


# ── RANK 19: project scoping (the cross-project $ leak fix) ──────────────────


@pytest.mark.asyncio
async def test_by_provider_scopes_to_current_project(tmp_path, monkeypatch):
    """The card lives on a PROJECT's Cost tab — it must show that project's
    per-provider spend, not the whole store's (RANK 19).

    Two projects, same provider + identical tokens, so the global rollup is
    exactly 2x either project. Scoping to one must halve cost / message /
    session counts.
    """
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("claude", "beta")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1", "timestamp": "2026-04-01T10:00:00Z", "role": "user"},
            {
                "project_slug": "alpha",
                "session_id": "A1",
                "timestamp": "2026-04-01T10:00:01Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 10000,
                "out_tok": 5000,
            },
            {"project_slug": "beta", "session_id": "B1", "timestamp": "2026-04-02T10:00:00Z", "role": "user"},
            {
                "project_slug": "beta",
                "session_id": "B1",
                "timestamp": "2026-04-02T10:00:01Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 10000,
                "out_tok": 5000,
            },
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    # No project selected → global rollup spans BOTH projects.
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)
    global_payload = await get_cost_by_provider(period="all")
    global_claude = next(r for r in global_payload["rows"] if r["provider"] == "claude")
    assert global_claude["message_count"] == 4
    assert global_claude["session_count"] == 2

    # current_log_path = alpha → only alpha's spend (no beta leak).
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/alpha")
    scoped_payload = await get_cost_by_provider(period="all")
    scoped_claude = next(r for r in scoped_payload["rows"] if r["provider"] == "claude")
    assert scoped_claude["message_count"] == 2  # alpha only
    assert scoped_claude["session_count"] == 1
    assert scoped_claude["cost_usd"] == pytest.approx(global_claude["cost_usd"] / 2)
    assert scoped_claude["cost_usd"] > 0


@pytest.mark.asyncio
async def test_by_provider_explicit_log_path_scopes_without_current_project(tmp_path, monkeypatch):
    """Explicit ``log_path`` scopes even with no ``current_log_path`` set."""
    store_db = tmp_path / "store.db"
    _seed(
        store_db,
        projects=[("claude", "alpha"), ("claude", "beta")],
        messages=[
            {
                "project_slug": "alpha",
                "session_id": "A1",
                "timestamp": "2026-04-01T10:00:01Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 10000,
                "out_tok": 5000,
            },
            {
                "project_slug": "beta",
                "session_id": "B1",
                "timestamp": "2026-04-02T10:00:01Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 999,
                "out_tok": 1,
            },
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    payload = await get_cost_by_provider(log_path="/anywhere/beta", period="all")
    rows = payload["rows"]
    assert len(rows) == 1
    assert rows[0]["provider"] == "claude"
    assert rows[0]["message_count"] == 1  # beta's single message only
