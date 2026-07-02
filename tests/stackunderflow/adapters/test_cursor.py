"""Unit tests for the Cursor (vscdb) adapter.

Builds a synthetic ``state.vscdb`` SQLite fixture in ``tmp_path`` carrying
two ``bubbleId:%`` rows (one user, one assistant with explicit
``tokenCount``) and one ``agentKv:blob:%`` row, all sharing one
``conversationId``. Then exercises ``enumerate`` / ``read`` /
``read(since_offset=...)`` end-to-end. Inherits the shared
``AdapterContract`` mixin so the storage-aware resume invariant runs
against a database-backed adapter.
"""

from __future__ import annotations

import json
import sqlite3
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.cursor import CursorAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


CONV_ID = "conv-abc-123"
OTHER_CONV_ID = "conv-other-999"


def _build_fixture(path: Path) -> None:
    """Create a vscdb-shaped SQLite file with 3 rows for one conversation."""
    conn = sqlite3.connect(path)
    try:
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)"
        )
        rows = [
            (
                "bubbleId:b1",
                json.dumps(
                    {
                        "conversationId": CONV_ID,
                        "type": 1,  # user
                        "text": "Refactor this please.",
                        "modelInfo": {"modelName": "claude-sonnet-4-6"},
                        "tokenCount": {"inputTokens": 0, "outputTokens": 0},
                        "createdAt": 1714000000000,
                    }
                ),
            ),
            (
                "bubbleId:b2",
                json.dumps(
                    {
                        "conversationId": CONV_ID,
                        "type": 2,  # assistant
                        "text": "Here is a refactor.",
                        "modelInfo": {"modelName": "claude-sonnet-4-6"},
                        "tokenCount": {"inputTokens": 120, "outputTokens": 480},
                        "createdAt": 1714000010000,
                    }
                ),
            ),
            (
                "agentKv:blob:k1",
                json.dumps(
                    {
                        "conversationId": CONV_ID,
                        "role": "tool",
                        "content": [{"type": "text", "text": "ran tests"}],
                        "providerOptions": {
                            "cursor": {
                                "modelName": "cursor-auto",
                                "requestId": "req-xyz",
                            }
                        },
                        "createdAt": "2026-04-29T10:00:00Z",
                    }
                ),
            ),
            (
                # A row from a different conversation — must NOT appear in
                # the per-conversation read for CONV_ID.
                "bubbleId:b3",
                json.dumps(
                    {
                        "conversationId": OTHER_CONV_ID,
                        "type": 1,
                        "text": "Different convo.",
                        "modelInfo": {"modelName": "claude-sonnet-4-6"},
                        "tokenCount": {"inputTokens": 0, "outputTokens": 0},
                        "createdAt": 1714000020000,
                    }
                ),
            ),
        ]
        conn.executemany(
            "INSERT INTO cursorDiskKV(key, value) VALUES (?, ?)", rows
        )
        conn.commit()
    finally:
        conn.close()


