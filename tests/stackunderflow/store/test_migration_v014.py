"""v014 migration: ``discovery_embeddings`` — opt-in semantic-search cache.

See ``stackunderflow/store/migrations/v014_discovery_embeddings.sql``
for the full design. This table is additive, ``IF NOT EXISTS``-guarded,
keyed on ``(session_id, message_id, model_name)``, and stores
``embedding`` as a raw float32 byte buffer. The migration is the only
schema change behind HANDOFF #10.
"""

from __future__ import annotations

import sqlite3
from datetime import UTC, datetime
from pathlib import Path

import pytest

from stackunderflow.store import db, schema

_EXPECTED_COLUMNS = (
    "session_id", "message_id", "model_name",
    "embedding", "embedding_dim", "created_ts",
)


@pytest.fixture
def conn(tmp_path: Path) -> sqlite3.Connection:
    c = db.connect(tmp_path / "store.db")
    schema.apply(c)
    yield c
    c.close()


class TestV014:
    def test_current_version_is_at_least_14(self) -> None:
        assert schema.CURRENT_VERSION >= 14

    def test_apply_lands_on_current_version(self, conn: sqlite3.Connection) -> None:
        assert conn.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION

    def test_discovery_embeddings_table_shape(self, conn: sqlite3.Connection) -> None:
        cols = [
            r["name"] for r in conn.execute("PRAGMA table_info(discovery_embeddings)").fetchall()
        ]
        assert tuple(cols) == _EXPECTED_COLUMNS

    def test_all_columns_not_null(self, conn: sqlite3.Connection) -> None:
        info = {
            r["name"]: r
            for r in conn.execute("PRAGMA table_info(discovery_embeddings)").fetchall()
        }
        for col in _EXPECTED_COLUMNS:
            assert info[col]["notnull"] == 1, f"{col} should be NOT NULL"

    def test_primary_key_is_compound(self, conn: sqlite3.Connection) -> None:
        # Compound PK on (session_id, message_id, model_name) — all
        # three appear in PRAGMA table_info with pk > 0 in declaration
        # order.
        info = {
            r["name"]: r
            for r in conn.execute("PRAGMA table_info(discovery_embeddings)").fetchall()
        }
        assert info["session_id"]["pk"] == 1
        assert info["message_id"]["pk"] == 2
        assert info["model_name"]["pk"] == 3

    def test_session_index_present(self, conn: sqlite3.Connection) -> None:
        idx = {
            r["name"]
            for r in conn.execute("PRAGMA index_list(discovery_embeddings)").fetchall()
        }
        assert "idx_discovery_embeddings_session" in idx
        assert "idx_discovery_embeddings_message" in idx

    def test_blob_round_trip(self, conn: sqlite3.Connection) -> None:
        # We don't import numpy in this test (the migration ships
        # independently of the optional extra). A raw byte string
        # stands in for the float32 buffer just to prove the BLOB
        # column round-trips byte-equal.
        blob = bytes(range(256)) * 4  # 1024 bytes, all values
        now_iso = datetime.now(UTC).isoformat()
        conn.execute(
            "INSERT INTO discovery_embeddings "
            "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
            "VALUES ('s1', 42, 'mx', ?, 256, ?)",
            (blob, now_iso),
        )
        row = conn.execute(
            "SELECT embedding, embedding_dim, created_ts FROM discovery_embeddings"
        ).fetchone()
        assert row[0] == blob
        assert int(row[1]) == 256
        assert row[2] == now_iso

    def test_primary_key_enforces_uniqueness(self, conn: sqlite3.Connection) -> None:
        now_iso = datetime.now(UTC).isoformat()
        conn.execute(
            "INSERT INTO discovery_embeddings "
            "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
            "VALUES ('s1', 1, 'm1', X'00', 1, ?)",
            (now_iso,),
        )
        with pytest.raises(sqlite3.IntegrityError):
            conn.execute(
                "INSERT INTO discovery_embeddings "
                "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
                "VALUES ('s1', 1, 'm1', X'00', 1, ?)",
                (now_iso,),
            )

    def test_same_message_different_model_allowed(self, conn: sqlite3.Connection) -> None:
        # The compound PK includes model_name so vectors from
        # different models can coexist.
        now_iso = datetime.now(UTC).isoformat()
        conn.execute(
            "INSERT INTO discovery_embeddings "
            "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
            "VALUES ('s1', 1, 'm1', X'00', 1, ?)",
            (now_iso,),
        )
        conn.execute(
            "INSERT INTO discovery_embeddings "
            "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
            "VALUES ('s1', 1, 'm2', X'00', 1, ?)",
            (now_iso,),
        )
        n = conn.execute(
            "SELECT COUNT(*) FROM discovery_embeddings"
        ).fetchone()[0]
        assert n == 2

    def test_reapply_is_idempotent(self, conn: sqlite3.Connection) -> None:
        # Insert a row, re-run migrations, and check the row survived
        # — plus the version is unchanged.
        now_iso = datetime.now(UTC).isoformat()
        conn.execute(
            "INSERT INTO discovery_embeddings "
            "(session_id, message_id, model_name, embedding, embedding_dim, created_ts) "
            "VALUES ('s1', 1, 'm1', X'00', 1, ?)",
            (now_iso,),
        )
        before_count = conn.execute(
            "SELECT COUNT(*) FROM discovery_embeddings"
        ).fetchone()[0]
        before_ver = conn.execute("PRAGMA user_version").fetchone()[0]

        schema.apply(conn)  # again
        assert conn.execute("PRAGMA user_version").fetchone()[0] == before_ver
        assert conn.execute(
            "SELECT COUNT(*) FROM discovery_embeddings"
        ).fetchone()[0] == before_count

    def test_additive_does_not_disturb_existing_tables(self, conn: sqlite3.Connection) -> None:
        names = {
            r["name"]
            for r in conn.execute(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
            ).fetchall()
        }
        for table in (
            "projects", "sessions", "messages", "usage_events",
            "session_mart", "discovery_telemetry", "captured_events",
        ):
            assert table in names, f"{table} missing after v014"

    def test_recovers_from_partial_apply(self, tmp_path: Path) -> None:
        """Operator hand-creates the table without bumping user_version
        — the next ``schema.apply`` must not choke on the existing table
        and must finish at CURRENT_VERSION.
        """
        c = db.connect(tmp_path / "store.db")
        try:
            # Apply migrations up to v013 by stepping user_version down
            # after the full apply. The simpler approach: apply, then
            # manually rewind. (Bypasses the version-skip guard.)
            schema.apply(c)
            c.execute("PRAGMA user_version = 13")
            # And re-create the table by hand to simulate the
            # partial state.
            c.execute("DROP TABLE discovery_embeddings")
            c.execute("""
                CREATE TABLE discovery_embeddings (
                    session_id      TEXT NOT NULL,
                    message_id      INTEGER NOT NULL,
                    model_name      TEXT NOT NULL,
                    embedding       BLOB NOT NULL,
                    embedding_dim   INTEGER NOT NULL,
                    created_ts      TEXT NOT NULL,
                    PRIMARY KEY (session_id, message_id, model_name)
                )
            """)
            assert c.execute("PRAGMA user_version").fetchone()[0] == 13
            # IF NOT EXISTS guard means the re-apply doesn't error and
            # lands on the right version.
            schema.apply(c)
            assert c.execute("PRAGMA user_version").fetchone()[0] == schema.CURRENT_VERSION
        finally:
            c.close()
