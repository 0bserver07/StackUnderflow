"""Backfill orchestrator — Wave 1 shape contract.

Wave 1 ships only the orchestrator skeleton: registries are empty until
Wave 2 lands, so :func:`backfill` returns zero-count reports. These
tests pin the orchestrator shape (the BackfillReport fields, the
empty-registry default, the ``force=True`` reset path) so Wave 2 can
fill in the bodies without changing the public surface.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest

from stackunderflow.etl import marts as marts_registry
from stackunderflow.etl import normalize as normalize_registry
from stackunderflow.etl.backfill import BackfillReport, backfill
from stackunderflow.etl.marts.base import MartBuilder
from stackunderflow.etl.watermark import get_watermark, set_watermark
from stackunderflow.store import db, schema


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


@pytest.fixture(autouse=True)
def _clean_registries():
    normalize_registry._clear()
    marts_registry._clear()
    yield
    normalize_registry._clear()
    marts_registry._clear()


def _seed_minimal_event(conn: sqlite3.Connection) -> int:
    """Insert one ``usage_events`` row by way of the upstream tables.

    Returns the inserted event id. Used to verify ``force=True`` empties
    the table even when there's data to drop.
    """
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, "
        "first_seen, last_modified) VALUES ('claude', 'p', 'p', 0, 0)"
    )
    proj_id = conn.execute("SELECT id FROM projects").fetchone()["id"]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, 's')",
        (proj_id,),
    )
    sess_id = conn.execute("SELECT id FROM sessions").fetchone()["id"]
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
        "VALUES (?, 0, '2026-04-01T00:00:00+00:00', 'assistant', '{}')",
        (sess_id,),
    )
    msg_id = conn.execute("SELECT id FROM messages").fetchone()["id"]
    conn.execute(
        "INSERT INTO usage_events ("
        "  source_message_fk, provider, project_id, session_id, "
        "  ts, day, role"
        ") VALUES (?, 'claude', ?, 's', "
        "          '2026-04-01T00:00:00+00:00', '2026-04-01', 'assistant')",
        (msg_id, proj_id),
    )
    return conn.execute(
        "SELECT id FROM usage_events ORDER BY id DESC LIMIT 1"
    ).fetchone()["id"]


# ── empty-registry shape (Wave 1 default) ───────────────────────────────────


def test_backfill_empty_store_empty_report(conn):
    """Fresh DB + empty registries → all-zero report."""
    report = backfill(conn)

    assert isinstance(report, BackfillReport)
    assert report.events_inserted == 0
    assert report.events_skipped_duplicate == 0
    assert report.marts_refreshed == {}
    # Timing must be a non-negative float (perf_counter delta).
    assert report.duration_seconds >= 0


def test_backfill_idempotent(conn):
    """Re-running on a fresh store yields the same shape."""
    first = backfill(conn)
    second = backfill(conn)

    # Both runs report zero-counts; the field shape is what we're
    # locking down for Wave 2.
    assert first.events_inserted == 0
    assert first.events_skipped_duplicate == 0
    assert first.marts_refreshed == {}
    assert second.events_inserted == 0
    assert second.events_skipped_duplicate == 0
    assert second.marts_refreshed == {}


# ── force=True reset ────────────────────────────────────────────────────────


def test_backfill_force_drops_events_and_marts(conn):
    """``force=True`` empties usage_events + every mart + watermarks."""
    _seed_minimal_event(conn)
    set_watermark(conn, "daily", 999)

    # Sanity: data is present before the force run.
    assert (
        conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == 1
    )
    assert get_watermark(conn, "daily") == 999

    backfill(conn, force=True)

    # All cleared. (No normalizers registered, so no re-population.)
    assert (
        conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == 0
    )
    assert get_watermark(conn, "daily") == 0
    for tbl in (
        "daily_mart",
        "session_mart",
        "project_mart",
        "provider_day_mart",
        "model_day_mart",
        "mart_watermark",
    ):
        count = conn.execute(f"SELECT COUNT(*) FROM {tbl}").fetchone()[0]  # noqa: S608 — table name is a hard-coded literal
        assert count == 0, f"{tbl} should be empty after force=True"


def test_backfill_force_is_idempotent(conn):
    """Running ``force=True`` twice doesn't error and leaves everything empty."""
    _seed_minimal_event(conn)

    backfill(conn, force=True)
    # Second force run on already-empty tables must still be a clean no-op.
    report = backfill(conn, force=True)

    assert report.events_inserted == 0
    assert (
        conn.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == 0
    )


# ── orchestrator wires marts even with empty normalizer registry ────────────


class _StubMart(MartBuilder):
    name = "stub"

    def refresh(self, conn, since_event_id: int) -> int:
        # No-op refresh: returns the existing watermark.
        return since_event_id


def test_backfill_calls_refresh_all_marts_when_normalizers_empty(conn):
    """Even with no normalizers, registered marts get a refresh pass.

    This pins the Wave 1 fall-through: ``refresh_all_marts`` runs so
    each mart can finalize its watermark even when no new events
    arrived. Wave 2 keeps this behaviour when it lands.
    """
    marts_registry.register("stub", _StubMart)

    report = backfill(conn)

    # The mart was visited even though no normalizer ran.
    assert "stub" in report.marts_refreshed
    assert report.marts_refreshed["stub"] == 0


def test_backfill_report_shape_locked():
    """Pin the dataclass field set so Wave 2 can't quietly add or drop fields."""
    report = BackfillReport()
    field_names = {f.name for f in BackfillReport.__dataclass_fields__.values()}
    assert field_names == {
        "events_inserted",
        "events_skipped_duplicate",
        "marts_refreshed",
        "duration_seconds",
    }
    # Defaults are sensible.
    assert report.events_inserted == 0
    assert report.events_skipped_duplicate == 0
    assert report.marts_refreshed == {}
    assert report.duration_seconds == 0.0
