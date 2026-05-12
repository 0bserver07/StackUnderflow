"""Unit tests for ``stackunderflow.services.discovery``.

Covers:

* Path-based ancestor matching (``find_sessions_in_path``).
* Tool-args + free-form mention search (``find_sessions_touching_file``).
* Substring search with snippet (``search_past_decisions``).
* Outcome-aware discovery (``find_sessions_where_action_worked`` /
  ``find_failure_modes_for_file`` / ``_classify_outcome``).
* Helper edge cases (``parse_since``, ``decode_slug_to_path``).

All tests use ``tmp_path`` or ``:memory:``; the maintainer's real
``~/.stackunderflow/store.db`` is never touched.
"""

from __future__ import annotations

import json
import sqlite3
from datetime import UTC, datetime, timedelta

import pytest

from stackunderflow.services.discovery import (
    OutcomeMatch,
    SessionMatch,
    _classify_outcome,
    decode_slug_to_path,
    find_failure_modes_for_file,
    find_sessions_in_path,
    find_sessions_touching_file,
    find_sessions_where_action_worked,
    parse_since,
    search_past_decisions,
)
from stackunderflow.store import db, schema

# ── seeding helpers ─────────────────────────────────────────────────────────


def _make_conn(tmp_path) -> sqlite3.Connection:
    """Open a real store at tmp_path and apply migrations."""
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _insert_message(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    seq: int,
    timestamp: str,
    role: str = "assistant",
    model: str = "claude-sonnet-4-5",
    content_text: str = "",
    tools_json: str = "[]",
) -> int:
    """Insert a message and return the assigned id.

    ``messages`` becomes a UNION view at v008; the trigger allocates ids
    from ``_messages_id_seq``. Mirroring the pattern in
    ``tests/stackunderflow/cli/test_etl_status.py``.
    """
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, ?, ?, 0, 0, 0, 0, ?, ?, '{}', 0)",
        (session_fk, seq, timestamp, role, model, content_text, tools_json),
    )
    row = conn.execute(
        "SELECT next_id - 1 AS mid FROM _messages_id_seq WHERE rowid_kind = 1"
    ).fetchone()
    return int(row["mid"])


def _seed_project(
    conn: sqlite3.Connection,
    *,
    provider: str = "claude",
    slug: str = "-Users-yad-dev-foo",
    path: str | None = None,
) -> int:
    """Insert a project row, return its id."""
    cur = conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, "
        " first_seen, last_modified) VALUES (?, ?, ?, ?, 0.0, 0.0)",
        (provider, slug, path, slug),
    )
    return int(cur.lastrowid)


def _seed_session(
    conn: sqlite3.Connection,
    *,
    project_id: int,
    session_id: str,
    first_ts: str,
    last_ts: str,
    message_count: int = 0,
) -> int:
    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
        " message_count) VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, first_ts, last_ts, message_count),
    )
    return int(cur.lastrowid)


def _seed_session_mart(
    conn: sqlite3.Connection,
    *,
    session_id: str,
    project_id: int,
    cost_usd: float,
    first_ts: str,
    last_ts: str,
) -> None:
    conn.execute(
        "INSERT INTO session_mart "
        "(session_id, project_id, provider, primary_model, first_ts, last_ts, "
        " message_count, cost_usd) "
        "VALUES (?, ?, 'claude', 'claude-sonnet-4-5', ?, ?, 1, ?)",
        (session_id, project_id, first_ts, last_ts, cost_usd),
    )


# ── helper-fn unit tests ────────────────────────────────────────────────────


class TestDecodeSlug:
    def test_standard_unix_path(self):
        assert decode_slug_to_path("-Users-yad-dev-foo") == "/Users/yad/dev/foo"

    def test_empty_returns_empty(self):
        assert decode_slug_to_path("") == ""

    def test_non_path_slug_returns_empty(self):
        # Cursor workspace ids and similar aren't fs paths.
        assert decode_slug_to_path("workspace-abc123") == ""


class TestParseSince:
    def test_none_passes_through(self):
        assert parse_since(None) is None
        assert parse_since("") is None

    def test_relative_days(self):
        out = parse_since("7d")
        # Result is an ISO datetime string; just check it's parseable
        # and roughly seven days back.
        parsed = datetime.fromisoformat(out)
        delta = datetime.now(UTC) - parsed
        assert timedelta(days=6, hours=23) <= delta <= timedelta(days=7, hours=1)

    def test_relative_weeks_hours_months(self):
        for token in ("1w", "24h", "1m"):
            assert parse_since(token) is not None

    def test_iso_date_only(self):
        out = parse_since("2026-01-01")
        assert out is not None
        # Date-only inputs gain a UTC timezone.
        assert "2026-01-01" in out

    def test_iso_full_datetime(self):
        out = parse_since("2026-04-01T12:00:00+00:00")
        assert out is not None
        assert "2026-04-01" in out

    def test_garbage_raises(self):
        with pytest.raises(ValueError):
            parse_since("yesterday")


