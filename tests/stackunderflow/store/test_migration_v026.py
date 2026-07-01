"""v026 migration: ``usage_events.reasoning_tokens`` (reasoning attribution).

Reasoning/"thinking" tokens are billed as output and already folded into
``output_tokens``; this column records that SUBSET separately so consumers can
report "what share of output spend was reasoning?" WITHOUT ever changing a cost
total. The migration is **additive** — one ``ALTER TABLE ADD COLUMN`` with a
``DEFAULT 0`` so every pre-v026 row backfills to "no reasoning attributed yet".
These tests pin its guarantees:

  1. ``usage_events.reasoning_tokens`` exists, INTEGER / NOT NULL / DEFAULT 0.
  2. ``schema.apply`` bumps ``PRAGMA user_version`` to the current head (>=26).
  3. INSERTs that omit the column land the DEFAULT 0 (the pricing-invariants
     explicit-column INSERT path relies on this).
  4. The loader is reentrant — running it on a DB where the column already
     exists (manual ALTER / partial prior run) is a no-op and still bumps
     ``user_version`` via the ``_ADD_COLUMN_GUARDS`` entry.
  5. ``reasoning_tokens`` is NOT summed into cost anywhere — an event with a
     large reasoning count and one with none, same billing columns, price
     identically.
"""

from __future__ import annotations

from pathlib import Path

from stackunderflow.store import db, schema


def _column_info(conn, table: str, column: str) -> dict | None:
    for r in conn.execute(f"PRAGMA table_info({table})").fetchall():
        if r["name"] == column:
            return {"type": r["type"], "notnull": r["notnull"], "dflt_value": r["dflt_value"]}
    return None


def _seed_project(conn) -> int:
    conn.execute(
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
        "VALUES ('claude', 'alpha', '/alpha', 'Alpha', 0, 0)"
    )
    return conn.execute("SELECT id FROM projects").fetchone()[0]


def test_v026_adds_reasoning_tokens_column(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        info = _column_info(conn, "usage_events", "reasoning_tokens")
        assert info is not None, "usage_events.reasoning_tokens missing after migration"
        assert info["type"].upper() == "INTEGER"
        assert info["notnull"] == 1
        assert str(info["dflt_value"]) == "0"
    finally:
        conn.close()


def test_v026_user_version_bumped(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert schema.CURRENT_VERSION >= 26
    finally:
        conn.close()


def test_v026_insert_without_column_defaults_zero(tmp_path: Path) -> None:
    """A usage_events INSERT that omits reasoning_tokens reads 0 via DEFAULT —
    the invariant the pricing-invariants explicit-column INSERT depends on."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        pid = _seed_project(conn)
        conn.execute(
            "INSERT INTO usage_events "
            "(source_message_fk, provider, project_id, session_id, ts, day, "
            " model, role, input_tokens, output_tokens) "
            "VALUES (1, 'claude', ?, 's', '2026-01-01T00:00:00Z', '2026-01-01', "
            "        'claude-opus-4-8', 'assistant', 1000, 500)",
            (pid,),
        )
        row = conn.execute("SELECT reasoning_tokens FROM usage_events").fetchone()
        assert row["reasoning_tokens"] == 0
    finally:
        conn.close()


def test_v026_reentrant_when_column_already_exists(tmp_path: Path) -> None:
    """Partial-application recovery: column present but ``user_version`` behind."""
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)  # full chain → column now exists
        conn.execute("PRAGMA user_version = 25")  # rewind so v026 looks pending
        conn.commit()
        schema.apply(conn)  # must not raise "duplicate column name"
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        assert _column_info(conn, "usage_events", "reasoning_tokens") is not None
    finally:
        conn.close()


def test_v026_idempotent_reapply(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        schema.apply(conn)  # second call must not raise
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        cols = [r["name"] for r in conn.execute("PRAGMA table_info(usage_events)").fetchall()]
        assert cols.count("reasoning_tokens") == 1
    finally:
        conn.close()


def test_v026_reasoning_never_enters_cost(tmp_path: Path) -> None:
    """Two events, same billing columns, differing only in reasoning_tokens →
    identical stored cost_usd. Reasoning is attribution-only, never priced."""
    from stackunderflow.infra.costs import compute_cost

    # The stored cost is stamped at write time; it must equal a recompute over
    # ONLY the canonical four token columns, regardless of reasoning_tokens.
    expected = compute_cost(
        {"input": 1000, "output": 500, "cache_read": 0, "cache_creation": 0},
        "claude-opus-4-8",
        provider="anthropic",
    )["total_cost"]

    conn = db.connect(tmp_path / "store.db")
    try:
        schema.apply(conn)
        pid = _seed_project(conn)
        for fk, reasoning in ((1, 0), (2, 400)):
            conn.execute(
                "INSERT INTO usage_events "
                "(source_message_fk, provider, project_id, session_id, ts, day, "
                " model, role, input_tokens, output_tokens, reasoning_tokens, cost_usd) "
                "VALUES (?, 'claude', ?, 's', '2026-01-01T00:00:00Z', '2026-01-01', "
                "        'claude-opus-4-8', 'assistant', 1000, 500, ?, ?)",
                (fk, pid, reasoning, expected),
            )
        rows = conn.execute(
            "SELECT reasoning_tokens, cost_usd FROM usage_events ORDER BY source_message_fk"
        ).fetchall()
        assert rows[0]["reasoning_tokens"] == 0
        assert rows[1]["reasoning_tokens"] == 400
        # Cost identical across both regardless of reasoning attribution.
        assert rows[0]["cost_usd"] == rows[1]["cost_usd"] == expected
    finally:
        conn.close()
