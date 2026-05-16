"""Latency regression: ``compute_yield`` on a real-store-shape store.

Marked ``@pytest.mark.slow`` so the default ``pytest -q`` run skips it.
Run explicitly with::

    pytest tests/stackunderflow/services/test_yield_perf.py -m slow

The synthetic shape mirrors the production bug (#fix/yield-timeout) where
``/api/yield`` timed out at 15 s on a project with ~95 sessions and ~250 K
messages. The pre-fix per-session ``git`` fan-out was the root cause; this
test pins the worst case to a generous ceiling so a future regression
that re-introduces the per-session subprocess pattern fails CI.

What the test stresses:

* a project with ~150 sessions (above the user's 95-session report)
* each session has 20+ messages stamped with the same cwd
* git is monkeypatched so the test never actually shells out — the cost
  signal is purely "how many fake git invocations did the service issue
  per session?", which is the metric that broke /api/yield in the wild
"""

from __future__ import annotations

import json
import sqlite3
import time
from collections.abc import Sequence
from dataclasses import dataclass

import pytest

from stackunderflow.services import yield_tracker
from stackunderflow.services.yield_tracker import compute_yield
from stackunderflow.store import db, schema


@dataclass
class _FakeRun:
    returncode: int = 0
    stdout: str = ""
    stderr: str = ""


class _GitCounter:
    """Records every ``subprocess.run`` invocation by git subcommand.

    Returns a canned successful response so the service can complete its
    classification pass without actually shelling out to git. The count
    table is the assertion target — see the test below.
    """

    def __init__(self) -> None:
        self.calls_by_sub: dict[str, int] = {}
        self.calls: list[Sequence[str]] = []

    def __call__(self, args, **_kwargs):
        self.calls.append(tuple(args))
        sub = args[3] if len(args) > 3 else ""
        self.calls_by_sub[sub] = self.calls_by_sub.get(sub, 0) + 1
        # Default canned responses sufficient for the service to finish.
        if sub == "rev-parse":
            return _FakeRun(returncode=0, stdout=".git\n")
        if sub == "log":
            joined = " ".join(args)
            if "--grep=" in joined:
                # No revert subjects.
                return _FakeRun(returncode=0, stdout="")
            # ``--max-count=...`` → return one commit so half the sessions
            # land in ``productive`` (rest are ``abandoned`` per per-session
            # window). Use a fixed sha + UTC timestamp inside every session
            # window — the service's classifier will pick whichever match.
            return _FakeRun(
                returncode=0,
                stdout=(
                    "abc1234abc1234abc1234abc1234abc1234abc12"
                    "|2026-04-01T12:00:00+00:00"
                    "|feat: synthetic\n"
                ),
            )
        if sub == "rev-list":
            # Reachability set — include the candidate sha so it stays
            # ``productive`` instead of flipping to ``reverted``.
            return _FakeRun(returncode=0, stdout="abc1234abc1234abc1234abc1234abc1234abc12\n")
        return _FakeRun()


def _seed_store(
    conn: sqlite3.Connection, *, project_slug: str, n_sessions: int, cwd: str,
) -> None:
    """Insert one project with ``n_sessions`` sessions, each on the same cwd."""
    conn.execute(
        "INSERT OR IGNORE INTO projects (provider, slug, display_name, "
        "first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
        ("claude", project_slug, project_slug, 0.0, 0.0),
    )
    project_id = conn.execute(
        "SELECT id FROM projects WHERE slug = ?", (project_slug,)
    ).fetchone()["id"]

    raw = json.dumps({"cwd": cwd, "type": "user"})
    raw_assist = json.dumps({"cwd": cwd, "type": "assistant"})

    for i in range(n_sessions):
        ts = f"2026-04-{(i % 28) + 1:02d}T10:00:{i % 60:02d}+00:00"
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, "
            "message_count) VALUES (?, ?, ?, ?, ?)",
            (project_id, f"sess-{i}", ts, ts, 2),
        )
        session_fk = conn.execute(
            "SELECT id FROM sessions WHERE session_id = ?", (f"sess-{i}",)
        ).fetchone()["id"]
        # First message stamps cwd; second is just there to flesh out
        # the session and exercise the bulk first-cwd batch lookup.
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            "input_tokens, output_tokens, raw_json) "
            "VALUES (?, 0, ?, 'user', 'claude-sonnet-4-20250514', 0, 0, ?)",
            (session_fk, ts, raw),
        )
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            "input_tokens, output_tokens, raw_json) "
            "VALUES (?, 1, ?, 'assistant', 'claude-sonnet-4-20250514', 100, 50, ?)",
            (session_fk, ts, raw_assist),
        )
    conn.commit()