# ── find_sessions_in_path ───────────────────────────────────────────────────


class TestFindSessionsInPath:
    def test_exact_project_path_matches(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00",
                      message_count=5)
        conn.commit()

        out = find_sessions_in_path(conn, "/Users/yad/dev/foo")
        assert len(out) == 1
        m = out[0]
        assert m.session_id == "s-1"
        assert m.project_slug == "-Users-yad-dev-foo"
        assert m.project_path == "/Users/yad/dev/foo"
        assert m.message_count == 5
        assert m.snippet is None

    def test_descendant_path_matches_ancestor_project(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()

        out = find_sessions_in_path(conn, "/Users/yad/dev/foo/src/main.py")
        assert len(out) == 1
        assert out[0].session_id == "s-1"

    def test_unrelated_path_does_not_match(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()

        # Sibling, not ancestor — must not match.
        assert find_sessions_in_path(conn, "/Users/yad/dev/bar") == []
        # Prefix-but-not-directory-ancestor must not match either.
        assert find_sessions_in_path(conn, "/Users/yad/dev/foobar") == []

    def test_ancestor_query_does_not_match_descendant_project(self, tmp_path):
        """Querying /Users must NOT return the project at /Users/yad/dev/foo."""
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()
        # /Users is *above* /Users/yad/dev/foo; per contract, only
        # ancestor-of-caller projects match. /Users isn't an ancestor.
        assert find_sessions_in_path(conn, "/Users") == []

    def test_provider_filter(self, tmp_path):
        conn = _make_conn(tmp_path)
        cpid = _seed_project(conn, provider="claude", slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=cpid, session_id="claude-s",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        cxpid = _seed_project(conn, provider="codex", slug="-Users-yad-dev-foo-codex")
        _seed_session(conn, project_id=cxpid, session_id="codex-s",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()

        # codex slug decodes to /Users/yad/dev/foo/codex which is
        # descendant of caller path /Users/yad/dev/foo... but the
        # ancestor relationship goes the other way: project must be
        # ancestor of caller. So neither codex nor claude descendants
        # match this path.
        out_claude = find_sessions_in_path(
            conn, "/Users/yad/dev/foo", provider="claude",
        )
        assert [m.session_id for m in out_claude] == ["claude-s"]

        out_codex = find_sessions_in_path(
            conn, "/Users/yad/dev/foo", provider="codex",
        )
        # codex project's path is /Users/yad/dev/foo/codex which is
        # a descendant of the caller, NOT an ancestor — so it doesn't
        # match.
        assert out_codex == []

    def test_since_filter(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="old-s",
                      first_ts="2025-01-01T00:00:00+00:00",
                      last_ts="2025-01-01T00:00:00+00:00")
        recent = (datetime.now(UTC) - timedelta(hours=1)).isoformat()
        _seed_session(conn, project_id=pid, session_id="recent-s",
                      first_ts=recent, last_ts=recent)
        conn.commit()

        out = find_sessions_in_path(conn, "/Users/yad/dev/foo", since="1d")
        assert [m.session_id for m in out] == ["recent-s"]

    def test_sort_by_last_ts_desc_and_limit(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="a",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-01T00:00:00+00:00")
        _seed_session(conn, project_id=pid, session_id="b",
                      first_ts="2026-04-02T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        _seed_session(conn, project_id=pid, session_id="c",
                      first_ts="2026-04-03T00:00:00+00:00",
                      last_ts="2026-04-03T00:00:00+00:00")
        conn.commit()

        out = find_sessions_in_path(conn, "/Users/yad/dev/foo", limit=2)
        assert [m.session_id for m in out] == ["c", "b"]

    def test_session_mart_cost_used_when_present(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        _seed_session_mart(conn, session_id="s-1", project_id=pid,
                           cost_usd=2.50,
                           first_ts="2026-04-01T00:00:00+00:00",
                           last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()

        out = find_sessions_in_path(conn, "/Users/yad/dev/foo")
        assert out[0].cost_usd == 2.50

    def test_empty_store_returns_empty_list(self, tmp_path):
        conn = _make_conn(tmp_path)
        assert find_sessions_in_path(conn, "/Users/yad/dev/foo") == []

    def test_stored_path_takes_precedence_over_slug(self, tmp_path):
        """When projects.path is non-null, it wins over the slug decode."""
        conn = _make_conn(tmp_path)
        pid = _seed_project(
            conn,
            slug="cursor-workspace-abc",
            path="/Users/yad/dev/has_underscore",
        )
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()

        # Slug doesn't decode to a unix path, so without the explicit
        # ``path`` we'd miss this — confirming the explicit path is
        # consulted.
        out = find_sessions_in_path(conn, "/Users/yad/dev/has_underscore")
        assert len(out) == 1
        assert out[0].project_path == "/Users/yad/dev/has_underscore"


# ── find_sessions_touching_file ─────────────────────────────────────────────


def _read_tool_blob(file_path: str) -> str:
    return json.dumps([{"name": "Read", "input": {"file_path": file_path}}])


def _edit_tool_blob(file_path: str) -> str:
    return json.dumps([{"name": "Edit", "input": {
        "file_path": file_path, "old_string": "x", "new_string": "y",
    }}])


def _write_tool_blob(file_path: str) -> str:
    return json.dumps([{"name": "Write", "input": {
        "file_path": file_path, "content": "hi",
    }}])


class TestFindSessionsTouchingFile:
    def test_read_mode_matches_read_tool(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s-read",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00",
                            message_count=1)
        target = "/Users/yad/dev/foo/src/main.py"
        _insert_message(conn, session_fk=sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        tools_json=_read_tool_blob(target))
        conn.commit()

        out = find_sessions_touching_file(conn, target, mode="read")
        assert [m.session_id for m in out] == ["s-read"]

    def test_read_mode_does_not_match_edit_tool(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s-edit",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        target = "/Users/yad/dev/foo/src/main.py"
        _insert_message(conn, session_fk=sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        tools_json=_edit_tool_blob(target))
        conn.commit()

        out = find_sessions_touching_file(conn, target, mode="read")
        assert out == []

    def test_write_mode_matches_edit_and_write(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk_e = _seed_session(conn, project_id=pid, session_id="s-edit",
                              first_ts="2026-04-01T00:00:00+00:00",
                              last_ts="2026-04-02T00:00:00+00:00")
        sfk_w = _seed_session(conn, project_id=pid, session_id="s-write",
                              first_ts="2026-04-01T00:00:00+00:00",
                              last_ts="2026-04-03T00:00:00+00:00")
        target = "/Users/yad/dev/foo/src/main.py"
        _insert_message(conn, session_fk=sfk_e, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        tools_json=_edit_tool_blob(target))
        _insert_message(conn, session_fk=sfk_w, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        tools_json=_write_tool_blob(target))
        conn.commit()

        out = find_sessions_touching_file(conn, target, mode="write")
        sids = {m.session_id for m in out}
        assert sids == {"s-edit", "s-write"}

    def test_any_mode_matches_freeform_content(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s-mention",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        target = "/Users/yad/dev/foo/src/main.py"
        _insert_message(conn, session_fk=sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        content_text=f"Let me look at {target} real quick.")
        conn.commit()

        out_any = find_sessions_touching_file(conn, target, mode="any")
        assert [m.session_id for m in out_any] == ["s-mention"]
        # read mode should NOT match a free-form content reference.
        out_read = find_sessions_touching_file(conn, target, mode="read")
        assert out_read == []

    def test_no_match_empty_list(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()

        assert find_sessions_touching_file(
            conn, "/somewhere/else.py", mode="any",
        ) == []

    def test_invalid_mode_raises(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError):
            find_sessions_touching_file(conn, "/x", mode="execute")

    def test_dedupes_session_with_multiple_hits(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        target = "/Users/yad/dev/foo/main.py"
        for i in range(3):
            _insert_message(conn, session_fk=sfk, seq=i,
                            timestamp="2026-04-01T00:00:00+00:00",
                            tools_json=_read_tool_blob(target))
        conn.commit()

        out = find_sessions_touching_file(conn, target, mode="any")
        assert len(out) == 1


# ── search_past_decisions ───────────────────────────────────────────────────


class TestSearchPastDecisions:
    def test_finds_substring_match(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        _insert_message(
            conn, session_fk=sfk, seq=0,
            timestamp="2026-04-01T00:00:00+00:00",
            content_text="We decided to ship the watcher behind a flag.",
        )
        conn.commit()

        out = search_past_decisions(conn, "watcher behind a flag")
        assert len(out) == 1
        assert out[0].snippet is not None
        assert "watcher behind a flag" in out[0].snippet

    def test_empty_query_returns_empty(self, tmp_path):
        conn = _make_conn(tmp_path)
        assert search_past_decisions(conn, "") == []
        assert search_past_decisions(conn, "   ") == []

    def test_no_match_returns_empty(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        _insert_message(conn, session_fk=sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        content_text="Unrelated content.")
        conn.commit()

        assert search_past_decisions(conn, "watcher") == []

    def test_project_filter(self, tmp_path):
        conn = _make_conn(tmp_path)
        a_pid = _seed_project(conn, slug="-Users-a")
        b_pid = _seed_project(conn, slug="-Users-b")
        a_sfk = _seed_session(conn, project_id=a_pid, session_id="a-s",
                              first_ts="2026-04-01T00:00:00+00:00",
                              last_ts="2026-04-02T00:00:00+00:00")
        b_sfk = _seed_session(conn, project_id=b_pid, session_id="b-s",
                              first_ts="2026-04-01T00:00:00+00:00",
                              last_ts="2026-04-02T00:00:00+00:00")
        _insert_message(conn, session_fk=a_sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        content_text="picked monorepo")
        _insert_message(conn, session_fk=b_sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        content_text="picked monorepo")
        conn.commit()

        out = search_past_decisions(conn, "monorepo", project="-Users-a")
        assert [m.project_slug for m in out] == ["-Users-a"]

    def test_since_filter(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        old_sfk = _seed_session(conn, project_id=pid, session_id="old",
                                first_ts="2024-01-01T00:00:00+00:00",
                                last_ts="2024-01-01T00:00:00+00:00")
        recent_sfk = _seed_session(
            conn, project_id=pid, session_id="recent",
            first_ts=(datetime.now(UTC) - timedelta(hours=1)).isoformat(),
            last_ts=(datetime.now(UTC) - timedelta(hours=1)).isoformat(),
        )
        _insert_message(conn, session_fk=old_sfk, seq=0,
                        timestamp="2024-01-01T00:00:00+00:00",
                        content_text="watcher decision")
        _insert_message(
            conn, session_fk=recent_sfk, seq=0,
            timestamp=(datetime.now(UTC) - timedelta(hours=1)).isoformat(),
            content_text="watcher decision",
        )
        conn.commit()

        out = search_past_decisions(conn, "watcher decision", since="1d")
        assert [m.session_id for m in out] == ["recent"]

    def test_snippet_truncates_around_match(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        long_text = ("X" * 500) + " NEEDLE here " + ("Y" * 500)
        _insert_message(conn, session_fk=sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        content_text=long_text)
        conn.commit()

        out = search_past_decisions(conn, "NEEDLE")
        assert out[0].snippet is not None
        # Snippet is roughly bounded; the exact upper bound is
        # ``2 * radius + len(query) + 2`` ellipses.
        assert "NEEDLE" in out[0].snippet
        assert len(out[0].snippet) <= 250

    def test_returns_session_match_dataclass(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        _insert_message(conn, session_fk=sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        content_text="we decided")
        conn.commit()

        out = search_past_decisions(conn, "decided")
        assert isinstance(out[0], SessionMatch)
        # to_dict round-trips JSON-serialisable.
        as_dict = out[0].to_dict()
        json.dumps(as_dict)  # must not raise


# ── outcome-aware discovery ─────────────────────────────────────────────────


_ANCHOR_FILE = "/Users/yad/dev/foo/cost.py"


def _edit_blob(file_path: str = _ANCHOR_FILE) -> str:
    return json.dumps([{"name": "Edit", "input": {
        "file_path": file_path, "old_string": "a", "new_string": "b",
    }}])


def _bash_blob(command: str) -> str:
    return json.dumps([{"name": "Bash", "input": {"command": command}}])


def _seed_msg(
    conn: sqlite3.Connection,
    *,
    session_fk: int,
    seq: int,
    role: str,
    content_text: str = "",
    tools_json: str = "[]",
    is_sidechain: int = 0,
    timestamp: str = "2026-04-01T00:00:00+00:00",
) -> int:
    """Insert one message (handling the v008 view trigger) and return its id."""
    conn.execute(
        "INSERT INTO messages "
        "(session_fk, seq, timestamp, role, model, "
        " input_tokens, output_tokens, cache_create_tokens, cache_read_tokens, "
        " content_text, tools_json, raw_json, is_sidechain) "
        "VALUES (?, ?, ?, ?, NULL, 0, 0, 0, 0, ?, ?, '{}', ?)",
        (session_fk, seq, timestamp, role, content_text, tools_json, is_sidechain),
    )
    row = conn.execute(
        "SELECT next_id - 1 AS mid FROM _messages_id_seq WHERE rowid_kind = 1"
    ).fetchone()
    return int(row["mid"])


def _seed_outcome_session(
    conn: sqlite3.Connection,
    *,
    session_id: str,
    turns: list[tuple],
    project_slug: str = "-Users-yad-dev-foo",
    last_ts: str = "2026-04-02T00:00:00+00:00",
    timestamp: str = "2026-04-01T00:00:00+00:00",
) -> tuple[int, list[int]]:
    """Seed a project (if new), a session, and a list of message ``turns``.

    Each turn is ``(role, content_text, tools_json[, is_sidechain])``.
    All messages share ``timestamp`` (the ``messages`` view is INSERT-only
    on a v008 store, so callers tune the time here rather than UPDATE-ing).
    Returns ``(session_fk, [msg_id, ...])``.
    """
    pid_row = conn.execute(
        "SELECT id FROM projects WHERE slug = ?", (project_slug,)
    ).fetchone()
    if pid_row is None:
        pid = _seed_project(conn, slug=project_slug)
    else:
        pid = int(pid_row["id"])
    sfk = _seed_session(
        conn, project_id=pid, session_id=session_id,
        first_ts="2026-04-01T00:00:00+00:00", last_ts=last_ts,
        message_count=len(turns),
    )
    ids: list[int] = []
    for seq, turn in enumerate(turns):
        role, content = turn[0], turn[1]
        tools = turn[2] if len(turn) > 2 else "[]"
        sc = turn[3] if len(turn) > 3 else 0
        ids.append(_seed_msg(
            conn, session_fk=sfk, seq=seq, role=role,
            content_text=content, tools_json=tools, is_sidechain=sc,
            timestamp=timestamp,
        ))
    return sfk, ids


# ── _classify_outcome (direct) ──────────────────────────────────────────────


def _rows(*specs: tuple) -> list[dict]:
    """Build classify-able rows. Each spec is ``(id, role, text[, tools[, sc]])``.

    ``_classify_outcome`` reads rows via ``row["key"]`` / ``.get`` so a
    plain dict stands in for a ``sqlite3.Row`` here.
    """
    out: list[dict] = []
    for s in specs:
        out.append({
            "id": s[0],
            "role": s[1],
            "content_text": s[2],
            "tools_json": s[3] if len(s) > 3 else "[]",
            "is_sidechain": s[4] if len(s) > 4 else 0,
        })
    return out


class TestClassifyOutcome:
    def test_positive_keyword_worked(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "user", "thanks, that worked perfectly!"),
        )
        outcome, evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "worked"
        assert mid == 2
        assert "worked" in evidence.lower()

    def test_negative_keyword_failed(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "user", "no, that broke the build"),
        )
        outcome, _evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "failed"
        assert mid == 2

    def test_revert_keyword_reverted(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "user", "actually, undo that change"),
        )
        outcome, _evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "reverted"
        assert mid == 2

    def test_agent_git_reset_is_reverted(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "assistant", "", _bash_blob("git reset --hard HEAD~1")),
            (3, "user", "ok thanks"),
        )
        outcome, evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "reverted"
        assert mid == 2
        assert "git reset" in evidence

    def test_agent_git_revert_is_reverted(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "assistant", "", _bash_blob("git revert abc1234 --no-edit")),
        )
        outcome, _evidence, _mid = _classify_outcome(rows, 0)
        assert outcome == "reverted"

    def test_silence_after_action_is_worked(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "assistant", "Done — applied the edit."),
        )
        outcome, evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "worked"
        assert mid == 2  # the last message of the session
        assert "no user complaint" in evidence

    def test_tool_only_followups_walk_further_then_worked(self):
        # Empty-content user messages are tool results — skipped — so we
        # "walk further" and land on silence ⇒ worked.
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "user", ""),               # tool_result
            (3, "assistant", "running the tests now"),
            (4, "user", ""),               # tool_result
            (5, "assistant", "tests pass"),
        )
        outcome, _evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "worked"
        assert mid == 5

    def test_last_message_is_uncertain(self):
        rows = _rows((1, "assistant", "", _edit_blob()))
        outcome, evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "uncertain"
        assert mid == 1
        assert "last recorded turn" in evidence

    def test_neutral_followups_exhaust_window_then_uncertain(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "user", "what about the other file?"),
            (3, "user", "and the tests?"),
            (4, "user", "any other usages?"),
            (5, "user", "what's the import path?"),
            (6, "user", "how about typing?"),
            (7, "user", "thanks that worked"),   # beyond the 5-turn window
        )
        outcome, _evidence, _mid = _classify_outcome(rows, 0)
        assert outcome == "uncertain"

    def test_no_problem_is_not_a_complaint(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "user", "no problem, looks good!"),
        )
        outcome, _evidence, _mid = _classify_outcome(rows, 0)
        assert outcome == "worked"

    def test_sidechain_after_action_is_transparent(self):
        # A Task sub-agent runs (and even reverts!) right after the edit;
        # the parent session's own next user turn is "thanks" ⇒ worked.
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "assistant", "", _bash_blob("git reset --hard"), 1),  # sidechain
            (3, "user", "sub-agent output", "[]", 1),                 # sidechain
            (4, "user", "perfect, thanks"),                           # parent
        )
        outcome, _evidence, mid = _classify_outcome(rows, 0)
        assert outcome == "worked"
        assert mid == 4

    def test_lookahead_param_respected(self):
        rows = _rows(
            (1, "assistant", "", _edit_blob()),
            (2, "user", "neutral one"),
            (3, "user", "thanks, that worked"),
        )
        # With lookahead=1 we stop after the first neutral turn ⇒ uncertain.
        assert _classify_outcome(rows, 0, lookahead=1)[0] == "uncertain"
        # With the default window we reach the "thanks" ⇒ worked.
        assert _classify_outcome(rows, 0)[0] == "worked"


# ── find_sessions_where_action_worked ───────────────────────────────────────


class TestFindSessionsWhereActionWorked:
    def test_returns_worked_session_only(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="ok", turns=[
            ("assistant", "", _edit_blob()),
            ("user", "thanks, that worked!"),
        ])
        _seed_outcome_session(conn, session_id="broke", turns=[
            ("assistant", "", _edit_blob()),
            ("user", "no, that broke it"),
        ], last_ts="2026-04-03T00:00:00+00:00")
        conn.commit()

        out = find_sessions_where_action_worked(conn, action="cost.py")
        assert [m.session_id for m in out] == ["ok"]
        m = out[0]
        assert isinstance(m, OutcomeMatch)
        assert m.outcome == "worked"
        assert m.outcome_msg_id > 0
        assert m.outcome_evidence
        json.dumps(m.to_dict())  # serialisable, includes the outcome keys
        assert set(m.to_dict()) >= {"outcome", "outcome_evidence", "outcome_msg_id"}

    def test_matches_action_in_message_text(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="s", turns=[
            ("user", "please add caching to the cost route"),
            ("assistant", "Sure — done.", _edit_blob("/x/cost.py")),
            ("user", "perfect, ship it"),
        ])
        conn.commit()
        out = find_sessions_where_action_worked(conn, action="add caching")
        assert [m.session_id for m in out] == ["s"]
        assert out[0].outcome == "worked"

    def test_silence_session_counts_as_worked(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="quiet", turns=[
            ("assistant", "", _edit_blob()),
            ("assistant", "All set."),
        ])
        conn.commit()
        out = find_sessions_where_action_worked(conn, action="Edit")
        assert [m.session_id for m in out] == ["quiet"]
        assert "no user complaint" in out[0].outcome_evidence

    def test_uncertain_session_excluded(self, tmp_path):
        conn = _make_conn(tmp_path)
        # anchor is the last message ⇒ uncertain ⇒ not "worked"
        _seed_outcome_session(conn, session_id="s", turns=[
            ("assistant", "", _edit_blob()),
        ])
        conn.commit()
        assert find_sessions_where_action_worked(conn, action="cost.py") == []

    def test_file_path_narrowing(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="touches", turns=[
            ("user", "let's add caching"),
            ("assistant", "", _edit_blob("/Users/yad/dev/foo/cost.py")),
            ("user", "thanks, works great"),
        ])
        _seed_outcome_session(conn, session_id="elsewhere", turns=[
            ("user", "let's add caching"),
            ("assistant", "", _edit_blob("/Users/yad/dev/foo/other.py")),
            ("user", "thanks, works great"),
        ], last_ts="2026-04-04T00:00:00+00:00")
        conn.commit()

        out = find_sessions_where_action_worked(
            conn, action="add caching", file_path="/Users/yad/dev/foo/cost.py",
        )
        assert [m.session_id for m in out] == ["touches"]

    def test_project_filter(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="a", project_slug="-Users-a", turns=[
            ("assistant", "", _edit_blob()),
            ("user", "thanks"),
        ])
        _seed_outcome_session(conn, session_id="b", project_slug="-Users-b", turns=[
            ("assistant", "", _edit_blob()),
            ("user", "thanks"),
        ])
        conn.commit()
        out = find_sessions_where_action_worked(
            conn, action="cost.py", project="-Users-a",
        )
        assert [m.session_id for m in out] == ["a"]

    def test_since_filter(self, tmp_path):
        conn = _make_conn(tmp_path)
        recent = (datetime.now(UTC) - timedelta(hours=1)).isoformat()
        _seed_outcome_session(conn, session_id="old", turns=[
            ("assistant", "", _edit_blob()),
            ("user", "thanks"),
        ], last_ts="2024-01-01T00:00:00+00:00",
           timestamp="2024-01-01T00:00:00+00:00")
        _seed_outcome_session(conn, session_id="new", turns=[
            ("assistant", "", _edit_blob()),
            ("user", "thanks"),
        ], last_ts=recent, timestamp=recent)
        conn.commit()
        out = find_sessions_where_action_worked(conn, action="cost.py", since="1d")
        assert [m.session_id for m in out] == ["new"]

    def test_limit(self, tmp_path):
        conn = _make_conn(tmp_path)
        for i in range(3):
            _seed_outcome_session(conn, session_id=f"s{i}", turns=[
                ("assistant", "", _edit_blob()),
                ("user", "thanks"),
            ], last_ts=f"2026-04-0{i + 1}T00:00:00+00:00")
        conn.commit()
        out = find_sessions_where_action_worked(conn, action="cost.py", limit=2)
        assert len(out) == 2
        # newest first
        assert [m.session_id for m in out] == ["s2", "s1"]

    def test_multi_action_attributes_outcome_to_the_right_anchor(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="s", turns=[
            ("assistant", "", _edit_blob("/x/foo.py")),
            ("user", "great, foo looks good"),
            ("assistant", "", _edit_blob("/x/bar.py")),
            ("user", "no, bar broke the tests"),
        ])
        conn.commit()
        # foo edit ⇒ followed by approval ⇒ worked
        assert [m.session_id for m in
                find_sessions_where_action_worked(conn, action="foo.py")] == ["s"]
        # bar edit ⇒ followed by complaint ⇒ not worked
        assert find_sessions_where_action_worked(conn, action="bar.py") == []

    def test_empty_action_and_empty_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        assert find_sessions_where_action_worked(conn, action="") == []
        assert find_sessions_where_action_worked(conn, action="   ") == []
        assert find_sessions_where_action_worked(conn, action="anything") == []

    def test_bad_since_raises(self, tmp_path):
        conn = _make_conn(tmp_path)
        with pytest.raises(ValueError):
            find_sessions_where_action_worked(conn, action="x", since="whenever")


# ── find_failure_modes_for_file ─────────────────────────────────────────────


class TestFindFailureModesForFile:
    def test_returns_failed_session(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="broke", turns=[
            ("assistant", "", _edit_blob("/Users/yad/dev/foo/cost.py")),
            ("user", "no, that doesn't work — still failing"),
        ])
        conn.commit()
        out = find_failure_modes_for_file(conn, "/Users/yad/dev/foo/cost.py")
        assert [m.session_id for m in out] == ["broke"]
        assert out[0].outcome == "failed"
        assert isinstance(out[0], OutcomeMatch)
        assert "user wrote" in out[0].outcome_evidence

    def test_returns_reverted_session(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="undone", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("assistant", "", _bash_blob("git checkout -- x/cost.py")),
        ])
        conn.commit()
        out = find_failure_modes_for_file(conn, "/x/cost.py")
        assert [m.session_id for m in out] == ["undone"]
        assert out[0].outcome == "reverted"

    def test_worked_session_not_a_failure_mode(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="fine", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "perfect, thanks"),
        ])
        conn.commit()
        assert find_failure_modes_for_file(conn, "/x/cost.py") == []

    def test_only_write_mode_mentions_anchor(self, tmp_path):
        conn = _make_conn(tmp_path)
        # The file is *read* (not edited) — should not be a candidate even
        # though the path is in tools_json.
        read_blob = json.dumps([{"name": "Read", "input": {"file_path": "/x/cost.py"}}])
        _seed_outcome_session(conn, session_id="readonly", turns=[
            ("assistant", "", read_blob),
            ("user", "no, that's wrong"),
        ])
        conn.commit()
        assert find_failure_modes_for_file(conn, "/x/cost.py") == []

    def test_uncertain_session_excluded(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_outcome_session(conn, session_id="s", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
        ])
        conn.commit()
        assert find_failure_modes_for_file(conn, "/x/cost.py") == []

    def test_anchors_on_last_edit(self, tmp_path):
        conn = _make_conn(tmp_path)
        # Edited, complained-about, re-edited, then approved ⇒ NOT a
        # failure mode any more (the last edit's outcome is "worked").
        _seed_outcome_session(conn, session_id="fixed", turns=[
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "no, that broke it"),
            ("assistant", "", _edit_blob("/x/cost.py")),
            ("user", "thanks, works now"),
        ])
        conn.commit()
        assert find_failure_modes_for_file(conn, "/x/cost.py") == []

    def test_limit_and_since(self, tmp_path):
        conn = _make_conn(tmp_path)
        for i in range(3):
            _seed_outcome_session(conn, session_id=f"f{i}", turns=[
                ("assistant", "", _edit_blob("/x/cost.py")),
                ("user", "no, that broke it"),
            ], last_ts=f"2026-04-0{i + 1}T00:00:00+00:00")
        conn.commit()
        out = find_failure_modes_for_file(conn, "/x/cost.py", limit=2)
        assert len(out) == 2
        assert [m.session_id for m in out] == ["f2", "f1"]
        with pytest.raises(ValueError):
            find_failure_modes_for_file(conn, "/x/cost.py", since="soon")

    def test_empty_store(self, tmp_path):
        conn = _make_conn(tmp_path)
        assert find_failure_modes_for_file(conn, "/x/cost.py") == []




def _telemetry_rows(conn):
    return {
        (r["command"], r["session_id"]): r["loaded_count"]
        for r in conn.execute(
            "SELECT command, session_id, loaded_count FROM discovery_telemetry"
        ).fetchall()
    }


class TestDiscoveryRecordsTelemetry:
    def test_find_sessions_in_path_records_loaded(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        _seed_session(conn, project_id=pid, session_id="s-2",
                      first_ts="2026-04-03T00:00:00+00:00",
                      last_ts="2026-04-04T00:00:00+00:00")
        conn.commit()

        out = find_sessions_in_path(conn, "/Users/yad/dev/foo")
        returned = {m.session_id for m in out}
        rows = _telemetry_rows(conn)
        assert all(cmd == "find_sessions_in_path" for cmd, _ in rows)
        assert {sid for _, sid in rows} == returned
        assert all(v == 1 for v in rows.values())

        # Calling again bumps, doesn't duplicate.
        find_sessions_in_path(conn, "/Users/yad/dev/foo")
        rows2 = _telemetry_rows(conn)
        assert set(rows2) == set(rows)
        assert all(v == 2 for v in rows2.values())

    def test_records_only_the_limited_subset(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        for i in range(3):
            _seed_session(conn, project_id=pid, session_id=f"s-{i}",
                          first_ts=f"2026-04-0{i+1}T00:00:00+00:00",
                          last_ts=f"2026-04-0{i+1}T00:00:00+00:00")
        conn.commit()

        out = find_sessions_in_path(conn, "/Users/yad/dev/foo", limit=1)
        assert len(out) == 1
        rows = _telemetry_rows(conn)
        assert len(rows) == 1
        assert (("find_sessions_in_path", out[0].session_id)) in rows

    def test_find_sessions_touching_file_records_loaded(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s-touch",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        _insert_message(
            conn, session_fk=sfk, seq=0,
            timestamp="2026-04-01T00:00:00+00:00",
            tools_json=json.dumps([
                {"name": "Read", "input": {"file_path": "/Users/yad/dev/foo/x.py"}}
            ]),
        )
        conn.commit()

        out = find_sessions_touching_file(conn, "/Users/yad/dev/foo/x.py")
        assert {m.session_id for m in out} == {"s-touch"}
        rows = _telemetry_rows(conn)
        assert rows == {("find_sessions_touching_file", "s-touch"): 1}

    def test_search_past_decisions_records_loaded(self, tmp_path):
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        sfk = _seed_session(conn, project_id=pid, session_id="s-dec",
                            first_ts="2026-04-01T00:00:00+00:00",
                            last_ts="2026-04-02T00:00:00+00:00")
        _insert_message(conn, session_fk=sfk, seq=0,
                        timestamp="2026-04-01T00:00:00+00:00",
                        content_text="we decided to use sqlite here")
        conn.commit()

        out = search_past_decisions(conn, "use sqlite")
        assert {m.session_id for m in out} == {"s-dec"}
        rows = _telemetry_rows(conn)
        assert rows == {("search_past_decisions", "s-dec"): 1}

    def test_no_matches_records_nothing(self, tmp_path):
        conn = _make_conn(tmp_path)
        _seed_project(conn, slug="-Users-yad-dev-foo")
        conn.commit()
        find_sessions_in_path(conn, "/somewhere/else")
        assert _telemetry_rows(conn) == {}

    def test_env_gate_disables_recording(self, tmp_path, monkeypatch):
        monkeypatch.setenv("STACKUNDERFLOW_DISCOVERY_TELEMETRY", "0")
        conn = _make_conn(tmp_path)
        pid = _seed_project(conn, slug="-Users-yad-dev-foo")
        _seed_session(conn, project_id=pid, session_id="s-1",
                      first_ts="2026-04-01T00:00:00+00:00",
                      last_ts="2026-04-02T00:00:00+00:00")
        conn.commit()
        out = find_sessions_in_path(conn, "/Users/yad/dev/foo")
        assert out  # query still works
        assert _telemetry_rows(conn) == {}
