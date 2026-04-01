"""Compute analytics from an enriched dataset.

Instead of N separate passes over the record list (one per stat section),
we use a collector-based approach: a single sweep feeds every collector
simultaneously, then each collector serialises its section of the output.
This is both faster and structurally distinct from the per-section-function
pattern used elsewhere.
"""

from __future__ import annotations

from collections import Counter, defaultdict
from datetime import datetime, timedelta
from functools import lru_cache
from pathlib import Path
from typing import Any

from stackunderflow.infra.costs import compute_cost

from .classifier import INTERRUPT_API, INTERRUPT_PREFIX
from .enricher import EnrichedDataset, Interaction, Record

# ── public entry ─────────────────────────────────────────────────────────────

def summarise(
    ds: EnrichedDataset,
    log_dir: str,
    *,
    tz_offset: int = 0,
) -> dict[str, Any]:
    """Produce the full statistics dict matching the API contract."""

    # single-pass collectors
    tools_c     = _ToolsCollector()
    models_c    = _ModelsCollector()
    sessions_c  = _SessionsCollector()
    errors_c    = _ErrorsCollector()
    cache_c     = _CacheCollector()

    for rec in ds.records:
        tools_c.ingest(rec)
        models_c.ingest(rec)
        sessions_c.ingest(rec)
        errors_c.ingest(rec)
        cache_c.ingest(rec)

    return {
        "overview":          _build_overview(ds, log_dir, tools_c),
        "tools":             tools_c.result(),
        "sessions":          sessions_c.result(),
        "daily_stats":       _daily(ds.records, ds.interactions, tz_offset),
        "hourly_pattern":    _hourly(ds.records, tz_offset),
        "errors":            errors_c.result(ds.records),
        "models":            models_c.result(),
        "user_interactions": _command_analysis(ds.records, ds.interactions),
        "cache":             cache_c.result(),
    }


# ── overview (needs data from multiple collectors) ───────────────────────────

def _build_overview(ds: EnrichedDataset, log_dir: str, tc: _ToolsCollector) -> dict:
    recs = ds.records
    tok = Counter[str]()
    for r in recs:
        for k, v in r.tokens.items():
            tok[k] += v

    name = "Unknown Project"
    dir_name = Path(log_dir).name
    if "/.claude/projects/" in log_dir:
        tail = log_dir.rsplit("/.claude/projects/", 1)[-1]
        name = tail.lstrip("-").replace("-", "/").rsplit("/", 1)[-1] if tail else name

    kind_counts: dict[str, int] = Counter(r.kind for r in recs)

    return {
        "project_name": name,
        "log_dir_name": dir_name,
        "project_path": log_dir,
        "total_messages": len(recs),
        "date_range": _time_bounds(recs),
        "sessions": len({r.session_id for r in recs}),
        "message_types": dict(kind_counts),
        "total_tokens": dict(tok),
        "total_cost": _aggregate_cost(recs),
    }


# ── collectors ───────────────────────────────────────────────────────────────

class _ToolsCollector:
    def __init__(self) -> None:
        self.usage: Counter[str] = Counter()
        self.errs: Counter[str] = Counter()

    def ingest(self, r: Record) -> None:
        for t in r.tools:
            self.usage[t["name"]] += 1
        if r.is_error:
            for t in r.tools:
                self.errs[t["name"]] += 1

    def result(self) -> dict:
        rates = {n: (self.errs[n] / c if c else 0) for n, c in self.usage.items()}
        return {"usage_counts": dict(self.usage), "error_counts": dict(self.errs), "error_rates": rates}


class _ModelsCollector:
    def __init__(self) -> None:
        self._data: dict[str, dict[str, int]] = {}

    def ingest(self, r: Record) -> None:
        if r.kind != "assistant" or r.model == "N/A":
            return
        m = self._data.setdefault(r.model, {
            "count": 0, "input_tokens": 0, "output_tokens": 0,
            "cache_creation_tokens": 0, "cache_read_tokens": 0,
        })
        m["count"] += 1
        m["input_tokens"] += r.tokens.get("input", 0)
        m["output_tokens"] += r.tokens.get("output", 0)
        m["cache_creation_tokens"] += r.tokens.get("cache_creation", 0)
        m["cache_read_tokens"] += r.tokens.get("cache_read", 0)

    def result(self) -> dict:
        return dict(self._data)


