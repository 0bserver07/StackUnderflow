import sqlite3
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.ingest.writer import ingest_file
from stackunderflow.store import db, schema


class _StubAdapter:
    name = "stub"

    def __init__(self, records):
        self._records = records

    def enumerate(self):
        return []

    def read(self, ref, *, since_offset=0):
        yield from self._records


def _ref(tmp: Path, mtime: float = 1.0, size: int = 10) -> SessionRef:
    fp = tmp / "x.jsonl"
    fp.write_bytes(b"x" * size)
    return SessionRef("stub", "-a", "s1", fp, mtime, size)


def _rec(seq: int, ts: str = "2026-01-01T00:00:00+00:00") -> Record:
    return Record(
        provider="stub", session_id="s1", seq=seq,
        timestamp=ts, role="user", model=None,
        input_tokens=0, output_tokens=0,
        cache_create_tokens=0, cache_read_tokens=0,
        content_text="", tools=(), cwd=None,
        is_sidechain=False, uuid="u", parent_uuid=None, raw={},
    )


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


def test_ingest_file_inserts_messages(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    adapter = _StubAdapter([_rec(0), _rec(1)])
    ingest_file(conn, adapter, ref)
    count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    assert count == 2


def test_ingest_file_creates_project_and_session(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    adapter = _StubAdapter([_rec(0)])
    ingest_file(conn, adapter, ref)
    projects = conn.execute("SELECT slug FROM projects").fetchall()
    sessions = conn.execute("SELECT session_id FROM sessions").fetchall()
    assert projects[0]["slug"] == "-a"
    assert sessions[0]["session_id"] == "s1"


def test_ingest_file_updates_ingest_log(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path, mtime=5.0, size=42)
    # File-mode resume now stores max(record.seq) so the next pass can ask
    # the adapter for "everything past this seq" — for real JSONL adapters
    # seq is the byte offset of the line, so this is the byte position of
    # the last consumed line.
    adapter = _StubAdapter([_rec(7), _rec(15)])
    ingest_file(conn, adapter, ref)
    row = conn.execute(
        "SELECT mtime, size, processed_offset, last_rowid, storage_kind, session_id "
        "FROM ingest_log WHERE file_path = ?",
        (str(ref.file_path),),
    ).fetchone()
    assert row["mtime"] == 5.0
    assert row["size"] == 42
    assert row["processed_offset"] == 15  # max seq seen
    assert row["last_rowid"] is None
    assert row["storage_kind"] == "file"
    assert row["session_id"] is None


def test_ingest_file_is_idempotent_on_seq(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    adapter = _StubAdapter([_rec(0), _rec(0)])  # duplicate seq
    ingest_file(conn, adapter, ref)
    count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    assert count == 1  # INSERT OR IGNORE


# ── zero-record files must not mint ghost projects ───────────────────────────
#
# The project/session upsert used to run before the first record was read, so
# any file the adapter could merely *name* got a project row. A file that then
# read out empty left that row behind permanently: the ingest_log write below
# marks even a zero-record file processed, and the enumerate pass skips
# unchanged files forever after. The result was path-less, message-less
# projects in every listing.


def test_zero_record_file_creates_no_project_or_session(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    ingest_file(conn, _StubAdapter([]), ref)

    assert conn.execute("SELECT COUNT(*) FROM projects").fetchone()[0] == 0
    assert conn.execute("SELECT COUNT(*) FROM sessions").fetchone()[0] == 0
    assert conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == 0


def test_zero_record_file_is_still_marked_processed(conn, tmp_path: Path) -> None:
    """Skip-unchanged stays intact — we just don't invent rows for it."""
    ref = _ref(tmp_path, mtime=3.0, size=64)
    ingest_file(conn, _StubAdapter([]), ref)

    row = conn.execute(
        "SELECT mtime, size, processed_offset FROM ingest_log "
        "WHERE file_path = ? AND session_id IS NULL",
        (str(ref.file_path),),
    ).fetchone()
    assert row is not None
    assert row["mtime"] == 3.0
    assert row["size"] == 64
    # Whole file consumed → the next pass won't re-scan it.
    assert row["processed_offset"] == 64


def test_file_that_later_yields_records_creates_rows_then(conn, tmp_path: Path) -> None:
    """A deferred upsert must still fire on a resumed read that finds records."""
    fp = tmp_path / "x.jsonl"
    fp.write_bytes(b"x" * 10)
    ref_v1 = SessionRef("stub", "-a", "s1", fp, 1.0, 10)
    ingest_file(conn, _StubAdapter([]), ref_v1)
    assert conn.execute("SELECT COUNT(*) FROM projects").fetchone()[0] == 0

    # The file grows and now holds real records. ``run_ingest`` resumes from
    # the stored offset; mirror that here.
    resume = conn.execute(
        "SELECT processed_offset FROM ingest_log "
        "WHERE file_path = ? AND session_id IS NULL",
        (str(fp),),
    ).fetchone()["processed_offset"]
    assert resume == 10

    fp.write_bytes(b"x" * 40)
    ref_v2 = SessionRef("stub", "-a", "s1", fp, 2.0, 40)
    ingest_file(
        conn, _StubAdapter([_rec(20), _rec(30)]), ref_v2, since_offset=resume,
    )

    assert [r["slug"] for r in conn.execute("SELECT slug FROM projects")] == ["-a"]
    assert [
        r["session_id"] for r in conn.execute("SELECT session_id FROM sessions")
    ] == ["s1"]
    assert conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0] == 2
    # The session counters were applied to the row created mid-loop.
    srow = conn.execute("SELECT message_count FROM sessions").fetchone()
    assert srow["message_count"] == 2


def test_zero_record_pass_still_bumps_a_known_project(conn, tmp_path: Path) -> None:
    """Not creating rows must not stall "last active" for a real project."""
    ref_v1 = _ref(tmp_path, mtime=1.0, size=10)
    ingest_file(conn, _StubAdapter([_rec(0)]), ref_v1)

    # Same project, a later pass that reads nothing new out of the file.
    ref_v2 = SessionRef("stub", "-a", "s1", ref_v1.file_path, 9.0, 10)
    ingest_file(conn, _StubAdapter([]), ref_v2)

    rows = conn.execute("SELECT slug, last_modified FROM projects").fetchall()
    assert len(rows) == 1
    assert rows[0]["last_modified"] == 9.0


def test_ingest_file_rollback_on_failure(conn, tmp_path: Path) -> None:
    class _BoomAdapter:
        name = "stub"

        def read(self, ref, *, since_offset=0):
            yield _rec(0)
            raise RuntimeError("boom")

    ref = _ref(tmp_path)
    with pytest.raises(RuntimeError):
        ingest_file(conn, _BoomAdapter(), ref)
    count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    assert count == 0
    log = conn.execute("SELECT * FROM ingest_log").fetchall()
    assert log == []
