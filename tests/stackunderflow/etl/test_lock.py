"""Tests for the single-watcher invariant lock at
``~/.stackunderflow/server.lock``.

The lock prevents two ``stackunderflow start`` invocations against the
same store from running two filesystem watchers (which would race on
ingest + mart refresh). Tests use ``tmp_path`` exclusively — never the
real lock file at ``~/.stackunderflow/server.lock``.

Coverage:

* Two acquires in the same process → second returns ``None``.
* ``release_watcher_lock`` → next acquire succeeds.
* Stale PID (process no longer exists) is reclaimed cleanly.
* Context-manager form releases on body exit + on exception.
* Status assembler reports ``watcher.lock_held_by`` PID.
* Lifespan simulation: two parallel ``server`` startups — only one
  spawns the watcher, the other logs WARNING + serves HTTP without one.
"""

from __future__ import annotations

import os
from pathlib import Path
from unittest import mock

import pytest

from stackunderflow.etl.lock import (
    LockHandle,
    acquire_watcher_lock,
    read_lock_holder,
    release_watcher_lock,
    watcher_lock,
)

# ── unit: acquire / release semantics ─────────────────────────────────────────


class TestAcquireRelease:
    def test_acquire_returns_handle_with_pid(self, tmp_path: Path) -> None:
        """First acquire on a fresh path writes our PID + returns a handle."""
        target = tmp_path / "server.lock"
        handle = acquire_watcher_lock(target)
        try:
            assert isinstance(handle, LockHandle)
            assert handle is not None
            assert handle.pid == os.getpid()
            assert handle.path == target
            # Metadata persisted: read_lock_holder finds our PID.
            assert read_lock_holder(target) == os.getpid()
        finally:
            release_watcher_lock(handle)

    def test_second_acquire_in_same_process_returns_none(self, tmp_path: Path) -> None:
        """Two acquires of the same lock from one process: second is None.

        The OS-level fcntl/msvcrt advisory lock is per-(fd, inode), so
        even within a single PID a second open + lock attempt sees the
        first FD's lock and refuses. This is the contract that makes
        the in-process test meaningful — without it, two watchers in
        the same process could still race.
        """
        target = tmp_path / "server.lock"
        first = acquire_watcher_lock(target)
        try:
            assert first is not None
            second = acquire_watcher_lock(target)
            assert second is None, (
                "Second acquire should return None while the first is held"
            )
        finally:
            release_watcher_lock(first)

    def test_release_frees_lock_for_reacquire(self, tmp_path: Path) -> None:
        """After release, the next acquire succeeds."""
        target = tmp_path / "server.lock"
        first = acquire_watcher_lock(target)
        assert first is not None
        release_watcher_lock(first)

        second = acquire_watcher_lock(target)
        try:
            assert second is not None, (
                "Re-acquire after release should succeed"
            )
            assert second.pid == os.getpid()
        finally:
            release_watcher_lock(second)

    def test_double_release_is_idempotent(self, tmp_path: Path) -> None:
        """Releasing twice is a no-op — the API contract for atexit."""
        target = tmp_path / "server.lock"
        handle = acquire_watcher_lock(target)
        assert handle is not None
        release_watcher_lock(handle)
        # Should not raise.
        release_watcher_lock(handle)
        assert handle.released is True

    def test_release_none_is_safe(self) -> None:
        """``release_watcher_lock(None)`` is the no-op path the lifespan
        relies on when no lock was ever acquired (e.g. ``--no-lock``)."""
        release_watcher_lock(None)


# ── stale-lock reclamation ────────────────────────────────────────────────────


