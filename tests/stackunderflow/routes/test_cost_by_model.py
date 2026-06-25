"""Tests for ``GET /api/cost-data/by-model`` — spend-by-model-over-time.

Powers the Cost tab's by-model time-series chart. The endpoint reads the
pre-aggregated ``model_day_mart`` and returns, per model, a daily cost +
message series plus a total, sorted by total cost descending, with cost
pre-converted into the active currency (parity with ``/api/cost-data/by-provider``).
"""

from __future__ import annotations

import pytest
from fastapi import HTTPException

from stackunderflow.routes.cost import get_cost_by_model
from stackunderflow.store import db, schema


def _seed_messages(store_db, *, projects, messages):
    """Seed projects + raw messages (no marts).

    Mirrors ``test_cost_by_provider._seed`` so the project-scoped path —
    which rolls up the ``messages`` table, not ``model_day_mart`` — is
    exercised directly. Each ``messages[]`` entry: ``project_slug,
    session_id, timestamp, role`` plus optional ``provider``, ``model``,
    ``in_tok``, ``out_tok``.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    project_pk: dict = {}
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
        ppk = project_pk[(prov, m["project_slug"])]
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
                0,
                0,
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


def _seed_model_day(store_db, rows):
    """Insert rows directly into ``model_day_mart``.

    Each row: (day, model, speed, cost_usd, input_tokens, output_tokens,
    cache_read, cache_create, message_count, session_count). The endpoint
    reads the mart only, so seeding it directly tests the endpoint logic
    without running the full ETL backfill.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.executemany(
        "INSERT INTO model_day_mart "
        "(day, model, speed, cost_usd, input_tokens, output_tokens, "
        " cache_read, cache_create, message_count, session_count) "
        "VALUES (?, ?, 'standard', ?, 0, 0, 0, 0, ?, 1)",
        rows,
    )
    conn.commit()
    conn.close()


@pytest.mark.asyncio
async def test_groups_models_with_daily_series_sorted_by_total(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    _seed_model_day(
        store_db,
        rows=[
            # (day, model, cost_usd, message_count)
            ("2026-04-01", "claude-fable-5", 700.0, 100),
            ("2026-04-01", "claude-opus-4-8", 30.0, 50),
            ("2026-04-02", "claude-fable-5", 400.0, 80),
            ("2026-04-02", "claude-opus-4-8", 20.0, 40),
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_model(period="all")

    assert payload["period"] == "all"
    models = payload["models"]
    assert [m["model"] for m in models] == ["claude-fable-5", "claude-opus-4-8"]
    # Fable outspends Opus here; sort is descending by total (public contract).
    assert models[0]["total_cost"] > models[1]["total_cost"]

    # Each model carries a per-day series, ordered by day.
    fable_daily = models[0]["daily"]
    assert [d["date"] for d in fable_daily] == ["2026-04-01", "2026-04-02"]
    assert fable_daily[0]["message_count"] == 100
    # total == sum of the daily slices (modulo currency rate, applied to both).
    assert models[0]["total_cost"] == pytest.approx(sum(d["cost_usd"] for d in fable_daily))

    assert "currency" in payload and "code" in payload["currency"]


@pytest.mark.asyncio
async def test_empty_store_returns_empty_models(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    payload = await get_cost_by_model(period="all")
    assert payload["models"] == []
    assert payload["period"] == "all"
    assert "currency" in payload


@pytest.mark.asyncio
async def test_invalid_period_400s(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    conn = db.connect(store_db)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    with pytest.raises(HTTPException) as exc:
        await get_cost_by_model(period="bogus")
    assert exc.value.status_code == 400
    assert "today" in exc.value.detail
    assert "all" in exc.value.detail


# ── RANK 19: project scoping (the global model_day_mart can't be project-keyed) ─


@pytest.mark.asyncio
async def test_by_model_scopes_to_current_project(tmp_path, monkeypatch):
    """When a project is active the by-model series rolls up THAT project's
    messages, not the whole store's (RANK 19) — ``model_day_mart`` is
    global-grain and can't answer the project question.

    alpha: 2 assistant + 1 user (same model, same day). beta: 1 assistant
    (same model/day). Scoping to alpha must count 2 (assistant only, user
    excluded); scoping to beta must count 1.
    """
    store_db = tmp_path / "store.db"
    _seed_messages(
        store_db,
        projects=[("claude", "alpha"), ("claude", "beta")],
        messages=[
            {"project_slug": "alpha", "session_id": "A1", "timestamp": "2026-04-01T09:00:00Z", "role": "user"},
            {
                "project_slug": "alpha",
                "session_id": "A1",
                "timestamp": "2026-04-01T10:00:00Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 1000,
                "out_tok": 500,
            },
            {
                "project_slug": "alpha",
                "session_id": "A1",
                "timestamp": "2026-04-01T11:00:00Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 1000,
                "out_tok": 500,
            },
            {
                "project_slug": "beta",
                "session_id": "B1",
                "timestamp": "2026-04-01T10:00:00Z",
                "role": "assistant",
                "model": "claude-sonnet-4-5",
                "in_tok": 1000,
                "out_tok": 500,
            },
        ],
    )
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)

    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/alpha")
    alpha = await get_cost_by_model(period="all")
    assert len(alpha["models"]) == 1
    m = alpha["models"][0]
    assert m["model"] == "claude-sonnet-4-5"
    assert len(m["daily"]) == 1
    assert m["daily"][0]["date"] == "2026-04-01"
    assert m["daily"][0]["message_count"] == 2  # assistant only — user excluded
    assert m["total_cost"] > 0

    # beta sees only its single assistant message — proves no cross-project leak.
    monkeypatch.setattr("stackunderflow.deps.current_log_path", "/fake/beta")
    beta = await get_cost_by_model(period="all")
    assert beta["models"][0]["daily"][0]["message_count"] == 1
    assert beta["models"][0]["total_cost"] == pytest.approx(m["total_cost"] / 2)
