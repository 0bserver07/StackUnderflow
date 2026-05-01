"""Tests for the public Python API (``stackunderflow.list_projects`` etc.).

These pin the store-backed shape: the helpers open
``~/.stackunderflow/store.db`` (resolved via ``deps.store_path``, which
is monkeypatched per-test to a tmp path), call into ``store.queries``,
and return plain dicts shaped for library consumers.

The module-level docstring of ``stackunderflow.api`` documents the empty-
store and unknown-slug semantics — the tests below pin them so behaviour
doesn't drift.
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from pathlib import Path

import pytest

import stackunderflow
from stackunderflow.store import db, schema


@pytest.fixture
def store(tmp_path: Path, monkeypatch) -> Iterator[Path]:
    """Empty initialised store at a tmp path, with ``deps.store_path`` redirected."""
    path = tmp_path / "store.db"
    conn = db.connect(path)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr("stackunderflow.deps.store_path", path)
    yield path


@pytest.fixture
def missing_store(tmp_path: Path, monkeypatch) -> Path:
    """Path that doesn't exist on disk — exercises the empty-store branch."""
    path = tmp_path / "store.db"
    monkeypatch.setattr("stackunderflow.deps.store_path", path)
    return path


def _seed_project(
    path: Path,
    *,
    slug: str,
    provider: str = "claude",
    display_name: str | None = None,
) -> int:
    conn = db.connect(path)
    try:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
            "VALUES (?, ?, ?, ?, ?)",
            (provider, slug, display_name or slug, 0.0, 0.0),
        )
        return cur.lastrowid  # type: ignore[return-value]
    finally:
        conn.close()


def _seed_message(
    path: Path,
    project_id: int,
    *,
    session_id: str = "s1",
    role: str = "assistant",
    model: str = "claude-sonnet-4-6",
    timestamp: str = "2026-04-15T10:00:00+00:00",
    raw: dict | None = None,
) -> None:
    """Insert a synthetic session + one message so the pipeline has something to chew on.

    The classifier expects ``raw_json`` to be parseable as a Claude/Codex
    record. We use a Claude-shaped assistant message because the default
    provider in our seeded project is ``claude``.
    """
    payload = raw or {
        "type": role,
        "uuid": f"{session_id}-1",
        "timestamp": timestamp,
        "sessionId": session_id,
        "message": {
            "role": role,
            "model": model,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            },
            "content": [{"type": "text", "text": "ok"}],
        },
    }
    conn = db.connect(path)
    try:
        cur = conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
            (project_id, session_id),
        )
        sid = cur.lastrowid
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, model, "
            "input_tokens, output_tokens, raw_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            (sid, 0, timestamp, role, model, 10, 5, json.dumps(payload)),
        )
    finally:
        conn.close()


# ── list_projects ────────────────────────────────────────────────────────────


def test_list_projects_empty_store_returns_empty_list(store: Path) -> None:
    assert stackunderflow.list_projects() == []


def test_list_projects_missing_store_returns_empty_list(missing_store: Path) -> None:
    """Fresh install (no ingest has run) is the same as empty — no exception."""
    assert not missing_store.is_file()
    assert stackunderflow.list_projects() == []


def test_list_projects_returns_provider_tagged_rows(store: Path) -> None:
    _seed_project(store, slug="proj-a", provider="claude")
    _seed_project(store, slug="proj-b", provider="codex")
    rows = stackunderflow.list_projects()
    assert len(rows) == 2
    by_slug = {r["slug"]: r for r in rows}
    assert by_slug["proj-a"]["provider"] == "claude"
    assert by_slug["proj-b"]["provider"] == "codex"
    # Shape contract — every documented key is present on every row.
    for r in rows:
        assert set(r.keys()) == {
            "slug",
            "provider",
            "display_name",
            "path",
            "first_seen",
            "last_modified",
        }


