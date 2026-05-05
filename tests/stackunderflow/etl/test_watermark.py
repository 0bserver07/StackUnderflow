"""Watermark helpers — read / write / refresh round-trip.

The marts layer relies on ``mart_watermark`` to track per-mart progress
through ``usage_events.id``. These tests pin the get/set round-trip,
the empty-store contract, and the refresh-with-empty-registry default
that Wave 1 ships with.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from stackunderflow.etl import marts as marts_registry
from stackunderflow.etl.marts.base import MartBuilder
from stackunderflow.etl.watermark import (
    get_watermark,
    refresh_all_marts,
    set_watermark,
)
from stackunderflow.store import db, schema


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    """Fresh DB with the schema applied."""
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


@pytest.fixture(autouse=True)
def _clean_marts_registry():
    """Each watermark test starts with an empty marts registry."""
    marts_registry._clear()
    yield
    marts_registry._clear()


# ── get/set round-trip ──────────────────────────────────────────────────────


def test_missing_mart_returns_zero(conn):
    """No row in mart_watermark → watermark is 0."""
    assert get_watermark(conn, "daily") == 0
    assert get_watermark(conn, "nope") == 0


def test_set_then_get(conn):
    set_watermark(conn, "daily", 42)
    assert get_watermark(conn, "daily") == 42


def test_set_overwrites_existing(conn):
    """Re-calling set_watermark upserts via ON CONFLICT."""
    set_watermark(conn, "daily", 10)
    set_watermark(conn, "daily", 25)
    assert get_watermark(conn, "daily") == 25


def test_set_stamps_refresh_ts(conn):
    """``last_refresh_ts`` must be populated (NOT NULL on the schema)."""
    set_watermark(conn, "daily", 1)
    row = conn.execute(
        "SELECT last_refresh_ts FROM mart_watermark WHERE mart_name = ?",
        ("daily",),
    ).fetchone()
    assert row is not None
    assert row["last_refresh_ts"]  # non-empty ISO timestamp


def test_set_independent_per_mart(conn):
    """Watermarks for different marts don't clobber each other."""
    set_watermark(conn, "daily", 100)
    set_watermark(conn, "session", 200)
    set_watermark(conn, "project", 300)
    assert get_watermark(conn, "daily") == 100
    assert get_watermark(conn, "session") == 200
    assert get_watermark(conn, "project") == 300


# ── refresh_all_marts ───────────────────────────────────────────────────────


def test_refresh_all_with_empty_registry_returns_empty_dict(conn):
    """Wave 1 default: no mart builders registered → ``refresh_all_marts``
    is a no-op that returns an empty dict, not None."""
    assert refresh_all_marts(conn) == {}


class _StubMart(MartBuilder):
    """Minimal mart that 'consumes' up to a fixed ceiling each refresh."""

    name = "stub"

    # Class-level so tests can override per-call without re-registering.
    ceiling = 0

    def refresh(self, conn, since_event_id: int) -> int:
        # Return either the ceiling (if higher) or the existing watermark
        # (no-op when ceiling hasn't moved).
        return max(int(since_event_id), int(_StubMart.ceiling))


def test_refresh_all_advances_watermark(conn):
    marts_registry.register("stub", _StubMart)
    _StubMart.ceiling = 50

    out = refresh_all_marts(conn)

    assert out == {"stub": 50}
    assert get_watermark(conn, "stub") == 50


def test_refresh_all_idempotent_no_new_events(conn):
    """Re-running with the same ceiling → events_processed = 0."""
    marts_registry.register("stub", _StubMart)
    _StubMart.ceiling = 50

    refresh_all_marts(conn)
    out_second = refresh_all_marts(conn)

    assert out_second == {"stub": 0}
    assert get_watermark(conn, "stub") == 50


def test_refresh_all_picks_up_from_existing_watermark(conn):
    """A pre-existing watermark must be passed to refresh as ``since``."""

    seen: list[int] = []

    class _RecordingMart(MartBuilder):
        name = "rec"

        def refresh(self, conn, since_event_id: int) -> int:
            seen.append(since_event_id)
            return since_event_id + 10

    set_watermark(conn, "rec", 100)
    marts_registry.register("rec", _RecordingMart)

    out = refresh_all_marts(conn)

    assert seen == [100]
    assert out == {"rec": 10}
    assert get_watermark(conn, "rec") == 110
