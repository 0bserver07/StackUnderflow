"""Wave 4E — end-to-end ETL pipeline integration test.

Builds a 10K-message synthetic store across five providers (claude, codex,
cursor, gemini, cline), runs the registered Normalizers over every
``messages`` row, refreshes every mart, validates the cost-conservation
invariant (``SUM(daily_mart.cost_usd) == SUM(usage_events.cost_usd)``),
then hits every dashboard route via FastAPI's ``TestClient`` asserting
status 200, non-empty payload, and per-route latency under 500 ms.

Why we don't call ``etl.backfill.backfill(conn)`` directly
----------------------------------------------------------
``backfill()`` ships as the orchestrator skeleton — its
``_run_normalizers`` body is documented as Wave-2-pending and currently
returns ``(0, 0)`` regardless of registered normalizers. The watcher
(``stackunderflow/etl/watcher.py::_normalize_recent``) is the production
code path that actually walks ``messages`` → ``usage_events``; we mirror
its loop here so the e2e test exercises the real Normalizer + MartBuilder
contracts. When Wave 4F (or whichever wave fills in ``_run_normalizers``)
lands, this helper can be deleted in favour of a single ``backfill(conn)``
call without touching the rest of the test.

Marker
------
Gated on ``@pytest.mark.slow`` — skipped by default, run with
``pytest -m slow``. The synthetic store lives in ``tmp_path``; the user's
real ``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import json
import random
import sqlite3
import time
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

import stackunderflow.deps as deps
from stackunderflow.etl import normalize as normalize_registry
from stackunderflow.etl.watermark import get_watermark, refresh_all_marts
from stackunderflow.routes import (
    bookmarks,
    cfg,
    commands,
    compare,
    context_budget,
    cost,
    data,
    misc,
    optimize,
    plan,
    projects,
    qa,
    search,
    sessions,
    tags,
    yield_route,
)
from stackunderflow.store import db, schema

pytestmark = pytest.mark.slow


# ── synthetic store generation ──────────────────────────────────────────────


# Five providers spanning the realistic mix the dashboard sees in the wild.
# ``cost_provider`` maps the StackUnderflow provider name to the pricer
# family (matches `etl/normalize/base.py::_PROVIDER_TO_PRICER`).
_PROVIDERS: tuple[dict[str, Any], ...] = (
    {
        "name": "claude",
        "models": (
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-5-20251101",
            "claude-haiku-4-5-20251001",
        ),
    },
    {
        "name": "codex",
        "models": ("gpt-5", "gpt-5-codex", "gpt-5-mini"),
    },
    {
        "name": "cursor",
        # Cursor messages mix Anthropic + OpenAI under the hood; mirror that
        # by splitting the model pool across both families. Composer-1 sits
        # alongside so the cursor normalizer's len(text)//4 estimate path
        # gets exercised when tokens are zero.
        "models": ("claude-sonnet-4-5-20250929", "gpt-5", "composer-1"),
    },
    {
        "name": "gemini",
        # Gemini falls through to Anthropic-shape pricing (see
        # ``_PROVIDER_TO_PRICER`` default). Token contracts are still
        # honoured since the messages-level shape is provider-agnostic.
        "models": (
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ),
    },
    {
        "name": "cline",
        # Cline runs Claude under the hood — pin one Claude id so the
        # rate-card lookup in `infra.costs.RATE_CARD` returns a non-zero
        # cost on insertion. Adding a second id exercises the multi-model
        # branch of the cline normalizer (which mirrors Claude's contract).
        "models": (
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
        ),
    },
)

# Spread 20 projects across the 5 providers (4 per provider).
_PROJECTS_PER_PROVIDER = 4
_DAYS = 30
_TOTAL_MESSAGES = 10_000

# Seed for reproducibility — every CI run produces the same fixture, so a
# regression in the cost-conservation check is reproducible from the test
# log alone.
_SEED = 4242


def _build_synthetic_store(store_db: Path) -> dict[str, Any]:
    """Create a 10K-message store across 20 projects × 30 days × 5 providers.

    Returns a metadata dict with row counts and a couple of slug references
    the route tests need (``primary_slug`` + its log_path) to hit the
    project-scoped dashboard endpoints.
    """
    rng = random.Random(_SEED)  # noqa: S311 — fixture jitter, not a security boundary

    conn = db.connect(store_db)
    schema.apply(conn)

    # ── projects ────────────────────────────────────────────────────────
    project_rows: list[dict[str, Any]] = []
    base_ts = 1_700_000_000.0  # arbitrary epoch — only relative ordering matters
    for prov_idx, prov in enumerate(_PROVIDERS):
        for j in range(_PROJECTS_PER_PROVIDER):
            slug = f"-Users-fixture-{prov['name']}-proj-{j:02d}"
            cur = conn.execute(
                "INSERT INTO projects (provider, slug, display_name, "
                "first_seen, last_modified, path) VALUES (?, ?, ?, ?, ?, ?)",
                (
                    prov["name"], slug, f"{prov['name']}/proj-{j:02d}",
                    base_ts + prov_idx * 100 + j,
                    base_ts + prov_idx * 100 + j + 1,
                    f"/fixture/{slug}",
                ),
            )
            project_rows.append({
                "id": int(cur.lastrowid),
                "provider": prov["name"],
                "slug": slug,
                "models": prov["models"],
            })

    # ── one session per project per day ─────────────────────────────────
    # Keeps session_id stable for the dedup pass; gives realistic
    # session_count rollups in session_mart.
    session_ids: dict[tuple[int, int], int] = {}
    sessions_inserted = 0
    for proj in project_rows:
        for d in range(_DAYS):
            session_id_str = f"{proj['slug']}-day-{d:02d}"
            day_iso = _day_iso(d)
            cur = conn.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, "
                "last_ts, message_count) VALUES (?, ?, ?, ?, 0)",
                (proj["id"], session_id_str, day_iso, day_iso),
            )
            session_ids[(proj["id"], d)] = int(cur.lastrowid)
            sessions_inserted += 1

    # ── 10K messages ────────────────────────────────────────────────────
    # Round-robin per project per day, with ~16-17 messages per (project,
    # day) cell on average. We keep deterministic counts so the cost
    # conservation invariant is exact rather than statistical.
    msg_rows: list[tuple] = []
    seq_counters: dict[int, int] = dict.fromkeys(session_ids.values(), 0)
    speed_ix = 0
    for n in range(_TOTAL_MESSAGES):
        proj = project_rows[n % len(project_rows)]
        d = (n // len(project_rows)) % _DAYS
        session_fk = session_ids[(proj["id"], d)]
        seq_counters[session_fk] += 1
        seq = seq_counters[session_fk]

        # Realistic token distributions per spec.
        input_tokens = rng.randint(200, 2000)
        output_tokens = rng.randint(50, 1500)
        cache_read = rng.randint(0, 5000)
        cache_create = rng.randint(0, 1500)

        model = proj["models"][n % len(proj["models"])]

        # 5% of *claude* messages get speed='fast' to exercise the priority
        # multiplier in the pricer. Other providers stay 'standard'.
        if proj["provider"] == "claude" and (n % 20) == 0:
            speed = "fast"
            speed_ix += 1
        else:
            speed = "standard"

        # Compose a deterministic ISO-8601 timestamp inside day ``d``.
        timestamp = f"2026-04-{(d % 30) + 1:02d}T{(n % 24):02d}:00:{(n % 60):02d}+00:00"

        # role: ~75% assistant (billable), 25% user (skipped by normalizers)
        role = "assistant" if (n % 4) != 0 else "user"

        msg_rows.append((
            session_fk, seq, timestamp, role, model,
            input_tokens, output_tokens, cache_create, cache_read,
            "fixture content",  # content_text — tiny, kept constant
            "[]",                  # tools_json
            json.dumps({"fixture": True, "n": n}),  # raw_json
            0,                                     # is_sidechain
            f"uuid-{n}",                           # uuid
            None,                                  # parent_uuid
            speed,
        ))

    conn.executemany(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        "content_text, tools_json, raw_json, is_sidechain, uuid, parent_uuid, speed) "
        "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        msg_rows,
    )

    # session.message_count is consumed by the dashboard payload — patch it
    # in one batch so we don't pay per-row.
    conn.execute(
        "UPDATE sessions SET message_count = ("
        "  SELECT COUNT(*) FROM messages m WHERE m.session_fk = sessions.id"
        ")"
    )
    conn.commit()
    conn.close()

    primary = project_rows[0]
    return {
        "messages_inserted": len(msg_rows),
        "projects_inserted": len(project_rows),
        "sessions_inserted": sessions_inserted,
        "fast_messages": speed_ix,
        "primary_slug": primary["slug"],
        "primary_log_path": f"/fixture/{primary['slug']}",
        "project_slugs": [p["slug"] for p in project_rows],
    }


def _day_iso(day_idx: int) -> str:
    """Stable ISO-8601 date for the ``day_idx``-th day in our 30-day window."""
    return f"2026-04-{(day_idx % 30) + 1:02d}T12:00:00+00:00"


def _run_normalizers_over_messages(conn: sqlite3.Connection) -> int:
    """Walk every ``messages`` row through its provider's Normalizer and
    insert the yielded events into ``usage_events``.

    Mirrors the watcher's ``_normalize_recent`` loop (per provider,
    LEFT JOIN usage_events to skip already-converted rows). Returns the
    total number of events inserted.

    NOTE: When ``etl.backfill._run_normalizers`` lands its real body in a
    future wave, this helper can be replaced with a single call to
    ``etl.backfill.backfill(conn)``.
    """
    inserted = 0
    for provider, normalizer_cls in normalize_registry.all().items():
        normalizer = normalizer_cls()
        rows = conn.execute(
            """
            SELECT m.id, m.session_fk, m.seq, m.timestamp, m.role, m.model,
                   m.input_tokens, m.output_tokens, m.cache_create_tokens,
                   m.cache_read_tokens, m.content_text, m.tools_json,
                   m.raw_json, m.is_sidechain, m.uuid, m.parent_uuid, m.speed,
                   s.session_id AS session_id, s.project_id AS project_id,
                   p.provider AS provider
              FROM messages m
              JOIN sessions s ON s.id = m.session_fk
              JOIN projects p ON p.id = s.project_id
         LEFT JOIN usage_events e ON e.source_message_fk = m.id
             WHERE p.provider = ?
               AND e.id IS NULL
            """,
            (provider,),
        ).fetchall()

        for row in rows:
            msg_row = dict(row)
            for ev in normalizer.normalize(msg_row):
                conn.execute(
                    """
                    INSERT OR IGNORE INTO usage_events (
                        source_message_fk, provider, account, project_id,
                        session_id, ts, day, model, speed,
                        input_tokens, output_tokens,
                        cache_read_tokens, cache_create_tokens,
                        cost_usd, cost_source, role, raw_extras
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (
                        msg_row["id"],
                        ev.get("provider", provider),
                        ev.get("account", "default"),
                        ev.get("project_id", msg_row["project_id"]),
                        ev.get("session_id", msg_row["session_id"]),
                        ev.get("ts", msg_row["timestamp"]),
                        ev.get("day", (msg_row["timestamp"] or "")[:10]),
                        ev.get("model", msg_row.get("model") or ""),
                        ev.get("speed", msg_row.get("speed", "standard")),
                        int(ev.get("input_tokens", 0)),
                        int(ev.get("output_tokens", 0)),
                        int(ev.get("cache_read_tokens", 0)),
                        int(ev.get("cache_create_tokens", 0)),
                        float(ev.get("cost_usd", 0.0)),
                        ev.get("cost_source", "rate_card"),
                        ev.get("role", msg_row.get("role", "")),
                        ev.get("raw_extras"),
                    ),
                )
                inserted += 1
    return inserted