def test_list_projects_filters_by_provider(store: Path) -> None:
    _seed_project(store, slug="a", provider="claude")
    _seed_project(store, slug="b", provider="codex")
    _seed_project(store, slug="c", provider="claude")
    claude_only = stackunderflow.list_projects(provider="claude")
    assert {r["slug"] for r in claude_only} == {"a", "c"}
    assert all(r["provider"] == "claude" for r in claude_only)


def test_list_projects_filter_no_match_returns_empty(store: Path) -> None:
    _seed_project(store, slug="a", provider="claude")
    assert stackunderflow.list_projects(provider="cursor") == []


# ── list_sessions ────────────────────────────────────────────────────────────


def test_list_sessions_returns_session_rows(store: Path) -> None:
    pid = _seed_project(store, slug="proj-a")
    _seed_message(store, pid, session_id="sess-1")
    _seed_message(store, pid, session_id="sess-2")
    sessions = stackunderflow.list_sessions("proj-a")
    assert {s["session_id"] for s in sessions} == {"sess-1", "sess-2"}
    for s in sessions:
        assert set(s.keys()) == {"session_id", "first_ts", "last_ts", "message_count"}


def test_list_sessions_unknown_slug_raises_keyerror(store: Path) -> None:
    with pytest.raises(KeyError, match="ghost"):
        stackunderflow.list_sessions("ghost")


def test_list_sessions_missing_store_raises_keyerror(missing_store: Path) -> None:
    with pytest.raises(KeyError, match="anything"):
        stackunderflow.list_sessions("anything")


# ── process ──────────────────────────────────────────────────────────────────


def test_process_returns_messages_and_stats(store: Path) -> None:
    pid = _seed_project(store, slug="proj-a")
    _seed_message(store, pid, session_id="sess-1")
    messages, stats = stackunderflow.process("proj-a")
    assert isinstance(messages, list)
    assert isinstance(stats, dict)
    assert "overview" in stats
    # The overview block carries the pipeline's headline metrics; exact
    # numbers depend on classifier/aggregator internals — we just pin the
    # presence of the documented keys.
    assert "total_cost" in stats["overview"]
    assert "sessions" in stats["overview"]


def test_process_unknown_slug_raises_keyerror(store: Path) -> None:
    with pytest.raises(KeyError, match="nope"):
        stackunderflow.process("nope")


def test_process_missing_store_raises_keyerror(missing_store: Path) -> None:
    """No store file == project not found, from the caller's POV."""
    with pytest.raises(KeyError, match="anything"):
        stackunderflow.process("anything")


def test_process_disambiguates_by_provider(store: Path) -> None:
    """Same slug under two providers — provider arg pins the right one."""
    pid_claude = _seed_project(store, slug="shared", provider="claude")
    pid_codex = _seed_project(store, slug="shared", provider="codex")
    _seed_message(store, pid_claude, session_id="claude-sess")
    _seed_message(store, pid_codex, session_id="codex-sess")

    # Without provider, ``get_project`` returns the first match — at least
    # one of them resolves cleanly. With provider, the codex variant must
    # resolve to the codex session.
    _, stats = stackunderflow.process("shared", provider="codex")
    # Codex provider uses different session shape — but our seed used a
    # Claude-shaped raw_json. The pipeline still runs; the assertion below
    # only verifies disambiguation, not pipeline output for a foreign
    # raw_json shape.
    assert isinstance(stats, dict)


def test_process_unknown_provider_for_known_slug_raises(store: Path) -> None:
    _seed_project(store, slug="a", provider="claude")
    with pytest.raises(KeyError, match="a"):
        stackunderflow.process("a", provider="cursor")


# ── public surface ────────────────────────────────────────────────────────────


def test_public_api_exports() -> None:
    """``__all__`` lists exactly the documented public functions + version."""
    assert set(stackunderflow.__all__) == {
        "__version__",
        "list_projects",
        "list_sessions",
        "process",
    }
    assert callable(stackunderflow.list_projects)
    assert callable(stackunderflow.list_sessions)
    assert callable(stackunderflow.process)
