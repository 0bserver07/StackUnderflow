"""COST-B concern 2 — golden-fixture parity for the by-model daily_mart path.

The project-scoped branch of ``GET /api/cost-data/by-model`` re-prices every
assistant message on every request (102 ms for a month window, 548 ms all-time,
measured on a 43K-message project). ``daily_mart`` answers the same question in
0.17-0.63 ms — but only for projects and periods where the substitution is
EXACT, because the two paths disagree in two measurable ways:

* the normalizer stores ``cost_usd = 0.0`` for a model it has no rate card for,
  while this route's raw path re-prices it through ``compute_cost``'s
  default-family fallback and invents spend (-65.4% over a week window on a
  real slug with 183 such events);
* ``daily_mart`` is keyed on a UTC day, so truncating ``period=week``'s rolling
  ``now - 7d`` instant to 10 chars swallows the whole boundary day (+8-29%).

Hence the gate: ``today``/``month``/``all`` on a materialised project with zero
non-``rate_card`` events. Everything else keeps the raw rollup, unchanged.

The fixtures here run the REAL ETL (``etl.backfill.backfill`` — production
normalizers, production mart builders) over seeded messages rather than
hand-writing mart rows, so "the mart path equals the raw path" is a claim about
production code, not about the test's own arithmetic.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

import pytest

import stackunderflow.routes.cost as cost_routes
from stackunderflow.etl.backfill import backfill
from stackunderflow.infra.costs import RATE_CARD
from stackunderflow.routes.cost import _by_model_mart_eligible, get_cost_by_model
from stackunderflow.store import db, schema

# Two ids the rate card knows → cost_source='rate_card' on every event.
PRICED_A = "claude-sonnet-4-5-20250929"
PRICED_B = "claude-opus-4-8"
# One it does not → cost_source='unknown', mart cost 0.0, raw cost invented.
UNPRICED = "some-proxy/mystery-model"

assert PRICED_A in RATE_CARD and PRICED_B in RATE_CARD
assert UNPRICED not in RATE_CARD


def _today() -> str:
    return datetime.now(UTC).strftime("%Y-%m-%d")


def _old_day() -> str:
    """A day comfortably outside the current month, for the ``all`` window."""
    first_of_month = datetime.now(UTC).replace(day=1)
    return (first_of_month - timedelta(days=40)).strftime("%Y-%m-%d")


def _seed(store_db, spec) -> None:
    """Seed projects + messages. ``spec``: ``{slug: [message dicts]}``.

    Message keys: ``day``, ``hour``, ``role``, optional ``model``, ``in_tok``,
    ``out_tok``, ``session``.
    """
    conn = db.connect(store_db)
    schema.apply(conn)
    for slug, messages in spec.items():
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES ('claude', ?, ?, 0.0, 0.0)",
            (slug, slug),
        )
        ppk = cur.lastrowid
        sessions: dict[str, int] = {}
        seqs: dict[int, int] = {}
        for m in messages:
            ts = f"{m['day']}T{m.get('hour', 10):02d}:00:00Z"
            sid = m.get("session", "S1")
            if sid not in sessions:
                scur = conn.execute(
                    "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                    "VALUES (?, ?, ?, ?, 0)",
                    (ppk, sid, ts, ts),
                )
                sessions[sid] = scur.lastrowid
            sfk = sessions[sid]
            seq = seqs.get(sfk, 0)
            seqs[sfk] = seq + 1
            conn.execute(
                "INSERT INTO messages "
                "(session_fk, seq, timestamp, role, model, input_tokens, output_tokens, "
                " cache_create_tokens, cache_read_tokens, content_text, tools_json, raw_json, "
                " is_sidechain, uuid, parent_uuid) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, '', '[]', '{}', 0, NULL, NULL)",
                (
                    sfk,
                    seq,
                    ts,
                    m["role"],
                    m.get("model"),
                    m.get("in_tok", 0),
                    m.get("out_tok", 0),
                ),
            )
    conn.commit()
    conn.close()


def _run_etl(store_db) -> None:
    conn = db.connect(store_db)
    try:
        schema.apply(conn)
        backfill(conn)
    finally:
        conn.close()


def _pure_messages() -> list[dict]:
    """Two models across two days plus a user turn the rollup must ignore."""
    return [
        {"day": _old_day(), "role": "user"},
        {"day": _old_day(), "hour": 10, "role": "assistant", "model": PRICED_A, "in_tok": 5000, "out_tok": 900},
        {"day": _old_day(), "hour": 11, "role": "assistant", "model": PRICED_B, "in_tok": 3000, "out_tok": 400},
        {"day": _today(), "hour": 9, "role": "user"},
        {"day": _today(), "hour": 10, "role": "assistant", "model": PRICED_A, "in_tok": 7000, "out_tok": 1100},
        {
            "day": _today(), "hour": 11, "role": "assistant",
            "model": PRICED_A, "in_tok": 2000, "out_tok": 300, "session": "S2",
        },
        {"day": _today(), "hour": 12, "role": "assistant", "model": PRICED_B, "in_tok": 1500, "out_tok": 250},
    ]


def _spy_paths(monkeypatch):
    """Record which rollup the route actually took."""
    seen: list[str] = []
    real_mart = cost_routes._build_by_model_rows_from_mart
    real_raw = cost_routes._build_by_model_rows_from_messages

    def mart(conn, **kw):
        seen.append("mart")
        return real_mart(conn, **kw)

    def raw(conn, **kw):
        seen.append("raw")
        return real_raw(conn, **kw)

    monkeypatch.setattr(cost_routes, "_build_by_model_rows_from_mart", mart)
    monkeypatch.setattr(cost_routes, "_build_by_model_rows_from_messages", raw)
    return seen


def _force_raw(monkeypatch):
    """Disable the gate so the same request re-runs on the raw rollup."""
    monkeypatch.setattr(cost_routes, "_by_model_mart_eligible", lambda conn, ids: False)


# ── (a) pure rate_card project: mart path == raw path, exactly ──────────────


@pytest.mark.asyncio
@pytest.mark.parametrize("period", ["today", "month", "all"])
async def test_mart_path_matches_raw_path_exactly(tmp_path, monkeypatch, period):
    """Golden fixture: same store, same request, both rollups — identical rows,
    identical totals, identical model names, identical ordering."""
    store_db = tmp_path / "store.db"
    slug = "-pure-rate-card"
    _seed(store_db, {slug: _pure_messages()})
    _run_etl(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    seen = _spy_paths(monkeypatch)
    fast = await get_cost_by_model(period=period)
    assert seen == ["mart"], f"expected the mart path for period={period}, took {seen}"

    _force_raw(monkeypatch)
    seen.clear()
    slow = await get_cost_by_model(period=period)
    assert seen == ["raw"]

    assert fast == slow, f"mart/raw divergence for period={period}"
    # …and it isn't trivially equal because both are empty.
    assert fast["models"], "fixture produced no rows — the parity claim is vacuous"
    assert {m["model"] for m in fast["models"]} == {PRICED_A, PRICED_B}
    assert sum(m["total_cost"] for m in fast["models"]) > 0


@pytest.mark.asyncio
async def test_all_window_reaches_further_back_than_month(tmp_path, monkeypatch):
    """Guards the fixture itself: if every message landed in the current month
    the ``all`` case above would prove nothing about the unbounded window."""
    store_db = tmp_path / "store.db"
    slug = "-pure-window"
    _seed(store_db, {slug: _pure_messages()})
    _run_etl(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    every = await get_cost_by_model(period="all")
    month = await get_cost_by_model(period="month")
    assert sum(m["total_cost"] for m in every["models"]) > sum(m["total_cost"] for m in month["models"])


# ── (b) any non-rate_card event → the gate routes to the raw path ───────────


@pytest.mark.asyncio
@pytest.mark.parametrize("period", ["today", "month", "all"])
async def test_unknown_model_event_forces_the_raw_path(tmp_path, monkeypatch, period):
    """One un-priced model is enough. The normalizer stores $0.00 for it while
    the raw path invents a number from the default family, so substituting the
    mart here would under-report real spend."""
    store_db = tmp_path / "store.db"
    slug = "-has-unknown"
    messages = _pure_messages() + [
        {"day": _today(), "hour": 13, "role": "assistant", "model": UNPRICED, "in_tok": 9000, "out_tok": 2000},
    ]
    _seed(store_db, {slug: messages})
    _run_etl(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    conn = db.connect(store_db)
    try:
        pids = [r.id for r in cost_routes.queries.get_projects_by_slug(conn, slug=slug)]
        assert _by_model_mart_eligible(conn, pids) is False
        stored = conn.execute(
            "SELECT cost_usd, cost_source FROM usage_events WHERE model = ?", (UNPRICED,)
        ).fetchone()
    finally:
        conn.close()
    assert stored["cost_source"] == "unknown"
    assert stored["cost_usd"] == 0.0  # what the mart would have reported

    seen = _spy_paths(monkeypatch)
    payload = await get_cost_by_model(period=period)
    assert seen == ["raw"], f"the gate let a dirty project onto the mart for period={period}"

    # The raw path prices the unknown model — proof the divergence is real and
    # that routing to the mart would have zeroed it out.
    mystery = next(m for m in payload["models"] if m["model"] == UNPRICED)
    assert mystery["total_cost"] > 0


# ── (c) period=week is never eligible, however clean the project ────────────


@pytest.mark.asyncio
async def test_week_always_takes_the_raw_path(tmp_path, monkeypatch):
    """``week`` is a rolling ``now - 7d`` instant; day-truncating it to the
    mart's grain pulls in the whole boundary day (+8-29% measured). Even on a
    project that clears every other gate, week stays on the raw rollup."""
    store_db = tmp_path / "store.db"
    slug = "-pure-week"
    _seed(store_db, {slug: _pure_messages()})
    _run_etl(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    conn = db.connect(store_db)
    try:
        pids = [r.id for r in cost_routes.queries.get_projects_by_slug(conn, slug=slug)]
        assert _by_model_mart_eligible(conn, pids) is True  # the project itself is clean
    finally:
        conn.close()

    seen = _spy_paths(monkeypatch)
    await get_cost_by_model(period="week")
    assert seen == ["raw"]
    assert "week" not in cost_routes._BY_MODEL_MART_PERIODS


# ── the structural gate: an un-materialised store must not answer "empty" ────


@pytest.mark.asyncio
async def test_unmaterialised_store_takes_the_raw_path(tmp_path, monkeypatch):
    """Without the ``project_mart`` check, an empty ``usage_events`` would pass
    the "no non-rate_card events" test vacuously and the endpoint would serve an
    empty chart for a project full of messages."""
    store_db = tmp_path / "store.db"
    slug = "-never-backfilled"
    _seed(store_db, {slug: _pure_messages()})  # no _run_etl
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    conn = db.connect(store_db)
    try:
        pids = [r.id for r in cost_routes.queries.get_projects_by_slug(conn, slug=slug)]
        assert _by_model_mart_eligible(conn, pids) is False
    finally:
        conn.close()

    seen = _spy_paths(monkeypatch)
    payload = await get_cost_by_model(period="all")
    assert seen == ["raw"]
    assert payload["models"], "un-materialised store served an empty by-model chart"


@pytest.mark.asyncio
async def test_global_branch_is_untouched(tmp_path, monkeypatch):
    """With no project selected the endpoint keeps reading ``model_day_mart``
    globally — the gate only ever applies to the project-scoped branch."""
    store_db = tmp_path / "store.db"
    _seed(store_db, {"-global-check": _pure_messages()})
    _run_etl(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", None)

    seen = _spy_paths(monkeypatch)
    payload = await get_cost_by_model(period="all")
    assert seen == [], "the global branch must not use either project-scoped rollup"
    assert payload["models"]


# ── documented residuals the gate cannot see (measured on the real store) ────


@pytest.mark.asyncio
async def test_zero_token_assistant_rows_are_the_known_message_count_residual(tmp_path, monkeypatch):
    """A zero-token assistant row produces NO ``usage_events`` row at all, so it
    carries no ``cost_source`` for the gate to reject — the mart path just
    counts one message fewer than the raw path (cost is $0 either way).

    Measured on the real store: -52 of 26,229 messages (-0.20%) on a project
    that clears every gate. Pinned here so the residual is a known property of
    the substitution rather than a surprise in a bug report.
    """
    store_db = tmp_path / "store.db"
    slug = "-zero-token-residual"
    messages = _pure_messages() + [
        {"day": _today(), "hour": 14, "role": "assistant", "model": PRICED_A, "in_tok": 0, "out_tok": 0},
    ]
    _seed(store_db, {slug: messages})
    _run_etl(store_db)
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    fast = await get_cost_by_model(period="all")
    _force_raw(monkeypatch)
    slow = await get_cost_by_model(period="all")

    fast_msgs = sum(d["message_count"] for m in fast["models"] for d in m["daily"])
    slow_msgs = sum(d["message_count"] for m in slow["models"] for d in m["daily"])
    assert slow_msgs - fast_msgs == 1, "the zero-token row is the only counted difference"
    # Cost is unaffected — the dropped row was free.
    assert sum(m["total_cost"] for m in fast["models"]) == pytest.approx(
        sum(m["total_cost"] for m in slow["models"])
    )
