#!/usr/bin/env python
"""Compare two stores after a `recommend mode` run, modulo the wall clock.

`_cache_store` writes `mode_recommendations` rows carrying `created_ts` and
`last_used_ts` — `datetime.now(UTC)` to the microsecond. Two implementations
run seconds apart, so those two columns are the harness's own clock and cannot
be compared; everything else about the write must match exactly:

  1. the same set of tables changed (only `mode_recommendations`),
  2. the same number of rows,
  3. every column except the two timestamps identical, row for row,
  4. both timestamps parse as ISO-8601 and `created_ts == last_used_ts`
     (which is what `_cache_store` writes on an insert).

Exit 0 with a one-line note when all four hold; exit 1 naming the break
otherwise.

    python rust/parity/skills_store_diff.py <store-a> <store-b> [--seed <store>]
"""

from __future__ import annotations

import sqlite3
import sys
from datetime import datetime
from pathlib import Path

VOLATILE = ("created_ts", "last_used_ts")


def rows(path: Path, table: str) -> list[dict]:
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        names = [r[1] for r in conn.execute(f"PRAGMA table_info({table})")]
        if not names:
            return []
        order = ", ".join(n for n in names if n != "id") or "rowid"
        return [
            dict(zip(names, r, strict=True))
            for r in conn.execute(f"SELECT {', '.join(names)} FROM {table} ORDER BY {order}")  # noqa: S608
        ]
    finally:
        conn.close()


def table_counts(path: Path) -> dict[str, int]:
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        names = [
            r[0]
            for r in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table' "
                "AND name NOT LIKE 'sqlite_%' ORDER BY name"
            )
        ]
        return {n: conn.execute(f"SELECT COUNT(*) FROM {n}").fetchone()[0] for n in names}  # noqa: S608
    finally:
        conn.close()


def fail(message: str) -> int:
    print(f"skills_store_diff: FAIL — {message}", file=sys.stderr)
    return 1


def main(argv: list[str]) -> int:
    args = [a for a in argv[1:] if not a.startswith("--")]
    seed = None
    if "--seed" in argv:
        seed = Path(argv[argv.index("--seed") + 1])
        args = [a for a in args if str(seed) != a]
    if len(args) != 2:
        print(__doc__)
        return 2
    left, right = Path(args[0]), Path(args[1])

    left_counts, right_counts = table_counts(left), table_counts(right)
    if left_counts != right_counts:
        changed = {
            name
            for name in set(left_counts) | set(right_counts)
            if left_counts.get(name) != right_counts.get(name)
        }
        return fail(f"row counts differ in {sorted(changed)}")

    if seed is not None:
        seed_counts = table_counts(seed)
        grew = {
            name
            for name in left_counts
            if left_counts[name] != seed_counts.get(name, 0)
        }
        if grew != {"mode_recommendations"}:
            return fail(f"tables changed other than mode_recommendations: {sorted(grew)}")

    left_rows = rows(left, "mode_recommendations")
    right_rows = rows(right, "mode_recommendations")
    if len(left_rows) != len(right_rows):
        return fail(f"mode_recommendations row count {len(left_rows)} != {len(right_rows)}")
    if not left_rows:
        return fail("mode_recommendations is empty on both sides — nothing was proven")

    for index, (a, b) in enumerate(zip(left_rows, right_rows, strict=True)):
        stable_a = {k: v for k, v in a.items() if k not in VOLATILE}
        stable_b = {k: v for k, v in b.items() if k not in VOLATILE}
        if stable_a != stable_b:
            return fail(f"row {index} differs: {stable_a} != {stable_b}")
        for row in (a, b):
            if row["created_ts"] != row["last_used_ts"]:
                return fail(f"row {index}: created_ts != last_used_ts on an insert")
            for column in VOLATILE:
                try:
                    datetime.fromisoformat(row[column])
                except (TypeError, ValueError):
                    return fail(f"row {index}: {column} is not ISO-8601: {row[column]!r}")

    print(
        f"skills_store_diff: OK — {len(left_rows)} mode_recommendations row(s) "
        "identical except the two wall-clock columns"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