@pytest.fixture(autouse=True)
def _isolate_cursor_cache(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Redirect the Cursor parse cache into ``tmp_path`` for every test.

    The adapter writes a fingerprint cache to ``~/.stackunderflow/cache/``
    on every successful read; tests must not pollute the developer's
    real cache directory.
    """
    from stackunderflow.infra import cursor_cache as _cc

    monkeypatch.setattr(
        _cc,
        "_default_cache_path",
        lambda: tmp_path / "cursor-results.json",
    )


@pytest.fixture()
def vscdb_path(tmp_path: Path) -> Path:
    fp = tmp_path / "state.vscdb"
    _build_fixture(fp)
    return fp


# ── targeted tests ────────────────────────────────────────────────────


def test_enumerate_yields_one_session_ref_per_conversation(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = list(adapter.enumerate())
    by_id = {r.session_id: r for r in refs}
    assert set(by_id.keys()) == {CONV_ID, OTHER_CONV_ID}
    ref = by_id[CONV_ID]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "cursor"
    # Synthetic fixture has no workspace metadata in any bubble, so the
    # adapter falls back to the literal "cursor" slug. The dedicated
    # per-workspace tests below seed real fsPath fields.
    assert ref.project_slug == "cursor"
    assert ref.source_kind == "database"
    assert ref.source_hint == {"conversation_id": CONV_ID}
    assert ref.file_path == vscdb_path
    assert ref.file_size > 0
    assert ref.file_mtime > 0


def test_enumerate_returns_empty_when_db_missing(tmp_path: Path) -> None:
    """Missing vscdb is not an error — Cursor simply isn't installed."""
    missing = tmp_path / "does-not-exist.vscdb"
    adapter = CursorAdapter(vscdb_path=missing)
    assert list(adapter.enumerate()) == []


def test_enumerate_handles_v3_positional_conversation_id(tmp_path: Path) -> None:
    """Cursor v3+ encodes conversationId as ``bubbleId:<conv>:<bubble>``.

    The JSON value no longer carries ``conversationId``; the adapter
    must extract it from the key.
    """
    import json
    import sqlite3
    db = tmp_path / "v3.vscdb"
    conn = sqlite3.connect(db)
    conn.execute("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
    conv = "v3-conv-aaa"
    bubble_payload = json.dumps({"_v": 3, "type": 1, "text": "hi"})
    for n in range(3):
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?, ?)",
            (f"bubbleId:{conv}:bub-{n}", bubble_payload),
        )
    conn.commit()
    conn.close()
    refs = list(CursorAdapter(vscdb_path=db).enumerate())
    assert [r.session_id for r in refs] == [conv]