# ── fixtures ────────────────────────────────────────────────────────────────


@pytest.fixture()
def populated_store(tmp_path: Path) -> Iterator[dict[str, Any]]:
    """Yields a metadata dict describing the synthetic store at ``tmp_path``.

    The ``store_path`` key is the SQLite file location — callers point
    ``deps.store_path`` at it via ``monkeypatch``. Uses ``tmp_path`` so
    the test never touches the user's real store.
    """
    store_db = tmp_path / "store.db"
    meta = _build_synthetic_store(store_db)
    meta["store_path"] = store_db
    yield meta


@pytest.fixture()
def fastapi_client(populated_store, monkeypatch) -> Iterator[TestClient]:
    """A FastAPI TestClient mounted on the same routers ``server.py`` uses.

    We mount the routers directly rather than importing the production
    ``app`` so the lifespan hooks (services init, ingest thread, watcher)
    don't run — the test fixture is the source of truth for store state.
    """
    monkeypatch.setattr(deps, "store_path", populated_store["store_path"])
    monkeypatch.setattr(
        deps, "current_log_path", populated_store["primary_log_path"]
    )
    monkeypatch.setattr(
        deps, "current_project_path", populated_store["primary_log_path"]
    )

    # Make sure the dashboard memo cache is empty between runs.
    data.invalidate_dashboard_cache()

    app = FastAPI()
    for router in (
        projects.router, data.router, cost.router, commands.router,
        sessions.router, search.router, qa.router, tags.router,
        bookmarks.router, misc.router, optimize.router, plan.router,
        compare.router, yield_route.router, context_budget.router,
        cfg.router,
    ):
        app.include_router(router)

    with TestClient(app) as client:
        yield client


