#!/usr/bin/env python
"""Assert that two SQLite files differ ONLY in `SQLITE_VERSION_NUMBER`.

`backup create`'s `_capture_state` copies the critical artifacts through
SQLite's online-backup API. The copy's page 1 is written by whichever SQLite
library is doing the copying, and that library stamps its own version into the
header's 4-byte `SQLITE_VERSION_NUMBER` field at offset 96 — 3053001 for the
CPython build on this host, 3053002 for rusqlite's bundled one. That single
field is DIV-257, and it is irreducible without pinning both implementations to
the same SQLite build.

Everything else must match, so this is a *stricter* check than `cmp`, not a
weaker one:

  1. the files are the same length,
  2. the ONLY differing byte offsets are 96..99,
  3. both header version numbers are recognised 3.53.x values,
  4. `sqlite_master` is identical, and
  5. every row of every table is identical.

Exit 0 with a one-line note per artifact when all five hold; exit 1 and say
which one broke otherwise.

    python rust/parity/sqlite_header_diff.py <dir-a> <dir-b>
"""
from __future__ import annotations

import pathlib
import sqlite3
import struct
import sys

VERSION_FIELD = range(96, 100)


def dump(path: pathlib.Path) -> list:
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        schema = conn.execute(
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name"
        ).fetchall()
        rows: list = [("__schema__", schema)]
        for (name,) in conn.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name"
        ).fetchall():
            if name.startswith("sqlite_"):
                continue
            rows.append(
                (name, conn.execute(f'SELECT * FROM "{name}"').fetchall())
            )
        return rows
    finally:
        conn.close()


def compare(a: pathlib.Path, b: pathlib.Path) -> str | None:
    """`None` when the pair is acceptable; otherwise the failure text."""
    left, right = a.read_bytes(), b.read_bytes()
    if len(left) != len(right):
        return f"{a.name}: sizes differ ({len(left)} vs {len(right)})"
    offsets = [i for i, (x, y) in enumerate(zip(left, right)) if x != y]
    unexpected = [i for i in offsets if i not in VERSION_FIELD]
    if unexpected:
        return (
            f"{a.name}: differs outside SQLITE_VERSION_NUMBER at byte offsets "
            f"{unexpected[:12]}{' …' if len(unexpected) > 12 else ''}"
        )
    versions = (
        struct.unpack(">I", left[96:100])[0],
        struct.unpack(">I", right[96:100])[0],
    )
    for version in versions:
        if not 3_053_000 <= version < 3_054_000:
            return f"{a.name}: unrecognised SQLITE_VERSION_NUMBER {version}"
    if dump(a) != dump(b):
        return f"{a.name}: header matches but the CONTENT differs"
    if not offsets:
        print(f"    {a.name}: byte-identical")
        return None
    print(f"    {a.name}: identical but for SQLITE_VERSION_NUMBER {versions[0]} vs {versions[1]} (DIV-257)")
    return None


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    left_dir, right_dir = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    names = sorted({p.name for p in left_dir.glob("*.db")} | {p.name for p in right_dir.glob("*.db")})
    if not names:
        print("sqlite_header_diff: neither side captured a database", file=sys.stderr)
        return 1
    failures = []
    for name in names:
        left, right = left_dir / name, right_dir / name
        if not left.is_file() or not right.is_file():
            failures.append(f"{name}: present on only one side")
            continue
        problem = compare(left, right)
        if problem:
            failures.append(problem)
    for problem in failures:
        print(problem, file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
