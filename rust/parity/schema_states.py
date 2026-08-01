#!/usr/bin/env python3
"""The Python half of ``rust/schema-differ.sh`` — and the differ's one dumper.

Three jobs, deliberately separated so no implementation ever describes itself:

``apply <db> [--to N]``
    ``store.db.connect`` + ``store.schema.apply``. ``--to N`` stops after
    migration N by filtering ``schema._discover()``, which is how a mid-version
    state is built; the runner's own loop, guards and transactions are
    untouched.

``seed <db> <fixture>``
    Put rows in a store that is *already* at some version, so the data
    migrations (v005, v008) have something to migrate. A from-empty differ can
    only ever prove the DDL.

``dump <db>``
    The neutral comparison text. Run against BOTH stores by this one script, so
    the bytes being compared come from one reader — a store dumped by the
    engine that wrote it is a tautology, not a proof.

The dump is ``sqlite_master`` in **rowid order**, which is creation order: it is
what ``.schema`` prints, and reproducing it is the actual claim. Sorting it would
hide exactly the class of divergence this differ exists to find (same objects,
different order ⇒ a different ``.schema``, a different backup diff, a different
``sqlite3 store.db .dump``).
"""

from __future__ import annotations

import sqlite3
import sys
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(_REPO_ROOT))

from stackunderflow.store import db, schema  # noqa: E402


# ── apply ────────────────────────────────────────────────────────────────────


def cmd_apply(db_path: Path, target: int | None) -> int:
    conn = db.connect(db_path)
    if target is None:
        schema.apply(conn)
    else:
        original = schema._discover
        schema._discover = lambda: [  # type: ignore[assignment]
            (version, path) for version, path in original() if version <= target
        ]
        try:
            schema.apply(conn)
        finally:
            schema._discover = original  # type: ignore[assignment]
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    conn.close()
    print(f"user_version={version}")
    return 0


# ── seed ─────────────────────────────────────────────────────────────────────

# Every fixture writes through plain SQL against whatever version the store is
# already on. They are intentionally small: the point is to cross a branch, not
# to be realistic. `messages-mixed` crosses all three of v008's routing legs
# (two real months, an empty timestamp, a malformed one).
_FIXTURES: dict[str, list[str]] = {
    "empty": [],
    "messages-mixed": [
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified)"
        " VALUES ('claude', '-home-me-a', '/home/me/a', 'a', 1.0, 2.0)",
        "INSERT INTO sessions (project_id, session_id) VALUES (1, 's-1')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (1, 0, '2026-01-15T00:00:00Z', 'user', '{\"a\":1}')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (1, 1, '2026-01-16T00:00:00Z', 'assistant', '{\"a\":2}')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (1, 2, '2026-02-01T00:00:00Z', 'user', '{\"a\":3}')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (1, 3, '2025-12-31T23:59:59Z', 'assistant', '{\"a\":4}')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (1, 4, '', 'user', '{\"a\":5}')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (1, 5, 'not-a-timestamp', 'user', '{\"a\":6}')",
    ],
    # v005's subject: the pre-0.6.1 collapse. No path evidence in the payloads,
    # so BOTH implementations take the unresolved branch and keep the legacy row
    # — which is what makes this fixture comparable at all while DIV-301 is open.
    "cursor-legacy": [
        "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified)"
        " VALUES ('cursor', 'cursor', NULL, 'cursor', 10.0, 20.0)",
        "INSERT INTO sessions (project_id, session_id) VALUES (1, 'conv-1')",
        "INSERT INTO sessions (project_id, session_id) VALUES (1, 'conv-2')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (1, 0, '2026-01-15T00:00:00Z', 'user', '{\"text\":\"hi\"}')",
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json)"
        " VALUES (2, 0, '2026-01-15T00:00:00Z', 'user', '')",
    ],
}


def cmd_seed(db_path: Path, fixture: str) -> int:
    if fixture not in _FIXTURES:
        print(f"schema_states: unknown fixture {fixture!r}", file=sys.stderr)
        return 2
    conn = db.connect(db_path)
    for statement in _FIXTURES[fixture]:
        conn.execute(statement)
    conn.commit()
    conn.close()
    return 0


# ── the partial-application states ───────────────────────────────────────────

