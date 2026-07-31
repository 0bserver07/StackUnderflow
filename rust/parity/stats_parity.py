#!/usr/bin/env python3
"""The Python half of the wave-5 statistics gate (RS-3-062 / -065).

Mirror of ``crates/stax-etl/src/bin/stats_parity.rs``: same arguments, same
output layout, driven through the real
``stackunderflow.store.queries.get_project_stats``. Run both against the SAME
store snapshot and ``diff -r`` the two output directories — one file per
top-level block of the statistics dict, so the diff is a per-block tally
instead of one all-or-nothing answer.

    python3 rust/parity/stats_parity.py dump <store.db> <slug|#id> <outdir> \
        [--tz N] [--messages]
    python3 rust/parity/stats_parity.py projects <store.db> [limit]

The store is opened ``mode=ro`` through a URI, like the Rust side. Nothing here
writes to it, and ``STACKUNDERFLOW_HOME`` should point at the snapshot's home
so the settings layer never reaches the maintainer's real one.

``json.dumps(obj, separators=(",", ":"))`` is the wire form on both sides; the
Rust binary renders through ``pyjson::dumps_compact``, which is this call.
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))


def open_ro(path: str) -> sqlite3.Connection:
    if "stackunderflow-data" in path:
        raise SystemExit(
            f"refusing to open {path}: the live dataset is READ-ONLY for this "
            "campaign. Work on the snapshot under rust/.parity-state/."
        )
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def cmd_projects(argv: list[str]) -> int:
    path = argv[0]
    limit = int(argv[1]) if len(argv) > 1 else 40
    conn = open_ro(path)
    rows = conn.execute(
        "SELECT p.id, p.provider, p.slug, COUNT(m.id) AS n "
        "FROM projects p "
        "LEFT JOIN sessions s ON s.project_id = p.id "
        "LEFT JOIN messages m ON m.session_fk = s.id "
        "GROUP BY p.id HAVING n > 0 ORDER BY n LIMIT ?",
        (limit,),
    ).fetchall()
    for r in rows:
        print(f"{r['id']}\t{r['provider'] or ''}\t{r['n']}\t{r['slug']}")
    return 0


def resolve_ids(conn: sqlite3.Connection, selector: str) -> list[int]:
    """``#42`` is one row; anything else is a slug, which may name several.

    ``UNIQUE(provider, slug)`` lets the same directory appear once per provider.
    Order matters — ``build_enriched_dataset`` takes ``log_dir`` from the FIRST
    id alone — so both sides order by id.
    """
    if selector.startswith("#"):
        return [int(selector[1:])]
    ids = [
        r[0]
        for r in conn.execute(
            "SELECT id FROM projects WHERE slug = ? ORDER BY id", (selector,)
        )
    ]
    if not ids:
        raise SystemExit(f"no project matches {selector!r}")
    return ids


def write_json(path: Path, value) -> None:
    path.write_text(json.dumps(value, separators=(",", ":")) + "\n", encoding="utf-8")


def cmd_dump(argv: list[str]) -> int:
    path, selector, outdir = argv[0], argv[1], argv[2]
    tz_offset = 0
    want_messages = False
    rest = list(argv[3:])
    while rest:
        flag = rest.pop(0)
        if flag == "--tz":
            tz_offset = int(rest.pop(0))
        elif flag == "--messages":
            want_messages = True
        else:
            raise SystemExit(f"unknown flag {flag!r}")

    from stackunderflow.store import queries

    conn = open_ro(path)
    ids = resolve_ids(conn, selector)

    started = time.perf_counter()
    messages, stats = queries.get_project_stats(
        conn, project_id=ids, tz_offset=tz_offset
    )
    elapsed = time.perf_counter() - started

    out = Path(outdir)
    (out / "blocks").mkdir(parents=True, exist_ok=True)
    write_json(out / "_all.json", stats)
    for name, block in stats.items():
        write_json(out / "blocks" / f"{name}.json", block)
    if want_messages:
        write_json(out / "messages.json", messages)

    with (out / "meta.txt").open("w", encoding="utf-8") as fh:
        fh.write(f"ids\t{ids}\n")
        fh.write(f"tz_offset\t{tz_offset}\n")
        fh.write(f"messages\t{len(messages)}\n")
    print(f"# messages\t{len(messages)}")
    print(f"# seconds\t{elapsed:.2f}")
    return 0


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    verb, rest = argv[0], argv[1:]
    if verb == "projects":
        return cmd_projects(rest)
    if verb == "dump":
        return cmd_dump(rest)
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    if "STACKUNDERFLOW_HOME" not in os.environ:
        print(
            "warning: STACKUNDERFLOW_HOME is unset — the settings layer will "
            "read the real home. Set it to the snapshot before trusting a diff.",
            file=sys.stderr,
        )
    raise SystemExit(main(sys.argv[1:]))
