"""Public Python API — store-backed, multi-provider.

These helpers are the canonical entry points for using StackUnderflow as
a library. They open the local SQLite store at ``~/.stackunderflow/store.db``
read-only, query through ``store.queries``, and return plain dicts so
callers don't have to import any internal types.

If the store doesn't exist yet (fresh install, no ingest), the helpers
degrade silently: ``list_projects()`` returns ``[]``, while ``process()``
raises ``KeyError`` for the missing slug — see each function's docstring.

This module is the package entry point — re-exported by
``stackunderflow/__init__.py`` so ``stackunderflow.list_projects()`` and
``stackunderflow.process()`` work directly. Internal helpers under
``stackunderflow.api.messages`` (HTTP-side pagination) remain importable
unchanged.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.store import db, queries

__all__ = ["list_projects", "list_sessions", "process"]


def _store_path() -> Path:
    """Resolve the store path lazily so tests can monkeypatch ``deps.store_path``.

    Imported at call time (not module load) because ``deps`` pulls in
    ``Settings`` and the route services type-hints; importing it eagerly
    here would create an import cycle for callers that ``import stackunderflow``
    before the server is wired up.
    """
    from stackunderflow import deps

    return deps.store_path


def _open_readonly(path: Path) -> sqlite3.Connection | None:
    """Return a connection or ``None`` if the store file is missing.

    The connection is opened with the project's standard PRAGMAs via
    ``db.connect``; we don't bind ``mode=ro`` on the URI because the
    callers here only read, and the standard connection is what every
    other read path in the codebase uses (consistent error surface).
    """
    if not path.is_file():
        return None
    return db.connect(path)


def list_projects(provider: str | None = None) -> list[dict]:
    """Return all projects in the local store, optionally filtered by provider.

    Each row is a dict with these keys::

        {
            "slug":          str,           # canonical project slug
            "provider":      str,           # "claude", "codex", "cursor", ...
            "display_name":  str,
            "path":          str | None,    # original log directory, if known
            "first_seen":    float,         # epoch seconds
            "last_modified": float,         # epoch seconds
        }

    Returns ``[]`` if the store does not exist (fresh install, no ingest
    has run yet) or if the filter excludes every row.
    """
    conn = _open_readonly(_store_path())
    if conn is None:
        return []
    try:
        rows = queries.list_projects(conn)
    finally:
        conn.close()
    out = [
        {
            "slug": p.slug,
            "provider": p.provider,
            "display_name": p.display_name,
            "path": p.path,
            "first_seen": p.first_seen,
            "last_modified": p.last_modified,
        }
        for p in rows
    ]
    if provider is not None:
        out = [r for r in out if r["provider"] == provider]
    return out


def list_sessions(project_slug: str, provider: str | None = None) -> list[dict]:
    """Return sessions for a project as a list of dicts.

    ``provider`` disambiguates when the same slug exists for multiple
    providers (the store enforces ``UNIQUE(provider, slug)`` so this is
    rare but possible).

    Each row::

        {"session_id": str, "first_ts": str | None,
         "last_ts": str | None, "message_count": int}

    Raises ``KeyError(project_slug)`` if the project is not in the store.
    """
    conn = _open_readonly(_store_path())
    if conn is None:
        raise KeyError(project_slug)
    try:
        project = _resolve_project(conn, project_slug, provider)
        sessions = queries.list_sessions(conn, project_id=project.id)
    finally:
        conn.close()
    return [
        {
            "session_id": s.session_id,
            "first_ts": s.first_ts,
            "last_ts": s.last_ts,
            "message_count": s.message_count,
        }
        for s in sessions
    ]


def process(
    project_slug: str,
    provider: str | None = None,
) -> tuple[list[dict], dict]:
    """Return ``(messages, stats)`` for a project from the store.

    Resolves ``project_slug`` (+ optional ``provider``) to a project id
    via ``store.queries.get_project`` then runs
    ``queries.get_project_stats`` — the same pipeline the dashboard uses.
    Returned ``messages`` is the formatter-shaped list and ``stats`` is
    the aggregator-shaped dict (with ``overview``, ``sessions``, ``cost``,
    ``tools``, etc.).

    ``provider`` disambiguates when the same slug exists for multiple
    providers (the store enforces ``UNIQUE(provider, slug)``, so the same
    slug can legitimately appear once per provider).

    Raises ``KeyError(project_slug)`` if the project is not in the store
    (or the store does not exist yet — that's still a "not found" case
    from the caller's point of view).
    """
    conn = _open_readonly(_store_path())
    if conn is None:
        raise KeyError(project_slug)
    try:
        project = _resolve_project(conn, project_slug, provider)
        return queries.get_project_stats(conn, project_id=project.id)
    finally:
        conn.close()


def _resolve_project(
    conn: sqlite3.Connection,
    slug: str,
    provider: str | None,
):
    """Find a project row by slug, optionally constrained to a provider.

    ``queries.get_project`` matches on slug only — it picks the first row
    if multiple providers have the same slug. When the caller supplies a
    provider we filter the full ``list_projects`` output ourselves so the
    constraint is honoured, then raise ``KeyError`` if no row matches.
    """
    if provider is None:
        row = queries.get_project(conn, slug=slug)
        if row is None:
            raise KeyError(slug)
        return row
    for p in queries.list_projects(conn):
        if p.slug == slug and p.provider == provider:
            return p
    raise KeyError(slug)
