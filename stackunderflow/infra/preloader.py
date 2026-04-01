"""Eagerly populate the cache with recently-active projects.

Uses a priority queue sorted by recency so the most relevant projects
are available first.  Processing is chunked with cooperative yields so
the event loop stays responsive during server startup.
"""

from __future__ import annotations

import asyncio
import heapq
import logging
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from stackunderflow.infra.cache import TieredCache

_log = logging.getLogger(__name__)

_INTER_PROJECT_PAUSE = 0.4   # seconds between projects
_INTRA_YIELD = 0.05          # yield before each heavy operation


async def warm(
    cache: TieredCache,
    current_log_path: str | None,
    *,
    skip_current: bool = False,
    cap: int | None = None,
) -> None:
    """Load the *cap* most-recently-touched projects into the cache."""
    if cap is None:
        from stackunderflow.settings import Settings
        cap = Settings().cache_warm_on_startup

    root = Path.home() / ".claude" / "projects"
    if not root.exists():
        return

    # Build a min-heap of (-mtime, path) so we pop most-recent first
    heap: list[tuple[float, Path]] = []
    for d in root.iterdir():
        if not d.is_dir():
            continue
        logs = list(d.glob("*.jsonl"))
        if not logs:
            continue
        if skip_current and str(d) == current_log_path:
            continue
        newest_mtime = max(f.stat().st_mtime for f in logs)
        heapq.heappush(heap, (-newest_mtime, d))

    from stackunderflow.pipeline import process as run_pipeline

    loaded = 0
    while heap and loaded < cap:
        _, log_dir = heapq.heappop(heap)
        lp = str(log_dir)

        # already warm?
        if cache.fetch(lp) is not None:
            continue

        await asyncio.sleep(_INTRA_YIELD)

        try:
            messages, stats = await asyncio.to_thread(run_pipeline, lp)
            cache.persist_stats(lp, stats)
            cache.persist_messages(lp, messages)
            cache.store(lp, messages, stats, force=True)
            loaded += 1
        except Exception as exc:
            _log.info("Preload skipped %s: %s", log_dir.name, exc)

        await asyncio.sleep(_INTER_PROJECT_PAUSE)

    _log.debug("Preloader finished: %d/%d projects loaded", loaded, cap)
