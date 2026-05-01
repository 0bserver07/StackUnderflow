"""Tests for ``services.yield_tracker``.

Strategy: seed an in-memory store with a project + session + a couple of
messages, then monkeypatch ``subprocess.run`` so the git calls are pure
fixtures. Real ``git log`` against a real repo is not run here — that
would make the suite flaky against whatever happens to be in the user's
filesystem.
"""

from __future__ import annotations

import json
import sqlite3
from collections.abc import Sequence
from dataclasses import dataclass

import pytest

from stackunderflow.services import yield_tracker
from stackunderflow.services.yield_tracker import (
    YieldEntry,
    compute_yield,
    yield_summary,
)
from stackunderflow.store import db, schema

# ── seed helpers ────────────────────────────────────────────────────────────


def _open(tmp_path) -> sqlite3.Connection:
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    return conn


def _insert_session(
    conn: sqlite3.Connection,
    *,
    slug: str,
    session_id: str,
    started_at: str,
    cwd: str | None,
    model: str = "claude-sonnet-4-20250514",
    input_tokens: int = 1000,
    output_tokens: int = 500,
) -> None:
    """Insert a project + session + two messages.

    The first message carries ``cwd`` (matches the real claude.py shape:
    ``cwd`` lives in the top-level JSON, not in ``message``).
    """
    conn.execute(
        "INSERT OR IGNORE INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, ?, ?)",
        ("claude", slug, slug, 0.0, 0.0),
    )
    project_id = conn.execute(
        "SELECT id FROM projects WHERE slug = ?", (slug,)
    ).fetchone()["id"]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, ?)",
        (project_id, session_id, started_at, started_at, 2),
    )
    session_fk = conn.execute(
        "SELECT id FROM sessions WHERE session_id = ?", (session_id,)
    ).fetchone()["id"]

    raw_user = json.dumps({"cwd": cwd or "", "type": "user"})
    raw_assistant = json.dumps({"cwd": cwd or "", "type": "assistant"})

    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, raw_json) "
        "VALUES (?, 0, ?, 'user', ?, 0, 0, ?)",
        (session_fk, started_at, model, raw_user),
    )
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
        "input_tokens, output_tokens, raw_json) "
        "VALUES (?, 1, ?, 'assistant', ?, ?, ?, ?)",
        (session_fk, started_at, model, input_tokens, output_tokens, raw_assistant),
    )
    conn.commit()


# ── subprocess stubs ────────────────────────────────────────────────────────


@dataclass
class _FakeRun:
    """Mimics the subset of ``CompletedProcess`` we touch."""

    returncode: int = 0
    stdout: str = ""
    stderr: str = ""


class _GitDouble:
    """Recorder + dispatcher for ``subprocess.run`` calls in yield_tracker.

    Maps the second-positional arg (the git subcommand) to a canned
    ``_FakeRun`` response. Falls back to a generic success / empty stdout
    when no rule matches so unit tests can stay focused.
    """

    def __init__(self, behaviours: dict[str, _FakeRun] | None = None):
        self.calls: list[Sequence[str]] = []
        self.behaviours: dict[str, _FakeRun] = behaviours or {}

    def __call__(self, args, **_kwargs):
        self.calls.append(tuple(args))
        # args = ["git", "-C", cwd, <subcommand>, ...]
        sub = args[3] if len(args) > 3 else ""
        return self.behaviours.get(sub, _FakeRun())


# ── service-level cases ─────────────────────────────────────────────────────


def test_no_repo_when_cwd_missing(tmp_path, monkeypatch):
    """Sessions with empty ``cwd`` short-circuit to ``no_repo``."""
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-a",
        session_id="sess-empty-cwd",
        started_at="2026-04-01T10:00:00+00:00",
        cwd="",
    )
    # Ensure no git is even attempted.
    git = _GitDouble()
    monkeypatch.setattr(yield_tracker.subprocess, "run", git)

    entries = compute_yield(conn, period="all")
    assert len(entries) == 1
    assert entries[0].classification == "no_repo"
    assert entries[0].cwd == ""
    assert git.calls == []


def test_no_repo_when_path_does_not_exist(tmp_path, monkeypatch):
    """A non-existent ``cwd`` skips the ``git rev-parse`` and stays ``no_repo``."""
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-b",
        session_id="sess-nope",
        started_at="2026-04-02T10:00:00+00:00",
        cwd="/path/does/not/exist/anywhere",
    )
    git = _GitDouble()
    monkeypatch.setattr(yield_tracker.subprocess, "run", git)

    entries = compute_yield(conn, period="all")
    assert entries[0].classification == "no_repo"
    # The path-existence pre-check filters this before subprocess runs.
    assert git.calls == []


