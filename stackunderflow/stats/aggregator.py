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
    tools_c       = _ToolsCollector()
    models_c      = _ModelsCollector()
    sessions_c    = _SessionsCollector()
    errors_c      = _ErrorsCollector()
    cache_c       = _CacheCollector()

    # analytics-expansion collectors (§1.1 – §1.8)
    sess_cost_c   = _SessionCostCollector()
    tool_cost_c   = _ToolCostCollector()
    token_comp_c  = _TokenCompositionCollector(tz_offset)
    sess_eff_c    = _SessionEfficiencyCollector()
    err_cost_c    = _ErrorCostCollector()

    for rec in ds.records:
        tools_c.ingest(rec)
        models_c.ingest(rec)
        sessions_c.ingest(rec)
        errors_c.ingest(rec)
        cache_c.ingest(rec)
        sess_cost_c.ingest(rec)
        tool_cost_c.ingest(rec)
        token_comp_c.ingest(rec)
        sess_eff_c.ingest(rec)
        err_cost_c.ingest(rec)

    # interaction-driven collectors (§1.2, §1.5, §1.6)
    cmd_cost_c    = _CommandCostCollector()
    outlier_c     = _OutlierCollector()
    retry_c       = _RetryCollector()

    for ix in ds.interactions:
        cmd_cost_c.ingest_interaction(ix)
        outlier_c.ingest_interaction(ix)
        retry_c.ingest_interaction(ix)

    return {
        "overview":           _build_overview(ds, log_dir, tools_c),
        "tools":              tools_c.result(),
        "sessions":           sessions_c.result(),
        "daily_stats":        _daily(ds.records, ds.interactions, tz_offset),
        "hourly_pattern":     _hourly(ds.records, tz_offset),
        "errors":             errors_c.result(ds.records),
        "models":             models_c.result(),
        "user_interactions":  _command_analysis(ds.records, ds.interactions),
        "cache":              cache_c.result(),
        # ── analytics expansion (docs/specs/analytics-expansion.md §1) ────
        "session_costs":      _safe(lambda: sess_cost_c.result(ds.interactions), []),
        "command_costs":      _safe(cmd_cost_c.result, []),
        "tool_costs":         _safe(tool_cost_c.result, {}),
        "token_composition":  _safe(token_comp_c.result, _empty_token_composition()),
        "outliers":           _safe(outlier_c.result, {"high_tool_commands": [], "high_step_commands": []}),
        "retry_signals":      _safe(retry_c.result, []),
        "session_efficiency": _safe(sess_eff_c.result, []),
        "error_cost":         _safe(lambda: err_cost_c.result(ds.interactions), _empty_error_cost()),
        "trends":             _safe(lambda: _trends(ds.records, ds.interactions, tz_offset), _empty_trends()),
    }


def _safe(fn, fallback):
    """Invoke ``fn()`` and swallow any exception — collectors must never break
    the whole dashboard payload if a single section fails."""
    try:
        return fn()
    except Exception:  # noqa: BLE001 — graceful degradation per spec §4
        return fallback


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


# ── analytics expansion collectors (docs/specs/analytics-expansion.md §1) ───

class _SessionCostCollector:
    """§1.1 — per-session cost/tokens/messages/errors, ranked desc by cost."""

    def __init__(self) -> None:
        self._s: dict[str, dict] = {}

    def ingest(self, r: Record) -> None:
        s = self._s.setdefault(r.session_id, {
            "t0": "", "t1": "",
            "msgs": 0, "errs": 0,
            "tokens": Counter(),
            "by_model": {},
            "models": set(),
        })
        s["msgs"] += 1
        if r.is_error:
            s["errs"] += 1
        if r.timestamp:
            if not s["t0"] or r.timestamp < s["t0"]:
                s["t0"] = r.timestamp
            if not s["t1"] or r.timestamp > s["t1"]:
                s["t1"] = r.timestamp
        for k, v in r.tokens.items():
            s["tokens"][k] += v
        if r.kind == "assistant" and r.model and r.model != "N/A":
            s["models"].add(r.model)
            m = s["by_model"].setdefault(r.model, Counter())
            for k, v in r.tokens.items():
                m[k] += v

    def result(self, interactions: list[Interaction]) -> list[dict]:
        cmds_by_session: Counter[str] = Counter()
        first_prompt_by_session: dict[str, tuple[str, str]] = {}
        for ix in sorted(interactions, key=lambda ix: ix.start_time or ""):
            sid = ix.session_id
            cmds_by_session[sid] += 1
            if sid not in first_prompt_by_session:
                first_prompt_by_session[sid] = (ix.start_time or "", ix.command.content or "")

        out: list[dict] = []
        for sid, s in self._s.items():
            duration = 0.0
            if s["t0"] and s["t1"]:
                try:
                    duration = max(0.0, (_parse_ts(s["t1"]) - _parse_ts(s["t0"])).total_seconds())
                except (ValueError, TypeError):
                    duration = 0.0

            cost = 0.0
            for model, tok_c in s["by_model"].items():
                cost += compute_cost(dict(tok_c), model)["total_cost"]

            first = first_prompt_by_session.get(sid, ("", ""))[1]
            preview = _preview(first, 140)

            out.append({
                "session_id": sid,
                "started_at": s["t0"],
                "ended_at": s["t1"],
                "duration_s": duration,
                "cost": cost,
                "tokens": dict(s["tokens"]),
                "messages": s["msgs"],
                "commands": cmds_by_session.get(sid, 0),
                "errors": s["errs"],
                "first_prompt_preview": preview,
                "models_used": sorted(s["models"]),
            })
        out.sort(key=lambda x: x["cost"], reverse=True)
        return out


