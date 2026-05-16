"""Schema migrations.

Migrations live under ``migrations/`` named ``vNNN_*.sql`` (DDL) or
``vNNN_*.py`` (data-only Python migrations that need to read existing
rows before rewriting them). The two flavours coexist; both kinds
participate in the version ordering keyed on the leading ``vNNN``.

- ``.sql`` files must set ``PRAGMA user_version = NNN`` as their last
  statement inside a transaction.
- ``.py`` files must expose ``def apply(conn: sqlite3.Connection) -> None``.
  The runner wraps the call in ``BEGIN/COMMIT`` and bumps
  ``PRAGMA user_version`` after a successful return.

``apply(conn)`` reads ``PRAGMA user_version`` and runs every migration
whose number is higher, in order. ``ALTER TABLE`` migrations are
additionally guarded by a ``PRAGMA table_info`` check so a
partially-applied state (column already added, ``user_version`` not
bumped) recovers cleanly instead of erroring on "duplicate column".
"""

from __future__ import annotations

import importlib.util
import sqlite3
from pathlib import Path

_MIGRATIONS_DIR = Path(__file__).parent / "migrations"

CURRENT_VERSION = 16


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
        if path.suffix == ".sql":
            sql = path.read_text()
            conn.executescript(sql)
        elif path.suffix == ".py":
            _run_python_migration(conn, version, path)
        else:  # pragma: no cover - defensive
            raise ValueError(f"Unsupported migration extension: {path}")


def _run_python_migration(
    conn: sqlite3.Connection, version: int, path: Path
) -> None:
    """Import ``path`` and run its ``apply(conn)`` inside a transaction.

    The transaction wraps both the migration body and the
    ``user_version`` bump so a crash mid-migration leaves the database
    on the previous version (no partial migration state).
    """
    spec = importlib.util.spec_from_file_location(
        f"stackunderflow.store.migrations.{path.stem}", path
    )
    if spec is None or spec.loader is None:  # pragma: no cover - defensive
        raise ImportError(f"Cannot load migration {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    if not hasattr(module, "apply"):
        raise AttributeError(
            f"Migration {path.name} must define `apply(conn)`"
        )

    conn.execute("BEGIN")
    try:
        module.apply(conn)
        conn.execute(f"PRAGMA user_version = {version}")
        conn.execute("COMMIT")
    except Exception:
        conn.execute("ROLLBACK")
        raise


# Per-migration "is this ADD COLUMN already done?" guards. Maps the
# migration number to (table, column). Only ``ALTER TABLE ADD COLUMN``
# migrations need entries here — full-rebuild migrations (like v002)
# rely on ``user_version`` alone.
_ADD_COLUMN_GUARDS: dict[int, tuple[str, str]] = {
    3: ("messages", "speed"),
    12: ("tool_mart", "calls_total"),
    13: ("sessions", "team_id"),
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
    for path in sorted(_MIGRATIONS_DIR.iterdir()):
        if path.suffix not in {".sql", ".py"}:
            continue
        stem = path.stem                  # "v001_initial" or "v004_..."
        if not (stem.startswith("v") and len(stem) >= 4 and stem[1:4].isdigit()):
            continue
        num = int(stem[1:4])
        out.append((num, path))
    out.sort(key=lambda x: x[0])
    return out