class _SessionsCollector:
    def __init__(self) -> None:
        self._s: dict[str, dict] = {}

    def ingest(self, r: Record) -> None:
        s = self._s.setdefault(r.session_id, {"n": 0, "t0": "", "t1": "", "errs": 0})
        s["n"] += 1
        if r.timestamp:
            if not s["t0"] or r.timestamp < s["t0"]:
                s["t0"] = r.timestamp
            if not s["t1"] or r.timestamp > s["t1"]:
                s["t1"] = r.timestamp
        if r.is_error:
            s["errs"] += 1

    def result(self) -> dict:
        durations: list[float] = []
        for s in self._s.values():
            if s["t0"] and s["t1"]:
                try:
                    d = (_parse_ts(s["t1"]) - _parse_ts(s["t0"])).total_seconds()
                    if d > 0:
                        durations.append(d)
                except (ValueError, TypeError):
                    pass
        n = len(self._s)
        return {
            "count": n,
            "average_duration_seconds": sum(durations) / len(durations) if durations else 0,
            "average_messages": sum(s["n"] for s in self._s.values()) / n if n else 0,
            "sessions_with_errors": sum(1 for s in self._s.values() if s["errs"]),
        }


class _ErrorsCollector:
    def __init__(self) -> None:
        self._cats: Counter[str] = Counter()
        self._details: list[dict] = []
        self._by_kind: Counter[str] = Counter()
        self._total = 0

    def ingest(self, r: Record) -> None:
        if not r.is_error:
            return
        self._total += 1
        self._by_kind[r.kind] += 1
        if r.timestamp:
            self._details.append({"timestamp": r.timestamp, "session_id": r.session_id, "model": r.model})
        cat = r.error_category
        if cat:
            self._cats[cat] += 1
        else:
            self._cats["Other"] += 1

    def result(self, all_records: list[Record]) -> dict:
        asst_details: list[dict] = []
        for i, r in enumerate(all_records):
            if r.kind == "assistant" and r.timestamp:
                nxt_err = all_records[i + 1].is_error if i + 1 < len(all_records) else False
                asst_details.append({"timestamp": r.timestamp, "is_error": nxt_err})
        return {
            "total": self._total,
            "rate": self._total / len(all_records) if all_records else 0,
            "by_type": dict(self._by_kind),
            "by_category": dict(self._cats),
            "error_details": self._details,
            "assistant_details": asst_details,
        }


class _CacheCollector:
    def __init__(self) -> None:
        self.created = self.read = self.w_created = self.w_read = self.asst = 0

    def ingest(self, r: Record) -> None:
        if r.kind != "assistant":
            return
        self.asst += 1
        cc = r.tokens.get("cache_creation", 0)
        cr = r.tokens.get("cache_read", 0)
        if cc:
            self.w_created += 1
            self.created += cc
        if cr:
            self.w_read += 1
            self.read += cr

    def result(self) -> dict:
        hr = (self.w_read / self.asst * 100) if self.asst else 0
        eff = (self.read / self.created * 100) if self.created else 0
        roi = ((self.read / self.created - 1) * 100) if self.created else 0
        saved = self.read - self.created
        cost_saved = self.read * 0.9 - self.created * 0.25
        return {
            "total_created": self.created,
            "total_read": self.read,
            "messages_with_cache_read": self.w_read,
            "messages_with_cache_created": self.w_created,
            "assistant_messages": self.asst,
            "hit_rate": round(hr, 1),
            "efficiency": round(min(100, eff), 1),
            "tokens_saved": saved,
            "cost_saved_base_units": round(cost_saved, 2),
            "break_even_achieved": self.read > self.created,
            "cache_roi": round(roi, 1),
        }


# ── time-bucketed stats (daily / hourly) ────────────────────────────────────