class _CommandCostCollector:
    """§1.2 — one entry per real user prompt (Interaction), top 50 desc by cost."""

    def __init__(self) -> None:
        self._items: list[dict] = []

    def ingest_interaction(self, ix: Interaction) -> None:
        tokens: Counter[str] = Counter()
        by_model: dict[str, Counter[str]] = {}
        had_error = False
        models_used: set[str] = set()
        for r in ix.responses + ix.tool_results:
            if r.is_error:
                had_error = True
            for k, v in r.tokens.items():
                tokens[k] += v
            if r.kind == "assistant" and r.model and r.model != "N/A":
                models_used.add(r.model)
                m = by_model.setdefault(r.model, Counter())
                for k, v in r.tokens.items():
                    m[k] += v

        cost = sum(compute_cost(dict(tok_c), model)["total_cost"] for model, tok_c in by_model.items())

        self._items.append({
            "interaction_id": ix.interaction_id,
            "session_id": ix.session_id,
            "timestamp": ix.start_time or "",
            "prompt_preview": _preview(ix.command.content or "", 200),
            "cost": cost,
            "tokens": dict(tokens),
            "tools_used": ix.tool_count,
            "steps": ix.assistant_steps,
            "models_used": sorted(models_used),
            "had_error": had_error,
        })

    def result(self) -> list[dict]:
        return sorted(self._items, key=lambda x: x["cost"], reverse=True)[:50]


class _ToolCostCollector:
    """§1.3 — per-tool cost with 1/N cost attribution across distinct tools per msg."""

    def __init__(self) -> None:
        self._data: dict[str, dict[str, float]] = {}

    def ingest(self, r: Record) -> None:
        if r.kind != "assistant" or not r.tools:
            return
        name_counts: Counter[str] = Counter(t.get("name", "?") for t in r.tools)
        distinct = list(name_counts.keys())
        if not distinct:
            return
        n = len(distinct)
        share = 1.0 / n
        msg_cost = 0.0
        if r.model and r.model != "N/A":
            msg_cost = compute_cost(r.tokens, r.model)["total_cost"]
        for name in distinct:
            d = self._data.setdefault(name, {
                "calls": 0,
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_tokens": 0,
                "cache_creation_tokens": 0,
                "cost": 0.0,
            })
            d["calls"] += name_counts[name]
            d["input_tokens"] += r.tokens.get("input", 0)
            d["output_tokens"] += r.tokens.get("output", 0)
            d["cache_read_tokens"] += r.tokens.get("cache_read", 0)
            d["cache_creation_tokens"] += r.tokens.get("cache_creation", 0)
            d["cost"] += msg_cost * share

    def result(self) -> dict:
        return {k: dict(v) for k, v in self._data.items()}


