"""Tests for the Wave 2C ETL filesystem watcher.

The watcher is timing-sensitive — these tests use generous timeouts
(1-2s) and ``threading.Event.wait()`` rather than ``time.sleep`` polling
so they don't flake on slow CI runners. The Rust-backed ``watchfiles``
library is FSEvents-driven on macOS and inotify-driven on Linux; both
deliver sub-100ms latency, but a CI box under load can easily drift
toward 500ms — hence the 2s ceiling on every wait.
"""

from __future__ import annotations

import sqlite3
import threading
import time
from collections.abc import Callable
from pathlib import Path

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.etl.watcher import (
    WatcherHandle,
    _adapter_for_path,
    start_watcher,
    watch_paths_for,
)
from stackunderflow.store import db, schema

# ── fakes ──────────────────────────────────────────────────────────────


class _FakeJsonlAdapter:
    """Minimal SourceAdapter pointing at a temp directory of JSONL files.

    Mirrors the Claude adapter contract closely enough that the
    existing ``run_ingest`` writer can drive it: ``enumerate`` walks the
    directory, ``read`` yields one ``Record`` per line keyed on byte
    offset (so file-mode resume works), and ``watch_paths`` returns the
    directory itself for the watcher to follow.
    """

    name = "stub"

    def __init__(self, root: Path) -> None:
        self._root = root
        self.read_calls: list[Path] = []

    def watch_paths(self) -> list[Path]:
        return [self._root]

    def enumerate(self):
        for fp in sorted(self._root.glob("*.jsonl")):
            stat = fp.stat()
            yield SessionRef(
                provider=self.name,
                project_slug=fp.stem,
                session_id=fp.stem,
                file_path=fp,
                file_mtime=stat.st_mtime,
                file_size=stat.st_size,
            )

    def read(self, ref: SessionRef, *, since_offset: int = 0):
        self.read_calls.append(ref.file_path)
        # One Record per line; seq = byte offset of the line start, so
        # the writer's resume math matches Claude's contract.
        offset = 0
        for raw_line in ref.file_path.read_bytes().splitlines(keepends=True):
            stripped = raw_line.strip()
            line_offset = offset
            offset += len(raw_line)
            if not stripped:
                continue
            if since_offset > 0 and line_offset <= since_offset:
                continue
            yield Record(
                provider=self.name,
                session_id=ref.session_id,
                seq=line_offset,
                timestamp="2026-05-01T00:00:00+00:00",
                role="assistant",
                model="stub-model",
                input_tokens=10,
                output_tokens=20,
                cache_create_tokens=0,
                cache_read_tokens=0,
                content_text=stripped.decode("utf-8", errors="replace"),
                tools=(),
                cwd=None,
                is_sidechain=False,
                uuid=f"{ref.session_id}:{line_offset}",
                parent_uuid=None,
                raw={"line": stripped.decode("utf-8", errors="replace")},
            )


def _conn_factory(db_path: Path) -> Callable[[], sqlite3.Connection]:
    def _make() -> sqlite3.Connection:
        c = db.connect(db_path)
        return c
    return _make


# ── unit tests ─────────────────────────────────────────────────────────


def test_watch_paths_for_filters_missing(tmp_path: Path) -> None:
    """``watch_paths_for`` drops paths that don't exist on disk so the
    watcher loop is never handed a missing root (which ``watchfiles``
    treats as a fatal startup error)."""
    real = tmp_path / "real"
    real.mkdir()
    fake = tmp_path / "missing"

    class _Adapter:
        name = "x"

        def watch_paths(self):
            return [real, fake]

        def enumerate(self):
            return iter(())

        def read(self, ref, *, since_offset=0):
            return iter(())

    paths = watch_paths_for(_Adapter())
    assert paths == [real]


def test_watch_paths_for_handles_no_method() -> None:
    """An adapter without ``watch_paths`` reports an empty list — the
    watcher silently ignores it. Default behaviour for the ~12 beta
    adapters that haven't been validated for live-watching yet.
    """

    class _Bare:
        name = "bare"

        def enumerate(self):
            return iter(())

        def read(self, ref, *, since_offset=0):
            return iter(())

    assert watch_paths_for(_Bare()) == []


def test_adapter_for_path_matches_root(tmp_path: Path) -> None:
    """Path-prefix matching: any file inside a watched root maps to the
    adapter that owns it. Used to dispatch each change event to the
    correct ingest run.
    """
    a_root = tmp_path / "a"
    a_root.mkdir()
    b_root = tmp_path / "b"
    b_root.mkdir()
    a_file = a_root / "x.jsonl"
    a_file.write_text("hello")

    class _A:
        name = "a"

    class _B:
        name = "b"

    adapter_paths = [(_A(), [a_root]), (_B(), [b_root])]
    matched = _adapter_for_path(str(a_file), adapter_paths)
    assert matched is not None
    assert matched.name == "a"


def test_no_watch_paths_returns_idle_handle(tmp_path: Path) -> None:
    """When every adapter has ``watch_paths() == []`` the watcher
    spawns no thread (returns an inert handle). Useful for the dozen
    beta adapters that haven't opted in to live-watching yet."""

    class _Idle:
        name = "idle"

        def watch_paths(self):
            return []

        def enumerate(self):
            return iter(())

        def read(self, ref, *, since_offset=0):
            return iter(())

    handle = start_watcher(_conn_factory(tmp_path / "store.db"), adapters=[_Idle()])
    assert isinstance(handle, WatcherHandle)
    handle.stop(timeout=1.0)


