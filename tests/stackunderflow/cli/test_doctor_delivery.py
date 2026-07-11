"""Tests for the doctor delivery scoreboard — data must REACH usage_events.

``_run_delivery_checks`` exists to expose the one failure class unit suites
proved unable to see: an adapter whose data loads (or sits on disk) but never
materializes into ``usage_events``. These tests pin each status, the exemption
path (data-driven via capabilities.json), the crash-free contract, and the
``--fail-on-gap`` CLI gate.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import _run_delivery_checks, cli
from stackunderflow.store import db, schema

# ── fake adapters (duck-typed; the checker only touches .name/.enumerate) ────


class _FakeAdapter:
    def __init__(self, name: str, disk_sessions: int):
        self.name = name
        self._n = disk_sessions

    def enumerate(self):
        yield from range(self._n)

    def read(self, ref, *, since_offset: int = 0):  # pragma: no cover - unused
        return iter(())


class _BrokenAdapter:
    name = "broken"

    def enumerate(self):
        raise RuntimeError("cannot walk source dir")

    def read(self, ref, *, since_offset: int = 0):  # pragma: no cover - unused
        return iter(())


# ── store seeding ─────────────────────────────────────────────────────────────


def _seed(store: Path, *, providers: dict[str, dict]) -> None:
    """Seed ``{provider: {"messages": N, "events": N, "assistant": N}}``.

    ``assistant`` writes role='assistant' rows into a messages partition —
    the billable-shaped rows the GAP verdict keys on. Omitting it models a
    provider whose loaded rows are all non-billable (user/system only).
    """
    conn = db.connect(store)
    schema.apply(conn)
    # A test-owned partition: schema.apply pre-creates the CURRENT month's
    # real partition with full NOT NULL constraints, so seeding that would
    # couple this test to the wall clock and the production column set.
    # The delivery check discovers partitions via sqlite_master LIKE
    # 'messages_%', so a far-future name is picked up all the same.
    conn.execute(
        "CREATE TABLE messages_209901 ("
        " id INTEGER PRIMARY KEY, session_fk INTEGER, role TEXT)"
    )
    for provider, spec in providers.items():
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, "
            "last_modified) VALUES (?, ?, ?, 0.0, 0.0)",
            (provider, f"-proj-{provider}", provider),
        )
        pid = int(cur.lastrowid or 0)
        cur = conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
            "message_count) VALUES (?, ?, '2026-07-01', '2026-07-01', ?)",
            (pid, f"s-{provider}", spec.get("messages", 0)),
        )
        sess_fk = int(cur.lastrowid or 0)
        for _ in range(spec.get("assistant", 0)):
            conn.execute(
                "INSERT INTO messages_209901 (session_fk, role) "
                "VALUES (?, 'assistant')",
                (sess_fk,),
            )
        for i in range(spec.get("events", 0)):
            conn.execute(
                "INSERT INTO usage_events (source_message_fk, provider, account, "
                "project_id, session_id, ts, day, model, speed, input_tokens, "
                "output_tokens, cache_read_tokens, cache_create_tokens, "
                "reasoning_tokens, cost_usd, cost_source, role) "
                "VALUES (?, ?, '', ?, ?, '2026-07-01T00:00:00Z', '2026-07-01', "
                "'m1', 'standard', 1, 1, 0, 0, 0, 0.01, 'rate_card', 'assistant')",
                (1_000_000 + hash((provider, i)) % 1_000_000, provider, pid,
                 f"s-{provider}"),
            )
    conn.commit()
    conn.close()


# ── status semantics ──────────────────────────────────────────────────────────


def _row(result: dict, provider: str) -> dict:
    return next(r for r in result["providers"] if r["provider"] == provider)


def test_ok_gap_diskgap_empty_statuses(tmp_path):
    store = tmp_path / "store.db"
    _seed(store, providers={
        "healthy": {"messages": 10, "events": 5, "assistant": 6},
        "stranded": {"messages": 7, "events": 0, "assistant": 7},
    })
    result = _run_delivery_checks(store, adapters_override=[
        _FakeAdapter("healthy", 3),
        _FakeAdapter("stranded", 2),
        _FakeAdapter("ondisk-only", 4),
        _FakeAdapter("unused", 0),
    ])
    assert _row(result, "healthy")["status"] == "OK"
    assert _row(result, "stranded")["status"] == "GAP"
    assert _row(result, "ondisk-only")["status"] == "DISK_GAP"
    assert _row(result, "unused")["status"] == "EMPTY"
    assert result["ok"] is False
    assert set(result["gaps"]) == {"stranded", "ondisk-only"}


def test_no_billable_is_not_a_gap(tmp_path):
    """Rows loaded but none assistant-shaped (a slash-command-only trial
    session): zero events is CORRECT output — reported, never failed."""
    store = tmp_path / "store.db"
    _seed(store, providers={
        "tried-once": {"messages": 1, "events": 0},  # no assistant rows
    })
    result = _run_delivery_checks(
        store, adapters_override=[_FakeAdapter("tried-once", 3)]
    )
    assert _row(result, "tried-once")["status"] == "NO_BILLABLE"
    assert result["ok"] is True
    assert result["gaps"] == []


def test_exemption_is_data_driven_from_capabilities(tmp_path):
    """A provider capabilities.json marks emits_usage_events=false is EXEMPT
    even with base rows and zero events — and does not fail the check."""
    store = tmp_path / "store.db"
    _seed(store, providers={"antigravity": {"messages": 12, "events": 0}})
    result = _run_delivery_checks(
        store, adapters_override=[_FakeAdapter("antigravity", 12)]
    )
    assert _row(result, "antigravity")["status"] == "EXEMPT"
    assert result["ok"] is True
    assert result["gaps"] == []


def test_broken_adapter_degrades_never_crashes(tmp_path):
    store = tmp_path / "store.db"
    _seed(store, providers={})
    result = _run_delivery_checks(store, adapters_override=[_BrokenAdapter()])
    row = _row(result, "broken")
    assert row["disk_sessions"] is None
    assert row["status"] == "EMPTY"  # can't prove a disk gap it can't see
    assert result["ok"] is True


def test_missing_store_reads_as_all_zero(tmp_path):
    result = _run_delivery_checks(
        tmp_path / "nope.db", adapters_override=[_FakeAdapter("ghost", 2)]
    )
    assert _row(result, "ghost")["status"] == "DISK_GAP"
    assert result["ok"] is False


def test_counts_are_reported_exactly(tmp_path):
    store = tmp_path / "store.db"
    _seed(store, providers={"healthy": {"messages": 10, "events": 5}})
    row = _row(
        _run_delivery_checks(store, adapters_override=[_FakeAdapter("healthy", 3)]),
        "healthy",
    )
    assert row["disk_sessions"] == 3
    assert row["base_sessions"] == 1
    assert row["base_messages"] == 10
    assert row["usage_events"] == 5


# ── CLI wiring ────────────────────────────────────────────────────────────────


def _invoke(runner: CliRunner, args, store_db: Path, monkeypatch, adapters):
    import stackunderflow.adapters as adapters_pkg

    monkeypatch.setattr(deps, "store_path", store_db)
    monkeypatch.setattr(adapters_pkg, "registered", lambda: adapters)
    return runner.invoke(cli, args)


def test_doctor_json_carries_delivery_and_default_exit_is_health_only(
    tmp_path, monkeypatch
):
    """A delivery gap alone does NOT change the default exit code (back-compat:
    doctor's exit has always meant store health) — but it is fully reported."""
    store = tmp_path / "store.db"
    _seed(store, providers={"stranded": {"messages": 7, "events": 0, "assistant": 7}})
    r = _invoke(CliRunner(), ["doctor", "--json"], store, monkeypatch,
                [_FakeAdapter("stranded", 1)])
    assert r.exit_code == 0, r.output
    body = json.loads(r.output)
    assert body["ok"] is True  # store health
    assert body["delivery"]["ok"] is False
    assert body["delivery"]["gaps"] == ["stranded"]


def test_doctor_fail_on_gap_gates_on_delivery(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store, providers={"stranded": {"messages": 7, "events": 0, "assistant": 7}})
    r = _invoke(CliRunner(), ["doctor", "--fail-on-gap"], store, monkeypatch,
                [_FakeAdapter("stranded", 1)])
    assert r.exit_code == 1
    assert "stranded" in r.output

    _seed(tmp_path / "clean.db", providers={"healthy": {"messages": 3, "events": 3}})
    r2 = _invoke(CliRunner(), ["doctor", "--fail-on-gap"], tmp_path / "clean.db",
                 monkeypatch, [_FakeAdapter("healthy", 1)])
    assert r2.exit_code == 0, r2.output


def test_doctor_text_output_renders_scoreboard(tmp_path, monkeypatch):
    store = tmp_path / "store.db"
    _seed(store, providers={"healthy": {"messages": 3, "events": 3}})
    r = _invoke(CliRunner(), ["doctor"], store, monkeypatch,
                [_FakeAdapter("healthy", 1)])
    assert r.exit_code == 0, r.output
    assert "delivery (" in r.output
    assert "healthy" in r.output
    assert "OK" in r.output


def test_bad_partition_degrades_safely_never_masks(tmp_path, monkeypatch):
    """One odd messages_% table must not zero the marts or flip a real GAP
    to NO_BILLABLE: metrics degrade independently, and a failed billable
    scan biases base-bearing zero-event providers to GAP (flag, never mask),
    with the envelope carrying billable_scan_error."""
    store = tmp_path / "store.db"
    _seed(store, providers={
        "healthy": {"messages": 3, "events": 3},
        "stranded": {"messages": 7, "events": 0, "assistant": 7},
    })
    conn = db.connect(store)
    # Poisoned partition created BEFORE the real one is scanned: matches the
    # LIKE filter but lacks the join columns, so its query raises.
    conn.execute("CREATE TABLE messages_000bad (whatever TEXT)")
    conn.execute(
        "INSERT INTO provider_day_mart (day, provider, cost_usd, "
        "message_count, session_count, project_count) "
        "VALUES ('2026-07-01', 'healthy', 1.0, 3, 1, 1)"
    )
    conn.commit()
    conn.close()

    result = _run_delivery_checks(store, adapters_override=[
        _FakeAdapter("healthy", 1), _FakeAdapter("stranded", 1),
    ])
    # Marts survived the poisoned partition (independent degradation).
    assert _row(result, "healthy")["mart_messages"] == 3
    assert _row(result, "healthy")["status"] == "OK"
    # The real gap is flagged, not masked as NO_BILLABLE.
    assert _row(result, "stranded")["status"] == "GAP"
    assert result["ok"] is False
    assert result.get("billable_scan_error") is True


def test_healthy_store_skips_billable_scan(tmp_path, monkeypatch):
    """When every provider has events (or is exempt/empty), the expensive
    per-partition scan must not run at all."""
    store = tmp_path / "store.db"
    _seed(store, providers={"healthy": {"messages": 3, "events": 3}})
    conn = db.connect(store)
    # A poisoned partition that would raise IF scanned — the assertion that
    # no error flag appears proves the scan was skipped.
    conn.execute("CREATE TABLE messages_000bad (whatever TEXT)")
    conn.commit()
    conn.close()

    result = _run_delivery_checks(
        store, adapters_override=[_FakeAdapter("healthy", 1)]
    )
    assert _row(result, "healthy")["status"] == "OK"
    assert "billable_scan_error" not in result
    assert result["ok"] is True
