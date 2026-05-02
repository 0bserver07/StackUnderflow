"""Schema migrations.

Migrations are `.sql` files under `migrations/` named `vNNN_*.sql`. Each
file must set `PRAGMA user_version = NNN` as its last statement inside a
transaction.

`apply(conn)` reads `PRAGMA user_version` and runs every migration whose
number is higher, in order. ``ALTER TABLE`` migrations are additionally
guarded by a ``PRAGMA table_info`` check so a partially-applied state
(column already added, ``user_version`` not bumped) recovers cleanly
instead of erroring on "duplicate column".
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

_MIGRATIONS_DIR = Path(__file__).parent / "migrations"

CURRENT_VERSION = 4


def apply(conn: sqlite3.Connection) -> None:
    """Run every pending migration against *conn*.

    Skips a migration entirely when its target column already exists on
    the target table — covers the case where an operator pre-ran the
    ``ALTER TABLE`` by hand or a previous migration crashed after the
    DDL but before bumping ``PRAGMA user_version``. In that case we still
    bump the version so subsequent migrations chain correctly.
    """
    current = conn.execute("PRAGMA user_version").fetchone()[0]
    for version, path in _discover():
        if version <= current:
            continue
        guard = _ADD_COLUMN_GUARDS.get(version)
        if guard is not None and _column_exists(conn, *guard):
            conn.execute(f"PRAGMA user_version = {version}")
            continue
        sql = path.read_text()
        conn.executescript(sql)


# Per-migration "is this ADD COLUMN already done?" guards. Maps the
# migration number to (table, column). Only ``ALTER TABLE ADD COLUMN``
# migrations need entries here — full-rebuild migrations (like v002)
# rely on ``user_version`` alone.
_ADD_COLUMN_GUARDS: dict[int, tuple[str, str]] = {
    3: ("messages", "speed"),
}


def _column_exists(conn: sqlite3.Connection, table: str, column: str) -> bool:
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    for r in rows:
        # PRAGMA table_info returns (cid, name, type, notnull, dflt_value, pk)
        # — sqlite3.Row supports both index and name access.
        name = r["name"] if hasattr(r, "keys") else r[1]
        if name == column:
            return True
    return False


def _discover() -> list[tuple[int, Path]]:
    out: list[tuple[int, Path]] = []
    for path in sorted(_MIGRATIONS_DIR.glob("v*.sql")):
        stem = path.stem                # "v001_initial"
        num = int(stem[1:4])             # "001" -> 1
        out.append((num, path))
    return out