class _TokenCompositionCollector:
    """§1.4 — token totals per day, globally, and per session."""

    def __init__(self, tz_offset: int = 0) -> None:
        self._tz = tz_offset
        self._daily: dict[str, Counter[str]] = {}
        self._totals: Counter[str] = Counter()
        self._per_session: dict[str, Counter[str]] = {}

    def ingest(self, r: Record) -> None:
        if not r.tokens:
            return
        for k, v in r.tokens.items():
            self._totals[k] += v
        sess = self._per_session.setdefault(r.session_id, Counter())
        for k, v in r.tokens.items():
            sess[k] += v
        day = _local_day(r.timestamp, self._tz)
        if day is not None:
            d = self._daily.setdefault(day, Counter())
            for k, v in r.tokens.items():
                d[k] += v

    def result(self) -> dict:
        return {
            "daily":       {k: dict(v) for k, v in self._daily.items()},
            "totals":      dict(self._totals),
            "per_session": {k: dict(v) for k, v in self._per_session.items()},
        }


class _OutlierCollector:
    """§1.5 — interactions with abnormally high tool/step counts."""

    def __init__(self) -> None:
        self._high_tool: list[dict] = []
        self._high_step: list[dict] = []

    def ingest_interaction(self, ix: Interaction) -> None:
        tc, steps = ix.tool_count, ix.assistant_steps
        if tc <= 20 and steps <= 15:
            return
        by_model: dict[str, Counter[str]] = {}
        for r in ix.responses + ix.tool_results:
            if r.kind == "assistant" and r.model and r.model != "N/A":
                m = by_model.setdefault(r.model, Counter())
                for k, v in r.tokens.items():
                    m[k] += v
        cost = sum(compute_cost(dict(tok_c), model)["total_cost"] for model, tok_c in by_model.items())
        entry = {
            "interaction_id": ix.interaction_id,
            "session_id": ix.session_id,
            "timestamp": ix.start_time or "",
            "prompt_preview": _preview(ix.command.content or "", 200),
            "tool_count": tc,
            "step_count": steps,
            "cost": cost,
        }
        if tc > 20:
            self._high_tool.append(entry)
        if steps > 15:
            self._high_step.append(entry)

    def result(self) -> dict:
        return {
            "high_tool_commands": sorted(self._high_tool, key=lambda x: x["tool_count"], reverse=True),
            "high_step_commands": sorted(self._high_step, key=lambda x: x["step_count"], reverse=True),
        }


class _RetryCollector:
    """§1.6 / polish §A1 — retry signals inside an Interaction.

    A retry fires when the same tool is invoked ≥2 times AND at least one
    preceding invocation was *followed by an error*. The follow-up error can
    surface in three places (none of them on the assistant record itself,
    which is why the v1 detection missed every chimera retry):

    1. The next ``tool_result`` record has ``is_error=True``.
    2. The next ``tool_result``'s textual content starts with ``"Error"`` or
       ``"failed"`` (covers stderr-only outputs that the classifier didn't
       pick up).
    3. A subsequent assistant message in the same Interaction matches
       ``INTERRUPT_API`` / ``INTERRUPT_PREFIX``.
    """

    def __init__(self) -> None:
        self._items: list[dict] = []

    def ingest_interaction(self, ix: Interaction) -> None:
        events = sorted(
            list(ix.responses) + list(ix.tool_results),
            key=lambda r: r.timestamp or "",
        )
        if not events:
            return

        # Per-tool: ordered list of failure flags, one per invocation.
        per_tool_flags: dict[str, list[bool]] = {}
        per_tool_wasted: dict[str, int] = {}

        for i, r in enumerate(events):
            if r.kind != "assistant" or not r.tools:
                continue
            failed = _next_record_signals_error(events, i)
            out_tok = r.tokens.get("output", 0)
            for t in r.tools:
                name = t.get("name", "?")
                per_tool_flags.setdefault(name, []).append(failed)
                if failed:
                    per_tool_wasted[name] = per_tool_wasted.get(name, 0) + out_tok

        for name, flags in per_tool_flags.items():
            if len(flags) < 2:
                continue
            # "at least one *preceding* invocation was followed by an error" —
            # a retry signal needs a follow-up invocation, so the failure
            # must occur in any but the last slot.
            if not any(flags[:-1]):
                continue
            # consecutive_failures = longest run of failed invocations of this tool.
            run = max_run = 0
            for f in flags:
                if f:
                    run += 1
                    if run > max_run:
                        max_run = run
                else:
                    run = 0
            wt = per_tool_wasted.get(name, 0)
            wc = 0.0
            if ix.model and ix.model != "N/A" and wt:
                wc = compute_cost(
                    {"input": 0, "output": wt, "cache_creation": 0, "cache_read": 0},
                    ix.model,
                )["total_cost"]
            self._items.append({
                "interaction_id": ix.interaction_id,
                "session_id": ix.session_id,
                "timestamp": ix.start_time or "",
                "tool": name,
                "consecutive_failures": max_run,
                "total_invocations": len(flags),
                "estimated_wasted_tokens": wt,
                "estimated_wasted_cost": wc,
            })

    def result(self) -> list[dict]:
        return list(self._items)