# ── tests ───────────────────────────────────────────────────────────────────


def test_etl_full_pipeline_against_synthetic_store(populated_store):
    """Build → normalize → marts → cost-conservation. No HTTP layer here.

    Pins the production-side invariants so a regression in the Normalizer
    contract or a mart builder's SQL surfaces as a test failure before the
    route tests below muddy the picture with a 200/!200 distinction.
    """
    store_db = populated_store["store_path"]
    assert populated_store["messages_inserted"] == _TOTAL_MESSAGES

    conn = db.connect(store_db)
    try:
        # ── normalize ─────────────────────────────────────────────────
        events_inserted = _run_normalizers_over_messages(conn)

        # Assistant rows with non-zero usage are billable; user rows and
        # zero-token assistant rows are dropped. We seed user every 4th
        # message so the lower bound is ~75% of 10K, but normalizers also
        # drop a few zero-token assistant rows. A loose lower bound is
        # safer than an exact equality and still catches a regression
        # where a normalizer suddenly drops 50%+ of its input.
        assert events_inserted >= int(_TOTAL_MESSAGES * 0.6), (
            f"events_inserted={events_inserted} — fewer than 60% of "
            f"{_TOTAL_MESSAGES} messages turned into events; a normalizer "
            f"likely regressed."
        )

        # Sanity: every event row points at a real messages row via the
        # FK we declared in the schema.
        orphaned = conn.execute(
            "SELECT COUNT(*) FROM usage_events e "
            "LEFT JOIN messages m ON m.id = e.source_message_fk "
            "WHERE m.id IS NULL"
        ).fetchone()[0]
        assert orphaned == 0

        # ── refresh every mart ────────────────────────────────────────
        marts_processed = refresh_all_marts(conn)
        # Wave 5 added tool_mart + command_mart; v011 added the
        # per-message-grain message_tool mart. All advance their
        # watermark like the rest, but the synthetic fixture has no
        # tools_json / raw_json tool_use blocks / user prompts so these
        # three content-dependent marts may report 0 events processed.
        assert set(marts_processed) == {
            "daily", "session", "project", "provider_day", "model_day",
            "tool", "command", "message_tool",
        }
        # Every Wave 2B mart must have consumed at least one event; the
        # content-dependent marts are not asserted > 0 here.
        for name in ("daily", "session", "project", "provider_day", "model_day"):
            assert marts_processed[name] > 0, (
                f"mart {name!r} consumed zero events"
            )
        # message_tool's watermark still advances to the highest event id
        # even though the fixture's raw_json carries no tool_use blocks.
        events_max = conn.execute("SELECT COALESCE(MAX(id), 0) FROM usage_events").fetchone()[0]
        assert get_watermark(conn, "message_tool") == events_max

        # ── row-count sanity ──────────────────────────────────────────
        # The content-dependent marts (tool, command, message_tool) are
        # intentionally excluded — the e2e synthetic fixture doesn't
        # include tools_json / raw_json tool_use blocks / user-prompt
        # fixtures, so those marts stay empty here. Their own unit tests
        # cover row population.
        for tbl in (
            "daily_mart", "session_mart", "project_mart",
            "provider_day_mart", "model_day_mart",
        ):
            # tbl comes from a hardcoded literal tuple — no user input.
            count = conn.execute(
                f"SELECT COUNT(*) FROM {tbl}"  # noqa: S608
            ).fetchone()[0]
            assert count > 0, f"{tbl} is empty after refresh"
        # message_tool_mart is queryable (empty given this fixture).
        assert conn.execute("SELECT COUNT(*) FROM message_tool_mart").fetchone()[0] == 0

        # ── cost-conservation invariants ──────────────────────────────
        # Every mart's COALESCE(SUM(cost_usd), 0) must equal the events
        # total. Floating-point comparison uses a 1e-4 tolerance because
        # five separate UPSERT paths each accumulate tiny rounding
        # differences in SQLite's REAL column.
        events_cost = float(conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_events"
        ).fetchone()[0])
        # Smoke test: at least *some* events priced at non-zero. The
        # rate-card has entries for every model we use, so this should
        # be substantial.
        assert events_cost > 0.0, (
            "events_cost is zero — pricing path likely broken. "
            "Check infra.costs.RATE_CARD and the per-provider normalizer "
            "cost_source flag."
        )

        for tbl, col in (
            ("daily_mart", "cost_usd"),
            ("provider_day_mart", "cost_usd"),
            ("model_day_mart", "cost_usd"),
            ("project_mart", "total_cost_usd"),
        ):
            # tbl + col come from a hardcoded literal tuple — no user input.
            mart_cost = float(conn.execute(
                f"SELECT COALESCE(SUM({col}), 0) FROM {tbl}"  # noqa: S608
            ).fetchone()[0])
            assert abs(mart_cost - events_cost) < 1e-4, (
                f"cost-conservation broken: {tbl}.{col} sum = {mart_cost} "
                f"but usage_events.cost_usd sum = {events_cost} "
                f"(delta {mart_cost - events_cost:.6f})"
            )

        # session_mart has one row per distinct session_id in events.
        expected_sessions = conn.execute(
            "SELECT COUNT(DISTINCT session_id) FROM usage_events"
        ).fetchone()[0]
        actual_sessions = conn.execute(
            "SELECT COUNT(*) FROM session_mart"
        ).fetchone()[0]
        assert actual_sessions == expected_sessions

        # project_mart has one row per project_id seen in events.
        expected_projects = conn.execute(
            "SELECT COUNT(DISTINCT project_id) FROM usage_events"
        ).fetchone()[0]
        actual_projects = conn.execute(
            "SELECT COUNT(*) FROM project_mart"
        ).fetchone()[0]
        assert actual_projects == expected_projects
    finally:
        conn.close()


