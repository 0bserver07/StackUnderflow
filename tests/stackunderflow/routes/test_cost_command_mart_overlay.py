"""``/api/cost-data`` ``command_costs`` block stays on the aggregator path.

HANDOFF §"What's left" #5 flagged a shape mismatch between the
aggregator's ``command_costs`` output and the ``command_mart`` grain.
This test file locks in the verified resolution: the mart cannot
reconstruct the per-Interaction shape (``interaction_id``,
``prompt_preview``, ``timestamp``, ``models_used``, ``tools_used``,
``steps``, ``had_error``) the frontend's ``CommandCostList`` consumes,
so the route stays aggregator-driven even when ``command_mart`` is
populated. Pairs with the ``tool_costs`` overlay test
(``test_cost_tool_mart_overlay.py``) — that one tested the *positive*
overlay, this one tests that the analogous overlay was deliberately
NOT wired for ``command_costs``.

The two contracts asserted here:

1. Populated ``command_mart`` does NOT swap out the aggregator's
   ``command_costs`` list — the aggregator output passes through
   intact (the per-Interaction shape is preserved).
2. Empty ``command_mart`` ALSO does not affect ``command_costs`` —
   same aggregator-driven behaviour, no fallback branch needed.

If a future ``interaction_mart`` lands at the per-Interaction grain
the per-Interaction shape needs, these tests will need updating to
exercise the overlay path instead.
"""

from __future__ import annotations

import pytest

from stackunderflow.routes.cost import get_cost_data
from stackunderflow.store import db, schema


def _connect(store_db):
    conn = db.connect(store_db)
    schema.apply(conn)
    return conn


def _insert_project(conn, provider, slug):
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, "
        "first_seen, last_modified) "
        "VALUES (?, ?, ?, 0.0, 0.0)",
        (provider, slug, slug),
    )
    return int(cur.lastrowid)


def _insert_project_mart(conn, *, project_id, provider, slug):
    """Inserting a project_mart row enables the mart-aware fast path."""
    conn.execute(
        "INSERT INTO project_mart "
        "(project_id, provider, slug, display_name, first_ts, last_ts, "
        " total_messages, total_sessions, total_input_tokens, "
        " total_output_tokens, total_cache_read, total_cache_create, "
        " total_cost_usd) "
        "VALUES (?, ?, ?, ?, '2026-04-01', '2026-04-30', 1, 1, "
        "100, 50, 0, 0, 0.5)",
        (project_id, provider, slug, slug),
    )


def _insert_daily_mart(conn, *, project_id, day, **kw):
    """Daily-mart row is needed because cost.py only triggers its
    overlay branch when the daily mart has rows for the project."""
    conn.execute(
        "INSERT INTO daily_mart "
        "(day, project_id, provider, model, speed, "
        " input_tokens, output_tokens, cache_read, cache_create, "
        " message_count, session_count, cost_usd) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (day, project_id, kw.get("provider", "claude"),
         kw.get("model", "claude-sonnet-4-5"), kw.get("speed", "standard"),
         kw.get("input_tokens", 0), kw.get("output_tokens", 0),
         kw.get("cache_read", 0), kw.get("cache_create", 0),
         kw.get("message_count", 0), kw.get("session_count", 0),
         kw.get("cost_usd", 0.0)),
    )


def _insert_command_mart(conn, *, project_id, day, command_name, **kw):
    conn.execute(
        "INSERT INTO command_mart "
        "(day, project_id, command_name, "
        " event_count, cost_usd, tokens_in, tokens_out, session_count) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        (day, project_id, command_name,
         kw.get("event_count", 0), kw.get("cost_usd", 0.0),
         kw.get("tokens_in", 0), kw.get("tokens_out", 0),
         kw.get("session_count", 0)),
    )


# Aggregator-shape sample: per-Interaction list with the fields the
# ``CommandCostList`` frontend consumer reads (analytics.ts §CommandCost).
# The mart row count + sums are deliberately at variance with the
# aggregator output so a silent swap would show up as a mismatch.
_AGGREGATOR_COMMAND_COSTS = [
    {
        "interaction_id": "ix-1",
        "session_id": "sess-A",
        "timestamp": "2026-04-01T12:00:00Z",
        "prompt_preview": "/init brand new project",
        "cost": 0.42,
        "tokens": {"input": 1000, "output": 500},
        "tools_used": 3,
        "steps": 2,
        "models_used": ["claude-sonnet-4-5"],
        "had_error": False,
    },
    {
        "interaction_id": "ix-2",
        "session_id": "sess-A",
        "timestamp": "2026-04-01T12:30:00Z",
        "prompt_preview": "fix the failing test",
        "cost": 0.17,
        "tokens": {"input": 400, "output": 200},
        "tools_used": 1,
        "steps": 1,
        "models_used": ["claude-sonnet-4-5"],
        "had_error": True,
    },
]


