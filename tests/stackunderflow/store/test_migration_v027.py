"""v027 migration: ``projects.worktree_of`` (worktree fragment attribution).

Parallel-agent worktrees fragment analytics into phantom sibling projects
(``<parent-slug>--worktrees-<name>`` / ``<parent-slug>--claude-worktrees-<name>``).
v027 adds one nullable TEXT column, ``projects.worktree_of``, holding the
PARENT project slug on such fragment rows; NULL = a normal project. These
tests pin its guarantees:

  1. ``projects.worktree_of`` exists, TEXT, nullable, no default (NULL).
  2. ``schema.apply`` bumps ``PRAGMA user_version`` to the current head (>=27).
  3. INSERTs that omit the column land NULL (the "normal project" state), and
     the column is writable (``attribute_fragments`` stamps it later).
  4. The migration applies cleanly on a GENUINE v26 store (the real migration
     chain run up to v026 first) and preserves existing rows.
  5. The loader is reentrant — column already present but ``user_version``
     behind (partial prior run) recovers via the ``_ADD_COLUMN_GUARDS`` entry
     instead of erroring on "duplicate column".
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

from stackunderflow.store import db, schema


def _column_info(conn, table: str, column: str) -> dict | None:
    for r in conn.execute(f"PRAGMA table_info({table})").fetchall():
        if r["name"] == column:
            return {"type": r["type"], "notnull": r["notnull"], "dflt_value": r["dflt_value"]}
    return None


def _apply_up_to(conn: sqlite3.Connection, target: int) -> None:
    """Run the REAL migration chain, stopping after *target*.

    Builds a genuine historical store (not a rewound head-version one) so the
    v26 → v27 step is exercised exactly as it will run on user machines.
    Mirrors ``schema.apply``'s dispatch: ``.sql`` scripts bump
    ``user_version`` themselves; ``.py`` migrations go through the runner.
    """
    for version, path in schema._discover():
        if version > target:
            break
        if path.suffix == ".sql":
            conn.executescript(path.read_text())
        else:
            schema._run_python_migration(conn, version, path)


def _seed_project(conn) -> int:
    conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'alpha', '/alpha', 'Alpha', 0, 0)"
    )
    return conn.execute("SELECT id FROM projects").fetchone()[0]


def test_v027_adds_worktree_of_column(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        info = _column_info(conn, "projects", "worktree_of")
        assert info is not None, "projects.worktree_of missing after migration"
        assert info["type"].upper() == "TEXT"
        assert info["notnull"] == 0  # nullable — NULL means "normal project"
        assert info["dflt_value"] is None
    finally:
        conn.close()


def test_v027_user_version_bumped(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 27
    finally:
        conn.close()


def test_v027_insert_without_column_defaults_null_and_is_writable(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        pid = _seed_project(conn)
        row = conn.execute("SELECT worktree_of FROM projects WHERE id = ?", (pid,)).fetchone()
        assert row["worktree_of"] is None  # normal project until attributed
        conn.execute(
            "UPDATE projects SET worktree_of = '-Users-x-app' WHERE id = ?", (pid,)
        )
        row = conn.execute("SELECT worktree_of FROM projects WHERE id = ?", (pid,)).fetchone()
        assert row["worktree_of"] == "-Users-x-app"
    finally:
        conn.close()


def test_v027_applies_on_a_genuine_v26_store(tmp_path: Path) -> None:
    """Real chain to v026, seed a row, then apply → v27 with the row intact."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _apply_up_to(conn, 26)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 26
        assert _column_info(conn, "projects", "worktree_of") is None
        pid = _seed_project(conn)
        conn.commit()

        schema.apply(conn)  # v26 → head

        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        row = conn.execute(
            "SELECT provider, slug, worktree_of FROM projects WHERE id = ?", (pid,)
        ).fetchone()
        assert row["provider"] == "claude"
        assert row["slug"] == "alpha"
        assert row["worktree_of"] is None  # additive: existing rows untouched
    finally:
        conn.close()


def test_v027_reentrant_when_column_already_exists(tmp_path: Path) -> None:
    """Partial-application recovery: column present but ``user_version`` behind."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)  # full chain → column now exists
        conn.execute("PRAGMA user_version = 26")  # rewind so v027 looks pending
        conn.commit()
        schema.apply(conn)  # must not raise "duplicate column name"
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert _column_info(conn, "projects", "worktree_of") is not None
    finally:
        conn.close()


def test_v027_idempotent_reapply(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        cols = [r["name"] for r in conn.execute("PRAGMA table_info(projects)").fetchall()]
        assert cols.count("worktree_of") == 1
    finally:
        conn.close()
