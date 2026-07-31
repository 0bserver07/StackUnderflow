#!/usr/bin/env python3
"""Build the byte-diff harness's two store states from the live dataset.

The P0 gate compares `stax-rs` against the Python CLI on the maintainer's
REAL store, which means the harness needs the real bytes — but the live
dataset is read-only for this campaign and the Python CLI *writes* (schema
migrations on connect, `discovery_telemetry` bumps, and
`SearchService.__init__` creates `search_index.db` outright). So every state
is a `sqlite3.Connection.backup()` snapshot: consistent even with a live
writer holding the WAL, and disposable.

Two states, because the four structured `memory` verbs take a different code
path depending on whether the FTS sidecar is populated:

    fresh/   store.db only            → discovery's `LIKE` scan
    fts/     store.db + search_index  → discovery's bm25 branch

One `STACKUNDERFLOW_HOME` per state, shared by both implementations — that is
the real deployment (one home, two binaries), and it keeps the snapshot cost
to one copy per state. Python runs first for every case so any migration it
performs happens before the Rust reader looks.

Usage:  build_state.py <live-dir> <state-dir> [--force]
"""

from __future__ import annotations

import json
import shutil
import sqlite3
import sys
import time
from pathlib import Path

# `config.json` is pinned rather than copied: the live one carries a host and
# a port (server settings) that have nothing to do with the memory verbs, and
# the two keys that DO matter here — the discovery budget and the rank weights
# — must be identical and explicit on both sides or the harness measures the
# maintainer's config drift instead of the port.
PINNED_CONFIG = {
    "version": "0.1.0",
    "auto_browser": False,
}


def snapshot(src: Path, dst: Path) -> float:
    """`Connection.backup()` — a consistent copy of a database with a live WAL.

    `cp` is wrong here: the live store has a 3-43 MB WAL and copying the main
    file alone yields a torn read. `backup()` walks pages under a read lock and
    materialises the committed state, which is exactly what the Python CLI
    would have seen.
    """
    started = time.time()
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists():
        dst.unlink()
    source = sqlite3.connect(f"file:{src}?mode=ro", uri=True)
    try:
        target = sqlite3.connect(str(dst))
        try:
            source.backup(target)
        finally:
            target.close()
    finally:
        source.close()
    return time.time() - started


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    force = "--force" in sys.argv[1:]
    if len(args) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    live, state = Path(args[0]), Path(args[1])

    store = live / "store.db"
    index = live / "search_index.db"
    if not store.is_file():
        print(f"build_state: no store at {store}", file=sys.stderr)
        return 1

    marker = state / ".built"
    if marker.is_file() and not force:
        print(f"build_state: {state} already built (use --force to rebuild)")
        return 0

    if state.exists():
        shutil.rmtree(state)
    (state / "fresh").mkdir(parents=True)
    (state / "fts").mkdir(parents=True)

    took = snapshot(store, state / "fresh" / "store.db")
    size = (state / "fresh" / "store.db").stat().st_size
    print(f"  fresh/store.db  {size / 1e9:.2f} GB in {took:.1f}s")

    # The second state's store must be byte-identical to the first, or a diff
    # between the two states measures the snapshot instead of the index.
    took = time.time()
    shutil.copy2(state / "fresh" / "store.db", state / "fts" / "store.db")
    print(f"  fts/store.db    copied in {time.time() - took:.1f}s")

    if index.is_file():
        took = snapshot(index, state / "fts" / "search_index.db")
        size = (state / "fts" / "search_index.db").stat().st_size
        conn = sqlite3.connect(f"file:{state / 'fts' / 'search_index.db'}?mode=ro", uri=True)
        rows = conn.execute("SELECT COUNT(*) FROM messages").fetchone()[0]
        conn.close()
        print(f"  fts/search_index.db  {size / 1e9:.2f} GB, {rows} rows, in {took:.1f}s")
        if rows == 0:
            print("  WARNING: the FTS state's index is EMPTY — both states "
                  "will take the LIKE path and the gate proves nothing.",
                  file=sys.stderr)
    else:
        print(f"  WARNING: no search_index.db at {index} — the fts state is "
              f"not populated", file=sys.stderr)

    for home in (state / "fresh", state / "fts"):
        (home / "config.json").write_text(
            json.dumps(PINNED_CONFIG, indent=2) + "\n", encoding="utf-8"
        )
        (home / "cache").mkdir(exist_ok=True)

    marker.write_text(
        json.dumps(
            {
                "source": str(live),
                "store_bytes": store.stat().st_size,
                "index_bytes": index.stat().st_size if index.is_file() else 0,
                "built_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    print(f"build_state: {state} ready")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
