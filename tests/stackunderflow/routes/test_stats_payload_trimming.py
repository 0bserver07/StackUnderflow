"""Tests for the /api/stats payload trimming controls (this PR).

Three knobs added to /api/stats so cheap clients can avoid the 4 MB body
the aggregator produces on real stores:

* ``?days=N``         — cap ``daily_stats`` to the most recent ``N`` calendar
  days (default 90, ``days=0`` disables).
* ``?include=block``  — repeated; return only the named top-level blocks
  plus ``currency``.
* ``?details=true``   — restore the legacy "full body" response; default
  strips ``command_details`` / ``assistant_details`` / etc.
"""

from __future__ import annotations

import pytest

from stackunderflow.routes import data as data_route
from stackunderflow.store import db, schema


def _seed_project(store_db, slug: str) -> int:
    conn = db.connect(store_db)
    schema.apply(conn)
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    project_id = cur.lastrowid
    conn.commit()
    conn.close()
    assert project_id is not None
    return project_id


def _fake_stats():
    """A stats payload mirroring the keys real /api/stats returns."""
    daily = {f"2026-04-{d:02d}": {"messages": d} for d in range(1, 31)}
    return {
        "overview": {"total_messages": 100, "project_name": "demo"},
        "tools": {"usage_counts": {"Read": 10}, "error_counts": {}, "error_rates": {}},
        "sessions": {"count": 3},
        "daily_stats": daily,
        "hourly_pattern": [0] * 24,
        "errors": {
            "total": 2,
            "rate": 0.02,
            "by_type": {"x": 1},
            "by_category": {"y": 1},
            "error_details": [{"timestamp": "2026-04-01T00:00:00Z"} for _ in range(50)],
            "assistant_details": [
                {"timestamp": "2026-04-01T00:00:00Z", "is_error": False} for _ in range(5000)
            ],
        },
        "models": {"claude-sonnet-4-6": {"messages": 100}},
        "user_interactions": {
            "real_user_messages": 10,
            "command_details": [
                {"timestamp": "x", "user_message": "y", "tool_names": []}
                for _ in range(1000)
            ],
            "tool_count_distribution": {str(i): i for i in range(50)},
        },
        "cache": {"hit_rate": 0.5},
        "session_costs": [{"session_id": str(i), "cost": float(i)} for i in range(100)],
        "command_costs": [{"command": "x", "cost": 1.0} for _ in range(100)],
        "outliers": {
            "high_tool_commands": [{"session_id": str(i)} for i in range(50)],
            "high_step_commands": [{"session_id": str(i)} for i in range(50)],
        },
        "retry_signals": [{"session_id": str(i)} for i in range(30)],
        "session_efficiency": [{"session_id": str(i)} for i in range(80)],
        "tool_costs": {"Read": 0.05},
        "token_composition": {"input": 100, "output": 50},
        "error_cost": {"usd": 0.01},
        "trends": {"messages_per_day": []},
    }


def _patch_stats(monkeypatch, store_db, slug):
    monkeypatch.setattr("stackunderflow.deps.store_path", store_db)
    monkeypatch.setattr("stackunderflow.deps.current_log_path", f"/fake/{slug}")

    def _impl(conn, *, project_id, tz_offset=0):  # noqa: ARG001
        return ([], _fake_stats())

    monkeypatch.setattr(
        "stackunderflow.routes.data.queries.get_project_stats", _impl,
    )


# ── default trimming ────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_default_strips_heavy_blocks(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-strip-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    stats = data_route.get_stats()

    # Heavy nested lists emptied — keys still present (shape stability).
    assert stats["user_interactions"]["command_details"] == []
    assert stats["errors"]["assistant_details"] == []
    assert stats["errors"]["error_details"] == []
    assert stats["user_interactions"]["tool_count_distribution"] == {}

    # Top-level heavy lists emptied too.
    for k in ("session_costs", "command_costs", "session_efficiency", "retry_signals"):
        assert stats[k] == [], f"{k} should be emptied by default"

    # Outliers capped at 10 entries per bucket.
    assert len(stats["outliers"]["high_tool_commands"]) == 10
    assert len(stats["outliers"]["high_step_commands"]) == 10

    # Lightweight keys still populated.
    assert stats["overview"]["total_messages"] == 100
    assert stats["models"] == {"claude-sonnet-4-6": {"messages": 100}}
    assert stats["currency"]["rate_from_usd"] == 1.0


# ── ?details=true opt-in ────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_details_true_returns_full_body(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-detail-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    stats = data_route.get_stats(details=True)

    assert len(stats["user_interactions"]["command_details"]) == 1000
    assert len(stats["errors"]["assistant_details"]) == 5000
    assert len(stats["session_costs"]) == 100
    assert len(stats["outliers"]["high_tool_commands"]) == 50


# ── ?days= cap on daily_stats ───────────────────────────────────────────────


@pytest.mark.asyncio
async def test_days_caps_daily_stats(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-days-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    stats = data_route.get_stats(days=7)

    assert len(stats["daily_stats"]) == 7
    # Should keep the *most recent* 7 entries.
    assert "2026-04-30" in stats["daily_stats"]
    assert "2026-04-24" in stats["daily_stats"]
    assert "2026-04-23" not in stats["daily_stats"]


@pytest.mark.asyncio
async def test_days_zero_disables_cap(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-days0-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    stats = data_route.get_stats(days=0)
    assert len(stats["daily_stats"]) == 30  # fixture seeds 30 days


@pytest.mark.asyncio
async def test_default_caps_to_90_days(tmp_path, monkeypatch):
    """No ``days=`` arg → default 90-day cap (fixture seeds 30, so unaffected)."""
    store_db = tmp_path / "store.db"
    slug = "-default-cap-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    stats = data_route.get_stats()
    # 30 < 90, so all entries retained.
    assert len(stats["daily_stats"]) == 30


# ── ?include= block filter ──────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_include_filters_to_named_blocks(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-inc-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    stats = data_route.get_stats(include=["overview", "models"])

    # Only the named blocks plus currency.
    assert set(stats.keys()) == {"overview", "models", "currency"}
    assert stats["overview"]["total_messages"] == 100


@pytest.mark.asyncio
async def test_include_unknown_blocks_are_silently_ignored(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-inc-unknown-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    stats = data_route.get_stats(include=["overview", "no_such_block"])

    assert set(stats.keys()) == {"overview", "currency"}


@pytest.mark.asyncio
async def test_include_empty_strings_dropped(tmp_path, monkeypatch):
    store_db = tmp_path / "store.db"
    slug = "-inc-empty-proj"
    _seed_project(store_db, slug)
    _patch_stats(monkeypatch, store_db, slug)

    # Empty-string includes should be no-ops (not a "return all but nothing").
    stats = data_route.get_stats(include=["", "  "])

    # All blocks preserved when no real keys requested.
    assert "overview" in stats
    assert "errors" in stats
