"""Unit tests for ``stackunderflow.services.discovery_telemetry``.

Covers the citation-feedback loop on discovery:

* ``record_loaded`` — bulk + idempotent + first/last timestamps + env gate
* ``record_cited`` — increments existing rows, records cites for
  never-loaded sessions, survives a later load
* ``cite_rate`` — 0.0 for never-loaded / loaded-but-uncited; ratio otherwise
* ``cite_rate_terms`` — clamped to [0, 1], demoted sessions zeroed,
  never-loaded sessions omitted
* ``demote_candidates`` / ``mark_demoted`` — both thresholds + the demote flag
* migration v009 idempotency + preservation of existing rows
* the ranking-term shape (higher cite_rate ⇒ higher score)
* a specific-session lookup records a cite (``record_cited``)

All tests use ``tmp_path``; the maintainer's real
``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import sqlite3
from datetime import UTC, datetime, timedelta

from stackunderflow.services import discovery_telemetry as telemetry
from stackunderflow.store import db, schema

# ── helpers ─────────────────────────────────────────────────────────────────


def _make_conn(tmp_path) -> sqlite3.Connection:
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _iso_days_ago(n: float) -> str:
    return (datetime.now(UTC) - timedelta(days=n)).isoformat()


def _seed_row(
    conn: sqlite3.Connection,
    *,
    command: str = "find_sessions_in_path",
    session_id: str,
    loaded: int,
    cited: int = 0,
    first_loaded_ts: str | None = None,
    demoted: int = 0,
) -> None:
    conn.execute(
        "INSERT INTO discovery_telemetry "
        "(command, session_id, loaded_count, cited_count, first_loaded_ts, "
        " last_loaded_ts, demoted) VALUES (?, ?, ?, ?, ?, ?, ?)",
        (command, session_id, loaded, cited, first_loaded_ts,
         first_loaded_ts, demoted),
    )


# ── record_loaded ───────────────────────────────────────────────────────────


class TestRecordLoaded:
    def test_bulk_insert_and_increment(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a", "b", "a"])
        rows = conn.execute(
            "SELECT session_id, loaded_count, cited_count FROM discovery_telemetry "
            "WHERE command = 'find_sessions_in_path' ORDER BY session_id"
        ).fetchall()
        # 'a' appeared twice in the same call → loaded_count 2; 'b' once.
        by_sid = {r["session_id"]: r["loaded_count"] for r in rows}
        assert by_sid == {"a": 2, "b": 1}
        assert all(r["cited_count"] == 0 for r in rows)

        # Re-surfacing bumps, does not duplicate.
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a"])
        by_sid = {
            r["session_id"]: r["loaded_count"]
            for r in conn.execute(
                "SELECT session_id, loaded_count FROM discovery_telemetry "
                "WHERE command = 'find_sessions_in_path'"
            ).fetchall()
        }
        assert by_sid == {"a": 3, "b": 1}
        # Exactly two rows — no duplicate (command, session_id).
        n = conn.execute(
            "SELECT COUNT(*) AS n FROM discovery_telemetry"
        ).fetchone()["n"]
        assert n == 2

    def test_per_command_rows_are_independent(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", ["s1"])
        telemetry.record_loaded(conn, "search_past_decisions", ["s1"])
        rows = conn.execute(
            "SELECT command, loaded_count FROM discovery_telemetry "
            "WHERE session_id = 's1' ORDER BY command"
        ).fetchall()
        assert [(r["command"], r["loaded_count"]) for r in rows] == [
            ("find_sessions_in_path", 1),
            ("search_past_decisions", 1),
        ]

    def test_empty_list_is_a_noop(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", [])
        assert conn.execute(
            "SELECT COUNT(*) AS n FROM discovery_telemetry"
        ).fetchone()["n"] == 0

    def test_sets_first_loaded_then_preserves_it(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", ["s1"])
        row = conn.execute(
            "SELECT first_loaded_ts, last_loaded_ts FROM discovery_telemetry "
            "WHERE session_id = 's1'"
        ).fetchone()
        first = row["first_loaded_ts"]
        assert first is not None
        assert row["last_loaded_ts"] == first

        telemetry.record_loaded(conn, "find_sessions_in_path", ["s1"])
        row = conn.execute(
            "SELECT first_loaded_ts, last_loaded_ts FROM discovery_telemetry "
            "WHERE session_id = 's1'"
        ).fetchone()
        assert row["first_loaded_ts"] == first  # unchanged
        assert row["last_loaded_ts"] >= first   # bumped (or equal on a fast clock)

    def test_env_gate_disables_writes(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_TELEMETRY", "0")
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a", "b"])
        assert conn.execute(
            "SELECT COUNT(*) AS n FROM discovery_telemetry"
        ).fetchone()["n"] == 0

        # Other falsy spellings.
        for val in ("false", "No", "OFF", ""):
            monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_TELEMETRY", val)
            assert telemetry.telemetry_enabled() is False
        # Anything else is on.
        for val in ("1", "true", "yes", "anything"):
            monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_TELEMETRY", val)
            assert telemetry.telemetry_enabled() is True
        monkeypatch.delenv("STACKUNDERFLOW_DISCOVERY_TELEMETRY", raising=False)
        assert telemetry.telemetry_enabled() is True

    def test_write_failure_swallowed(self, tmp_path):
        # A connection with no discovery_telemetry table (pre-v009) must
        # not raise out of record_loaded.
        conn = db.connect(tmp_path / "bare.db")  # no schema.apply
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a"])  # no raise
        conn.close()


# ── record_cited ────────────────────────────────────────────────────────────


class TestRecordCited:
    def test_increments_existing_row(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a"])
        telemetry.record_cited(conn, "a")
        row = conn.execute(
            "SELECT loaded_count, cited_count, last_cited_ts FROM discovery_telemetry "
            "WHERE session_id = 'a'"
        ).fetchone()
        assert (row["loaded_count"], row["cited_count"]) == (1, 1)
        assert row["last_cited_ts"] is not None

    def test_bumps_every_command_row_for_the_session(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a"])
        telemetry.record_loaded(conn, "search_past_decisions", ["a"])
        telemetry.record_cited(conn, "a")
        cited = {
            r["command"]: r["cited_count"]
            for r in conn.execute(
                "SELECT command, cited_count FROM discovery_telemetry "
                "WHERE session_id = 'a'"
            ).fetchall()
        }
        assert cited == {"find_sessions_in_path": 1, "search_past_decisions": 1}

    def test_never_loaded_still_records_the_cite(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_cited(conn, "ghost")
        rows = conn.execute(
            "SELECT command, loaded_count, cited_count FROM discovery_telemetry "
            "WHERE session_id = 'ghost' ORDER BY command"
        ).fetchall()
        # Fanned across all three commands, zero loads, one cite each.
        assert {r["command"] for r in rows} == set(telemetry.VALID_COMMANDS)
        assert all((r["loaded_count"], r["cited_count"]) == (0, 1) for r in rows)
        # cite_rate is still 0.0 — never loaded.
        assert telemetry.cite_rate(conn, "find_sessions_in_path", "ghost") == 0.0

    def test_cite_then_load_makes_the_cite_count(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_cited(conn, "later")  # cite before any load
        telemetry.record_loaded(conn, "find_sessions_in_path", ["later"])
        row = conn.execute(
            "SELECT loaded_count, cited_count FROM discovery_telemetry "
            "WHERE command = 'find_sessions_in_path' AND session_id = 'later'"
        ).fetchone()
        assert (row["loaded_count"], row["cited_count"]) == (1, 1)
        assert telemetry.cite_rate(conn, "find_sessions_in_path", "later") == 1.0

    def test_source_command_narrows_the_never_loaded_seed(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_cited(conn, "scoped", source_command="search_past_decisions")
        rows = conn.execute(
            "SELECT command FROM discovery_telemetry WHERE session_id = 'scoped'"
        ).fetchall()
        assert [r["command"] for r in rows] == ["search_past_decisions"]

    def test_empty_session_id_and_env_gate(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        telemetry.record_cited(conn, "")
        assert conn.execute(
            "SELECT COUNT(*) AS n FROM discovery_telemetry"
        ).fetchone()["n"] == 0
        monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_TELEMETRY", "0")
        telemetry.record_cited(conn, "x")
        assert conn.execute(
            "SELECT COUNT(*) AS n FROM discovery_telemetry"
        ).fetchone()["n"] == 0


# ── cite_rate / cite_rate_terms ─────────────────────────────────────────────


class TestCiteRate:
    def test_never_loaded_is_zero(self, tmp_path):
        conn = _make_conn(tmp_path)
        assert telemetry.cite_rate(conn, "find_sessions_in_path", "nope") == 0.0

    def test_loaded_but_never_cited_is_zero(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a"])
        assert telemetry.cite_rate(conn, "find_sessions_in_path", "a") == 0.0

    def test_ratio(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="a", loaded=4, cited=2)
        assert telemetry.cite_rate(conn, "find_sessions_in_path", "a") == 0.5

    def test_ratio_can_exceed_one(self, tmp_path):
        # cited can outpace loaded once cross-command cites land — cite_rate
        # returns the raw ratio (the *_terms variant is the clamped one).
        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="a", loaded=1, cited=3)
        assert telemetry.cite_rate(conn, "find_sessions_in_path", "a") == 3.0

    def test_terms_clamps_and_excludes_demoted_and_unloaded(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="hot", loaded=2, cited=1)        # 0.5
        _seed_row(conn, session_id="overcited", loaded=1, cited=5)  # clamp → 1.0
        _seed_row(conn, session_id="demoted", loaded=10, cited=8, demoted=1)  # → 0.0
        _seed_row(conn, session_id="ghost", loaded=0, cited=1)      # omitted
        terms = telemetry.cite_rate_terms(conn, "find_sessions_in_path")
        assert terms == {"hot": 0.5, "overcited": 1.0, "demoted": 0.0}
        assert "ghost" not in terms

    def test_read_failure_returns_empty(self, tmp_path):
        conn = db.connect(tmp_path / "bare.db")  # no schema
        assert telemetry.cite_rate(conn, "find_sessions_in_path", "a") == 0.0
        assert telemetry.cite_rate_terms(conn, "find_sessions_in_path") == {}
        conn.close()


# ── ranking integration shape ───────────────────────────────────────────────


class TestRankingTerm:
    def test_higher_cite_rate_scores_higher(self, tmp_path):
        """Two sessions, equal everything *except* cite_rate — the one with
        the higher cite_rate gets the higher cite-term score, so when the
        cite term is added to an otherwise-equal base it ranks first.

        This mirrors spec §Tests "Ranking integration" — the actual
        composition into ``pack_within_budget`` lands at merge time with
        spec 03; here we lock in the term contract it consumes.
        """
        from dataclasses import dataclass

        @dataclass(frozen=True)
        class _M:  # minimal SessionMatch stand-in (only session_id is read)
            session_id: str

        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="loved", loaded=4, cited=4)   # 1.0
        _seed_row(conn, session_id="meh", loaded=4, cited=1)     # 0.25
        terms = telemetry.cite_rate_terms(conn, "find_sessions_in_path")

        # Equal base score for both; only the cite term differs.
        base = 0.40 * 0.5 + 0.15 * 0.5 + 0.15 * 0.5  # recency/cost/relevance equal
        weight = 0.30
        score_loved = base + weight * terms.get(_M("loved").session_id, 0.0)
        score_meh = base + weight * terms.get(_M("meh").session_id, 0.0)
        assert score_loved > score_meh
        # And an unknown session falls back to 0.0 cleanly.
        assert terms.get(_M("unknown").session_id, 0.0) == 0.0


# ── demote_candidates / mark_demoted ────────────────────────────────────────


class TestDemoteCandidates:
    def test_respects_both_thresholds(self, tmp_path):
        conn = _make_conn(tmp_path)
        # qualifies: many loads, old, zero cites
        _seed_row(conn, session_id="noise", loaded=25, cited=0,
                  first_loaded_ts=_iso_days_ago(10))
        # too few loads
        _seed_row(conn, session_id="rare", loaded=5, cited=0,
                  first_loaded_ts=_iso_days_ago(30))
        # too young (surfaced a lot but only today)
        _seed_row(conn, session_id="fresh", loaded=50, cited=0,
                  first_loaded_ts=_iso_days_ago(1))
        # has a citation
        _seed_row(conn, session_id="useful", loaded=30, cited=1,
                  first_loaded_ts=_iso_days_ago(20))
        # already demoted
        _seed_row(conn, session_id="gone", loaded=40, cited=0,
                  first_loaded_ts=_iso_days_ago(20), demoted=1)
        cands = telemetry.demote_candidates(conn, min_loads=20, min_age_days=7)
        assert [(c, s, n) for c, s, n in cands] == [
            ("find_sessions_in_path", "noise", 25),
        ]

    def test_empty_when_nothing_hits(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="a", loaded=3, cited=0,
                  first_loaded_ts=_iso_days_ago(30))
        assert telemetry.demote_candidates(conn) == []

    def test_sorted_worst_first(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="a", loaded=25, cited=0,
                  first_loaded_ts=_iso_days_ago(10))
        _seed_row(conn, session_id="b", loaded=99, cited=0,
                  first_loaded_ts=_iso_days_ago(10))
        cands = telemetry.demote_candidates(conn, min_loads=20, min_age_days=7)
        assert [s for _, s, _ in cands] == ["b", "a"]

    def test_mark_demoted_sets_flag(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="noise", loaded=25, cited=0,
                  first_loaded_ts=_iso_days_ago(10))
        n = telemetry.mark_demoted(conn, [("find_sessions_in_path", "noise")])
        assert n == 1
        assert conn.execute(
            "SELECT demoted FROM discovery_telemetry WHERE session_id = 'noise'"
        ).fetchone()["demoted"] == 1
        # Now it no longer shows as a candidate.
        assert telemetry.demote_candidates(conn, min_loads=20, min_age_days=7) == []
        # mark_demoted on nothing is a clean no-op.
        assert telemetry.mark_demoted(conn, []) == 0

    def test_never_loaded_rows_excluded(self, tmp_path):
        # A cite-before-load row has first_loaded_ts NULL — never a candidate.
        conn = _make_conn(tmp_path)
        telemetry.record_cited(conn, "ghost")
        assert telemetry.demote_candidates(conn, min_loads=0, min_age_days=0) == []


# ── iter_telemetry ──────────────────────────────────────────────────────────


class TestIterTelemetry:
    def test_shape_and_filters(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_loaded(conn, "find_sessions_in_path", ["a", "b"])
        telemetry.record_loaded(conn, "search_past_decisions", ["a"])
        telemetry.record_cited(conn, "a")

        rows = telemetry.iter_telemetry(conn)
        assert len(rows) == 3
        sample = rows[0]
        assert set(sample) >= {
            "command", "session_id", "loaded_count", "cited_count",
            "cite_rate", "first_loaded_ts", "last_loaded_ts",
            "last_cited_ts", "demoted",
        }
        assert isinstance(sample["demoted"], bool)

        # command filter
        only_search = telemetry.iter_telemetry(conn, command="search_past_decisions")
        assert {r["command"] for r in only_search} == {"search_past_decisions"}
        # session filter
        only_a = telemetry.iter_telemetry(conn, session_id="a")
        assert {r["session_id"] for r in only_a} == {"a"}
        assert len(only_a) == 2  # two commands surfaced 'a'
        # limit
        assert len(telemetry.iter_telemetry(conn, limit=1)) == 1
        # cite_rate computed
        a_in_path = next(
            r for r in only_a if r["command"] == "find_sessions_in_path"
        )
        assert a_in_path["cite_rate"] == 1.0

    def test_read_failure_returns_empty(self, tmp_path):
        conn = db.connect(tmp_path / "bare.db")
        assert telemetry.iter_telemetry(conn) == []
        conn.close()


# ── migration v009 ──────────────────────────────────────────────────────────


class TestMigrationV009:
    def test_brings_fresh_db_to_version_9_with_table(self, tmp_path):
        conn = _make_conn(tmp_path)
        assert conn.execute("PRAGMA user_version").fetchone()[0] >= 9
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        cols = {r[1] for r in conn.execute(
            "PRAGMA table_info(discovery_telemetry)"
        ).fetchall()}
        assert {"command", "session_id", "loaded_count", "cited_count",
                "first_loaded_ts", "last_loaded_ts", "last_cited_ts",
                "demoted"} <= cols

    def test_idempotent_and_preserves_rows(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_row(conn, session_id="keep", loaded=7, cited=3,
                  first_loaded_ts="2026-01-01T00:00:00+00:00")
        # Re-apply — must be a no-op (already at v009) and keep the row.
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] >= 9
        row = conn.execute(
            "SELECT loaded_count, cited_count FROM discovery_telemetry "
            "WHERE session_id = 'keep'"
        ).fetchone()
        assert (row["loaded_count"], row["cited_count"]) == (7, 3)

    def test_partial_apply_recovery(self, tmp_path):
        # Table exists but user_version wasn't bumped (crash between DDL
        # and PRAGMA). ``IF NOT EXISTS`` lets the migration re-run cleanly.
        conn = db.connect(tmp_path / "store.db")
        schema.apply(conn)  # → v9
        conn.execute("PRAGMA user_version = 8")  # simulate the crash window
        schema.apply(conn)  # must not raise on CREATE TABLE
        assert conn.execute("PRAGMA user_version").fetchone()[0] >= 9


# ── session-lookup citation recording ───────────────────────────────────────


class TestSessionLookupRecordsCite:
    """Looking up a specific session records a citation against discovery
    telemetry. That loop was driven by the retired MCP ``session_query``
    tool; ``record_cited`` is the service-layer primitive it called, and
    remains the contract a specific-session lookup relies on.
    """

    def test_specific_session_lookup_records_a_cite(self, tmp_path):
        conn = _make_conn(tmp_path)
        # The session was never surfaced by a discovery command — the
        # lookup still records the cite, fanned across every known
        # command so whichever one surfaces it next already carries it.
        telemetry.record_cited(conn, "sess-xyz")
        rows = conn.execute(
            "SELECT command, loaded_count, cited_count FROM discovery_telemetry "
            "WHERE session_id = 'sess-xyz' ORDER BY command"
        ).fetchall()
        assert {r["command"] for r in rows} == set(telemetry.VALID_COMMANDS)
        assert all((r["loaded_count"], r["cited_count"]) == (0, 1) for r in rows)

    def test_no_session_id_records_nothing(self, tmp_path):
        conn = _make_conn(tmp_path)
        telemetry.record_cited(conn, "")
        assert conn.execute(
            "SELECT COUNT(*) AS n FROM discovery_telemetry"
        ).fetchone()["n"] == 0

    def test_env_gate_skips_the_cite(self, tmp_path, monkeypatch):
        conn = _make_conn(tmp_path)
        monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_TELEMETRY", "0")
        telemetry.record_cited(conn, "sess-xyz")
        assert conn.execute(
            "SELECT COUNT(*) AS n FROM discovery_telemetry"
        ).fetchone()["n"] == 0