@pytest.mark.slow
def test_compute_yield_scales_subquadratically_with_session_count(
    tmp_path, monkeypatch,
):
    """Latency-regression sentinel for ``services.yield_tracker``.

    The fix-yield-timeout pipeline is supposed to do **one** git
    subprocess fan-out per *distinct cwd*, not per *session*. Concretely
    that means for one project with N sessions on the same cwd the
    service should issue exactly:

        rev-parse        : 1
        log (windowed)   : 1
        rev-list HEAD    : 1
        log --grep=revert: 1

    A regression that re-introduces the per-session pattern (``git log
    ... --since={start}`` per session, ``git show`` per kept commit,
    ``git merge-base --is-ancestor`` per kept commit) blows past those
    counts proportional to N and times the route out on a real store.

    We also pin a generous wall-clock ceiling so any future SQL
    regression in the bulk cwd lookup is caught — the realistic 150-
    session count completes in well under 2 s on the maintainer's
    M1/M2 hardware (and far under that with subprocess stubbed).
    """
    cwd = str(tmp_path / "synthetic-repo")
    (tmp_path / "synthetic-repo").mkdir()

    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    _seed_store(conn, project_slug="-perf", n_sessions=150, cwd=cwd)

    counter = _GitCounter()
    monkeypatch.setattr(yield_tracker.subprocess, "run", counter)

    t0 = time.perf_counter()
    entries = compute_yield(conn, period="all", project_filter=["-perf"])
    elapsed = time.perf_counter() - t0

    assert len(entries) == 150
    # Wall-clock ceiling — generous so it doesn't flake on a slow CI
    # runner. The pre-fix code took >40s on the maintainer's local
    # store; this assertion would catch any regression that puts it
    # back into multi-second territory on synthetic data.
    assert elapsed < 2.0, f"compute_yield took {elapsed:.2f}s — perf regression"

    # Per-distinct-cwd assertions — these are the contract that prevents
    # a regression from sliding back into per-session subprocess fan-out.
    # One project, one cwd → exactly one of each git op.
    assert counter.calls_by_sub.get("rev-parse", 0) == 1
    # One windowed log + one revert-grep log = 2 ``log`` calls.
    assert counter.calls_by_sub.get("log", 0) == 2
    assert counter.calls_by_sub.get("rev-list", 0) == 1
    # ``show`` and ``merge-base`` were the per-session ops in v1; both
    # must stay at zero in the bulk pipeline.
    assert counter.calls_by_sub.get("show", 0) == 0
    assert counter.calls_by_sub.get("merge-base", 0) == 0


@pytest.mark.slow
def test_compute_yield_caps_per_project_session_count(
    tmp_path, monkeypatch,
):
    """The default per-project cap (200) trims a pathological project's tail.

    Even with the bulk pipeline, a single project with thousands of
    sessions in one period would still pay N round-trips on the SQL side
    and N classifications client-side. The cap is the safety net — most-
    recent ``cap`` sessions are kept, older ones are dropped before any
    git work runs. Default is 200; this test pins it via env to 50 to
    keep the synthetic dataset small.
    """
    cwd = str(tmp_path / "syn")
    (tmp_path / "syn").mkdir()

    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    _seed_store(conn, project_slug="-cap", n_sessions=300, cwd=cwd)

    monkeypatch.setattr(yield_tracker.subprocess, "run", _GitCounter())
    monkeypatch.setenv(
        "STACKUNDERFLOW_YIELD_MAX_SESSIONS_PER_PROJECT", "50",
    )

    entries = compute_yield(conn, period="all", project_filter=["-cap"])
    # Cap kicks in at 50, so we get exactly 50 entries back even though
    # the store has 300 sessions in scope.
    assert len(entries) == 50

    # ``unlimited`` disables the cap entirely.
    monkeypatch.setenv(
        "STACKUNDERFLOW_YIELD_MAX_SESSIONS_PER_PROJECT", "unlimited",
    )
    entries = compute_yield(conn, period="all", project_filter=["-cap"])
    assert len(entries) == 300