class TestStaleReclamation:
    def test_stale_pid_is_reclaimed(self, tmp_path: Path) -> None:
        """A lock file whose recorded PID is dead → next acquire wins.

        Strategy: write a known-dead PID into the lock file (PID 999999
        — far beyond Linux's default 32768 max but unallocated on every
        platform we test on). ``acquire_watcher_lock`` should clear the
        stale metadata and acquire normally.

        Note: the OS-level flock isn't actually held in this case (the
        previous owner doesn't exist) so we *should* be able to take
        the lock; the only thing the stale detection does is clean the
        ``lock_held_by`` metadata so the status surface doesn't lie.
        """
        target = tmp_path / "server.lock"
        # Plant a dead PID. 999999 is beyond /proc/sys/kernel/pid_max
        # default on Linux and not allocated on macOS — best portable
        # approximation of "dead".
        target.write_text("999999\n2026-01-01T00:00:00+00:00\n")

        handle = acquire_watcher_lock(target)
        try:
            assert handle is not None
            assert handle.pid == os.getpid()
            # Metadata refreshed to our PID.
            assert read_lock_holder(target) == os.getpid()
        finally:
            release_watcher_lock(handle)

    def test_unparseable_metadata_is_handled(self, tmp_path: Path) -> None:
        """Garbage in the lock file doesn't crash acquire."""
        target = tmp_path / "server.lock"
        target.write_text("not-a-pid\n")

        handle = acquire_watcher_lock(target)
        try:
            assert handle is not None
        finally:
            release_watcher_lock(handle)


# ── context manager form ──────────────────────────────────────────────────────


class TestContextManager:
    def test_context_manager_releases_on_exit(self, tmp_path: Path) -> None:
        """``with watcher_lock(...)`` releases at the end of the block."""
        target = tmp_path / "server.lock"
        with watcher_lock(target) as handle:
            assert handle is not None
            assert handle.pid == os.getpid()
            # Inside the with: lock is held — second acquire is None.
            second = acquire_watcher_lock(target)
            assert second is None

        # After the with: lock is released — acquire succeeds.
        re = acquire_watcher_lock(target)
        try:
            assert re is not None
        finally:
            release_watcher_lock(re)

    def test_context_manager_releases_on_exception(self, tmp_path: Path) -> None:
        """An exception inside the ``with`` body must not leak the lock."""
        target = tmp_path / "server.lock"
        with pytest.raises(RuntimeError, match="boom"):
            with watcher_lock(target) as handle:
                assert handle is not None
                raise RuntimeError("boom")

        # Lock is free now even though the body raised.
        re = acquire_watcher_lock(target)
        try:
            assert re is not None
        finally:
            release_watcher_lock(re)


# ── read_lock_holder ──────────────────────────────────────────────────────────


class TestReadLockHolder:
    def test_returns_none_for_missing_file(self, tmp_path: Path) -> None:
        assert read_lock_holder(tmp_path / "nope.lock") is None

    def test_returns_none_for_empty_file(self, tmp_path: Path) -> None:
        target = tmp_path / "server.lock"
        target.write_text("")
        assert read_lock_holder(target) is None

    def test_parses_pid_from_first_line(self, tmp_path: Path) -> None:
        target = tmp_path / "server.lock"
        target.write_text("4242\n2026-04-01T12:00:00+00:00\n")
        assert read_lock_holder(target) == 4242

    def test_handles_pid_only_format(self, tmp_path: Path) -> None:
        """Backwards compat with a hypothetical PID-only writer."""
        target = tmp_path / "server.lock"
        target.write_text("4242")
        assert read_lock_holder(target) == 4242


# ── status surface ────────────────────────────────────────────────────────────


class TestStatusSurface:
    def test_status_reports_lock_holder_pid(self, tmp_path: Path, monkeypatch) -> None:
        """``assemble_status()['watcher']['lock_held_by']`` returns the PID
        recorded in the lock file."""
        from stackunderflow.etl import lock as lock_mod
        from stackunderflow.etl.status import assemble_status
        from stackunderflow.store import db, schema

        # Redirect the default lock path into tmp_path so the assembler
        # reads our test file rather than the user's real lock.
        target = tmp_path / "server.lock"
        monkeypatch.setattr(lock_mod, "DEFAULT_LOCK_PATH", target)

        # Apply schema so the assembler's mart probes don't crash.
        db_path = tmp_path / "store.db"
        conn = db.connect(db_path)
        schema.apply(conn)
        conn.close()

        # No lock yet → lock_held_by is None.
        conn = db.connect(db_path)
        try:
            payload = assemble_status(conn)
            assert payload["watcher"]["lock_held_by"] is None
        finally:
            conn.close()

        # Acquire the lock → lock_held_by becomes our PID.
        handle = acquire_watcher_lock(target)
        try:
            conn = db.connect(db_path)
            try:
                payload = assemble_status(conn)
                assert payload["watcher"]["lock_held_by"] == os.getpid()
            finally:
                conn.close()
        finally:
            release_watcher_lock(handle)


