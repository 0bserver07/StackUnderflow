"""Cross-test helpers.

``set_home_env`` patches the OS-specific env var that ``pathlib.Path.home()``
reads from. On POSIX that's ``HOME``; on Windows it's ``USERPROFILE``.
Tests that need to redirect ``Path.home()`` at a ``tmp_path`` should call
this helper instead of ``monkeypatch.setenv("HOME", ...)`` so they work
on both runners.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path


def set_home_env(monkeypatch, home: Path | str) -> None:
    """Redirect ``Path.home()`` to ``home`` on POSIX and Windows.

    POSIX honours ``HOME``; Windows honours ``USERPROFILE`` (with
    ``HOMEDRIVE`` + ``HOMEPATH`` as fallbacks). Setting both makes
    ``Path.home()`` and ``os.path.expanduser("~")`` both resolve to
    ``home`` regardless of platform.
    """
    home_str = str(home)
    monkeypatch.setenv("HOME", home_str)
    if sys.platform == "win32":
        monkeypatch.setenv("USERPROFILE", home_str)
        # HOMEDRIVE + HOMEPATH composed by some path helpers; keep them
        # consistent so nothing in the suite reaches around USERPROFILE.
        drive, tail = os.path.splitdrive(home_str)
        if drive:
            monkeypatch.setenv("HOMEDRIVE", drive)
            monkeypatch.setenv("HOMEPATH", tail or "\\")


def _norm_path(p: Path | str) -> str:
    """Normalise a path for cross-platform equality.

    ``os.path.normcase`` lowercases and flips separators on Windows (a
    no-op on POSIX); ``normpath`` collapses ``.``/``..``. Neither touches
    the filesystem, so this works on non-existent paths.
    """
    return os.path.normcase(os.path.normpath(str(p)))


def assert_same_path(actual: Path | str, expected: Path | str) -> None:
    """Assert two paths are logically equal regardless of separator/case.

    Use instead of ``assert Path(a) == Path(b)`` when either side may have
    been through ``.resolve()`` — which drive-prefixes and may re-case on
    Windows — or built from a forward-slash string literal. This is the
    lever for porting the POSIX-shaped path assertions flagged in
    ``test.yml`` to the Windows matrix.
    """
    assert _norm_path(actual) == _norm_path(expected), f"{actual!r} != {expected!r}"
