"""Wave 4B: ingest writer auto-normalizes.

After every per-file ingest transaction, the writer runs the matching
provider normalizer (Wave 2A) over the just-inserted messages and
inserts events into ``usage_events``. Marts auto-refresh via
``refresh_all_marts`` so the watcher path matches the backfill path.

These tests pin the contract:

* A 5-record ingest yields 5 messages **and** 5 events.
* daily_mart populates after the per-file commit.
* Re-running the same adapter is idempotent — no duplicate events.
* Rows for an unknown provider (no normalizer registered) silently
  skip the event-insert step but still write the messages.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.etl import marts as marts_registry
from stackunderflow.etl import normalize as normalize_registry
from stackunderflow.ingest.writer import ingest_file
from stackunderflow.store import db, schema

# ── fixtures ─────────────────────────────────────────────────────────


class _StubClaudeAdapter:
    """Minimal adapter that emits Claude-shaped Records.

    Mirrors the adapter Protocol the writer expects: ``read`` yields
    ``Record`` instances. Identifies as the ``claude`` provider so the
    registered ClaudeNormalizer dispatches.
    """

    name = "claude"

    def __init__(self, records: list[Record]):
        self._records = records

    def enumerate(self):
        return []

    def read(self, ref, *, since_offset=0):
        yield from self._records


def _claude_record(seq: int, *, role: str = "assistant") -> Record:
    """Build one Claude Record ready for the writer.

    Assistant rows carry non-zero tokens so the normalizer emits an
    event; user rows are skipped by ClaudeNormalizer (role check).
    """
    return Record(
        provider="claude",
        session_id="s1",
        seq=seq,
        timestamp=f"2026-04-25T00:00:{seq:02d}+00:00",
        role=role,
        model="claude-sonnet-4-5-20250929" if role == "assistant" else None,
        input_tokens=1_000 if role == "assistant" else 0,
        output_tokens=500 if role == "assistant" else 0,
        cache_create_tokens=0,
        cache_read_tokens=200 if role == "assistant" else 0,
        content_text="hello world" if role == "assistant" else "ping",
        tools=(),
        cwd=None,
        is_sidechain=False,
        uuid=f"u-{seq}",
        parent_uuid=None,
        raw={},
        speed="standard",
    )


def _ref(tmp: Path, *, mtime: float = 1.0, size: int = 10) -> SessionRef:
    fp = tmp / "claude.jsonl"
    fp.write_bytes(b"x" * size)
    return SessionRef("claude", "-claude-proj", "s1", fp, mtime, size)


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


@pytest.fixture(autouse=True)
def _restore_default_registries():
    """Restore default normalizer + mart registrations.

    Other tests in the suite call ``_clear()``; we re-register so the
    writer's hook actually finds a normalizer + the marts to refresh.
    """
    normalize_registry._clear()
    marts_registry._clear()

    from stackunderflow.etl.normalize.claude import ClaudeNormalizer
    normalize_registry.register("claude", ClaudeNormalizer)

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


# ── tests ────────────────────────────────────────────────────────────


def test_writer_inserts_messages_and_events(conn, tmp_path: Path) -> None:
    """5 assistant records → 5 messages + 5 events."""
    records = [_claude_record(i) for i in range(5)]
    adapter = _StubClaudeAdapter(records)

    ingest_file(conn, adapter, _ref(tmp_path))

    messages = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    events = conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
    assert messages == 5
    assert events == 5


def test_writer_populates_daily_mart_after_commit(conn, tmp_path: Path) -> None:
    """The post-commit ``refresh_all_marts`` call picks up the new events."""
    records = [_claude_record(i) for i in range(5)]
    adapter = _StubClaudeAdapter(records)

    ingest_file(conn, adapter, _ref(tmp_path))

    # daily_mart must have at least one row (one row per (day, project,
    # provider, model, speed) — synthetic batch lands on a single key).
    rows = conn.execute("SELECT COUNT(*) FROM daily_mart").fetchone()[0]
    assert rows >= 1
    # Per-row totals should equal the events table — single key bucket.
    daily_cost = float(
        conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM daily_mart"
        ).fetchone()[0]
    )
    events_cost = float(
        conn.execute(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM usage_events"
        ).fetchone()[0]
    )
    assert abs(daily_cost - events_cost) < 1e-9


def test_writer_is_idempotent_on_rerun(conn, tmp_path: Path) -> None:
    """Re-ingesting the same records doesn't duplicate events.

    The first pass writes 5 messages + 5 events. The second pass sees
    no new messages (same seqs → INSERT OR IGNORE), so no rows are
    flagged as ``count_added`` — the normalize hook is skipped. End
    state is identical.
    """
    records = [_claude_record(i) for i in range(5)]

    ref = _ref(tmp_path)
    ingest_file(conn, _StubClaudeAdapter(records), ref)
    ingest_file(conn, _StubClaudeAdapter(records), ref)

    messages = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    events = conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
    assert messages == 5
    assert events == 5


def test_writer_skips_normalize_for_unregistered_provider(
    conn, tmp_path: Path,
) -> None:
    """Provider with no normalizer → messages still land, no events.

    Mirrors the rare case where a user has disabled betas and a
    not-default-on adapter (e.g. ``opencode``) ingests against a
    Wave-2A install. The hook silently no-ops; the next
    ``stackunderflow etl backfill`` picks the messages up if a
    normalizer is later registered.
    """
    # Re-clear the registry inside this test — restoring is what the
    # autouse fixture does after.
    normalize_registry._clear()

    records = [_claude_record(i) for i in range(3)]
    ingest_file(conn, _StubClaudeAdapter(records), _ref(tmp_path))

    messages = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    events = conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
    assert messages == 3
    assert events == 0


def test_writer_inserts_events_only_for_billable_rows(
    conn, tmp_path: Path,
) -> None:
    """User messages → no events. Assistant messages → one event each.

    ``ClaudeNormalizer`` filters on ``role == 'assistant' AND model
    IS NOT NULL AND any token > 0``. A mixed batch lets us verify the
    hook honours that contract end-to-end.
    """
    records = [
        _claude_record(0, role="user"),
        _claude_record(1, role="assistant"),
        _claude_record(2, role="user"),
        _claude_record(3, role="assistant"),
    ]
    ingest_file(conn, _StubClaudeAdapter(records), _ref(tmp_path))

    messages = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    events = conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
    assert messages == 4  # all four rows recorded
    assert events == 2    # only the two assistant rows produced events


def test_writer_normalize_failure_does_not_fail_ingest(
    conn, tmp_path: Path,
) -> None:
    """A normalizer that raises must not roll back the messages insert.

    Wave 4B's hook wraps ``_normalize_new_messages`` in a broad
    ``except`` so a buggy / mis-registered normalizer can't break
    ingest. Messages still commit; events stay empty.
    """
    class _BoomNormalizer:
        provider_name = "claude"

        def normalize(self, msg_row: dict):
            raise RuntimeError("synthetic boom")

    normalize_registry._clear()
    normalize_registry.register("claude", _BoomNormalizer)

    records = [_claude_record(i) for i in range(3)]
    ingest_file(conn, _StubClaudeAdapter(records), _ref(tmp_path))

    messages = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    events = conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
    # The poison normalizer is per-row; the writer logs and moves on,
    # so messages persist but no events were generated.
    assert messages == 3
    assert events == 0


def test_normalize_and_insert_event_persists_reasoning_tokens(conn) -> None:
    """The writer's INSERT carries the normalizer's ``reasoning_tokens`` through
    to the ``usage_events`` row (v026). Missing key defaults to 0."""
    from stackunderflow.ingest.writer import normalize_and_insert_event

    conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES ('droid', 'p', '/p', 'P', 0, 0)"
    )
    pid = conn.execute("SELECT id FROM projects").fetchone()[0]

    msg_row = {"id": 4242, "provider": "droid", "project_id": pid, "session_id": "s"}
    event = {
        "ts": "2026-04-25T00:00:00+00:00",
        "day": "2026-04-25",
        "model": "claude-sonnet-4-5-20250929",
        "input_tokens": 200,
        "output_tokens": 400,   # already includes the 100 reasoning tokens
        "reasoning_tokens": 100,
        "cost_source": "rate_card",
        "role": "assistant",
    }
    inserted, skipped = normalize_and_insert_event(conn, msg_row, event)
    assert (inserted, skipped) == (1, 0)

    row = conn.execute(
        "SELECT output_tokens, reasoning_tokens FROM usage_events WHERE source_message_fk = 4242"
    ).fetchone()
    assert row["output_tokens"] == 400
    assert row["reasoning_tokens"] == 100

    # An event with no reasoning_tokens key lands the DEFAULT 0.
    msg_row2 = {"id": 4343, "provider": "droid", "project_id": pid, "session_id": "s"}
    event2 = dict(event)
    del event2["reasoning_tokens"]
    normalize_and_insert_event(conn, msg_row2, event2)
    row2 = conn.execute(
        "SELECT reasoning_tokens FROM usage_events WHERE source_message_fk = 4343"
    ).fetchone()
    assert row2["reasoning_tokens"] == 0