def _daily(
    records: list[Record],
    interactions: list[Interaction],
    tz_offset: int,
) -> dict:
    buckets: dict[str, _DayBucket] = {}

    for r in records:
        day = _local_day(r.timestamp, tz_offset)
        if day is None:
            continue
        b = buckets.setdefault(day, _DayBucket())
        b.msgs += 1
        b.session_ids.add(r.session_id)
        if r.is_error:
            b.errs += 1
        if r.kind == "assistant":
            b.asst += 1
            if r.model and r.model != "N/A":
                md = b.model_tokens.setdefault(r.model, Counter())
                for k, v in r.tokens.items():
                    md[k] += v
        for k, v in r.tokens.items():
            b.tokens[k] += v

    # interruption tracking via sorted scan
    ordered = sorted(records, key=lambda r: r.timestamp or "")
    for i, r in enumerate(ordered):
        if r.kind != "user" or r.has_tool_result or not r.timestamp:
            continue
        if _is_interrupt_text(r.content):
            continue
        day = _local_day(r.timestamp, tz_offset)
        if day is None:
            continue
        b = buckets.setdefault(day, _DayBucket())
        b.user_cmds += 1
        if _next_is_interrupt(ordered, i):
            b.int_cmds += 1

    out: dict[str, dict] = {}
    for day, b in buckets.items():
        day_cost = 0.0
        model_costs: dict[str, dict] = {}
        for model, tok_c in b.model_tokens.items():
            cb = compute_cost(dict(tok_c), model)
            model_costs[model] = cb
            day_cost += cb["total_cost"]

        ir = (b.int_cmds / b.user_cmds * 100) if b.user_cmds else 0
        er = (b.errs / b.asst * 100) if b.asst else 0
        out[day] = {
            "messages": b.msgs,
            "sessions": len(b.session_ids),
            "tokens": dict(b.tokens),
            "cost": {"total": day_cost, "by_model": model_costs},
            "user_commands": b.user_cmds,
            "interrupted_commands": b.int_cmds,
            "interruption_rate": round(ir, 1),
            "errors": b.errs,
            "assistant_messages": b.asst,
            "error_rate": round(er, 1),
        }
    return out


class _DayBucket:
    __slots__ = ("msgs", "tokens", "session_ids", "model_tokens",
                 "user_cmds", "int_cmds", "errs", "asst")

    def __init__(self) -> None:
        self.msgs = 0
        self.tokens: Counter[str] = Counter()
        self.session_ids: set[str] = set()
        self.model_tokens: dict[str, Counter[str]] = {}
        self.user_cmds = 0
        self.int_cmds = 0
        self.errs = 0
        self.asst = 0