# Per-route latency budget for the e2e HTTP sweep. Generous (500 ms) per
# the spec — the regression suite below pins much tighter budgets against
# a pre-populated marts fixture without paying the normalize/refresh tax
# inline. CI can be a few times slower than a dev box; we widen here so a
# noisy build agent doesn't flap the e2e suite.
_E2E_BUDGET_MS = 500


# Per-route entries: (label, method, url, *, accept_404=False).
# ``/api/etl/status`` is listed for forward compatibility — the route is
# referenced in the task spec but not implemented in the current main.
# Until it lands, the test accepts a 404 response in lieu of a 200 so the
# rest of the sweep keeps catching real regressions on the existing
# routes.
_E2E_ROUTES: tuple[tuple[str, str, str, bool], ...] = (
    ("projects_with_stats", "GET", "/api/projects?include_stats=true", False),
    ("dashboard_data", "GET", "/api/dashboard-data", False),
    ("cost_data", "GET", "/api/cost-data", False),
    ("cost_data_by_provider", "GET", "/api/cost-data/by-provider?period=month", False),
    ("compare", "GET", "/api/compare?period=month", False),
    ("yield", "GET", "/api/yield?period=week", False),
    ("optimize", "GET", "/api/optimize?period=month", False),
    ("messages_summary", "GET", "/api/messages/summary", False),
    ("etl_status", "GET", "/api/etl/status", True),
)