def test_no_repo_when_rev_parse_fails(tmp_path, monkeypatch):
    """A real directory that isn't a git repo gets ``no_repo``."""
    cwd = tmp_path / "notarepo"
    cwd.mkdir()
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-c",
        session_id="sess-not-a-repo",
        started_at="2026-04-03T10:00:00+00:00",
        cwd=str(cwd),
    )
    git = _GitDouble({"rev-parse": _FakeRun(returncode=128, stderr="fatal: not a git repo")})
    monkeypatch.setattr(yield_tracker.subprocess, "run", git)

    entries = compute_yield(conn, period="all")
    assert entries[0].classification == "no_repo"


def test_productive_when_unreverted_commit_lands(tmp_path, monkeypatch):
    """A commit lands within 24h, no revert, reachable from HEAD → productive."""
    cwd = tmp_path / "repo"
    cwd.mkdir()
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-d",
        session_id="sess-prod",
        started_at="2026-04-04T10:00:00+00:00",
        cwd=str(cwd),
    )
    full_sha = "abcdef1234567890abcdef1234567890abcdef12"

    def _run(args, **_kwargs):
        sub = args[3] if len(args) > 3 else ""
        if sub == "rev-parse":
            return _FakeRun(returncode=0, stdout=".git\n")
        if sub == "log":
            joined = " ".join(args)
            if "--grep=" in joined:
                # No revert subjects mention this commit.
                return _FakeRun(returncode=0, stdout="")
            return _FakeRun(
                returncode=0,
                stdout=f"{full_sha}|feat: add yield tracker\n",
            )
        if sub == "show":
            return _FakeRun(returncode=0, stdout="2026-04-04T11:30:00+00:00\n")
        if sub == "merge-base":
            return _FakeRun(returncode=0)  # reachable from HEAD = kept
        return _FakeRun()

    monkeypatch.setattr(yield_tracker.subprocess, "run", _run)

    entries = compute_yield(conn, period="all")
    assert len(entries) == 1
    e = entries[0]
    assert e.classification == "productive"
    assert e.follow_commit_sha == full_sha
    assert e.follow_commit_msg == "feat: add yield tracker"
    assert e.follow_commit_age_hours == pytest.approx(1.5)


def test_abandoned_when_no_commit_lands(tmp_path, monkeypatch):
    """``git log`` returns empty → abandoned."""
    cwd = tmp_path / "repo"
    cwd.mkdir()
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-e",
        session_id="sess-abandoned",
        started_at="2026-04-05T10:00:00+00:00",
        cwd=str(cwd),
    )
    git = _GitDouble({
        "rev-parse": _FakeRun(returncode=0, stdout=".git\n"),
        "log": _FakeRun(returncode=0, stdout=""),  # no commits in window
    })
    monkeypatch.setattr(yield_tracker.subprocess, "run", git)

    entries = compute_yield(conn, period="all")
    assert entries[0].classification == "abandoned"
    assert entries[0].follow_commit_sha is None


def test_reverted_when_revert_log_exists(tmp_path, monkeypatch):
    """A subject containing ``revert <shortsha>`` flips the verdict to reverted."""
    cwd = tmp_path / "repo"
    cwd.mkdir()
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-f",
        session_id="sess-reverted",
        started_at="2026-04-06T10:00:00+00:00",
        cwd=str(cwd),
    )
    full_sha = "abcdef1234567890abcdef1234567890abcdef12"
    short = full_sha[:7]
    revert_log_call = {"matched": False}

    def _run(args, **_kwargs):
        sub = args[3] if len(args) > 3 else ""
        if sub == "rev-parse":
            return _FakeRun(returncode=0)
        if sub == "log":
            # First "log" call returns the candidate commit; subsequent
            # "log --grep" call returns a hit so the commit looks reverted.
            joined = " ".join(args)
            if "--grep=" in joined:
                revert_log_call["matched"] = True
                return _FakeRun(returncode=0, stdout=f'Revert "feat: add thing" ({short})\n')
            return _FakeRun(returncode=0, stdout=f"{full_sha}|feat: add thing\n")
        if sub == "show":
            return _FakeRun(returncode=0, stdout="2026-04-06T11:00:00+00:00\n")
        if sub == "merge-base":
            return _FakeRun(returncode=0)  # reachable but reverted by subject
        return _FakeRun()

    monkeypatch.setattr(yield_tracker.subprocess, "run", _run)

    entries = compute_yield(conn, period="all")
    assert entries[0].classification == "reverted"
    assert entries[0].follow_commit_sha == full_sha
    assert revert_log_call["matched"]


