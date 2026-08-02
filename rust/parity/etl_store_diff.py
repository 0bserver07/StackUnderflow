#!/usr/bin/env python
"""Compare two post-backfill stores: schema, every row of every table, one mask.

`parity/sqlite_header_diff.py` is the right tool for a *copy* (its whole point
is that only `SQLITE_VERSION_NUMBER` may differ). It is the wrong tool for two
independent WRITES, because a backfill stamps `mart_watermark.last_refresh_ts`
with `datetime.now(UTC)` and the two implementations run seconds apart. This
script is the write-side equivalent: identical everywhere, with exactly one
column masked and the mask reported rather than applied silently.

What is asserted
----------------

1. `sqlite_master` is identical — same tables, same indexes, same SQL text.
2. Every row of every table is identical, in `rowid`/primary-key order, with
   `mart_watermark.last_refresh_ts` replaced by a fixed token on both sides.
3. Every OTHER column of `mart_watermark` — including `last_event_id`, the one
   that decides whether the next refresh does any work — is compared exactly.

What is reported, not asserted
------------------------------

The number of DISTINCT `last_refresh_ts` values each side wrote.
`stackunderflow/etl/marts/watermark.py`'s `set_watermark` calls
`datetime.now(UTC)` itself, once per mart, so the reference stamps eight
different instants; `stax_etl::marts::watermark::refresh_all_marts` takes one
injected `now` and stamps all eight with it. That is the second finding in
`rust/parity/DIV-e-etl.md`, filed against the HTTP endpoint and inherited by
the CLI verb that shares the orchestrator. It is a wall clock either way, so no
differ can hold it to equality; the COUNT is the falsifiable part and it is
printed on every run.

    python rust/parity/etl_store_diff.py <dir-a> <dir-b>

Exit 0 when 1–3 hold, 1 otherwise.
"""
from __future__ import annotations

import pathlib
import sqlite3
import sys

MASKED = {("mart_watermark", "last_refresh_ts")}
TOKEN = "<CLOCK>"


def connect(path: pathlib.Path) -> sqlite3.Connection:
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def schema(conn: sqlite3.Connection) -> list[tuple]:
    return [
        tuple(r)
        for r in conn.execute(
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name"
        )
    ]


def tables(conn: sqlite3.Connection) -> list[str]:
    return [
        r[0]
        for r in conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' "
            "AND name NOT LIKE 'sqlite_%' ORDER BY name"
        )
    ]


def rows(conn: sqlite3.Connection, table: str) -> list[tuple]:
    """Every row, column-ordered, masked where this script says to mask.

    Ordered by every column so the comparison does not depend on the engine's
    row order — two writers can produce the same SET of rows in two page
    layouts, and that is not a divergence.
    """
    cols = [r["name"] for r in conn.execute(f"PRAGMA table_info({table})")]
    if not cols:
        return []
    order = ", ".join(f'"{c}"' for c in cols)
    out = []
    for row in conn.execute(f'SELECT * FROM "{table}" ORDER BY {order}'):
        out.append(
            tuple(
                TOKEN if (table, col) in MASKED else row[col]
                for col in cols
            )
        )
    return out


def clock_spread(conn: sqlite3.Connection) -> int | None:
    try:
        return int(
            conn.execute(
                "SELECT COUNT(DISTINCT last_refresh_ts) FROM mart_watermark"
            ).fetchone()[0]
        )
    except sqlite3.Error:
        return None


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__)
        return 2
    a_dir, b_dir = (pathlib.Path(p) for p in argv[1:3])
    a_path, b_path = a_dir / "store.db", b_dir / "store.db"
    for path in (a_path, b_path):
        if not path.exists():
            print(f"FAIL: no store at {path}")
            return 1

    a, b = connect(a_path), connect(b_path)
    try:
        if schema(a) != schema(b):
            print("FAIL: sqlite_master differs")
            only_a = set(map(str, schema(a))) - set(map(str, schema(b)))
            only_b = set(map(str, schema(b))) - set(map(str, schema(a)))
            for item in sorted(only_a):
                print(f"  only in A: {item}")
            for item in sorted(only_b):
                print(f"  only in B: {item}")
            return 1

        failed = False
        total = 0
        for table in tables(a):
            ra, rb = rows(a, table), rows(b, table)
            total += len(ra)
            if ra == rb:
                continue
            failed = True
            print(f"FAIL: {table} differs — {len(ra)} rows vs {len(rb)}")
            for index, (x, y) in enumerate(zip(ra, rb)):
                if x != y:
                    print(f"  first differing row {index}:")
                    print(f"    A {x}")
                    print(f"    B {y}")
                    break
        if failed:
            return 1

        print(
            f"OK: sqlite_master identical, {len(tables(a))} tables, "
            f"{total} rows identical (mart_watermark.last_refresh_ts masked)"
        )
        print(
            "    distinct last_refresh_ts: "
            f"A={clock_spread(a)} B={clock_spread(b)} "
            "(DIV-e-etl finding 2 — Python re-reads the clock per mart)"
        )
        return 0
    finally:
        a.close()
        b.close()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
