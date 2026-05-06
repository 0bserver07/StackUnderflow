"""Wave 4B: backfill actually populates ``usage_events``.

Wave 1's tests pinned the orchestrator's *shape* (BackfillReport
fields, force=True drops, empty-registry no-op). These tests pin the
orchestrator's *behaviour* now that the body is wired:

* Streams every messages-row through its provider's normalizer.
* Inserts events with ``INSERT OR IGNORE`` against ``uniq_events_msg``,
  so re-runs are idempotent.
* ``force=True`` rebuilds events + marts from scratch, end-state
  identical to a fresh first run.
* ``mart_watermark`` advances after the run so subsequent watcher
  cycles don't re-process the same events.
* All five default marts get rows.
* Daily mart total cost ≈ sum of compute_cost over events.

Synthetic store: 100 messages × 3 providers (claude, codex, cursor)
seeded directly via SQL so the tests don't depend on the adapter
boundary — the contract under test is "messages → events → marts",
not "JSONL → messages".
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from stackunderflow.etl import marts as marts_registry
from stackunderflow.etl import normalize as normalize_registry
from stackunderflow.etl.backfill import BackfillReport, backfill
from stackunderflow.etl.watermark import get_watermark
from stackunderflow.store import db, schema

# ── fixtures ─────────────────────────────────────────────────────────


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


@pytest.fixture(autouse=True)
def _restore_default_registries():
    """Default registrations live in the package ``__init__``s.

    Other tests in the suite sometimes call ``_clear()``; restore the
    default registrations before this module's tests run so backfill
    actually has normalizers + marts to dispatch to. Use a fresh import
    of each module to re-run its top-level ``register()`` calls.
    """
    # The cleanest way to re-trigger the default registrations is to
    # explicitly re-import the per-provider / per-mart modules and
    # re-register. (We can't reload the package's __init__ because
    # Python caches it.)
    normalize_registry._clear()
    marts_registry._clear()

    from stackunderflow.etl.normalize.claude import ClaudeNormalizer
    from stackunderflow.etl.normalize.codex import CodexNormalizer
    from stackunderflow.etl.normalize.cursor import CursorNormalizer
    normalize_registry.register("claude", ClaudeNormalizer)
    normalize_registry.register("codex", CodexNormalizer)
    normalize_registry.register("cursor", CursorNormalizer)

    from stackunderflow.etl.marts.daily import DailyMartBuilder
    from stackunderflow.etl.marts.model_day import ModelDayMartBuilder
    from stackunderflow.etl.marts.project import ProjectMartBuilder
    from stackunderflow.etl.marts.provider_day import ProviderDayMartBuilder
    from stackunderflow.etl.marts.session import SessionMartBuilder
    marts_registry.register("daily", DailyMartBuilder)
    marts_registry.register("session", SessionMartBuilder)
    marts_registry.register("project", ProjectMartBuilder)
    marts_registry.register("provider_day", ProviderDayMartBuilder)
    marts_registry.register("model_day", ModelDayMartBuilder)

    yield

    normalize_registry._clear()
    marts_registry._clear()


# ── seeding helpers ──────────────────────────────────────────────────


# Synthetic shape per provider — enough to exercise the normalizer
# without coupling to adapter / fixture quirks.
_PROVIDER_FIXTURES = {
    "claude": {
        "model": "claude-sonnet-4-5-20250929",
        "input_tokens": 1_200,
        "output_tokens": 800,
        "cache_read_tokens": 400,
        "cache_create_tokens": 0,
        "raw_json": "{}",
    },
    "codex": {
        "model": "gpt-5",
        "input_tokens": 1_500,
        "output_tokens": 1_000,
        "cache_read_tokens": 200,
        "cache_create_tokens": 0,
        # Pre-canonicalised tokens — the codex normalizer accepts both
        # raw OpenAI shape and canonical messages columns.
        "raw_json": json.dumps({
            "info": {
                "last_token_usage": {
                    "input_tokens": 1_500,
                    "output_tokens": 1_000,
                    "cached_input_tokens": 200,
                    "reasoning_output_tokens": 0,
                },
            },
        }),
    },
    "cursor": {
        # Cursor v3 carries explicit token counts when available.
        "model": "claude-4.5-sonnet-thinking",
        "input_tokens": 600,
        "output_tokens": 400,
        "cache_read_tokens": 0,
        "cache_create_tokens": 0,
        "raw_json": json.dumps({
            "tokenCount": {"inputTokens": 600, "outputTokens": 400},
        }),
    },
}


def _seed_store(
    conn: sqlite3.Connection,
    *,
    messages_per_provider: int = 100,
) -> dict[str, int]:
    """Seed the store with N messages × 3 providers.

    Returns ``{provider: assistant_message_count}`` so tests can
    cross-reference against ``events_inserted``. Half the seeded
    messages are user rows (skipped by the normalizers); the other
    half are assistant rows (one event each).
    """
    counts: dict[str, int] = {}
    for provider in ("claude", "codex", "cursor"):
        # 1 project + 1 session per provider keeps the join graph
        # straightforward.
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, "
            "first_seen, last_modified) VALUES (?, ?, ?, 0, 0)",
            (provider, f"{provider}-proj", f"{provider}-proj"),
        )
        proj_id = cur.lastrowid
        cur = conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
            (proj_id, f"{provider}-sess"),
        )
        sess_fk = cur.lastrowid

        fix = _PROVIDER_FIXTURES[provider]
        assistant_count = 0
        for i in range(messages_per_provider):
            role = "assistant" if i % 2 == 0 else "user"
            ts = f"2026-04-25T00:00:{i % 60:02d}+00:00"
            conn.execute(
                "INSERT INTO messages ("
                "  session_fk, seq, timestamp, role, model, "
                "  input_tokens, output_tokens, cache_read_tokens, "
                "  cache_create_tokens, content_text, tools_json, "
                "  raw_json, is_sidechain, uuid, parent_uuid, speed"
                ") VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    sess_fk,
                    i,
                    ts,
                    role,
                    fix["model"] if role == "assistant" else None,
                    fix["input_tokens"] if role == "assistant" else 0,
                    fix["output_tokens"] if role == "assistant" else 0,
                    fix["cache_read_tokens"] if role == "assistant" else 0,
                    fix["cache_create_tokens"] if role == "assistant" else 0,
                    "hello world " * 8,
                    "[]",
                    fix["raw_json"] if role == "assistant" else "{}",
                    0,
                    f"{provider}-{i}",
                    None,
                    "standard",
                ),
            )
            if role == "assistant":
                assistant_count += 1
        conn.commit()
        counts[provider] = assistant_count
    return counts


# ── tests ────────────────────────────────────────────────────────────


def test_backfill_inserts_events_per_provider(conn):
    """Run backfill on a 300-message store: expect events for every
    assistant row across all 3 providers."""
    counts = _seed_store(conn, messages_per_provider=100)
    expected_events = sum(counts.values())  # 50 assistant per provider × 3 = 150

    report = backfill(conn)

    assert isinstance(report, BackfillReport)
    assert report.events_inserted == expected_events, (
        f"expected {expected_events} events, got {report.events_inserted}"
    )
    assert report.events_skipped_duplicate == 0

    # One event per assistant message — no duplicates, every provider
    # contributed.
    rows = conn.execute(
        "SELECT provider, COUNT(*) c FROM usage_events GROUP BY provider"
    ).fetchall()
    by_provider = {r["provider"]: r["c"] for r in rows}
    assert by_provider == counts


def test_backfill_idempotent_via_uniq_events_msg(conn):
    """Re-running backfill on an already-converted store inserts 0 new
    events. The ``uniq_events_msg`` UNIQUE index turns every retry into
    a counted skip via ``INSERT OR IGNORE``."""
    _seed_store(conn, messages_per_provider=100)
    first = backfill(conn)
    assert first.events_inserted > 0

    second = backfill(conn)

    assert second.events_inserted == 0, (
        "second backfill must not insert duplicates"
    )
    # Same end-state: same total events.
    assert (
        conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
        == first.events_inserted
    )


def test_backfill_force_rebuilds_to_identical_state(conn):
    """``force=True`` wipes events + marts and rebuilds from scratch.

    End state matches a fresh first run — same event count, same total
    cost, same mart row counts.
    """
    _seed_store(conn, messages_per_provider=100)
    first = backfill(conn)

    # Capture end-state.
    expected_events = first.events_inserted
    expected_total_cost = float(
        conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_events"
        ).fetchone()[0]
    )

    rebuilt = backfill(conn, force=True)

    assert rebuilt.events_inserted == expected_events
    actual_events = conn.execute(
        "SELECT COUNT(*) FROM usage_events"
    ).fetchone()[0]
    assert actual_events == expected_events

    actual_total_cost = float(
        conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_events"
        ).fetchone()[0]
    )
    # Allow for floating-point noise (we're summing 150 cost rows).
    assert abs(actual_total_cost - expected_total_cost) < 1e-9


def test_backfill_advances_mart_watermarks(conn):
    """After backfill, every mart's watermark should equal the max
    event id — subsequent watcher cycles must not re-process the same
    events.
    """
    _seed_store(conn, messages_per_provider=100)
    backfill(conn)

    max_event_id = int(
        conn.execute("SELECT MAX(id) FROM usage_events").fetchone()[0] or 0
    )
    assert max_event_id > 0

    for mart_name in ("daily", "session", "project", "provider_day", "model_day"):
        wm = get_watermark(conn, mart_name)
        assert wm == max_event_id, (
            f"{mart_name} watermark should be {max_event_id}, was {wm}"
        )


def test_backfill_populates_all_five_marts(conn):
    """After backfill, every mart table has rows."""
    _seed_store(conn, messages_per_provider=100)
    backfill(conn)

    for mart in (
        "daily_mart",
        "session_mart",
        "project_mart",
        "provider_day_mart",
        "model_day_mart",
    ):
        n = conn.execute(f"SELECT COUNT(*) FROM {mart}").fetchone()[0]  # noqa: S608 — fixed literals
        assert n > 0, f"{mart} should have rows after backfill"


def test_daily_mart_cost_matches_event_cost_sum(conn):
    """Daily mart total cost ≈ SUM(usage_events.cost_usd)."""
    _seed_store(conn, messages_per_provider=100)
    backfill(conn)

    events_total = float(
        conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_events"
        ).fetchone()[0]
    )
    daily_total = float(
        conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM daily_mart"
        ).fetchone()[0]
    )
    assert abs(events_total - daily_total) < 1e-6, (
        f"daily_mart total ({daily_total}) should match events total "
        f"({events_total})"
    )


def test_backfill_partial_state_recovers_on_rerun(conn):
    """Pre-seed a partial conversion (one provider already done) — the
    next backfill picks up the rest without duplicating.
    """
    _seed_store(conn, messages_per_provider=20)

    # Pre-insert events for claude only (simulating a partial prior run).
    claude_msg_ids = [
        r[0] for r in conn.execute(
            "SELECT m.id FROM messages m "
            " JOIN sessions s ON s.id = m.session_fk "
            " JOIN projects p ON p.id = s.project_id "
            "WHERE p.provider = 'claude' AND m.role = 'assistant'"
        ).fetchall()
    ]
    proj_id = conn.execute(
        "SELECT id FROM projects WHERE provider = 'claude'"
    ).fetchone()[0]
    for mid in claude_msg_ids:
        conn.execute(
            "INSERT INTO usage_events ("
            "  source_message_fk, provider, project_id, session_id, "
            "  ts, day, role, model, input_tokens, output_tokens"
            ") VALUES (?, 'claude', ?, 'claude-sess', "
            "          '2026-04-25T00:00:00+00:00', '2026-04-25', "
            "          'assistant', 'pre-seeded', 0, 0)",
            (mid, proj_id),
        )
    conn.commit()
    pre_count = conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]

    report = backfill(conn)

    # Claude rows should all be skipped as duplicates; codex + cursor
    # rows should all be inserted.
    assert report.events_skipped_duplicate == len(claude_msg_ids)
    # New events = codex + cursor assistant messages (10 each at 20/2 = 10).
    assert report.events_inserted == 20  # codex 10 + cursor 10
    final = conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
    assert final == pre_count + report.events_inserted