def _next_record_signals_error(events: list[Record], idx: int) -> bool:
    """True iff the records following ``events[idx]`` indicate the tool batch
    just invoked ended in failure. See ``_RetryCollector`` for the full rule."""
    j = idx + 1
    while j < len(events):
        r = events[j]
        if r.kind == "assistant":
            txt = r.content or ""
            if txt.startswith(INTERRUPT_API) or txt.startswith(INTERRUPT_PREFIX):
                return True
            # Hit the next assistant turn without seeing an error in between.
            return False
        # tool_result record
        if r.is_error:
            return True
        stripped = (r.content or "").lstrip()
        if stripped.startswith("Error") or stripped.startswith("failed"):
            return True
        j += 1
    return False


# Search tool heuristic for §1.7 classification.
_SEARCH_BY_NAME = {"Grep", "Glob"}


def _is_search_tool_name(name: str) -> bool:
    return name in _SEARCH_BY_NAME or "search" in name.lower()


class _SessionEfficiencyCollector:
    """§1.7 — per-session tool-mix ratios, idle gaps, classification."""

    _IDLE_THRESHOLD_S = 30.0
    _IDLE_CLASS_RATIO = 0.4
    _EDIT_HEAVY_MIN = 0.25
    _RESEARCH_SUM_MIN = 0.6
    _RESEARCH_EDIT_MAX = 0.1

    def __init__(self) -> None:
        self._s: dict[str, dict] = {}

    def ingest(self, r: Record) -> None:
        s = self._s.setdefault(r.session_id, {
            "timestamps": [],
            "tools": Counter(),
        })
        if r.timestamp:
            s["timestamps"].append(r.timestamp)
        for t in r.tools:
            s["tools"][t.get("name", "?")] += 1

    def result(self) -> list[dict]:
        out: list[dict] = []
        for sid, s in self._s.items():
            total = sum(s["tools"].values())
            search = sum(c for n, c in s["tools"].items() if _is_search_tool_name(n))
            edit = s["tools"].get("Edit", 0) + s["tools"].get("Write", 0)
            read = s["tools"].get("Read", 0)
            bash = s["tools"].get("Bash", 0)
            sr = search / total if total else 0.0
            er = edit / total if total else 0.0
            rr = read / total if total else 0.0
            br = bash / total if total else 0.0

            times = sorted(t for t in s["timestamps"] if t)
            total_idle = max_idle = 0.0
            duration_s = 0.0
            if times:
                try:
                    duration_s = max(
                        0.0, (_parse_ts(times[-1]) - _parse_ts(times[0])).total_seconds()
                    )
                except (ValueError, TypeError):
                    duration_s = 0.0
            for a, b in zip(times, times[1:]):
                try:
                    gap = (_parse_ts(b) - _parse_ts(a)).total_seconds()
                except (ValueError, TypeError):
                    continue
                if gap >= self._IDLE_THRESHOLD_S:
                    total_idle += gap
                    if gap > max_idle:
                        max_idle = gap

            if er >= self._EDIT_HEAVY_MIN:
                classification = "edit-heavy"
            elif sr + rr >= self._RESEARCH_SUM_MIN and er < self._RESEARCH_EDIT_MAX:
                classification = "research-heavy"
            elif duration_s > 0 and total_idle > duration_s * self._IDLE_CLASS_RATIO:
                classification = "idle-heavy"
            else:
                classification = "balanced"

            out.append({
                "session_id": sid,
                "search_ratio": sr,
                "edit_ratio": er,
                "read_ratio": rr,
                "bash_ratio": br,
                "idle_gap_total_s": total_idle,
                "idle_gap_max_s": max_idle,
                "classification": classification,
            })
        return out