# `_ADD_COLUMN_GUARDS`'s reason for existing: an operator (or a crash) applied
# the DDL and the version bump never happened. The runner has to recover, and it
# is the one branch a from-empty run can never reach. Each entry is
# (version, the statement that pre-applies the body, ...) run at version-1.
_PARTIALS: dict[int, list[str]] = {
    3: ["ALTER TABLE messages ADD COLUMN speed TEXT NOT NULL DEFAULT 'standard'"],
    13: ["ALTER TABLE sessions ADD COLUMN team_id TEXT"],
    26: ["ALTER TABLE usage_events ADD COLUMN reasoning_tokens INTEGER NOT NULL DEFAULT 0"],
    27: ["ALTER TABLE projects ADD COLUMN worktree_of TEXT"],
}


def cmd_partial(db_path: Path, version: int) -> int:
    if version not in _PARTIALS:
        print(f"schema_states: no partial state for v{version}", file=sys.stderr)
        return 2
    conn = db.connect(db_path)
    before = conn.execute("PRAGMA user_version").fetchone()[0]
    for statement in _PARTIALS[version]:
        conn.execute(statement)
    # The whole point: the DDL is in, the version is NOT bumped.
    conn.execute(f"PRAGMA user_version = {before}")
    conn.commit()
    conn.close()
    return 0


def cmd_list_partials() -> int:
    print(" ".join(str(version) for version in sorted(_PARTIALS)))
    return 0


# ── dump ─────────────────────────────────────────────────────────────────────

# Tables whose rows are compared when a fixture put data in them. Ordered by a
# stable key so row order is never the thing that differs.
_DATA_TABLES = (
    ("projects", "id"),
    ("sessions", "id"),
    ("usage_events", "id"),
    ("_messages_id_seq", "rowid_kind"),
)


def cmd_dump(db_path: Path, with_data: bool) -> int:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    out: list[str] = []
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    out.append(f"user_version={version}")
    out.append("--- sqlite_master (rowid order) ---")
    rows = conn.execute(
        "SELECT type, name, tbl_name, COALESCE(sql, '<none>') FROM sqlite_master"
        " WHERE name NOT LIKE 'sqlite_%' ORDER BY rowid"
    ).fetchall()
    for kind, name, tbl_name, sql in rows:
        out.append(f"[{kind}] {name} (on {tbl_name})")
        out.append(sql)
        out.append("<<<")

    if with_data:
        out.append("--- data ---")
        present = {
            name
            for (name,) in conn.execute(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'view')"
            ).fetchall()
        }
        for table, order_by in _DATA_TABLES:
            if table not in present:
                continue
            out.append(f"# {table}")
            for row in conn.execute(f"SELECT * FROM {table} ORDER BY {order_by}"):
                out.append(repr(tuple(row)))
        # `messages` is a view after v008 and a table before it; either way the
        # rows and their partition are what matters.
        if "messages" in present:
            out.append("# messages")
            for row in conn.execute(
                "SELECT id, session_fk, seq, timestamp, role, raw_json FROM messages"
                " ORDER BY id"
            ):
                out.append(repr(tuple(row)))
        for (name,) in conn.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table'"
            " AND (name GLOB 'messages_[0-9][0-9][0-9][0-9][0-9][0-9]'"
            "      OR name = 'messages_unknown') ORDER BY name"
        ).fetchall():
            out.append(f"# {name}")
            for row in conn.execute(
                f"SELECT id, session_fk, seq, timestamp FROM {name} ORDER BY id"
            ):
                out.append(repr(tuple(row)))
    conn.close()
    print("\n".join(out))
    return 0


# ── entry ────────────────────────────────────────────────────────────────────


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    verb, rest = argv[0], argv[1:]
    if verb == "list-partials":
        return cmd_list_partials()
    if not rest:
        print(f"schema_states: {verb} needs a database path", file=sys.stderr)
        return 2
    db_path = Path(rest[0])
    tail = rest[1:]
    if verb == "apply":
        target = None
        if tail and tail[0] == "--to":
            target = int(tail[1])
        return cmd_apply(db_path, target)
    if verb == "seed":
        return cmd_seed(db_path, tail[0])
    if verb == "partial":
        return cmd_partial(db_path, int(tail[0]))
    if verb == "dump":
        return cmd_dump(db_path, with_data="--data" in tail)
    print(f"schema_states: unknown verb {verb!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
