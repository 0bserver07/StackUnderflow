"""Schema migrations.

Migrations are `.sql` files under `migrations/` named `vNNN_*.sql`. Each
file must set `PRAGMA user_version = NNN` as its last statement inside a
transaction.

`apply(conn)` reads `PRAGMA user_version` and runs every migration whose
number is higher, in order.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

_MIGRATIONS_DIR = Path(__file__).parent / "migrations"

CURRENT_VERSION = 1


def apply(conn: sqlite3.Connection) -> None:
    """Run every pending migration against *conn*."""
    current = conn.execute("PRAGMA user_version").fetchone()[0]
    for version, path in _discover():
        if version <= current:
            continue
        sql = path.read_text()
        conn.executescript(sql)


def _discover() -> list[tuple[int, Path]]:
    out: list[tuple[int, Path]] = []
    for path in sorted(_MIGRATIONS_DIR.glob("v*.sql")):
        stem = path.stem                # "v001_initial"
        num = int(stem[1:4])             # "001" -> 1
        out.append((num, path))
    return out