def test_read_yields_records_for_target_conversation(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    assert len(refs) == 1
    records = list(adapter.read(refs[0]))
    # 2 bubbles + 1 agentKv = 3, all in CONV_ID; the OTHER_CONV_ID row
    # is filtered out.
    assert len(records) == 3
    seqs = [r.seq for r in records]
    assert seqs == sorted(seqs)
    assert all(isinstance(r, Record) for r in records)
    roles = [r.role for r in records]
    assert "user" in roles and "assistant" in roles and "tool" in roles


def test_read_assistant_record_has_explicit_tokens(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    assistant = next(r for r in records if r.role == "assistant")
    assert assistant.input_tokens == 120
    assert assistant.output_tokens == 480
    assert assistant.cache_create_tokens == 0
    assert assistant.cache_read_tokens == 0
    assert assistant.model == "claude-sonnet-4-6"
    # Explicit tokens => not estimated.
    assert assistant.raw.get("cost_source") != "estimated"


def test_read_user_record_estimates_tokens_when_zero(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    user = next(r for r in records if r.role == "user")
    # "Refactor this please." is 21 chars → 21 // 4 == 5
    assert user.input_tokens == len("Refactor this please.") // 4
    assert user.output_tokens == 0
    assert user.raw.get("cost_source") == "estimated"


def test_read_agent_kv_record_uses_provider_options_model(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    tool = next(r for r in records if r.role == "tool")
    assert tool.model == "cursor-auto"
    assert tool.content_text == "ran tests"


def test_read_since_offset_drops_earlier_rows(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    full = list(adapter.read(refs[0]))
    midpoint = full[len(full) // 2].seq
    resumed = list(adapter.read(refs[0], since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


def test_record_uuid_is_stable_session_plus_rowid(vscdb_path: Path) -> None:
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    records = list(adapter.read(refs[0]))
    for rec in records:
        assert rec.uuid == f"{CONV_ID}:{rec.seq}"
        assert rec.parent_uuid is None


# ── fingerprint cache integration ─────────────────────────────────────


def test_second_read_hits_cache_and_skips_sqlite(
    vscdb_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Once the cache is warm, a second ``read()`` must not open SQLite.

    Verified by counting calls to ``CursorAdapter._open_readonly`` —
    the only path the adapter takes to touch the on-disk DB.
    """
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    assert refs

    # First call — cache miss, must open SQLite.
    first = list(adapter.read(refs[0]))
    assert first

    # Now wrap _open_readonly to count invocations on the second call.
    calls = {"n": 0}
    real_open = CursorAdapter._open_readonly

    def counting_open(path):  # type: ignore[no-untyped-def]
        calls["n"] += 1
        return real_open(path)

    monkeypatch.setattr(CursorAdapter, "_open_readonly", staticmethod(counting_open))

    second = list(adapter.read(refs[0]))
    assert calls["n"] == 0, "second read must hit fingerprint cache, not SQLite"

    # Records must match the live parse result.
    assert [r.uuid for r in second] == [r.uuid for r in first]
    assert [r.role for r in second] == [r.role for r in first]
    assert [
        (r.input_tokens, r.output_tokens) for r in second
    ] == [(r.input_tokens, r.output_tokens) for r in first]


def test_resume_read_bypasses_cache(
    vscdb_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """``since_offset > 0`` must always re-query SQLite (no cache slice)."""
    adapter = CursorAdapter(vscdb_path=vscdb_path)
    refs = [r for r in adapter.enumerate() if r.session_id == CONV_ID]
    full = list(adapter.read(refs[0]))
    assert full

    calls = {"n": 0}
    real_open = CursorAdapter._open_readonly

    def counting_open(path):  # type: ignore[no-untyped-def]
        calls["n"] += 1
        return real_open(path)

    monkeypatch.setattr(CursorAdapter, "_open_readonly", staticmethod(counting_open))

    midpoint = full[len(full) // 2].seq
    list(adapter.read(refs[0], since_offset=midpoint))
    assert calls["n"] == 1, "resume reads must always re-parse the DB"


# ── per-workspace project_slug ────────────────────────────────────────


def _bubble_with_paths(
    conv: str,
    bubble_id: str,
    file_paths: list[str] | None = None,
    tool_target: str | None = None,
    text: str = "hello",
) -> tuple[str, str]:
    """Build a (key, json_value) pair shaped like a Cursor v3+ bubble.

    ``file_paths`` populates ``context.fileSelections`` with the
    canonical Cursor URI shape; ``tool_target`` plants an absolute path
    inside ``toolFormerData.params`` (the dominant signal in the user's
    real vscdb).
    """
    payload: dict = {"_v": 3, "type": 1, "text": text}
    if file_paths:
        payload["context"] = {
            "fileSelections": [
                {
                    "uri": {"fsPath": p, "path": p, "scheme": "file"},
                    "uuid": str(i),
                }
                for i, p in enumerate(file_paths)
            ],
        }
    if tool_target:
        payload["toolFormerData"] = {
            "name": "read_file",
            "params": json.dumps({"targetFile": tool_target}),
        }
    return f"bubbleId:{conv}:{bubble_id}", json.dumps(payload)


def _build_workspace_fixture(
    path: Path, rows: list[tuple[str, str]]
) -> None:
    """Materialise ``rows`` into a fresh state.vscdb at *path*."""
    conn = sqlite3.connect(path)
    try:
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)"
        )
        conn.executemany(
            "INSERT INTO cursorDiskKV(key, value) VALUES (?, ?)", rows
        )
        conn.commit()
    finally:
        conn.close()


def test_enumerate_assigns_distinct_slugs_for_distinct_workspaces(
    tmp_path: Path,
) -> None:
    """Two conversations rooted at different cwds → two distinct slugs.

    Reproduces the user-facing bug where every cursor conversation
    collapsed under one ``"cursor"`` project. The fixture references
    ``/Users/dev/projects/alpha`` from one conv and
    ``/Users/dev/projects/beta`` from another; each must surface its
    own Claude-style ``-Users-dev-projects-X`` slug.
    """
    db = tmp_path / "state.vscdb"
    rows = [
        _bubble_with_paths(
            "conv-alpha",
            "b1",
            file_paths=[
                "/Users/dev/projects/alpha/src/main.ts",
                "/Users/dev/projects/alpha/README.md",
            ],
        ),
        _bubble_with_paths(
            "conv-alpha",
            "b2",
            tool_target="/Users/dev/projects/alpha/package.json",
        ),
        _bubble_with_paths(
            "conv-beta",
            "b1",
            file_paths=[
                "/Users/dev/projects/beta/lib/util.py",
                "/Users/dev/projects/beta/tests/test_util.py",
            ],
        ),
    ]
    _build_workspace_fixture(db, rows)

    adapter = CursorAdapter(vscdb_path=db)
    refs = sorted(adapter.enumerate(), key=lambda r: r.session_id)
    assert [r.session_id for r in refs] == ["conv-alpha", "conv-beta"]
    assert refs[0].project_slug == "-Users-dev-projects-alpha"
    assert refs[1].project_slug == "-Users-dev-projects-beta"
    # And, critically, the two slugs differ — the bug was that they
    # both collapsed to "cursor".
    assert refs[0].project_slug != refs[1].project_slug


def test_enumerate_falls_back_to_cursor_when_no_paths(tmp_path: Path) -> None:
    """A conversation with zero referenced paths gets the fallback slug.

    Mirrors the user's real machine where one short chat ("how does the
    diff feature work") has no fsPath data anywhere — graceful
    degradation must keep that conversation visible under the legacy
    ``"cursor"`` umbrella rather than dropping it.
    """
    db = tmp_path / "state.vscdb"
    rows = [
        _bubble_with_paths("conv-empty", "b1", text="just a question"),
    ]
    _build_workspace_fixture(db, rows)

    refs = list(CursorAdapter(vscdb_path=db).enumerate())
    assert len(refs) == 1
    assert refs[0].project_slug == "cursor"


def test_enumerate_picks_majority_workspace_when_paths_diverge(
    tmp_path: Path,
) -> None:
    """When most paths root at one workspace and a stray path leaks
    elsewhere, the slug must reflect the dominant workspace — not the
    home-directory LCP.

    Without the >= 50 % coverage rule the LCP of all paths would be
    ``/Users/dev`` (which we'd then slug as ``-Users-dev``), losing the
    workspace signal. The user's real ``KayTEL`` conversation hit this
    case before the fix.
    """
    db = tmp_path / "state.vscdb"
    rows = [
        _bubble_with_paths(
            "conv-mixed",
            "b1",
            file_paths=[
                "/Users/dev/work/kaytel/src/a.ts",
                "/Users/dev/work/kaytel/src/b.ts",
                "/Users/dev/work/kaytel/README.md",
            ],
        ),
        _bubble_with_paths(
            # Single stray reference — must NOT poison the slug.
            "conv-mixed",
            "b2",
            tool_target="/Users/dev/elsewhere/notes.md",
        ),
    ]
    _build_workspace_fixture(db, rows)

    refs = list(CursorAdapter(vscdb_path=db).enumerate())
    assert len(refs) == 1
    assert refs[0].project_slug == "-Users-dev-work-kaytel"


def test_enumerate_uses_mentions_folder_selections(tmp_path: Path) -> None:
    """``context.mentions.folderSelections`` is also a workspace signal.

    Cursor records folders the user dragged onto the chat under the
    ``mentions`` map keyed by ``file://`` URI; we should treat those
    folders as candidates exactly like file selections.
    """
    db = tmp_path / "state.vscdb"
    payload = {
        "_v": 3,
        "type": 1,
        "text": "look at this folder",
        "context": {
            "mentions": {
                "folderSelections": {
                    "file:///Users/dev/projects/zeta": [{"uuid": "1"}],
                },
                "fileSelections": {},
            },
        },
    }
    rows = [(f"bubbleId:conv-zeta:b1", json.dumps(payload))]
    _build_workspace_fixture(db, rows)

    refs = list(CursorAdapter(vscdb_path=db).enumerate())
    assert len(refs) == 1
    assert refs[0].project_slug == "-Users-dev-projects-zeta"


def test_enumerate_rejects_paths_above_user_directory(tmp_path: Path) -> None:
    """A bare ``/Users/dev`` reference must not become the workspace.

    The minimum-depth guard ensures we don't emit ``-Users-dev`` as a
    slug — that would be the user's whole home directory and defeats
    the per-workspace split.
    """
    db = tmp_path / "state.vscdb"
    rows = [
        _bubble_with_paths(
            "conv-shallow",
            "b1",
            file_paths=["/Users/dev"],
        ),
    ]
    _build_workspace_fixture(db, rows)

    refs = list(CursorAdapter(vscdb_path=db).enumerate())
    assert len(refs) == 1
    # Path is too shallow to count as a workspace root — fall back.
    assert refs[0].project_slug == "cursor"


def test_read_propagates_workspace_slug(tmp_path: Path) -> None:
    """Records are still keyed on session_id, but the SessionRef carries
    the workspace slug end-to-end so the store/aggregator stamps the
    correct ``project_id``."""
    db = tmp_path / "state.vscdb"
    rows = [
        _bubble_with_paths(
            "conv-alpha",
            "b1",
            file_paths=[
                # Two files under different subdirectories of the same
                # project root — coverage breaks the tie so the slug
                # lands on ``alpha`` rather than a stray subdirectory.
                "/Users/dev/projects/alpha/src/main.ts",
                "/Users/dev/projects/alpha/tests/test_main.ts",
            ],
        ),
    ]
    _build_workspace_fixture(db, rows)

    adapter = CursorAdapter(vscdb_path=db)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].project_slug == "-Users-dev-projects-alpha"
    records = list(adapter.read(refs[0]))
    # The records themselves don't carry project_slug (it lives on the
    # SessionRef the caller already holds), so just confirm that the
    # adapter still produces records correctly under a real slug.
    assert len(records) >= 1
    assert all(r.session_id == "conv-alpha" for r in records)


# ── shared adapter contract ────────────────────────────────────────────


class TestCursorAdapterContract(unittest.TestCase, AdapterContract):
    """Runs every AdapterContract invariant against the Cursor fixture."""

    def setUp(self) -> None:
        # Build a fresh fixture per test method into a tmpdir we own.
        import tempfile

        from stackunderflow.infra import cursor_cache as _cc

        self._tmpdir = tempfile.TemporaryDirectory()
        path = Path(self._tmpdir.name) / "state.vscdb"
        _build_fixture(path)

        # Redirect the parse cache into the same tmpdir so the test
        # never touches the user's real ``~/.stackunderflow/cache/``.
        cache_dir = Path(self._tmpdir.name)
        self._orig_cache_path = _cc._default_cache_path
        _cc._default_cache_path = lambda: cache_dir / "cursor-results.json"

        self.adapter = CursorAdapter(vscdb_path=path)

    def tearDown(self) -> None:
        from stackunderflow.infra import cursor_cache as _cc

        _cc._default_cache_path = self._orig_cache_path
        self._tmpdir.cleanup()


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_malformed_token_count_and_selections_do_not_crash(tmp_path: Path) -> None:
    """String token counts fall back to the len//4 estimate, and truthy
    non-list ``fileSelections`` / ``attachedFoldersNew`` must not crash
    the workspace-slug derivation inside enumerate()."""
    fp = tmp_path / "state.vscdb"
    conn = sqlite3.connect(fp)
    try:
        conn.execute(
            "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)"
        )
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES (?, ?)",
            (
                "bubbleId:conv-bad:b1",
                json.dumps({
                    "conversationId": "conv-bad",
                    "type": 2,  # assistant
                    "text": "four char chunks here",  # 21 chars -> 5 est
                    "tokenCount": {"inputTokens": "garbage", "outputTokens": [1]},
                    "createdAt": 1714000000000,
                    # Truthy non-lists — previously TypeError in _paths_in_bubble.
                    "context": {"fileSelections": 7},
                    "attachedFoldersNew": "not-a-list",
                }),
            ),
        )
        conn.commit()
    finally:
        conn.close()

    adapter = CursorAdapter(vscdb_path=fp)
    refs = list(adapter.enumerate())  # slug derivation walks the bubbles
    assert len(refs) == 1
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    # Garbage counts coerce to 0 -> estimation path kicks in.
    assert rec.input_tokens == len("four char chunks here") // 4
    assert rec.output_tokens == 0
    assert rec.raw.get("cost_source") == "estimated"
