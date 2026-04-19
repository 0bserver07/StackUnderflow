import sqlite3
from pathlib import Path

import pytest

from stackunderflow.store import db


def test_connect_creates_file(tmp_path: Path) -> None:
    db_path = tmp_path / "store.db"
    conn = db.connect(db_path)
    try:
        assert db_path.exists()
    finally:
        conn.close()


def test_connect_sets_wal(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        mode = conn.execute("PRAGMA journal_mode").fetchone()[0]
        assert mode.lower() == "wal"
    finally:
        conn.close()


def test_connect_enables_foreign_keys(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        fk = conn.execute("PRAGMA foreign_keys").fetchone()[0]
        assert fk == 1
    finally:
        conn.close()


def test_connect_row_factory_returns_dicts(tmp_path: Path) -> None:
    conn = db.connect(tmp_path / "store.db")
    try:
        conn.execute("CREATE TABLE t (x INTEGER, y TEXT)")
        conn.execute("INSERT INTO t VALUES (1, 'a')")
        row = conn.execute("SELECT x, y FROM t").fetchone()
        assert row["x"] == 1
        assert row["y"] == "a"
    finally:
        conn.close()
