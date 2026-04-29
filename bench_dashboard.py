"""Local benchmark for the /api/dashboard-data memo cache.

Runs one cold call (after invalidating) and N hot calls against the in-process
route function — no HTTP, no asyncio loop juggling beyond ``asyncio.run``.
Prints cold + hot latencies and confirms every payload is byte-identical so
regressions in the cache key, payload shape, or invalidation logic show up
immediately.

Usage::

    python bench_dashboard.py <project-slug> [--hot-iters N] [--tz-offset M]

The slug must exist in the store at ``deps.store_path`` — usually one ingested
through the normal ``stackunderflow init`` flow.
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import sys
import time

import stackunderflow.deps as deps
from stackunderflow.routes import data as data_route
from stackunderflow.store import db, queries


def _resolve_project_id(slug: str) -> int:
    conn = db.connect(deps.store_path)
    try:
        row = queries.get_project(conn, slug=slug)
    finally:
        conn.close()
    if row is None:
        print(f"error: no project found for slug {slug!r}", file=sys.stderr)
        raise SystemExit(2)
    return row.id


async def _time_call(tz_offset: int) -> tuple[float, dict, str]:
    t0 = time.perf_counter()
    payload = await data_route.get_dashboard_data(timezone_offset=tz_offset)
    elapsed_ms = (time.perf_counter() - t0) * 1000.0
    body = json.dumps(payload, sort_keys=True, default=str).encode("utf-8")
    digest = hashlib.md5(body).hexdigest()
    return elapsed_ms, payload, digest


async def _bench(slug: str, hot_iters: int, tz_offset: int) -> int:
    project_id = _resolve_project_id(slug)
    deps.current_log_path = f"/fake/{slug}"
    print(f"benching slug={slug!r} project_id={project_id} tz_offset={tz_offset}")

    # cold: invalidate so we always pay the full cost on the first call
    data_route.invalidate_dashboard_cache(slug)
    cold_ms, cold_payload, cold_digest = await _time_call(tz_offset)
    cold_bytes = len(json.dumps(cold_payload, sort_keys=True, default=str))
    print(f"cold: {cold_ms:7.1f} ms   md5={cold_digest}   bytes={cold_bytes:,}")

    hot_ms: list[float] = []
    digests = {cold_digest}
    for i in range(hot_iters):
        ms, _, digest = await _time_call(tz_offset)
        hot_ms.append(ms)
        digests.add(digest)
        print(f"hot {i+1}: {ms:7.1f} ms   md5={digest}")

    if len(digests) != 1:
        print(f"error: payloads differed across calls: {digests}", file=sys.stderr)
        return 1

    avg_hot = sum(hot_ms) / len(hot_ms) if hot_ms else float("nan")
    speedup = cold_ms / avg_hot if avg_hot else float("nan")
    print(
        f"\nsummary: cold={cold_ms:.1f}ms  hot_avg={avg_hot:.1f}ms"
        f"  speedup={speedup:.1f}x  payload={cold_bytes:,} bytes"
    )

    # surface the size of the first paginated message slice — useful when
    # tuning ``messages_initial_load`` or sanity-checking that the lean
    # payload still carries the page the dashboard renders on first load.
    first_page = cold_payload.get("messages_page", [])
    print(f"first_page entries: {len(first_page)}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("slug", help="project slug as it appears in the store")
    parser.add_argument("--hot-iters", type=int, default=5, help="hot calls after the cold miss")
    parser.add_argument("--tz-offset", type=int, default=0, help="timezone_offset query arg")
    args = parser.parse_args(argv)
    return asyncio.run(_bench(args.slug, args.hot_iters, args.tz_offset))


if __name__ == "__main__":
    raise SystemExit(main())
