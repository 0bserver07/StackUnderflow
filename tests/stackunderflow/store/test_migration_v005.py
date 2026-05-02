"""v004 migration: redistribute legacy cursor sessions across workspaces.

These tests pin the migration's contract:

1. Sessions whose ``raw_json`` payloads contain workspace path data are
   reparented onto a fresh ``cursor`` project keyed on the derived slug.
2. Sessions with no path data stay under the legacy ``cursor`` row,
   which gets a flagged ``display_name`` so the dashboard can surface
   the limitation.
3. When *every* session is redistributed the legacy row is dropped.
4. The migration is idempotent — running it twice doesn't churn rows.
5. ``PRAGMA user_version`` is bumped to 4 on success and the migration
   transaction rolls back cleanly on failure.

Each test seeds a v003 schema (project + session + messages) and runs
``schema.apply`` to exercise the v004 step in the same loop the live
binary uses.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema


def _seed_v003(conn: sqlite3.Connection) -> None:
    """Apply v001 + v002 + v003 to *conn* but stop before v004."""
    migrations_dir = (
        Path(__file__).resolve().parents[3]
        / "stackunderflow" / "store" / "migrations"
    )
    for name in (
        "v001_initial.sql",
        "v002_ingest_log_multistore.sql",
        "v003_messages_speed.sql",
    ):
        sql = (migrations_dir / name).read_text()
        conn.executescript(sql)
    # PRAGMA user_version is set by v003's last statement; assert.
    assert conn.execute("PRAGMA user_version").fetchone()[0] == 3


def _bubble_payload(file_paths: list[str]) -> str:
    """Mimic the shape of a Cursor bubble JSON payload that the live
    adapter persists into ``messages.raw_json``."""
    return json.dumps({
        "_v": 3,
        "type": 1,
        "text": "hi",
        "context": {
            "fileSelections": [
                {"uri": {"fsPath": p, "path": p, "scheme": "file"}, "uuid": str(i)}
                for i, p in enumerate(file_paths)
            ],
        },
    })


def _seed_legacy_cursor_session(
    conn: sqlite3.Connection,
    *,
    session_id: str,
    file_paths: list[str] | None,
) -> None:
    """Insert a (legacy "cursor" project) → session → messages chain.

    Creates the legacy project row on first use; subsequent calls reuse
    it. ``file_paths=None`` produces a single empty payload so the
    session has no workspace evidence.
    """
    legacy = conn.execute(
        "SELECT id FROM projects WHERE provider = 'cursor' AND slug = 'cursor'"
    ).fetchone()
    if legacy is None:
        cur = conn.execute(
            "INSERT INTO projects (provider, slug, display_name, "
            "first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
            ("cursor", "cursor", "cursor", 1700000000.0, 1700000000.0),
        )
        proj_id = cur.lastrowid
    else:
        proj_id = legacy["id"]

    cur = conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (?, ?)",
        (proj_id, session_id),
    )
    sess_pk = cur.lastrowid

    payload = _bubble_payload(file_paths or [])
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
        "VALUES (?, ?, ?, ?, ?)",
        (sess_pk, 0, "2026-04-01T00:00:00+00:00", "user", payload),
    )


# ── tests ─────────────────────────────────────────────────────────────


def test_v004_redistributes_sessions_with_path_data(tmp_path: Path) -> None:
    """Two legacy sessions referencing two workspaces split into two
    distinct project rows; the legacy "cursor" row is dropped because
    every session was reparented."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _seed_v003(conn)
        _seed_legacy_cursor_session(
            conn,
            session_id="conv-alpha",
            file_paths=[
                "/Users/dev/projects/alpha/src/main.ts",
                "/Users/dev/projects/alpha/tests/test_main.ts",
            ],
        )
        _seed_legacy_cursor_session(
            conn,
            session_id="conv-beta",
            file_paths=[
                # Two distinct subdirectories so the deepest-shared
                # ancestor lands at the project root, not at a single
                # subfolder both files would share.
                "/Users/dev/projects/beta/lib/util.py",
                "/Users/dev/projects/beta/tests/test_util.py",
            ],
        )
        conn.commit()

        schema.apply(conn)

        slugs = sorted(
            r["slug"]
            for r in conn.execute(
                "SELECT slug FROM projects WHERE provider = 'cursor'"
            )
        )
        assert slugs == [
            "-Users-dev-projects-alpha",
            "-Users-dev-projects-beta",
        ]
        # Legacy collapse removed.
        assert "cursor" not in slugs
        assert conn.execute("PRAGMA user_version = 5
    finally:
        conn.close()


def test_v004_keeps_legacy_row_when_session_has_no_paths(tmp_path: Path) -> None:
    """Sessions with no path evidence stay under the legacy ``cursor``
    project, but the row gets a flagged display_name so the UI can warn
    the user."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _seed_v003(conn)
        _seed_legacy_cursor_session(
            conn, session_id="conv-noinfo", file_paths=None,
        )
        conn.commit()

        schema.apply(conn)

        row = conn.execute(
            "SELECT slug, display_name FROM projects "
            "WHERE provider = 'cursor' AND slug = 'cursor'"
        ).fetchone()
        assert row is not None
        assert "legacy" in row["display_name"].lower()
        assert "reingest" in row["display_name"].lower()
    finally:
        conn.close()


def test_v004_mixed_redistributes_and_keeps_legacy_for_residual(
    tmp_path: Path,
) -> None:
    """Mixed input → reparents what it can, keeps a renamed legacy row
    holding the residual session."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _seed_v003(conn)
        _seed_legacy_cursor_session(
            conn,
            session_id="conv-with-paths",
            file_paths=[
                "/Users/dev/projects/gamma/a.ts",
                "/Users/dev/projects/gamma/b.ts",
            ],
        )
        _seed_legacy_cursor_session(
            conn, session_id="conv-no-paths", file_paths=None,
        )
        conn.commit()

        schema.apply(conn)

        slugs = sorted(
            r["slug"]
            for r in conn.execute(
                "SELECT slug FROM projects WHERE provider = 'cursor'"
            )
        )
        assert "cursor" in slugs                    # residual still there
        assert "-Users-dev-projects-gamma" in slugs  # split out

        # The reparented session must point at the gamma project, not
        # the legacy row.
        gamma_id = conn.execute(
            "SELECT id FROM projects WHERE slug = '-Users-dev-projects-gamma'"
        ).fetchone()[0]
        residual_id = conn.execute(
            "SELECT id FROM projects WHERE slug = 'cursor'"
        ).fetchone()[0]
        assert conn.execute(
            "SELECT project_id FROM sessions WHERE session_id = 'conv-with-paths'"
        ).fetchone()[0] == gamma_id
        assert conn.execute(
            "SELECT project_id FROM sessions WHERE session_id = 'conv-no-paths'"
        ).fetchone()[0] == residual_id
    finally:
        conn.close()


def test_v004_is_idempotent(tmp_path: Path) -> None:
    """Running the migration twice (e.g. after a process restart) must
    not duplicate projects or move sessions a second time."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _seed_v003(conn)
        _seed_legacy_cursor_session(
            conn,
            session_id="conv-x",
            file_paths=[
                "/Users/dev/projects/iota/x.ts",
                "/Users/dev/projects/iota/y.ts",
            ],
        )
        conn.commit()

        schema.apply(conn)
        first_projects = list(conn.execute(
            "SELECT id, provider, slug FROM projects ORDER BY id"
        ))

        # Re-applying the chain must be a no-op.
        schema.apply(conn)
        second_projects = list(conn.execute(
            "SELECT id, provider, slug FROM projects ORDER BY id"
        ))

        # Compare by tuple shape so sqlite3.Row identity doesn't matter.
        assert [tuple(r) for r in first_projects] == [
            tuple(r) for r in second_projects
        ]
    finally:
        conn.close()


def test_v004_no_op_when_no_legacy_cursor_project(tmp_path: Path) -> None:
    """A fresh DB (no legacy cursor row) must still bump user_version
    without erroring."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version = 5
        # No cursor project should have appeared from thin air.
        cnt = conn.execute(
            "SELECT COUNT(*) FROM projects WHERE provider = 'cursor'"
        ).fetchone()[0]
        assert cnt == 0
    finally:
        conn.close()


def test_v004_preserves_messages_under_reparented_session(tmp_path: Path) -> None:
    """Messages stay attached to their session FK across reparenting —
    the migration only touches ``sessions.project_id`` and the
    ``projects`` table, never ``messages``."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _seed_v003(conn)
        _seed_legacy_cursor_session(
            conn,
            session_id="conv-preserve",
            file_paths=[
                "/Users/dev/projects/kappa/main.go",
                "/Users/dev/projects/kappa/util.go",
            ],
        )
        conn.commit()

        # Before migration: 1 message under conv-preserve.
        sess_pk = conn.execute(
            "SELECT id FROM sessions WHERE session_id = 'conv-preserve'"
        ).fetchone()[0]
        before = conn.execute(
            "SELECT COUNT(*) FROM messages WHERE session_fk = ?",
            (sess_pk,),
        ).fetchone()[0]
        assert before == 1

        schema.apply(conn)

        # After migration: same session, same message, new project.
        after = conn.execute(
            "SELECT COUNT(*) FROM messages WHERE session_fk = ?",
            (sess_pk,),
        ).fetchone()[0]
        assert after == 1
        new_project = conn.execute(
            "SELECT p.slug FROM sessions s "
            "JOIN projects p ON p.id = s.project_id "
            "WHERE s.id = ?",
            (sess_pk,),
        ).fetchone()[0]
        assert new_project == "-Users-dev-projects-kappa"
    finally:
        conn.close()