class _ErrorCostCollector:
    """§1.8 — total errors, retry-cost estimate, errors-by-tool, top interactions.

    The retry-cost estimate is *decoupled* from retry-signal detection: every
    error record is charged the output-token cost of the next assistant message
    in the same Interaction (or the error record's own output tokens if it is
    itself an assistant). Errors are expensive whether or not the agent loops.

    Tool attribution works by matching ``tool_use_id`` on error ``tool_result``
    blocks back to the ``tool_use`` blocks we observed on prior assistant
    records — tool_result error records themselves have no ``tools`` list.
    """

    def __init__(self) -> None:
        self.total_errors = 0
        # tool_use_id → tool_name, populated from every assistant record we see
        # so errors_by_tool can attribute tool_result errors correctly
        self._tool_id_to_name: dict[str, str] = {}

    def ingest(self, r: Record) -> None:
        if r.kind == "assistant":
            for t in r.tools:
                tid = t.get("id")
                name = t.get("name")
                if tid and name:
                    self._tool_id_to_name[tid] = name
        if r.is_error:
            self.total_errors += 1

    def result(self, interactions: list[Interaction]) -> dict:
        errors_by_tool: Counter[str] = Counter()
        est_retry_tokens = 0
        est_retry_cost = 0.0
        ranked: list[tuple[int, Interaction]] = []

        for ix in interactions:
            timeline = sorted(
                ix.responses + ix.tool_results,
                key=lambda rec: rec.timestamp or "",
            )
            err_count = 0
            for idx, rec in enumerate(timeline):
                if not rec.is_error:
                    continue
                err_count += 1
                # Tool attribution: match tool_use_id on error tool_result blocks.
                for name in self._tool_names_for_error(rec):
                    errors_by_tool[name] += 1
                # Fallback: if the error record carries its own tool_use blocks
                # (rare — mainly assistants flagged as errors), count those too.
                if rec.kind != "user":
                    for t in rec.tools:
                        nm = t.get("name")
                        if nm:
                            errors_by_tool[nm] += 1
                # Retry cost: own output tokens if assistant, else next assistant.
                tokens, model = self._retry_tokens_and_model(rec, timeline, idx)
                if tokens and model and model != "N/A":
                    est_retry_tokens += tokens
                    est_retry_cost += compute_cost(
                        {"input": 0, "output": tokens,
                         "cache_creation": 0, "cache_read": 0},
                        model,
                    )["total_cost"]
            if err_count > 0:
                ranked.append((err_count, ix))

        ranked.sort(key=lambda p: p[0], reverse=True)
        top_error_commands: list[dict] = [
            _interaction_to_outlier_command(ix) for _, ix in ranked[:10]
        ]

        return {
            "total_errors": self.total_errors,
            "estimated_retry_tokens": est_retry_tokens,
            "estimated_retry_cost": est_retry_cost,
            "errors_by_tool": dict(errors_by_tool),
            "top_error_commands": top_error_commands,
        }

    def _tool_names_for_error(self, r: Record) -> list[str]:
        """Resolve tool name(s) for an error record by inspecting the raw
        ``tool_result`` blocks and looking up their ``tool_use_id`` in the
        accumulated id→name map built from prior assistant tool_use blocks.
        Returns only tool names that could be resolved — we'd rather drop a
        noisy "Unknown" bucket than pollute the attribution.
        """
        raw = r.raw_data
        if not isinstance(raw, dict):
            return []
        msg = raw.get("message")
        if not isinstance(msg, dict):
            return []
        content = msg.get("content")
        if not isinstance(content, list):
            return []
        names: list[str] = []
        for block in content:
            if not isinstance(block, dict):
                continue
            if block.get("type") != "tool_result" or not block.get("is_error"):
                continue
            tid = block.get("tool_use_id")
            if tid and tid in self._tool_id_to_name:
                names.append(self._tool_id_to_name[tid])
        return names

    @staticmethod
    def _retry_tokens_and_model(
        error_rec: Record,
        timeline: list[Record],
        idx: int,
    ) -> tuple[int, str]:
        """Return ``(output_tokens, model)`` to charge for this error record.

        If the error is itself an assistant with output tokens, use those and
        its own model. Otherwise, scan forward in the interaction timeline for
        the next assistant message and use its output tokens + model.
        """
        if error_rec.kind == "assistant":
            out = int(error_rec.tokens.get("output", 0) or 0)
            if out and error_rec.model and error_rec.model != "N/A":
                return out, error_rec.model
        for j in range(idx + 1, len(timeline)):
            cand = timeline[j]
            if cand.kind == "assistant" and cand.model and cand.model != "N/A":
                return int(cand.tokens.get("output", 0) or 0), cand.model
        return 0, ""


