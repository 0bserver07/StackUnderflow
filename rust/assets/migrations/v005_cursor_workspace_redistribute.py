"""v004: redistribute legacy cursor sessions across per-workspace projects.

Background
----------
Before v0.6.1 the cursor adapter stamped every conversation with a fixed
``project_slug = "cursor"``. That collapsed every workspace the user ever
opened in Cursor into a single project row, hiding workspace context in
the dashboard. v0.6.1 derives a real per-workspace slug from the absolute
file paths each conversation references (see
``stackunderflow.adapters.cursor._workspace_slug_for_conversation``).

Without a migration, the new logic only takes effect when a session is
*re-ingested*; ``ingest_log`` keys cursor entries on
``(file_path, session_id)``, so a vscdb whose mtime/size haven't changed
is skipped, and the legacy "cursor" project lingers.

What this migration does
------------------------
For every existing cursor session it:

1. Reads the session's stored ``raw_json`` payloads from the ``messages``
   table.
2. Re-derives the workspace slug from those payloads using the same
   helpers the live adapter uses, so the migration result and a future
   re-ingest agree.
3. Reparents the session row onto the appropriate ``projects`` row
   (creating that row when needed).
4. Drops the now-empty legacy "cursor" project row when no sessions
   remain. If at least one session could not be redistributed (no path
   data — typical for short, model-only chats), the legacy row is kept
   and its ``display_name`` is updated to flag the situation.

The migration is idempotent: re-running it after success is a no-op
because every cursor session already lives under a non-"cursor" slug.

Determinism
-----------
The slug-derivation helpers are imported lazily so a future change to
adapter internals does not silently shift migration semantics. We also
catch import / parse errors and degrade gracefully — a partial
redistribute is still a strict improvement over the legacy collapse.
"""

from __future__ import annotations

import json
import logging
import sqlite3

_log = logging.getLogger(__name__)

_LEGACY_SLUG = "cursor"
_LEGACY_DISPLAY_NAME = "cursor (legacy — reingest to split by workspace)"


def apply(conn: sqlite3.Connection) -> None:
    """Run the redistribute pass.

    Called inside a transaction managed by ``schema._run_python_migration``
    so any exception leaves the DB on the previous user_version.
    """
    legacy = conn.execute(
        "SELECT id FROM projects WHERE provider = 'cursor' AND slug = ?",
        (_LEGACY_SLUG,),
    ).fetchone()
    if legacy is None:
        # No legacy collapse to fix — fresh DB or already migrated.
        return
    legacy_id = legacy["id"] if hasattr(legacy, "keys") else legacy[0]

    # Late import: the adapter is part of the same package, but importing
    # it at module load time would create a hard dependency from the
    # store layer onto an adapter, which we'd rather avoid.
    from stackunderflow.adapters import cursor as cursor_adapter

    sessions = conn.execute(
        "SELECT id, session_id FROM sessions WHERE project_id = ?",
        (legacy_id,),
    ).fetchall()

    unresolved = 0
    moved = 0
    for srow in sessions:
        sess_pk = srow["id"]
        slug = _derive_slug_for_session(conn, sess_pk, cursor_adapter)
        if slug is None or slug == _LEGACY_SLUG:
            unresolved += 1
            continue

        target_id = _ensure_project(conn, slug)
        # Reparent the session.
        conn.execute(
            "UPDATE sessions SET project_id = ? WHERE id = ?",
            (target_id, sess_pk),
        )
        moved += 1

    # Clean up: if every session was redistributed, drop the legacy row;
    # otherwise rename it so it's clearly tagged in the dashboard.
    remaining = conn.execute(
        "SELECT COUNT(*) FROM sessions WHERE project_id = ?",
        (legacy_id,),
    ).fetchone()[0]
    if remaining == 0:
        conn.execute("DELETE FROM projects WHERE id = ?", (legacy_id,))
    else:
        conn.execute(
            "UPDATE projects SET display_name = ? WHERE id = ?",
            (_LEGACY_DISPLAY_NAME, legacy_id),
        )

    _log.info(
        "v004 cursor redistribute: moved=%d unresolved=%d remaining=%d",
        moved, unresolved, remaining,
    )


def _derive_slug_for_session(
    conn: sqlite3.Connection,
    session_fk: int,
    cursor_adapter,
) -> str | None:
    """Return the new slug for one session by replaying the adapter's
    workspace-slug rule against the persisted ``raw_json`` payloads."""
    rows = conn.execute(
        "SELECT raw_json FROM messages WHERE session_fk = ?",
        (session_fk,),
    ).fetchall()
    paths: list[str] = []
    for r in rows:
        raw = r["raw_json"] if hasattr(r, "keys") else r[0]
        if not raw:
            continue
        try:
            payload = json.loads(raw)
        except (TypeError, ValueError):
            continue
        if not isinstance(payload, dict):
            continue
        try:
            paths.extend(cursor_adapter._paths_in_bubble(payload))
        except Exception as exc:  # pragma: no cover - defensive
            _log.debug("paths_in_bubble failed: %s", exc)

    if not paths:
        return None
    try:
        root = cursor_adapter._derive_workspace_root(paths)
    except Exception as exc:  # pragma: no cover - defensive
        _log.debug("derive_workspace_root failed: %s", exc)
        return None
    if root is None:
        return None
    try:
        return cursor_adapter._slug_for(root)
    except Exception as exc:  # pragma: no cover - defensive
        _log.debug("slug_for failed: %s", exc)
        return None


def _ensure_project(conn: sqlite3.Connection, slug: str) -> int:
    """Return the project_id for (provider='cursor', slug=*slug*).

    Creates the row when missing, copying ``first_seen`` / ``last_modified``
    from the legacy row's earliest / latest session timestamp so the new
    project row has a sensible recency signal.
    """
    row = conn.execute(
        "SELECT id FROM projects WHERE provider = 'cursor' AND slug = ?",
        (slug,),
    ).fetchone()
    if row is not None:
        return row["id"] if hasattr(row, "keys") else row[0]

    # Borrow timestamps from the legacy row so the new project row has
    # a plausible first_seen / last_modified rather than 0.
    legacy = conn.execute(
        "SELECT first_seen, last_modified FROM projects "
        "WHERE provider = 'cursor' AND slug = ?",
        (_LEGACY_SLUG,),
    ).fetchone()
    if legacy is not None:
        first_seen = legacy["first_seen"] if hasattr(legacy, "keys") else legacy[0]
        last_modified = legacy["last_modified"] if hasattr(legacy, "keys") else legacy[1]
    else:
        first_seen = 0.0
        last_modified = 0.0

    cur = conn.execute(
        "INSERT INTO projects "
        "(provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES ('cursor', ?, ?, ?, ?, ?)",
        (slug, None, slug, first_seen, last_modified),
    )
    assert cur.lastrowid is not None  # noqa: S101
    return cur.lastrowid
