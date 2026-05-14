"""Bench the 4 ``optimize`` detectors: ``message_tool_mart`` vs raw scan.

Runs each detector twice on a side-loaded copy of the user's store:

1. ``mart`` path — ``message_tool_mart`` fully populated (what ships).
2. ``raw`` path — same store with ``message_tool_mart`` *emptied*, so the
   detector falls through to the raw-``messages`` scan.

Reports ms per detector for both paths plus the speedup. Run with::

    python bench_optimize_mart.py /tmp/wt-optimize/test-store.db
"""

from __future__ import annotations

import shutil
import sys
import time
from pathlib import Path

from stackunderflow.reports import optimize as optimize_mod
from stackunderflow.reports.optimize import (
    _detect_bash_output_limits,
    _detect_ghost_agents,
    _detect_junk_reads,
    _detect_low_read_edit_ratio,
)
from stackunderflow.reports.scope import Scope
from stackunderflow.store import db


def _bench_detector(label, fn, conn):
    t0 = time.perf_counter()
    findings = fn(conn)
    dt = (time.perf_counter() - t0) * 1000
    return label, dt, len(findings)


def _run_all(conn, scope):
    return [
        _bench_detector("junk_reads",         lambda c: _detect_junk_reads(c, scope=scope), conn),
        _bench_detector("bash_output_limits", lambda c: _detect_bash_output_limits(c, scope=scope), conn),
        _bench_detector("low_read_edit",      lambda c: _detect_low_read_edit_ratio(c, scope=scope), conn),
        _bench_detector("ghost_agents",       lambda c: _detect_ghost_agents(c, scope=scope), conn),
    ]


def main():
    if len(sys.argv) != 2:
        print("usage: python bench_optimize_mart.py <store.db>")
        raise SystemExit(2)
    src = Path(sys.argv[1])
    if not src.is_file():
        print(f"no such file: {src}")
        raise SystemExit(2)

    # Bench ghost_agents with synthetic registered agents so neither path
    # short-circuits at the "no agents to ghost" guard. We don't probe the
    # real ~/.claude/agents/ directory — that varies per machine and is
    # outside the project blast radius this script wants to measure.
    optimize_mod._registered_agents = lambda: [
        ("explorer-bench", Path("/tmp/explorer-bench.md")),
        ("reviewer-bench", Path("/tmp/reviewer-bench.md")),
        ("writer-bench", Path("/tmp/writer-bench.md")),
    ]

    # All-time scope so we exercise the whole mart / messages table.
    scope = Scope(label="all", since=None, until=None)

    print(f"source: {src}")
    print(f"size: {src.stat().st_size / 1e9:.1f} GB")

    # ── A) mart path ───────────────────────────────────────────────────
    mart_db = Path("/tmp/bench-mart.db")
    if mart_db.exists():
        mart_db.unlink()
    shutil.copy2(src, mart_db)
    conn = db.connect(mart_db)
    n_mt = conn.execute("SELECT COUNT(*) AS n FROM message_tool_mart").fetchone()["n"]
    n_msg = conn.execute("SELECT COUNT(*) AS n FROM messages").fetchone()["n"]
    print(f"message_tool_mart rows: {n_mt:,}")
    print(f"messages rows:          {n_msg:,}")
    print()
    print("=== mart path (message_tool_mart populated) ===")
    mart_results = _run_all(conn, scope)
    for label, dt, n in mart_results:
        print(f"  {label:<22s}  {dt:8.1f} ms   findings={n}")
    conn.close()

    # ── B) raw path ────────────────────────────────────────────────────
    raw_db = Path("/tmp/bench-raw.db")
    if raw_db.exists():
        raw_db.unlink()
    shutil.copy2(src, raw_db)
    conn = db.connect(raw_db)
    conn.execute("DELETE FROM message_tool_mart")
    # ALSO empty tool_mart so the Wave 5 short-circuit doesn't intercept
    # — we want the full raw-json fallback to run for an honest comparison.
    try:
        conn.execute("DELETE FROM tool_mart")
    except Exception:  # noqa: BLE001
        pass
    conn.commit()
    print()
    print("=== raw path (message_tool_mart emptied, tool_mart emptied) ===")
    raw_results = _run_all(conn, scope)
    for label, dt, n in raw_results:
        print(f"  {label:<22s}  {dt:8.1f} ms   findings={n}")
    conn.close()

    # ── C) summary ─────────────────────────────────────────────────────
    print()
    print("=== speedup (raw / mart) ===")
    for (label, mart_dt, _), (_, raw_dt, _) in zip(mart_results, raw_results, strict=True):
        if mart_dt > 0:
            ratio = raw_dt / mart_dt
            print(f"  {label:<22s}  raw={raw_dt:8.1f}ms  mart={mart_dt:8.1f}ms  speedup={ratio:6.1f}x")
        else:
            print(f"  {label:<22s}  raw={raw_dt:8.1f}ms  mart={mart_dt:8.1f}ms  speedup=inf")


if __name__ == "__main__":
    main()
