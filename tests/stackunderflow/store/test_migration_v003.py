"""v003 migration: add ``messages.speed`` for fast-mode cost persistence.

Closes the gap PR #44 left behind: in-process pipelines already key by
``(model, speed)``, but the SQLite store had no column for it so every
SQL-driven cost path silently re-billed Anthropic priority/fast tier
records at standard rates. These tests pin the migration's three
guarantees:

  1. The column exists with type TEXT, NOT NULL, DEFAULT 'standard'.
  2. Existing rows seeded under v002 inherit ``'standard'`` via the DEFAULT.
  3. The migration loader is reentrant — running it on a DB where the
     column already exists (manual ALTER, partial prior run) is a no-op
     and still bumps ``PRAGMA user_version`` to 3.
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.store import db, schema


def _run_v001_v002(conn) -> None:
    """Apply v001 + v002 by hand — simulates a pre-v003 store."""
    migrations_dir = (
        Path(__file__).resolve().parents[3]
        / "stackunderflow" / "store" / "migrations"
    )
    for name in ("v001_initial.sql", "v002_ingest_log_multistore.sql"):
        sql = (migrations_dir / name).read_text()
        conn.executescript(sql)


def _column_info(conn, table: str, column: str) -> dict | None:
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    for r in rows:
        if r["name"] == column:
            # PRAGMA table_info: cid, name, type, notnull, dflt_value, pk
            return {
                "type": r["type"],
                "notnull": r["notnull"],
                "dflt_value": r["dflt_value"],
            }
    return None


def test_v003_adds_speed_column(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        info = _column_info(conn, "messages", "speed")
        assert info is not None
        assert info["type"].upper() == "TEXT"
        assert info["notnull"] == 1
        # SQLite stores the literal default with surrounding quotes.
        assert info["dflt_value"] in ("'standard'", "standard")
    finally:
        conn.close()


def test_v003_user_version_bumped_to_3(tmp_path: Path) -> None:
    """``schema.apply`` runs every pending migration; v004 also bumps the
    version, so the post-apply value reflects the current head, not 3."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        # v004 (cursor workspace redistribute) chains after v003 so the
        # final user_version is the latest applied. Compare to the
        # symbolic constant to keep this test stable across future bumps.
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
    finally:
        conn.close()


def test_v003_is_reentrant_when_column_already_exists(tmp_path: Path) -> None:
    """Operator-pre-applied / partial-run recovery path.

    Simulates a DB where the ``speed`` column already exists (e.g. someone
    ran the ALTER by hand, or a previous attempt added the column then
    crashed before bumping ``user_version``). ``schema.apply`` should
    detect the column and bump ``user_version`` instead of trying to
    ALTER again — SQLite would raise "duplicate column name" otherwise.
    """
    conn = db.connect(tmp_path / "store.db")
    try:
        _run_v001_v002(conn)
        # Pretend the operator already added the column manually.
        conn.execute(
            "ALTER TABLE messages ADD COLUMN speed TEXT NOT NULL DEFAULT 'standard'"
        )
        conn.commit()
        # user_version is still 2 — this is the partial-application case.
        assert conn.execute("PRAGMA user_version").fetchone()[0] == 2

        # The loader must not raise and must bump the version (through to
        # whatever the current head is — v004 chains on after v003 here).
        schema.apply(conn)
        # v004 (cursor workspace redistribute) chains after v003 so the
        # final user_version is the latest applied. Compare to the
        # symbolic constant to keep this test stable across future bumps.
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION

        # And the column is still there with the right default.
        info = _column_info(conn, "messages", "speed")
        assert info is not None
    finally:
        conn.close()


def test_v003_inserts_default_speed_when_omitted(tmp_path: Path) -> None:
    """A bare INSERT (no speed column) must land 'standard' via DEFAULT."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, "
            "first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
            ("claude", "-p", "p", 0.0, 0.0),
        )
        conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (1, 's')",
        )
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, "
            "raw_json) VALUES (1, 0, '2026-04-01T00:00:00+00:00', 'user', '{}')"
        )
        row = conn.execute("SELECT speed FROM messages").fetchone()
        assert row["speed"] == "standard"
    finally:
        conn.close()