# ── lifespan simulation ───────────────────────────────────────────────────────


class TestLifespanSingleWatcher:
    """End-to-end-ish: simulate two ``server._lifespan`` startups against
    the same tmp_path lock and assert only the first spawns a watcher.

    We don't actually bring up FastAPI — we exercise the same acquire +
    spawn-or-skip branches the lifespan does, with the watcher itself
    mocked. This isolates the contract under test (single-watcher
    invariant) from FastAPI / uvicorn lifecycle noise.
    """

    def test_only_first_lifespan_spawns_watcher(self, tmp_path: Path) -> None:
        target = tmp_path / "server.lock"
        spawned: list[str] = []

        def _fake_start_watcher(*_args, **_kwargs):
            spawned.append("yes")

            class _Stub:
                class _T:
                    def is_alive(self) -> bool:
                        return True

                thread = _T()

                def stop(self, timeout: float = 5.0) -> None:
                    pass

            return _Stub()

        # Each "instance" mirrors what server._lifespan does for the
        # watcher: try to acquire the lock; if held, skip the spawn.
        def _instance() -> tuple[LockHandle | None, bool]:
            handle = acquire_watcher_lock(target)
            if handle is None:
                return None, False
            _fake_start_watcher()
            return handle, True

        first_handle, first_spawned = _instance()
        try:
            assert first_handle is not None
            assert first_spawned is True
            assert spawned == ["yes"]

            # Second instance — same lock path, lock is held.
            second_handle, second_spawned = _instance()
            assert second_handle is None
            assert second_spawned is False
            assert spawned == ["yes"], (
                "Second lifespan should not have spawned a watcher"
            )

            # Read holder: should still report the first instance's PID.
            assert read_lock_holder(target) == os.getpid()
        finally:
            release_watcher_lock(first_handle)

        # After release, a third instance can spawn.
        third_handle, third_spawned = _instance()
        try:
            assert third_handle is not None
            assert third_spawned is True
            assert spawned == ["yes", "yes"]
        finally:
            release_watcher_lock(third_handle)


# ── platform fallback ────────────────────────────────────────────────────────


class TestPlatformFallback:
    def test_acquire_when_fcntl_missing(self, tmp_path: Path, monkeypatch) -> None:
        """If neither fcntl (POSIX) nor msvcrt (Windows) loads, the
        module logs a warning and proceeds without an OS-level lock.

        We simulate the import failure by monkeypatching the module's
        platform check + intercepting the imports. Documents the gap
        rather than enforcing the invariant on exotic platforms.
        """
        # Force the win32 branch and pretend msvcrt is missing.
        from stackunderflow.etl import lock as lock_mod

        monkeypatch.setattr(lock_mod, "_PLATFORM", "win32")

        real_import = __builtins__["__import__"] if isinstance(__builtins__, dict) else __builtins__.__import__

        def _no_msvcrt(name, *a, **kw):
            if name == "msvcrt":
                raise ImportError("simulated")
            return real_import(name, *a, **kw)

        with mock.patch("builtins.__import__", side_effect=_no_msvcrt):
            target = tmp_path / "server.lock"
            handle = acquire_watcher_lock(target)
            try:
                # Best-effort: the function should have returned a handle
                # even though OS-level locking is unavailable. The user
                # gets the documented gap, not a crash.
                assert handle is not None
            finally:
                release_watcher_lock(handle)
