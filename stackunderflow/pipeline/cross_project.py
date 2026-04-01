"""Roll up per-project statistics into a single global view.

Returns all available daily data (not capped to 30 days) so the frontend
can filter by any time range.
"""

from __future__ import annotations

import asyncio
import logging
from collections import defaultdict
from datetime import datetime
from typing import Any

_log = logging.getLogger(__name__)


# ── public API ───────────────────────────────────────────────────────────────

async def aggregate(
    projects: list[dict],
    mem_cache: Any,
    disk_cache: Any,
) -> dict:
    """Combine stats from every project into a single summary."""

    # accumulators — collect ALL days, not just a fixed window
    tok_by_day = defaultdict(lambda: {"input": 0, "output": 0})
    cost_by_day: dict[str, float] = defaultdict(float)
    cost_parts_by_day = defaultdict(lambda: {"input": 0.0, "output": 0.0, "cache": 0.0})
    # per-model daily cost: day → model → cost
    model_day_cost: dict[str, dict[str, float]] = defaultdict(lambda: defaultdict(float))
    # per-model totals
    model_totals: dict[str, dict[str, Any]] = defaultdict(lambda: {
        "count": 0, "input_tokens": 0, "output_tokens": 0, "cost": 0.0,
    })

    lifetime = _LifetimeTotals()

    for proj in projects:
        stats = _pull(proj, mem_cache, disk_cache)
        if stats is None:
            continue

        _accum_lifetime(stats, lifetime)
        _accum_daily_all(stats, tok_by_day, cost_by_day, cost_parts_by_day, model_day_cost)
        _accum_models(stats, model_totals)

    # sort all collected days chronologically
    all_days = sorted(set(tok_by_day.keys()) | set(cost_by_day.keys()))

    # collect all model names seen
    all_models = sorted({m for d in model_day_cost.values() for m in d} | set(model_totals.keys()))

    return {
        "total_projects": len(projects),
        "first_use_date": lifetime.earliest.isoformat() if lifetime.earliest else None,
        "last_use_date": lifetime.latest.isoformat() if lifetime.latest else None,
        "total_input_tokens": lifetime.inp,
        "total_output_tokens": lifetime.out,
        "total_cache_read_tokens": lifetime.cr,
        "total_cache_write_tokens": lifetime.cw,
        "total_commands": lifetime.cmds,
        "total_cost": lifetime.cost,
        "models": {m: dict(model_totals[m]) for m in all_models},
        "daily_token_usage": [
            {"date": ds, "input": tok_by_day[ds]["input"], "output": tok_by_day[ds]["output"]}
            for ds in all_days
        ],
        "daily_costs": [
            {
                "date": ds,
                "cost": cost_by_day[ds],
                "input_cost": cost_parts_by_day[ds]["input"],
                "output_cost": cost_parts_by_day[ds]["output"],
                "cache_cost": cost_parts_by_day[ds]["cache"],
                "by_model": dict(model_day_cost[ds]),
            }
            for ds in all_days
        ],
    }


async def background_process(
    projects: list[dict],
    mem_cache: Any,
    disk_cache: Any,
    cap: int = 5,
) -> int:
    """Process uncached projects in the background.  Returns count processed."""
    from stackunderflow.pipeline import process as run_pipeline

    done = 0
    pending = [p for p in projects if not p.get("in_cache") and not p.get("stats")]
    for proj in pending[:cap]:
        try:
            lp = proj["log_path"]
            messages, stats = await asyncio.to_thread(run_pipeline, lp)
            disk_cache.persist_stats(lp, stats)
            disk_cache.persist_messages(lp, messages)
            mem_cache.store(lp, messages, stats)
            done += 1
            await asyncio.sleep(0.1)
        except Exception as exc:
            _log.error("Background process %s: %s", proj.get("dir_name"), exc)
    return done


# ── internals ────────────────────────────────────────────────────────────────

class _LifetimeTotals:
    __slots__ = ("inp", "out", "cr", "cw", "cmds", "cost", "earliest", "latest")

    def __init__(self) -> None:
        self.inp = self.out = self.cr = self.cw = self.cmds = 0
        self.cost = 0.0
        self.earliest: datetime | None = None
        self.latest: datetime | None = None


def _pull(proj: dict, mem: Any, disk: Any) -> dict | None:
    lp = proj["log_path"]
    if proj.get("in_cache"):
        hit = mem.fetch(lp)
        if hit:
            return hit[1]
    return disk.load_stats(lp)


def _accum_lifetime(stats: dict, lt: _LifetimeTotals) -> None:
    ov = stats.get("overview", {})
    t = ov.get("total_tokens", {})
    lt.inp += t.get("input", 0)
    lt.out += t.get("output", 0)
    lt.cr += t.get("cache_read", 0)
    lt.cw += t.get("cache_creation", 0)
    lt.cmds += stats.get("user_interactions", {}).get("user_commands_analyzed", 0)
    lt.cost += ov.get("total_cost", 0)

    date_range = ov.get("date_range", {})
    for raw, attr in [(date_range.get("start"), "earliest"), (date_range.get("end"), "latest")]:
        if not raw:
            continue
        try:
            dt = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        except (ValueError, TypeError):
            continue
        current = getattr(lt, attr)
        if attr == "earliest":
            if current is None or dt < current:
                lt.earliest = dt
        else:
            if current is None or dt > current:
                lt.latest = dt


def _accum_daily_all(
    stats: dict,
    tok: dict,
    cost: dict,
    parts: dict,
    model_day: dict,
) -> None:
    """Accumulate daily data from a project — no date filtering."""
    block = stats.get("daily_stats")
    if not isinstance(block, dict):
        return
    for ds, day in block.items():
        t = day.get("tokens", {})
        tok[ds]["input"] += t.get("input", 0)
        tok[ds]["output"] += t.get("output", 0)
        cd = day.get("cost", {})
        cost[ds] += cd.get("total", 0)
        for model_name, mc in cd.get("by_model", {}).items():
            mc_total = mc.get("total_cost", 0)
            model_day[ds][model_name] += mc_total
            parts[ds]["input"] += mc.get("input_cost", 0)
            parts[ds]["output"] += mc.get("output_cost", 0)
            parts[ds]["cache"] += mc.get("cache_creation_cost", 0) + mc.get("cache_read_cost", 0)


def _accum_models(stats: dict, totals: dict) -> None:
    """Accumulate per-model totals from a project."""
    for model, data in stats.get("models", {}).items():
        t = totals[model]
        t["count"] += data.get("count", 0)
        t["input_tokens"] += data.get("input_tokens", 0)
        t["output_tokens"] += data.get("output_tokens", 0)
        from stackunderflow.infra.costs import compute_cost
        tokens = {
            "input": data.get("input_tokens", 0),
            "output": data.get("output_tokens", 0),
            "cache_creation": data.get("cache_creation_tokens", 0),
            "cache_read": data.get("cache_read_tokens", 0),
        }
        t["cost"] += compute_cost(tokens, model)["total_cost"]