def _hourly(records: list[Record], tz_offset: int) -> dict:
    msg_h: Counter[int] = Counter()
    tok_h: dict[int, Counter[str]] = defaultdict(Counter)
    for r in records:
        h = _local_hour(r.timestamp, tz_offset)
        if h is None:
            continue
        msg_h[h] += 1
        for k, v in r.tokens.items():
            tok_h[h][k] += v
    return {
        "messages": {h: msg_h.get(h, 0) for h in range(24)},
        "tokens": {
            h: dict(tok_h[h]) if h in tok_h
            else {"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}
            for h in range(24)
        },
    }


# ── command analysis ─────────────────────────────────────────────────────────

# Search tool classification uses a trie-like approach: tool names are checked
# against a set of known file-search tools, and Bash commands are inspected
# by extracting the leading verb from each pipeline segment.

_FILE_SEARCH_TOOLS = {"Grep", "Glob", "LS"}

# Bash verbs that indicate a search operation (first token in a pipe segment)
_SEARCH_VERBS = {"grep", "rg", "find", "fd", "locate", "which", "whereis", "ls", "ag", "ack"}


def _is_search_invocation(tool: dict) -> bool:
    """Determine whether a single tool-use block represents a search."""
    name = tool.get("name", "")
    if name in _FILE_SEARCH_TOOLS:
        return True
    if name != "Bash":
        return False
    cmd = tool.get("input", {}).get("command", "")
    return _cmd_has_search_verb(cmd) if cmd else False


@lru_cache(maxsize=512)
def _cmd_has_search_verb(cmd: str) -> bool:
    """Check if any segment of a shell command starts with a search verb."""
    for segment in cmd.lower().split("|"):
        segment = segment.strip()
        if not segment:
            continue
        # handle && and ; separators within segments
        for sub in segment.replace("&&", ";").split(";"):
            tokens = sub.strip().split()
            if tokens and tokens[0] in _SEARCH_VERBS:
                return True
    return False


def _count_search_tools(tools: list[dict]) -> int:
    return sum(1 for t in tools if _is_search_invocation(t))


# ── public: timezone-sensitive stat recomputation from cached dicts ──────────

def recompute_tz_stats(
    messages: list[dict],
    tz_offset: int,
) -> dict[str, Any]:
    """Recompute only the timezone-sensitive stats (daily_stats, hourly_pattern)
    from already-formatted message dicts.

    This avoids reconstructing Record objects in route handlers.  The function
    builds lightweight proxy objects that satisfy _daily / _hourly expectations.
    """
    recs = [_DictProxy(m) for m in messages]
    return {
        "daily_stats": _daily(recs, [], tz_offset),  # type: ignore[arg-type]
        "hourly_pattern": _hourly(recs, tz_offset),  # type: ignore[arg-type]
    }


class _DictProxy:
    """Lightweight adapter so _daily/_hourly can read message dicts as if they were Records."""
    __slots__ = ("timestamp", "session_id", "kind", "model", "tokens",
                 "is_error", "content", "has_tool_result")

    def __init__(self, m: dict) -> None:
        self.timestamp = m.get("timestamp", "")
        self.session_id = m.get("session_id", "")
        self.kind = m.get("type", "assistant")
        self.model = m.get("model", "N/A")
        self.tokens = m.get("tokens", {})
        self.is_error = m.get("error", False)
        self.content = m.get("content", "")
        self.has_tool_result = m.get("has_tool_result", False)


def _command_analysis(records: list[Record], interactions: list[Interaction]) -> dict:
    ordered = sorted(records, key=lambda r: r.timestamp or "")

    # interaction lookup
    ix_lut: dict[str, Interaction] = {}
    for ix in interactions:
        ix_lut[f"{ix.command.timestamp}|{ix.command.content[:64]}"] = ix

    details: list[dict] = []
    n_cmds = n_tooled = total_tools = total_search = total_steps = 0
    dist: Counter[int] = Counter()

    for i, r in enumerate(ordered):
        if r.kind != "user" or r.has_tool_result:
            continue
        is_int = _is_interrupt_text(r.content)

        key = f"{r.timestamp}|{r.content[:64]}"
        ix = ix_lut.get(key)

        if ix:
            tc, model, steps = ix.tool_count, ix.model, ix.assistant_steps
            tnames = [t.get("name", "?") for t in ix.tools_used[:tc]]
            search_n = _count_search_tools(ix.tools_used[:tc])
        else:
            tc, model, steps, tnames, search_n = _scan_forward(ordered, i)

        followed = _next_is_interrupt(ordered, i)
        est_tok = max(1.0, len(r.content) / 4)

        details.append({
            "user_message": r.content,
            "user_message_truncated": (r.content[:100] + "...") if len(r.content) > 100 else r.content,
            "timestamp": r.timestamp,
            "session_id": r.session_id,
            "tools_used": tc,
            "tool_names": tnames,
            "has_tools": tc > 0,
            "assistant_steps": steps,
            "model": model,
            "is_interruption": is_int,
            "followed_by_interruption": followed,
            "estimated_tokens": est_tok,
            "search_tools_used": search_n,
        })

        if not is_int:
            n_cmds += 1
            total_steps += steps
            total_search += search_n
            if tc:
                n_tooled += 1
                total_tools += tc
            dist[tc] += 1

    # interruption rates
    non_int = sum(1 for d in details if not d["is_interruption"])
    int_followed = sum(1 for d in details if not d["is_interruption"] and d["followed_by_interruption"])
    ir = (int_followed / non_int * 100) if non_int else 0

    by_tc: dict[int, dict] = {}
    tc_buckets: dict[int, list[bool]] = defaultdict(list)
    for d in details:
        if not d["is_interruption"]:
            tc_buckets[d["tools_used"]].append(d["followed_by_interruption"])
    for tc_val, flags in tc_buckets.items():
        n_int = sum(flags)
        by_tc[tc_val] = {
            "rate": round(n_int / len(flags) * 100, 1) if flags else 0,
            "total_commands": len(flags),
            "interrupted_commands": n_int,
        }

    mdist: Counter[str] = Counter()
    for d in details:
        if not d["is_interruption"] and d["model"] != "N/A":
            mdist[d["model"]] += 1

    tok_sum = sum(d["estimated_tokens"] for d in details if not d["is_interruption"])
    pct_t = (n_tooled / n_cmds * 100) if n_cmds else 0
    avg_t = total_tools / n_cmds if n_cmds else 0
    avg_tw = total_tools / n_tooled if n_tooled else 0
    avg_s = total_steps / n_cmds if n_cmds else 0
    avg_tok = tok_sum / n_cmds if n_cmds else 0
    ni_with_tools = sum(1 for d in details if d["has_tools"] and not d["is_interruption"])
    pct_st = (ni_with_tools / n_cmds * 100) if n_cmds else 0
    srch_pct = (total_search / total_tools * 100) if total_tools else 0

    return {
        "real_user_messages": sum(1 for r in records if r.kind == "user" and not r.has_tool_result),
        "user_commands_analyzed": n_cmds,
        "commands_requiring_tools": n_tooled,
        "commands_without_tools": n_cmds - n_tooled,
        "percentage_requiring_tools": round(pct_t, 1),
        "total_tools_used": total_tools,
        "total_search_tools": total_search,
        "search_tool_percentage": round(srch_pct, 1),
        "total_assistant_steps": total_steps,
        "avg_tools_per_command": round(avg_t, 2),
        "avg_tools_when_used": round(avg_tw, 2),
        "avg_steps_per_command": round(avg_s, 2),
        "avg_tokens_per_command": round(avg_tok, 1),
        "percentage_steps_with_tools": round(pct_st, 1),
        "tool_count_distribution": dict(dist),
        "command_details": details,
        "interruption_rate": round(ir, 1),
        "non_interruption_commands": non_int,
        "commands_followed_by_interruption": int_followed,
        "tool_interruption_rates": by_tc,
        "model_distribution": dict(mdist),
    }


def _scan_forward(ordered: list[Record], idx: int) -> tuple[int, str, int, list[str], int]:
    tc = 0
    model = "N/A"
    steps = 0
    names: list[str] = []
    search = 0
    j = idx + 1
    while j < len(ordered):
        nxt = ordered[j]
        if nxt.kind == "user" and not nxt.has_tool_result:
            break
        if nxt.kind == "assistant":
            steps += 1
            if nxt.model and nxt.model != "N/A":
                model = nxt.model
            for t in nxt.tools:
                tc += 1
                names.append(t.get("name", "?"))
                if _is_search_invocation(t):
                    search += 1
        j += 1
    return tc, model, steps, names, search


# ── small helpers ────────────────────────────────────────────────────────────

def _parse_ts(ts: str) -> datetime:
    return datetime.fromisoformat(ts.replace("Z", "+00:00"))


def _local_day(ts: str, offset: int) -> str | None:
    if not ts:
        return None
    try:
        return (_parse_ts(ts) + timedelta(minutes=offset)).strftime("%Y-%m-%d")
    except (ValueError, TypeError):
        return None


def _local_hour(ts: str, offset: int) -> int | None:
    if not ts:
        return None
    try:
        return (_parse_ts(ts) + timedelta(minutes=offset)).hour
    except (ValueError, TypeError):
        return None


def _time_bounds(recs: list[Record]) -> dict:
    stamps = [r.timestamp for r in recs if r.timestamp]
    return {"start": min(stamps), "end": max(stamps)} if stamps else {"start": None, "end": None}


def _aggregate_cost(recs: list[Record]) -> float:
    by_model: dict[str, Counter[str]] = {}
    for r in recs:
        if r.kind == "assistant" and r.model and r.model != "N/A":
            c = by_model.setdefault(r.model, Counter())
            for k, v in r.tokens.items():
                c[k] += v
    return sum(compute_cost(dict(c), m)["total_cost"] for m, c in by_model.items())


def _is_interrupt_text(text: str) -> bool:
    return text.startswith(INTERRUPT_PREFIX) or text.startswith(INTERRUPT_API)


def _next_is_interrupt(ordered: list[Record], idx: int) -> bool:
    j = idx + 1
    while j < len(ordered):
        nxt = ordered[j]
        if nxt.kind == "assistant" and nxt.content.strip() == INTERRUPT_API:
            return True
        if nxt.kind == "user" and not nxt.has_tool_result:
            return _is_interrupt_text(nxt.content)
        j += 1
    return False
