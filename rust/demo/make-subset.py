#!/usr/bin/env python3
"""Build the differ's store: a browser-sized subset of the real one.

    python3 make-subset.py <live-store.db> <out.db> [KEEP ...]

Why a subset exists at all: the in-memory VFS holds the whole database in wasm
linear memory, so the ceiling is wasm32's address space and the maintainer's
3.9 GB store does not fit (DIV-332). Both the CLI and the wasm engine then read
the *same* subset file, so the reduction bounds how much of the store the proof
covers — never whether the two agree on it.

Two steps, in this order and for a reason:

1. `Connection.backup()` rather than `cp`. The live store has a multi-megabyte
   WAL and a watcher appending to it; copying the main file alone yields a torn
   read. `backup()` walks pages under a read lock — the same choice
   `parity/build_state.py` documents.
2. Delete whole message partitions, then `VACUUM`. Deleting *rows* rather than
   dropping tables keeps the `messages` UNION-ALL view valid (§6b: the view is
   the shape the queries plan against), and every other table — sessions,
   projects, the marts — is left exactly as it was.

Defaults keep `messages_202607` and `messages_202608`, which is what wave 9
measured: 3.9 GB → 227 MB.
"""

from __future__ import annotations

import os
import sqlite3
import sys
import time
from pathlib import Path

DEFAULT_KEEP = ("messages_202607", "messages_202608")


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    source, destination = Path(argv[1]), Path(argv[2])
    keep = set(argv[3:]) or set(DEFAULT_KEEP)
    if not source.is_file():
        print(f"make-subset: no store at {source}", file=sys.stderr)
        return 2

    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        destination.unlink()

    started = time.time()
    live = sqlite3.connect(f"file:{source}?mode=ro", uri=True)
    snapshot = sqlite3.connect(str(destination))
    live.backup(snapshot)
    live.close()
    print(f"snapshot   {time.time() - started:6.1f}s  {destination.stat().st_size / 1e6:8.1f} MB")

    started = time.time()
    partitions = [
        name
        for (name,) in snapshot.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'messages\\_%' ESCAPE '\\'"
        )
    ]
    dropped = [name for name in partitions if name not in keep]
    for name in dropped:
        snapshot.execute(f'DELETE FROM "{name}"')
    snapshot.commit()
    print(f"emptied    {time.time() - started:6.1f}s  {len(dropped)} of {len(partitions)} partitions")

    started = time.time()
    snapshot.execute("VACUUM")
    snapshot.close()
    print(f"vacuum     {time.time() - started:6.1f}s  {os.path.getsize(destination) / 1e6:8.1f} MB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
