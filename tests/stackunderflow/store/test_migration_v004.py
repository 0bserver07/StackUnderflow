"""v004 migration: clear the legacy ``"<synthetic>"`` model id from rows.

Claude Code stamps ``message.model = "<synthetic>"`` on locally generated
placeholder records (API errors, "No response requested.", auth-failed
stubs). Versions of the Claude adapter before this migration passed that
sentinel through verbatim, so the literal string surfaced as a distinct
``<synthetic>`` row in ``stackunderflow compare`` — zero tokens, zero
cost, pure noise.

These tests pin the migration's two guarantees:

  1. Every ``model = '<synthetic>'`` row gets ``model = NULL``; rows with
     real model ids (and rows already at NULL) are left alone.
  2. ``raw_json`` and ``content_text`` are untouched — only the bogus
     model id column is cleared.
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.store import db, schema


def _run_through_v003(conn) -> None:
    """Apply v001..v003 by hand — simulates a pre-v004 store."""
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


def _seed_messages(conn, rows: list[tuple[str | None, str]]) -> None:
    """Insert ``(model, content_text)`` rows after ensuring a parent project/session."""
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, "
        "first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
        ("claude", "-proj", "proj", 0.0, 0.0),
    )
    conn.execute(
        "INSERT INTO sessions (project_id, session_id) VALUES (1, 's1')",
    )
    for i, (model, text) in enumerate(rows):
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, "
            "model, content_text, raw_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (1, i, "2026-04-01T00:00:00+00:00", "assistant", model, text, "{}"),
        )
    conn.commit()


def test_v004_clears_synthetic_model_only(tmp_path: Path) -> None:
    """Rows with ``model = '<synthetic>'`` get NULL'd; everything else
    keeps its model id (or stays NULL)."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _run_through_v003(conn)
        _seed_messages(conn, [
            ("<synthetic>", "API Error: rate limit"),
            ("claude-opus-4-7", "real response"),
            (None, "user-side row, never had a model"),
            ("<synthetic>", "No response requested."),
        ])

        # Apply the full chain — only v004 should run on this store.
        schema.apply(conn)

        rows = conn.execute(
            "SELECT model, content_text FROM messages ORDER BY seq"
        ).fetchall()
        # Rows 0 and 3 (the two synthetic ones) are now NULL.
        assert rows[0]["model"] is None
        assert rows[3]["model"] is None
        # Real model id is preserved.
        assert rows[1]["model"] == "claude-opus-4-7"
        # Pre-existing NULL stays NULL.
        assert rows[2]["model"] is None
        # No <synthetic> survives anywhere.
        leftover = conn.execute(
            "SELECT COUNT(*) FROM messages WHERE model = '<synthetic>'"
        ).fetchone()[0]
        assert leftover == 0
    finally:
        conn.close()


def test_v004_preserves_content_and_raw(tmp_path: Path) -> None:
    """The error-message body and raw blob stay intact — we only rewrite
    the bogus model id column."""
    conn = db.connect(tmp_path / "store.db")
    try:
        _run_through_v003(conn)
        conn.execute(
            "INSERT INTO projects (provider, slug, display_name, "
            "first_seen, last_modified) VALUES (?, ?, ?, ?, ?)",
            ("claude", "-proj", "proj", 0.0, 0.0),
        )
        conn.execute(
            "INSERT INTO sessions (project_id, session_id) VALUES (1, 's1')",
        )
        conn.execute(
            "INSERT INTO messages (session_fk, seq, timestamp, role, "
            "model, content_text, raw_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                1, 0, "2026-04-01T00:00:00+00:00", "assistant",
                "<synthetic>",
                "API Error: 400 due to tool use concurrency issues.",
                '{"message":{"model":"<synthetic>"},"isApiErrorMessage":true}',
            ),
        )
        conn.commit()

        schema.apply(conn)

        row = conn.execute(
            "SELECT model, content_text, raw_json FROM messages"
        ).fetchone()
        assert row["model"] is None
        # Body survived verbatim.
        assert row["content_text"] == "API Error: 400 due to tool use concurrency issues."
        # raw_json keeps the original blob (raw still references <synthetic>;
        # only the dedicated column is cleared).
        assert "<synthetic>" in row["raw_json"]
    finally:
        conn.close()


def test_v004_is_idempotent(tmp_path: Path) -> None:
    """Running the migration on a clean store (no ``<synthetic>`` rows)
    is a no-op — and re-running ``schema.apply`` after the first call
    must not re-run v004 or raise."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        before = conn.execute(
            "PRAGMA user_version"
        ).fetchone()[0]
        schema.apply(conn)
        after = conn.execute("PRAGMA user_version").fetchone()[0]
        assert before == after == schema.CURRENT_VERSION
    finally:
        conn.close()