def _aggregator_stats() -> dict:
    """Stats payload the route's ``get_project_stats`` would return."""
    return {
        "session_costs": [{"session_id": "sess-A", "cost": 0.59}],
        "command_costs": [dict(row) for row in _AGGREGATOR_COMMAND_COSTS],
        "tool_costs": {},
        "token_composition": {"daily": {}, "totals": {}, "per_session": {}},
        "outliers": {}, "retry_signals": [], "session_efficiency": [],
        "error_cost": {}, "trends": {},
    }


@pytest.mark.asyncio
async def test_command_costs_preserved_when_command_mart_populated(
    tmp_path, monkeypatch,
):
    """Populated ``command_mart`` must NOT swap out aggregator output.

    The mart's (day, project, command_name) grain throws away the
    per-Interaction fields the frontend's ``CommandCostList`` reads
    (``interaction_id``, ``prompt_preview``, ``timestamp``,
    ``models_used``, ``tools_used``, ``steps``, ``had_error``). The
    only honest behaviour is to leave the aggregator's list intact.
    """
    store_db = tmp_path / "store.db"
    slug = "-cmd-overlay"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01", cost_usd=0.59)
    # Mart rows whose sums DO NOT match the aggregator output above —
    # if the route ever silently swaps to the mart, this divergence
    # will surface as a shape change in the assertion below.
    _insert_command_mart(
        conn, project_id=pid, day="2026-04-01",
        command_name="/init",
        event_count=4, cost_usd=999.0, tokens_in=9_999, tokens_out=9_999,
        session_count=1,
    )
    _insert_command_mart(
        conn, project_id=pid, day="2026-04-01",
        command_name="freeform",
        event_count=7, cost_usd=999.0, tokens_in=9_999, tokens_out=9_999,
        session_count=2,
    )
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _aggregator_stats()),
    )

    payload = await get_cost_data()

    # Byte-equivalent passthrough — same list, same per-row fields,
    # mart's $999 / 9999-token rollup nowhere in sight.
    assert payload["command_costs"] == _AGGREGATOR_COMMAND_COSTS
    # And confirm the per-Interaction shape survives end-to-end —
    # if some future overlay turned the list into a dict, this would fail.
    assert isinstance(payload["command_costs"], list)
    first = payload["command_costs"][0]
    assert first["interaction_id"] == "ix-1"
    assert first["prompt_preview"] == "/init brand new project"
    assert first["models_used"] == ["claude-sonnet-4-5"]


@pytest.mark.asyncio
async def test_command_costs_preserved_when_command_mart_empty(
    tmp_path, monkeypatch,
):
    """Empty ``command_mart`` — same aggregator passthrough, no fallback.

    Mirrors the ``tool_costs`` empty-mart-fallback test so a future
    refactor that adds an overlay branch must ALSO add an explicit
    empty-mart fallback to keep this test green.
    """
    store_db = tmp_path / "store.db"
    slug = "-cmd-fallback"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", slug)
    _insert_project_mart(conn, project_id=pid, provider="claude", slug=slug)
    _insert_daily_mart(conn, project_id=pid, day="2026-04-01", cost_usd=0.59)
    # No command_mart rows on purpose.
    conn.commit()
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")
    monkeypatch.setattr(
        "stackunderflow.routes.cost.queries.get_project_stats",
        lambda conn, *, project_id, tz_offset=0: ([], _aggregator_stats()),
    )

    payload = await get_cost_data()
    assert payload["command_costs"] == _AGGREGATOR_COMMAND_COSTS


@pytest.mark.asyncio
async def test_command_mart_for_project_helper_loses_per_interaction_grain(
    tmp_path,
):
    """``command_mart_for_project`` cannot return the per-Interaction shape.

    Pins the structural mismatch HANDOFF #5 flagged: the helper returns
    rollup rows keyed on ``command_name``, not the per-Interaction
    fields (``interaction_id``, ``prompt_preview``, …) the frontend
    consumes. Asserting the helper's shape here makes the limitation
    explicit so a future contributor doesn't assume the helper is a
    drop-in source for the route's ``command_costs`` block.
    """
    from stackunderflow.store import mart_queries

    store_db = tmp_path / "store.db"
    conn = _connect(store_db)
    pid = _insert_project(conn, "claude", "-shape-check")
    _insert_command_mart(
        conn, project_id=pid, day="2026-04-01",
        command_name="/init",
        event_count=4, cost_usd=0.42,
        tokens_in=1000, tokens_out=500, session_count=1,
    )
    conn.commit()

    rows = mart_queries.command_mart_for_project(conn, project_id=pid)
    conn.close()

    # The helper IS populated and works — it just returns rollup rows.
    assert len(rows) == 1
    row = rows[0]
    # The fields the helper provides — all rollup-grain.
    assert set(row) == {
        "command_name", "event_count", "cost_usd",
        "tokens_in", "tokens_out", "session_count",
    }
    # The fields the aggregator emits per Interaction — none of these
    # exist on the mart grain, by construction.
    for missing in (
        "interaction_id", "session_id", "timestamp", "prompt_preview",
        "tools_used", "steps", "models_used", "had_error",
    ):
        assert missing not in row, (
            f"command_mart_for_project unexpectedly carries {missing!r}; "
            "if this fires the route can finally migrate off the aggregator."
        )