def test_reverted_when_commit_unreachable(tmp_path, monkeypatch):
    """No revert in subjects, but ``--is-ancestor`` returns 1 → reverted."""
    cwd = tmp_path / "repo"
    cwd.mkdir()
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-g",
        session_id="sess-wiped",
        started_at="2026-04-07T10:00:00+00:00",
        cwd=str(cwd),
    )
    full_sha = "f00b" + "0" * 36

    def _run(args, **_kwargs):
        sub = args[3] if len(args) > 3 else ""
        if sub == "rev-parse":
            return _FakeRun(returncode=0)
        if sub == "log":
            joined = " ".join(args)
            if "--grep=" in joined:
                return _FakeRun(returncode=0, stdout="")  # no revert subject match
            return _FakeRun(returncode=0, stdout=f"{full_sha}|wip\n")
        if sub == "show":
            return _FakeRun(returncode=0, stdout="2026-04-07T11:00:00+00:00\n")
        if sub == "merge-base":
            return _FakeRun(returncode=1)  # not reachable from HEAD
        return _FakeRun()

    monkeypatch.setattr(yield_tracker.subprocess, "run", _run)

    entries = compute_yield(conn, period="all")
    assert entries[0].classification == "reverted"


def test_git_timeout_yields_no_repo(tmp_path, monkeypatch):
    """A hung git call (``TimeoutExpired``) is swallowed as ``no_repo``."""
    import subprocess as _sp

    cwd = tmp_path / "slowrepo"
    cwd.mkdir()
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-h",
        session_id="sess-timeout",
        started_at="2026-04-08T10:00:00+00:00",
        cwd=str(cwd),
    )

    def _hang(args, **_kwargs):
        raise _sp.TimeoutExpired(cmd=args, timeout=5)

    monkeypatch.setattr(yield_tracker.subprocess, "run", _hang)
    entries = compute_yield(conn, period="all")
    assert entries[0].classification == "no_repo"


def test_period_filters_to_window(tmp_path, monkeypatch):
    """Sessions outside ``period`` are excluded entirely."""
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-i",
        session_id="sess-old",
        started_at="2020-01-01T10:00:00+00:00",
        cwd="",
    )
    _insert_session(
        conn,
        slug="-i",
        session_id="sess-recent",
        started_at="2099-01-01T10:00:00+00:00",
        cwd="",
    )
    git = _GitDouble()
    monkeypatch.setattr(yield_tracker.subprocess, "run", git)

    entries = compute_yield(conn, period="month")
    # Neither window contains both stamps; just check the function ran
    # without raising and the period filter honoured the parsed scope.
    ids = {e.session_id for e in entries}
    assert "sess-old" not in ids


def test_project_filter_drops_other_slugs(tmp_path, monkeypatch):
    """Sessions whose ``project_slug`` isn't in ``project_filter`` are dropped."""
    conn = _open(tmp_path)
    _insert_session(
        conn,
        slug="-keep",
        session_id="sess-keep",
        started_at="2026-04-10T10:00:00+00:00",
        cwd="",
    )
    _insert_session(
        conn,
        slug="-drop",
        session_id="sess-drop",
        started_at="2026-04-10T11:00:00+00:00",
        cwd="",
    )
    git = _GitDouble()
    monkeypatch.setattr(yield_tracker.subprocess, "run", git)

    entries = compute_yield(conn, period="all", project_filter=["-keep"])
    ids = [e.session_id for e in entries]
    assert ids == ["sess-keep"]


def test_yield_summary_aggregates_counts_and_costs():
    entries = [
        YieldEntry("a", "p", "/x", "2026-04-11T00:00:00+00:00", 1.5, "productive"),
        YieldEntry("b", "p", "/x", "2026-04-11T01:00:00+00:00", 0.5, "productive"),
        YieldEntry("c", "p", "/x", "2026-04-11T02:00:00+00:00", 2.0, "reverted"),
        YieldEntry("d", "p", "/x", "2026-04-11T03:00:00+00:00", 3.0, "abandoned"),
        YieldEntry("e", "p", "",   "2026-04-11T04:00:00+00:00", 0.25, "no_repo"),
    ]
    summary = yield_summary(entries)
    assert summary["productive"] == 2
    assert summary["reverted"] == 1
    assert summary["abandoned"] == 1
    assert summary["no_repo"] == 1
    assert summary["total"] == 5
    assert summary["productive_cost"] == pytest.approx(2.0)
    assert summary["reverted_cost"] == pytest.approx(2.0)
    assert summary["abandoned_cost"] == pytest.approx(3.0)
    assert summary["no_repo_cost"] == pytest.approx(0.25)
    assert summary["total_cost"] == pytest.approx(7.25)


def test_yield_summary_empty_returns_zero_baseline():
    summary = yield_summary([])
    for k in ("productive", "reverted", "abandoned", "no_repo", "total"):
        assert summary[k] == 0
    for k in ("productive_cost", "reverted_cost", "abandoned_cost", "no_repo_cost", "total_cost"):
        assert summary[k] == 0.0


def test_week_alias_normalises_to_7days():
    """``period='week'`` is accepted as a friendly alias for ``7days``."""
    from stackunderflow.services.yield_tracker import _normalize_period

    assert _normalize_period("week") == "7days"
    # Unrecognised values pass through untouched so ``parse_period`` raises.
    assert _normalize_period("month") == "month"
    assert _normalize_period("today") == "today"
