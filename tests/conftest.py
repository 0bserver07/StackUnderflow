"""Cross-test helpers.

``set_home_env`` patches the OS-specific env var that ``pathlib.Path.home()``
reads from. On POSIX that's ``HOME``; on Windows it's ``USERPROFILE``.
Tests that need to redirect ``Path.home()`` at a ``tmp_path`` should call
this helper instead of ``monkeypatch.setenv("HOME", ...)`` so they work
on both runners.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from stackunderflow.etl import marts as _marts
from stackunderflow.etl import normalize as _normalize

# Snapshot the import-time ETL registries ONCE, before any test runs. The mart
# and normalize registries are populated purely as import side-effects (no
# loader fn), so once a test calls their ``_clear()`` the only way back is to
# re-apply these defaults. Capturing here — at conftest import, while the
# registries are still pristine — makes the restore in ``_restore_etl_registries``
# order-independent (it never snapshots a state a polluting fixture already
# cleared).
_MART_DEFAULTS = _marts.all()
_NORMALIZE_DEFAULTS = _normalize.all()


@pytest.fixture(autouse=True)
def _restore_etl_registries():
    """Stop a test's ``marts._clear()`` / ``normalize._clear()`` from leaking.

    Several ingest/etl tests clear these process-global registries to exercise
    dispatch in isolation, re-register a subset, and leave them cleared at
    teardown. That silently poisons every later test relying on the import-time
    defaults — e.g. the multi-provider mart fast-path test calls
    ``refresh_all_marts``, which iterates the mart registry and materialises
    nothing if it's empty, so its ``project_mart`` rows never appear and the
    route falls through to the (patched-to-explode) full pipeline. Restoring the
    known-good defaults after every test confines a ``_clear()`` to its owner.
    """
    yield
    _marts._REGISTRY.clear()
    _marts._REGISTRY.update(_MART_DEFAULTS)
    _normalize._REGISTRY.clear()
    _normalize._REGISTRY.update(_NORMALIZE_DEFAULTS)


def set_home_env(monkeypatch, home: Path | str) -> None:
    """Redirect ``Path.home()`` to ``home`` on POSIX and Windows.

    POSIX honours ``HOME``; Windows honours ``USERPROFILE`` (with
    ``HOMEDRIVE`` + ``HOMEPATH`` as fallbacks). Setting both makes
    ``Path.home()`` and ``os.path.expanduser("~")`` both resolve to
    ``home`` regardless of platform.
    """
    home_str = str(home)
    monkeypatch.setenv("HOME", home_str)
    # Set the Windows vars UNCONDITIONALLY — not gated on ``sys.platform``.
    # Callers commonly monkeypatch ``sys.platform`` to ``"linux"``/``"darwin"``
    # *before* calling this (to exercise an adapter's non-Windows branch); on a
    # real Windows runner that means ``sys.platform`` is no longer ``"win32"``
    # here, so a gated set would skip ``USERPROFILE`` and ``Path.home()`` would
    # resolve to the real host home instead of ``home``. The vars are inert on
    # POSIX (``Path.home()`` reads ``HOME`` there), so always setting them is safe.
    monkeypatch.setenv("USERPROFILE", home_str)
    # HOMEDRIVE + HOMEPATH composed by some path helpers; keep them
    # consistent so nothing in the suite reaches around USERPROFILE.
    # ``splitdrive`` returns an empty drive on POSIX, so the guard below
    # naturally no-ops there.
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


def app_route_paths(app) -> set[str]:
    """Every path a FastAPI app serves — robust across FastAPI/Starlette versions.

    FastAPI >=0.138 (Starlette 1.x) stopped flattening ``include_router`` routes
    into ``app.routes``; they now sit behind a lazy ``_IncludedRouter`` proxy, so
    a plain ``{r.path for r in app.routes}`` scan still finds the directly
    ``@app.get``-decorated routes but misses every ``/api/*`` route (requests
    still resolve — only introspection is affected). Union the flat ``app.routes``
    paths (which carry the ``{param:path}`` converter form) with the OpenAPI
    ``paths`` (which expands the lazy routers) so route-registration assertions
    hold on both the old and new FastAPI.
    """
    paths = {p for r in app.routes if isinstance(p := getattr(r, "path", None), str)}
    try:
        paths |= set(app.openapi().get("paths", {}))
    except Exception:  # pragma: no cover - openapi build is best-effort here
        pass
    return paths
