"""Unit tests for ``stackunderflow.services.discovery``.

Covers:

* Path-based ancestor matching (``find_sessions_in_path``).
* Tool-args + free-form mention search (``find_sessions_touching_file``).
* Substring search with snippet (``search_past_decisions``).
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
    SessionMatch,
    decode_slug_to_path,
    find_sessions_in_path,
    find_sessions_touching_file,
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