# ── integration tests ─────────────────────────────────────────────────


def test_append_triggers_refresh(tmp_path: Path) -> None:
    """Append a JSONL line → adapter.read fires → message lands → the
    watcher logs a refresh cycle within ~500ms.

    We use a ``threading.Event`` flipped when the messages count
    increases, then ``Event.wait`` so the test isn't a tight
    sleep-loop. 2s ceiling is generous enough for the slowest CI box.
    """
    src_root = tmp_path / "src"
    src_root.mkdir()
    db_path = tmp_path / "store.db"

    # Boot the schema once so the watcher's first cycle has somewhere
    # to write — the lifespan-applied schema-apply step doesn't run in
    # this unit test.
    boot = db.connect(db_path)
    schema.apply(boot)
    boot.close()

    adapter = _FakeJsonlAdapter(src_root)
    handle = start_watcher(_conn_factory(db_path), adapters=[adapter])
    try:
        # Give watchfiles a moment to spin up the FSEvents watch
        # before we cause the first event. 100ms is enough on macOS;
        # 200ms is enough margin to keep this test stable on a heavily
        # loaded CI box.
        time.sleep(0.2)

        target = src_root / "session.jsonl"
        target.write_text('{"line": "first"}\n')

        seen = threading.Event()

        def _watch_db() -> None:
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                try:
                    c = db.connect(db_path)
                    n = c.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
                    c.close()
                except sqlite3.Error:
                    n = 0
                if n >= 1:
                    seen.set()
                    return
                time.sleep(0.05)

        t = threading.Thread(target=_watch_db, daemon=True)
        t.start()

        assert seen.wait(timeout=2.0), (
            "Watcher did not insert a message within 2s of file append"
        )

        c = db.connect(db_path)
        try:
            count = c.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            assert count >= 1
        finally:
            c.close()
    finally:
        handle.stop(timeout=2.0)


def test_burst_writes_collapse_into_one_cycle(tmp_path: Path) -> None:
    """A 5-line burst within the 200ms debounce window should fire one
    refresh cycle, not five. We assert by counting the times the fake
    adapter's ``read`` is called: a single cycle picks up every new
    line in one pass.

    Note: ``run_ingest`` calls ``adapter.read`` once per ``SessionRef``
    that's seen change. With a single growing file, that's exactly one
    call per debounce-window-coalesced cycle.
    """
    src_root = tmp_path / "src"
    src_root.mkdir()
    db_path = tmp_path / "store.db"
    boot = db.connect(db_path)
    schema.apply(boot)
    boot.close()

    adapter = _FakeJsonlAdapter(src_root)

    handle = start_watcher(
        _conn_factory(db_path),
        adapters=[adapter],
        debounce_ms=300,  # generous so the burst always fits inside
    )
    try:
        time.sleep(0.2)  # let the watcher spin up

        target = src_root / "session.jsonl"
        # 5 quick appends inside the debounce window. We open the file
        # in append mode and fsync-equivalent flushes between writes so
        # FSEvents/inotify can see them.
        with target.open("a") as fh:
            for i in range(5):
                fh.write(f'{{"line": "burst-{i}"}}\n')
                fh.flush()
                # ~10ms apart: well under the 300ms debounce, so
                # watchfiles must coalesce them into one yield.
                time.sleep(0.01)

        # Wait long enough for the debounce window to close + one
        # refresh cycle to run.
        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            c = db.connect(db_path)
            n = c.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            c.close()
            if n >= 5:
                break
            time.sleep(0.05)

        # Settle: give the system a beat in case a stray follow-up
        # cycle was queued. Then assert the read call count.
        time.sleep(0.4)

        # Exactly one ingest cycle — i.e., adapter.read was called once
        # for the (single) growing JSONL ref. Allow ≤2 for jitter on
        # CI: macOS's FSEvents occasionally fires a grouping followed
        # by a stragglers callback for the same file.
        assert 1 <= len(adapter.read_calls) <= 2, (
            f"Expected debounce to coalesce burst into 1 cycle, got "
            f"{len(adapter.read_calls)} read() calls"
        )

        c = db.connect(db_path)
        try:
            count = c.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
            assert count == 5
        finally:
            c.close()
    finally:
        handle.stop(timeout=2.0)


def test_stop_terminates_thread_within_timeout(tmp_path: Path) -> None:
    """``WatcherHandle.stop()`` must signal the loop and join the thread
    within the supplied timeout. Critical because the daemon thread
    pattern is the only thing keeping the process responsive on
    Ctrl+C — a slow-shutdown bug here would leave the FastAPI app
    hanging.
    """
    src_root = tmp_path / "src"
    src_root.mkdir()
    db_path = tmp_path / "store.db"
    boot = db.connect(db_path)
    schema.apply(boot)
    boot.close()

    adapter = _FakeJsonlAdapter(src_root)
    handle = start_watcher(_conn_factory(db_path), adapters=[adapter])
    time.sleep(0.1)
    assert handle.thread.is_alive()

    t0 = time.monotonic()
    handle.stop(timeout=3.0)
    elapsed = time.monotonic() - t0

    assert not handle.thread.is_alive(), "watcher thread did not exit on stop()"
    # Should be well under the 3s budget — the rust_timeout=1000ms
    # cycle bound on the watcher loop guarantees a wake within 1s.
    assert elapsed < 3.0, f"watcher stop took {elapsed:.2f}s (>3s)"
