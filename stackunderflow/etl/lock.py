"""Single-watcher invariant via an OS-level file lock.

Two ``stackunderflow start`` invocations against the same store would
otherwise spin up two filesystem watchers — both racing to ingest the
same JSONL appends, both refreshing marts. The right answer is to fence
the watcher behind a process-exclusive lock at
``~/.stackunderflow/server.lock`` so only the first instance runs the
watcher; subsequent instances still serve HTTP (the dashboard reads
from the store and is happy without a watcher) but skip the watcher
spawn.

Design
------

* **POSIX**: ``fcntl.flock(LOCK_EX | LOCK_NB)``. Released automatically
  when the file descriptor closes (process exit covers the abnormal
  case). The ``flock`` advisory lock is per-(file-descriptor, inode);
  ``LOCK_NB`` makes it non-blocking so a second invocation gets a clean
  ``BlockingIOError`` instead of waiting forever.
* **Windows**: ``msvcrt.locking(LK_NBLCK)`` against the first byte of
  the file. Different mechanism than ``fcntl`` but the same semantics
  (advisory, exclusive, released on close). When ``msvcrt`` is missing
  for some reason — e.g. the test env mocks the platform — we log and
  proceed without locking; the gap is documented at module level.
* **Stale handling**: if the lock file exists but contains a PID that
  no longer maps to a live process, ``acquire_watcher_lock`` reclaims
  it. We never trust the *content* of the lock file as a primary signal
  (advisory locks don't need it); the PID is purely informational —
  surfaced in the status route's ``watcher.lock_held_by`` so a user can
  see which instance owns the watcher. The OS-level ``flock`` itself is
  the actual gate. If the OS still considers the lock held by a dead
  process (rare — mostly a kernel-internal cleanup race), we surface
  that as "lock held" and let the admin restart cleanly.

Returns ``LockHandle | None``. ``None`` means another live process
already holds the lock; the caller should serve HTTP without spawning
the watcher and surface the held-by PID via ``read_lock_holder()``.
"""

from __future__ import annotations

import atexit
import logging
import os
import sys
import threading
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO
from stackunderflow.settings import app_dir

_log = logging.getLogger(__name__)

# Default lock file path. Tests inject a tmp_path-rooted alternative.
DEFAULT_LOCK_PATH: Path = app_dir() / "server.lock"

# Read once via a module-level alias so Pyright doesn't narrow
# ``sys.platform`` to a Literal and flag the cross-platform branches as
# unreachable. The runtime value is identical; the type is just ``str``.
_PLATFORM: str = sys.platform


@dataclass
class LockHandle:
    """Live handle for an acquired watcher lock.

    Holding a reference keeps the underlying file descriptor open, which
    is what keeps the OS-level advisory lock alive. Calling
    :func:`release_watcher_lock` (or letting the handle go out of scope
    on process exit) releases the lock and removes the PID/start_ts
    metadata file.
    """

    path: Path
    pid: int
    fh: IO[str] | None = None
    _released: bool = field(default=False, init=False, repr=False)
    _lock: threading.Lock = field(default_factory=threading.Lock, init=False, repr=False)

    @property
    def released(self) -> bool:
        return self._released


def _is_pid_alive(pid: int) -> bool:
    """Return True iff *pid* refers to a live process on this host.

    POSIX: ``os.kill(pid, 0)`` raises ``ProcessLookupError`` if no such
    process exists, ``PermissionError`` if it exists but we can't signal
    it (still alive, owned by another user — counts as alive). On
    Windows we fall back to a best-effort check; the lock fence is the
    OS file lock so a wrong answer here only affects the informational
    ``lock_held_by`` field.
    """
    if pid <= 0:
        return False
    if _PLATFORM == "win32":
        try:
            import ctypes  # type: ignore[import-untyped]

            # Win32 ``PROCESS_QUERY_INFORMATION`` flag — kept lowercase
            # so ruff's N806 (function-locals lowercase) is happy.
            process_query_information = 0x0400
            kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
            handle = kernel32.OpenProcess(process_query_information, False, pid)
            if not handle:
                return False
            kernel32.CloseHandle(handle)
            return True
        except Exception:  # noqa: BLE001 — best-effort
            return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


def read_lock_holder(path: Path | None = None) -> int | None:
    """Return the PID recorded in *path* (if any), or ``None``.

    Best-effort — used for the status surface so the dashboard can show
    "watcher held by PID N". Caller should treat a non-``None`` return
    value as advisory: the OS-level ``flock`` is the source of truth
    for whether the lock is actually held.
    """
    target = Path(path) if path else DEFAULT_LOCK_PATH
    try:
        text = target.read_text().strip()
    except (OSError, FileNotFoundError):
        return None
    if not text:
        return None
    # Format: ``<pid>\n<start_ts>``. Older versions wrote PID-only;
    # accept both.
    first = text.splitlines()[0].strip()
    try:
        return int(first)
    except ValueError:
        return None