def _interaction_to_outlier_command(ix: Interaction) -> dict:
    """Render an Interaction as an ``OutlierCommand`` dict (shape shared with
    ``_OutlierCollector`` and the frontend ``OutlierCommand`` TypedDict)."""
    by_model: dict[str, Counter[str]] = {}
    for r in ix.responses + ix.tool_results:
        if r.kind == "assistant" and r.model and r.model != "N/A":
            m = by_model.setdefault(r.model, Counter())
            for k, v in r.tokens.items():
                m[k] += v
    cost = sum(
        compute_cost(dict(tok_c), model)["total_cost"]
        for model, tok_c in by_model.items()
    )
    return {
        "interaction_id": ix.interaction_id,
        "session_id": ix.session_id,
        "timestamp": ix.start_time or "",
        "prompt_preview": _preview(ix.command.content or "", 200),
        "tool_count": ix.tool_count,
        "step_count": ix.assistant_steps,
        "cost": cost,
    }


def _empty_token_composition() -> dict:
    return {"daily": {}, "totals": {}, "per_session": {}}


def _empty_error_cost() -> dict:
    return {
        "total_errors": 0,
        "estimated_retry_tokens": 0,
        "estimated_retry_cost": 0.0,
        "errors_by_tool": {},
        "top_error_commands": [],
    }


def _preview(text: str, limit: int) -> str:
    if not text:
        return ""
    return text.replace("\n", " ").replace("\r", " ").strip()[:limit]


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


# ── trends (§1.9) ───────────────────────────────────────────────────────────

_TREND_ZERO: dict[str, float] = {
    "cost_per_command": 0.0,
    "errors_per_command": 0.0,
    "tools_per_command": 0.0,
    "tokens_per_command": 0.0,
    "commands": 0,
    "cost": 0.0,
}


def _empty_trends() -> dict:
    return {
        "current_week": dict(_TREND_ZERO),
        "prior_week":   dict(_TREND_ZERO),
        "delta_pct":    dict(_TREND_ZERO),
    }


def _trends(
    records: list[Record],
    interactions: list[Interaction],
    tz_offset: int,  # noqa: ARG001 — signature parity with other sections
) -> dict:
    """Compare the last 7 days to the prior 7 days using ``overview.date_range.end``."""
    stamps = [r.timestamp for r in records if r.timestamp]
    if not stamps:
        return _empty_trends()
    try:
        end = _parse_ts(max(stamps))
    except (ValueError, TypeError):
        return _empty_trends()

    cur_start = end - timedelta(days=7)
    prior_start = end - timedelta(days=14)

    current: list[Interaction] = []
    prior:   list[Interaction] = []
    for ix in interactions:
        if not ix.start_time:
            continue
        try:
            t = _parse_ts(ix.start_time)
        except (ValueError, TypeError):
            continue
        if cur_start < t <= end:
            current.append(ix)
        elif prior_start < t <= cur_start:
            prior.append(ix)

    cur_m = _trend_metrics(current)
    prior_m = _trend_metrics(prior)

    delta: dict[str, float] = {}
    for k, cur_v in cur_m.items():
        prior_v = prior_m[k]
        if k == "commands":
            delta[k] = cur_v - prior_v
        elif prior_v == 0:
            delta[k] = 0.0
        else:
            delta[k] = (cur_v - prior_v) / prior_v * 100

    return {"current_week": cur_m, "prior_week": prior_m, "delta_pct": delta}


def _trend_metrics(ixs: list[Interaction]) -> dict:
    if not ixs:
        return dict(_TREND_ZERO)
    total_cost = 0.0
    total_errors = 0
    total_tools = 0
    total_tokens = 0
    for ix in ixs:
        by_model: dict[str, Counter[str]] = {}
        for r in ix.responses + ix.tool_results:
            if r.is_error:
                total_errors += 1
            for v in r.tokens.values():
                total_tokens += v
            if r.kind == "assistant" and r.model and r.model != "N/A":
                m = by_model.setdefault(r.model, Counter())
                for k, v in r.tokens.items():
                    m[k] += v
        total_tools += ix.tool_count
        total_cost += sum(
            compute_cost(dict(tok_c), model)["total_cost"]
            for model, tok_c in by_model.items()
        )
    n = len(ixs)
    return {
        "cost_per_command":   total_cost / n,
        "errors_per_command": total_errors / n,
        "tools_per_command":  total_tools / n,
        "tokens_per_command": total_tokens / n,
        "commands": n,
        "cost": total_cost,
    }
