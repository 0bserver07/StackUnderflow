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
    adapter = _StubAdapter([_rec(0)])
    ingest_file(conn, adapter, ref)
    row = conn.execute(
        "SELECT mtime, size, processed_offset FROM ingest_log WHERE file_path = ?",
        (str(ref.file_path),),
    ).fetchone()
    assert row["mtime"] == 5.0
    assert row["size"] == 42
    assert row["processed_offset"] == 42


def test_ingest_file_is_idempotent_on_seq(conn, tmp_path: Path) -> None:
    ref = _ref(tmp_path)
    adapter = _StubAdapter([_rec(0), _rec(0)])  # duplicate seq
    ingest_file(conn, adapter, ref)
    count = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
    assert count == 1  # INSERT OR IGNORE


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