def test_dashboard_routes_return_real_data_under_budget(
    populated_store, fastapi_client
):
    """Sweep every dashboard route against the populated synthetic store.

    Asserts:

    1. Every route returns 200 (or 404 for the not-yet-implemented
       ``/api/etl/status`` placeholder — see ``_E2E_ROUTES``).
    2. Every 200 response has a non-empty body.
    3. Every route finishes in under ``_E2E_BUDGET_MS`` ms.

    First pass (cold) populates the in-process aggregator + dashboard
    memo cache; we still measure cold timing because that's what
    real-world latency feels like for a fresh dashboard load.
    """
    # Need at least events to give every route a payload to chew on. We
    # repeat the pipeline run from the previous test inline because each
    # ``populated_store`` fixture invocation builds a fresh DB.
    conn = db.connect(populated_store["store_path"])
    try:
        _run_normalizers_over_messages(conn)
        refresh_all_marts(conn)
    finally:
        conn.close()

    timings: list[tuple[str, float, int]] = []
    for label, method, url, accept_404 in _E2E_ROUTES:
        t0 = time.perf_counter()
        resp = fastapi_client.request(method, url)
        elapsed_ms = (time.perf_counter() - t0) * 1000
        timings.append((label, elapsed_ms, resp.status_code))

        if accept_404 and resp.status_code == 404:
            # ``/api/etl/status`` placeholder branch — log timing for
            # observability but skip the body check.
            continue

        assert resp.status_code == 200, (
            f"{method} {url} → {resp.status_code}: {resp.text[:200]}"
        )
        body = resp.json()
        # Non-emptiness check: every route returns either a list or a
        # dict; both should have at least one element / key when the
        # store is populated. A 200 with ``{}`` would mean the route
        # silently fell through to an empty branch despite having
        # 10K real messages to chew on.
        assert body, f"{method} {url} returned empty body: {body!r}"

        assert elapsed_ms < _E2E_BUDGET_MS, (
            f"{method} {url} took {elapsed_ms:.1f}ms (budget {_E2E_BUDGET_MS}ms)"
        )

    # Print a nice timing table for the slow-suite log so a tightening of
    # the budget can be calibrated from real CI numbers.
    print("\nE2E route timings (cold):")  # noqa: T201
    for label, ms, status in timings:
        print(f"  {label:32s} {ms:7.1f}ms  status={status}")  # noqa: T201
