import sqlite3
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest import run_ingest
from stackunderflow.store import db, schema


class _StubAdapter:
    name = "stub"

    def __init__(self, refs, records_per_ref):
        self._refs = refs
        self._records = records_per_ref

    def enumerate(self):
        yield from self._refs

    def read(self, ref, *, since_offset=0):
        yield from self._records.get(ref.session_id, [])


def _rec(seq: int) -> Record:
    return Record(
        provider="stub", session_id="s1", seq=seq,
        timestamp="2026-01-01T00:00:00+00:00", role="user", model=None,
        input_tokens=0, output_tokens=0,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text=f"m{seq}", tools=(), cwd=None,
        is_sidechain=False, uuid="u", parent_uuid=None, raw={},
    )


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def test_initial_load(conn, tmp_path: Path) -> None:
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 100)
    ref = SessionRef("stub", "-a", "s1", fp, file_mtime=1.0, file_size=100)
    run_ingest(conn, [_StubAdapter([ref], {"s1": [_rec(0), _rec(1)]})])
    assert conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == 2


def test_unchanged_file_skipped(conn, tmp_path: Path) -> None:
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 100)
    ref = SessionRef("stub", "-a", "s1", fp, file_mtime=1.0, file_size=100)

    call_count = {"n": 0}

    class _CountingAdapter(_StubAdapter):
        def read(self, ref, *, since_offset=0):
            call_count["n"] += 1
            yield from super().read(ref, since_offset=since_offset)

    adapter = _CountingAdapter([ref], {"s1": [_rec(0)]})
    run_ingest(conn, [adapter])
    run_ingest(conn, [adapter])  # second time
    assert call_count["n"] == 1


def test_appended_file_reads_only_tail(conn, tmp_path: Path) -> None:
    """File-mode resume stores max(seq) of yielded records, so on a follow-up
    pass the adapter is asked for ``since_offset = that seq``. For real
    JSONL adapters seq is the byte offset of the source line, so this maps
    to "skip everything up to and including the last consumed line."
    """
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 100)
    ref_v1 = SessionRef("stub", "-a", "s1", fp, file_mtime=1.0, file_size=100)
    # Use seq values that mimic line byte offsets (Claude/Codex semantics).
    run_ingest(conn, [_StubAdapter([ref_v1], {"s1": [_rec(0), _rec(40), _rec(80)]})])

    # grow the file
    fp.write_bytes(b"x" * 200)

    captured_offset = {"v": -1}

    class _CapturingAdapter(_StubAdapter):
        def read(self, ref, *, since_offset=0):
            captured_offset["v"] = since_offset
            yield _rec(since_offset + 1)

    ref_v2 = SessionRef("stub", "-a", "s1", fp, file_mtime=2.0, file_size=200)
    run_ingest(conn, [_CapturingAdapter([ref_v2], {})])
    # The highest seq from the first ingest was 80; the second pass should
    # resume from there.
    assert captured_offset["v"] == 80


def test_zero_record_file_leaves_no_project_and_is_not_re_read(
    conn, tmp_path: Path
) -> None:
    """A file the adapter can name but reads nothing out of mints no rows.

    This is the ghost-project shape: a rollout whose metadata carries no
    working directory gets a synthetic slug, and if the file then yields no
    records the project row used to be permanent — the ingest_log marks even
    a zero-record file processed, so this pass never comes back. Both halves
    matter: no rows, and still no re-read.
    """
    fp = tmp_path / "ghost.jsonl"
    fp.write_bytes(b"x" * 100)
    ref = SessionRef(
        "stub", "stub-0000-ghost", "s1", fp, file_mtime=1.0, file_size=100,
    )

    call_count = {"n": 0}

    class _CountingAdapter(_StubAdapter):
        def read(self, ref, *, since_offset=0):
            call_count["n"] += 1
            yield from super().read(ref, since_offset=since_offset)

    adapter = _CountingAdapter([ref], {})  # nothing to yield for s1
    run_ingest(conn, [adapter])
    run_ingest(conn, [adapter])

    assert call_count["n"] == 1  # skip-unchanged still holds
    assert conn.execute("SELECT COUNT(*) FROM projects").fetchone()[0] == 0
    assert conn.execute("SELECT COUNT(*) FROM sessions").fetchone()[0] == 0


def test_truncated_file_full_reparse(conn, tmp_path: Path) -> None:
    fp = tmp_path / "a.jsonl"
    fp.write_bytes(b"x" * 200)
    ref_v1 = SessionRef("stub", "-a", "s1", fp, file_mtime=1.0, file_size=200)
    run_ingest(conn, [_StubAdapter([ref_v1], {"s1": [_rec(0)]})])

    # shrink
    fp.write_bytes(b"x" * 50)

    captured_offset = {"v": -1}

    class _CapturingAdapter(_StubAdapter):
        def read(self, ref, *, since_offset=0):
            captured_offset["v"] = since_offset
            return iter([])

    ref_v2 = SessionRef("stub", "-a", "s1", fp, file_mtime=2.0, file_size=50)
    run_ingest(conn, [_CapturingAdapter([ref_v2], {})])
    assert captured_offset["v"] == 0  # full reparse