def _try_acquire_os_lock(fh: IO[str]) -> bool:
    """Attempt the OS-level non-blocking exclusive lock. Returns True on success."""
    if _PLATFORM == "win32":
        try:
            import msvcrt  # type: ignore[import-not-found]
        except ImportError:
            _log.warning(
                "etl.lock: msvcrt unavailable; concurrent-watcher invariant "
                "not enforced on this platform"
            )
            # Treat as "acquired" — we can't actually lock, but failing
            # closed would prevent any watcher from ever starting on a
            # Windows box without msvcrt (vanishingly rare but worth
            # being explicit). Best effort.
            return True
        try:
            # Lock the first byte. ``LK_NBLCK`` is non-blocking so a
            # second invocation raises immediately instead of blocking.
            msvcrt.locking(fh.fileno(), msvcrt.LK_NBLCK, 1)
            return True
        except OSError:
            return False
    try:
        import fcntl
    except ImportError:
        _log.warning(
            "etl.lock: fcntl unavailable on %s; concurrent-watcher invariant "
            "not enforced",
            sys.platform,
        )
        return True
    try:
        fcntl.flock(fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except (OSError, BlockingIOError):
        return False


def _release_os_lock(fh: IO[str]) -> None:
    """Release the OS-level lock. Idempotent / best-effort."""
    if _PLATFORM == "win32":
        try:
            import msvcrt  # type: ignore[import-not-found]
            try:
                # Rewind to the byte we locked before unlocking.
                fh.seek(0)
                msvcrt.locking(fh.fileno(), msvcrt.LK_UNLCK, 1)
            except OSError:
                pass
        except ImportError:
            return
        return
    try:
        import fcntl
        try:
            fcntl.flock(fh.fileno(), fcntl.LOCK_UN)
        except OSError:
            pass
    except ImportError:
        return


def acquire_watcher_lock(path: Path | None = None) -> LockHandle | None:
    """Try to acquire the watcher singleton lock.

    Parameters
    ----------
    path:
        Override the default ``~/.stackunderflow/server.lock`` location.
        Tests pass ``tmp_path / "server.lock"`` to keep the real lock
        untouched.

    Returns
    -------
    LockHandle
        On success, a handle the caller must keep referenced for the
        lifetime of the watcher. Pass to :func:`release_watcher_lock`
        on shutdown (or rely on process exit to drop the FD).
    None
        Another live process already holds the lock. The caller should
        log a warning and continue without starting a watcher.
    """
    target = Path(path) if path else DEFAULT_LOCK_PATH
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        _log.warning("etl.lock: could not create %s parent: %s", target, exc)
        return None

    # Stale-detect: if the file exists but its recorded PID is dead,
    # truncate so we don't carry forward misleading "lock_held_by"
    # metadata. The OS-level ``flock`` may still be held in some kernel
    # corner cases — those will fall through to the acquire attempt and
    # we'll honour the actual answer the kernel gives us.
    existing_pid = read_lock_holder(target)
    if existing_pid is not None and not _is_pid_alive(existing_pid):
        try:
            target.write_text("")
            _log.info(
                "etl.lock: cleared stale PID %d from %s",
                existing_pid, target,
            )
        except OSError:
            pass

    # Open r+ if exists, else create. ``a+`` would seek to end on
    # writes, complicating the truncate; ``r+`` requires the file to
    # exist; so explicit two-phase open is the safest path.
    try:
        if not target.exists():
            target.touch()
        fh = open(target, "r+")  # noqa: SIM115 — kept open for lock duration
    except OSError as exc:
        _log.warning("etl.lock: could not open %s: %s", target, exc)
        return None

    if not _try_acquire_os_lock(fh):
        try:
            fh.close()
        except OSError:
            pass
        return None

    pid = os.getpid()
    try:
        from datetime import UTC, datetime
        start_ts = datetime.now(UTC).isoformat()
        fh.seek(0)
        fh.truncate()
        fh.write(f"{pid}\n{start_ts}\n")
        fh.flush()
    except OSError as exc:
        # Lock is acquired but we couldn't record metadata — surface a
        # warning and keep the lock. The status route will report
        # ``lock_held_by: None`` until next start, which is still better
        # than failing to fence two watchers.
        _log.warning("etl.lock: could not write metadata to %s: %s", target, exc)

    handle = LockHandle(path=target, pid=pid, fh=fh)

    # atexit fallback: even when the FastAPI lifespan shutdown doesn't
    # run (Ctrl+C interrupts uvicorn before lifespan cleanup, OOM kill,
    # etc.) we want the lock metadata cleared. The OS-level flock is
    # released by the kernel on FD close; this hook just keeps the
    # ``lock_held_by`` field honest for the next ``stackunderflow start``.
    atexit.register(release_watcher_lock, handle)

    _log.info("etl.lock: acquired watcher lock at %s (pid=%d)", target, pid)
    return handle


def release_watcher_lock(handle: LockHandle | None) -> None:
    """Release a previously-acquired lock. Safe to call with ``None`` or twice.

    Removes the metadata file on success so a subsequent
    ``read_lock_holder`` returns ``None``. The OS-level lock is
    released by closing the FD; on POSIX the kernel does this
    automatically when the process exits, so even if the caller skips
    the explicit release, two parallel ``stackunderflow start``
    invocations cannot race.
    """
    if handle is None:
        return
    with handle._lock:  # noqa: SLF001 — module-private synchroniser
        if handle._released:
            return
        handle._released = True
        fh = handle.fh
        handle.fh = None
    if fh is None:
        return
    _release_os_lock(fh)
    try:
        fh.close()
    except OSError:
        pass
    # Best-effort clean of metadata. We *don't* delete the file itself
    # — leaving it in place keeps the inode stable across restarts so
    # any external monitor watching the path doesn't see file churn.
    try:
        handle.path.write_text("")
    except OSError:
        pass


@contextmanager
def watcher_lock(path: Path | None = None) -> Iterator[LockHandle | None]:
    """Context manager wrapping :func:`acquire_watcher_lock`.

    Yields the handle (or ``None`` if the lock is held). Always releases
    on exit, even if the body raised — so a watcher startup crash inside
    the ``with`` block doesn't leak the lock.
    """
    handle = acquire_watcher_lock(path)
    try:
        yield handle
    finally:
        release_watcher_lock(handle)


__all__ = [
    "LockHandle",
    "DEFAULT_LOCK_PATH",
    "acquire_watcher_lock",
    "release_watcher_lock",
    "read_lock_holder",
    "watcher_lock",
]
