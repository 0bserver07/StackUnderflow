"""Unit tests for stackunderflow.stats.aggregator."""

from __future__ import annotations

import pytest

from stackunderflow.stats.aggregator import (
    _CacheCollector,
    _CommandCostCollector,
    _ErrorCostCollector,
    _ErrorsCollector,
    _ModelsCollector,
    _OutlierCollector,
    _RetryCollector,
    _SessionCostCollector,
    _SessionEfficiencyCollector,
    _SessionsCollector,
    _TokenCompositionCollector,
    _ToolCostCollector,
    _ToolsCollector,
    _cmd_has_search_verb,
    _daily,
    _hourly,
    _is_search_invocation,
    _local_day,
    _local_hour,
    _time_bounds,
    _trends,
    recompute_tz_stats,
    summarise,
)
from stackunderflow.stats.enricher import EnrichedDataset, Interaction, Record


# ── helpers ───────────────────────────────────────────────────────────────────

_TOKENS = {"input": 10, "output": 5, "cache_creation": 0, "cache_read": 0}
_MODEL = "claude-sonnet-4-20250514"


def _rec(
    *,
    session_id: str = "s1",
    kind: str = "assistant",
    timestamp: str = "2026-01-01T12:00:00Z",
    model: str = _MODEL,
    content: str = "",
    tokens: dict | None = None,
    tools: list[dict] | None = None,
    is_error: bool = False,
    error_category: str | None = None,
    is_interruption: bool = False,
    has_tool_result: bool = False,
) -> Record:
    return Record(
        session_id=session_id,
        kind=kind,
        timestamp=timestamp,
        model=model,
        content=content,
        tokens=tokens if tokens is not None else dict(_TOKENS),
        tools=tools if tools is not None else [],
        is_error=is_error,
        error_category=error_category,
        is_interruption=is_interruption,
        has_tool_result=has_tool_result,
        uuid="u1",
        parent_uuid=None,
        is_sidechain=False,
        message_id="m1",
        cwd="/tmp",
        raw_data={},
    )


def _ds(records: list[Record], interactions: list[Interaction] | None = None) -> EnrichedDataset:
    return EnrichedDataset(
        records=records,
        interactions=interactions or [],
        sessions={},
    )


# ── _ToolsCollector ───────────────────────────────────────────────────────────

def test_tools_collector_counts_uses():
    c = _ToolsCollector()
    c.ingest(_rec(tools=[{"name": "Bash", "id": "t1", "input": {}}]))
    c.ingest(_rec(tools=[{"name": "Bash", "id": "t2", "input": {}}, {"name": "Grep", "id": "t3", "input": {}}]))
    r = c.result()
    assert r["usage_counts"]["Bash"] == 2
    assert r["usage_counts"]["Grep"] == 1


def test_tools_collector_counts_errors():
    c = _ToolsCollector()
    c.ingest(_rec(tools=[{"name": "Edit", "id": "t1", "input": {}}], is_error=True))
    r = c.result()
    assert r["error_counts"]["Edit"] == 1


def test_tools_collector_error_rate_zero_when_no_error():
    c = _ToolsCollector()
    c.ingest(_rec(tools=[{"name": "Read", "id": "t1", "input": {}}], is_error=False))
    r = c.result()
    assert r["error_rates"]["Read"] == 0.0


def test_tools_collector_error_rate_calculated():
    c = _ToolsCollector()
    tool = {"name": "Write", "id": "t1", "input": {}}
    c.ingest(_rec(tools=[tool], is_error=True))
    c.ingest(_rec(tools=[tool], is_error=True))
    c.ingest(_rec(tools=[tool], is_error=False))
    r = c.result()
    assert pytest.approx(r["error_rates"]["Write"], rel=1e-3) == 2 / 3


def test_tools_collector_empty():
    c = _ToolsCollector()
    r = c.result()
    assert r["usage_counts"] == {}


# ── _ModelsCollector ──────────────────────────────────────────────────────────

def test_models_collector_accumulates_tokens():
    c = _ModelsCollector()
    c.ingest(_rec(kind="assistant", model=_MODEL, tokens={"input": 100, "output": 50, "cache_creation": 0, "cache_read": 0}))
    r = c.result()
    assert _MODEL in r
    assert r[_MODEL]["input_tokens"] == 100
    assert r[_MODEL]["output_tokens"] == 50
    assert r[_MODEL]["count"] == 1


def test_models_collector_skips_non_assistant():
    c = _ModelsCollector()
    c.ingest(_rec(kind="user", model=_MODEL))
    assert c.result() == {}


def test_models_collector_skips_na_model():
    c = _ModelsCollector()
    c.ingest(_rec(kind="assistant", model="N/A"))
    assert c.result() == {}


def test_models_collector_aggregates_multiple_records():
    c = _ModelsCollector()
    tok = {"input": 10, "output": 5, "cache_creation": 0, "cache_read": 0}
    c.ingest(_rec(kind="assistant", model=_MODEL, tokens=tok))
    c.ingest(_rec(kind="assistant", model=_MODEL, tokens=tok))
    r = c.result()[_MODEL]
    assert r["count"] == 2
    assert r["input_tokens"] == 20


# ── _SessionsCollector ────────────────────────────────────────────────────────

def test_sessions_collector_count():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1"))
    c.ingest(_rec(session_id="s2"))
    c.ingest(_rec(session_id="s1"))
    assert c.result()["count"] == 2


def test_sessions_collector_average_messages():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1"))
    c.ingest(_rec(session_id="s1"))
    c.ingest(_rec(session_id="s2"))
    r = c.result()
    assert r["average_messages"] == 1.5


def test_sessions_collector_duration():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-01-01T10:00:00Z"))
    c.ingest(_rec(session_id="s1", timestamp="2026-01-01T11:00:00Z"))
    r = c.result()
    assert r["average_duration_seconds"] == 3600.0


def test_sessions_collector_sessions_with_errors():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1", is_error=True))
    c.ingest(_rec(session_id="s2", is_error=False))
    assert c.result()["sessions_with_errors"] == 1


def test_sessions_collector_no_duration_single_message():
    c = _SessionsCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-01-01T10:00:00Z"))
    assert c.result()["average_duration_seconds"] == 0.0


# ── _ErrorsCollector ─────────────────────────────────────────────────────────

def test_errors_collector_total():
    c = _ErrorsCollector()
    c.ingest(_rec(is_error=True, error_category="Timeout"))
    c.ingest(_rec(is_error=True, error_category="Timeout"))
    c.ingest(_rec(is_error=False))
    recs = [_rec(is_error=True), _rec(is_error=True), _rec(is_error=False)]
    r = c.result(recs)
    assert r["total"] == 2


def test_errors_collector_by_category():
    c = _ErrorsCollector()
    c.ingest(_rec(is_error=True, error_category="Permission Error"))
    c.ingest(_rec(is_error=True, error_category="Permission Error"))
    c.ingest(_rec(is_error=True, error_category=None))
    r = c.result([])
    assert r["by_category"]["Permission Error"] == 2
    assert r["by_category"]["Other"] == 1


def test_errors_collector_rate():
    c = _ErrorsCollector()
    c.ingest(_rec(is_error=True))
    recs = [_rec(is_error=True), _rec(is_error=False), _rec(is_error=False)]
    r = c.result(recs)
    assert pytest.approx(r["rate"], rel=1e-3) == 1 / 3


def test_errors_collector_empty():
    c = _ErrorsCollector()
    r = c.result([])
    assert r["total"] == 0
    assert r["rate"] == 0


# ── _CacheCollector ───────────────────────────────────────────────────────────

def test_cache_collector_hit_rate():
    c = _CacheCollector()
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 100, "cache_read": 200}))
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0}))
    r = c.result()
    assert r["hit_rate"] == 50.0  # 1 of 2 assistant messages had cache_read


def test_cache_collector_break_even():
    c = _CacheCollector()
    # read more than created → break even
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 100, "cache_read": 200}))
    assert c.result()["break_even_achieved"] is True


def test_cache_collector_no_break_even():
    c = _CacheCollector()
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 500, "cache_read": 100}))
    assert c.result()["break_even_achieved"] is False


def test_cache_collector_tokens_saved():
    c = _CacheCollector()
    c.ingest(_rec(kind="assistant", tokens={"input": 0, "output": 0, "cache_creation": 100, "cache_read": 300}))
    assert c.result()["tokens_saved"] == 200


def test_cache_collector_empty():
    c = _CacheCollector()
    r = c.result()
    assert r["hit_rate"] == 0.0
    assert r["break_even_achieved"] is False
    assert r["tokens_saved"] == 0


def test_cache_collector_skips_non_assistant():
    c = _CacheCollector()
    c.ingest(_rec(kind="user", tokens={"input": 0, "output": 0, "cache_creation": 1000, "cache_read": 1000}))
    assert c.result()["assistant_messages"] == 0


# ── time helpers ──────────────────────────────────────────────────────────────

def test_local_day_basic():
    assert _local_day("2026-01-15T10:30:00Z", 0) == "2026-01-15"


def test_local_day_with_positive_offset():
    # UTC 23:00 + 120min → next day
    assert _local_day("2026-01-15T23:00:00Z", 120) == "2026-01-16"


def test_local_day_with_negative_offset():
    # UTC 01:00 - 120min → previous day
    assert _local_day("2026-01-15T01:00:00Z", -120) == "2026-01-14"


def test_local_day_empty_string():
    assert _local_day("", 0) is None


def test_local_hour_basic():
    assert _local_hour("2026-01-15T14:00:00Z", 0) == 14


def test_local_hour_with_offset():
    # UTC 23:00 + 120min → hour 1
    assert _local_hour("2026-01-15T23:00:00Z", 120) == 1


def test_local_hour_empty_string():
    assert _local_hour("", 0) is None


def test_time_bounds_returns_min_max():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z"),
        _rec(timestamp="2026-01-03T10:00:00Z"),
        _rec(timestamp="2026-01-02T10:00:00Z"),
    ]
    b = _time_bounds(recs)
    assert b["start"] == "2026-01-01T10:00:00Z"
    assert b["end"] == "2026-01-03T10:00:00Z"


def test_time_bounds_empty():
    b = _time_bounds([])
    assert b["start"] is None
    assert b["end"] is None


def test_time_bounds_skips_empty_timestamp():
    recs = [_rec(timestamp=""), _rec(timestamp="2026-01-01T10:00:00Z")]
    b = _time_bounds(recs)
    assert b["start"] == "2026-01-01T10:00:00Z"


# ── _is_search_invocation ─────────────────────────────────────────────────────

@pytest.mark.parametrize("name,expected", [
    ("Grep", True),
    ("Glob", True),
    ("LS", True),
    ("Read", False),
    ("Edit", False),
    ("Write", False),
])
def test_is_search_invocation_named_tools(name: str, expected: bool):
    assert _is_search_invocation({"name": name}) is expected


def test_is_search_invocation_bash_grep():
    assert _is_search_invocation({"name": "Bash", "input": {"command": "grep foo bar.txt"}}) is True


def test_is_search_invocation_bash_find():
    assert _is_search_invocation({"name": "Bash", "input": {"command": "find . -name '*.py'"}}) is True


def test_is_search_invocation_bash_non_search():
    assert _is_search_invocation({"name": "Bash", "input": {"command": "npm install"}}) is False


def test_is_search_invocation_bash_no_command():
    assert _is_search_invocation({"name": "Bash", "input": {}}) is False


# ── _cmd_has_search_verb ──────────────────────────────────────────────────────

def test_cmd_has_search_verb_simple():
    assert _cmd_has_search_verb("grep foo bar") is True


def test_cmd_has_search_verb_pipe():
    assert _cmd_has_search_verb("cat file | grep pattern") is True


def test_cmd_has_search_verb_rg():
    assert _cmd_has_search_verb("rg 'pattern' src/") is True


def test_cmd_has_search_verb_ls():
    assert _cmd_has_search_verb("ls -la") is True


def test_cmd_has_search_verb_none():
    assert _cmd_has_search_verb("python main.py") is False


def test_cmd_has_search_verb_semicolon():
    assert _cmd_has_search_verb("echo hi; grep foo bar") is True


# ── _daily ────────────────────────────────────────────────────────────────────

def test_daily_groups_by_date():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z"),
        _rec(timestamp="2026-01-01T12:00:00Z"),
        _rec(timestamp="2026-01-02T10:00:00Z"),
    ]
    result = _daily(recs, [], 0)
    assert "2026-01-01" in result
    assert "2026-01-02" in result
    assert result["2026-01-01"]["messages"] == 2


def test_daily_structure_keys():
    recs = [_rec(timestamp="2026-01-01T10:00:00Z")]
    day = _daily(recs, [], 0)["2026-01-01"]
    assert set(day.keys()) >= {"messages", "sessions", "tokens", "errors", "user_commands"}


def test_daily_counts_errors():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z", is_error=True),
        _rec(timestamp="2026-01-01T11:00:00Z", is_error=False),
    ]
    result = _daily(recs, [], 0)
    assert result["2026-01-01"]["errors"] == 1


def test_daily_skips_empty_timestamps():
    recs = [_rec(timestamp=""), _rec(timestamp="2026-01-01T10:00:00Z")]
    result = _daily(recs, [], 0)
    assert len(result) == 1


# ── _hourly ───────────────────────────────────────────────────────────────────

def test_hourly_has_all_24_hours():
    result = _hourly([], 0)
    assert set(result["messages"].keys()) == set(range(24))
    assert set(result["tokens"].keys()) == set(range(24))


def test_hourly_counts_messages():
    recs = [
        _rec(timestamp="2026-01-01T10:00:00Z"),
        _rec(timestamp="2026-01-01T10:30:00Z"),
        _rec(timestamp="2026-01-01T14:00:00Z"),
    ]
    result = _hourly(recs, 0)
    assert result["messages"][10] == 2
    assert result["messages"][14] == 1


def test_hourly_empty_hours_are_zero():
    result = _hourly([], 0)
    for h in range(24):
        assert result["messages"][h] == 0


# ── summarise ─────────────────────────────────────────────────────────────────

def test_summarise_has_required_keys():
    ds = _ds([_rec(kind="user", timestamp="2026-01-01T12:00:00Z")])
    result = summarise(ds, log_dir="/tmp/test")
    required = {"overview", "tools", "sessions", "daily_stats", "hourly_pattern",
                "errors", "models", "user_interactions", "cache"}
    assert required <= set(result.keys())


def test_summarise_overview_total_messages():
    recs = [_rec(), _rec(), _rec()]
    ds = _ds(recs)
    r = summarise(ds, log_dir="/tmp")
    assert r["overview"]["total_messages"] == 3


def test_summarise_empty_dataset():
    ds = _ds([])
    r = summarise(ds, log_dir="/tmp")
    assert r["overview"]["total_messages"] == 0
    assert r["errors"]["total"] == 0


def test_summarise_project_name_from_log_dir():
    ds = _ds([])
    r = summarise(ds, log_dir="/Users/test/.claude/projects/-Users-test-myproject")
    assert r["overview"]["project_name"] == "myproject"


# ── recompute_tz_stats ────────────────────────────────────────────────────────

def test_recompute_tz_stats_returns_daily_and_hourly():
    msgs = [{"timestamp": "2026-01-01T10:00:00Z", "session_id": "s1", "type": "assistant",
              "model": _MODEL, "tokens": _TOKENS, "error": False,
              "content": "", "has_tool_result": False}]
    result = recompute_tz_stats(msgs, 0)
    assert "daily_stats" in result
    assert "hourly_pattern" in result


def test_recompute_tz_stats_empty():
    result = recompute_tz_stats([], 0)
    assert "daily_stats" in result
    assert "hourly_pattern" in result
    assert result["daily_stats"] == {}


# ── analytics-expansion helpers ──────────────────────────────────────────────

def _ix(
    *,
    interaction_id: str = "ix1",
    session_id: str = "s1",
    start_time: str = "2026-02-01T10:00:00Z",
    model: str = _MODEL,
    command_content: str = "do the thing",
    responses: list[Record] | None = None,
    tool_results: list[Record] | None = None,
    tools_used: list[dict] | None = None,
    tool_count: int | None = None,
    assistant_steps: int | None = None,
) -> Interaction:
    cmd = _rec(
        kind="user",
        session_id=session_id,
        timestamp=start_time,
        content=command_content,
        model="N/A",
    )
    responses = responses or []
    tool_results = tool_results or []
    tools_used = tools_used or []
    if tool_count is None:
        tool_count = len(tools_used)
    if assistant_steps is None:
        assistant_steps = len(responses)
    return Interaction(
        interaction_id=interaction_id,
        command=cmd,
        responses=list(responses),
        tool_results=list(tool_results),
        session_id=session_id,
        start_time=start_time,
        end_time=start_time,
        model=model,
        tool_count=tool_count,
        assistant_steps=assistant_steps,
        tools_used=list(tools_used),
    )


# ── _SessionCostCollector (§1.1) ─────────────────────────────────────────────

def test_session_cost_collector_ranks_by_cost():
    c = _SessionCostCollector()
    # Cheap session
    c.ingest(_rec(session_id="a", kind="assistant",
                  tokens={"input": 10, "output": 5, "cache_creation": 0, "cache_read": 0},
                  timestamp="2026-02-01T00:00:00Z"))
    # Expensive session
    c.ingest(_rec(session_id="b", kind="assistant",
                  tokens={"input": 10_000, "output": 5_000, "cache_creation": 0, "cache_read": 0},
                  timestamp="2026-02-01T00:00:00Z"))
    c.ingest(_rec(session_id="b", kind="assistant",
                  tokens={"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
                  timestamp="2026-02-01T00:05:00Z"))
    out = c.result(interactions=[])
    assert [s["session_id"] for s in out] == ["b", "a"]
    assert out[0]["duration_s"] == 300.0
    assert out[0]["tokens"]["input"] == 10_000
    assert out[0]["messages"] == 2


def test_session_cost_collector_first_prompt_preview_and_commands():
    c = _SessionCostCollector()
    c.ingest(_rec(session_id="s", kind="user", timestamp="2026-02-01T00:00:00Z"))
    c.ingest(_rec(session_id="s", kind="assistant", timestamp="2026-02-01T00:01:00Z"))
    interactions = [
        _ix(interaction_id="i1", session_id="s",
            start_time="2026-02-01T00:00:00Z", command_content="first prompt\nwith newline"),
        _ix(interaction_id="i2", session_id="s",
            start_time="2026-02-01T00:02:00Z", command_content="second prompt"),
    ]
    out = c.result(interactions)
    assert out[0]["commands"] == 2
    assert out[0]["first_prompt_preview"] == "first prompt with newline"


def test_session_cost_collector_empty():
    assert _SessionCostCollector().result(interactions=[]) == []


# ── _CommandCostCollector (§1.2) ─────────────────────────────────────────────

def test_command_cost_collector_basic():
    c = _CommandCostCollector()
    asst = _rec(kind="assistant", model=_MODEL,
                tokens={"input": 1000, "output": 500, "cache_creation": 0, "cache_read": 0})
    ix = _ix(interaction_id="x1", session_id="s1", responses=[asst],
             tools_used=[{"name": "Bash", "id": "t1", "input": {}}], tool_count=1)
    c.ingest_interaction(ix)
    out = c.result()
    assert len(out) == 1
    assert out[0]["interaction_id"] == "x1"
    assert out[0]["cost"] > 0
    assert out[0]["tokens"]["input"] == 1000
    assert out[0]["tools_used"] == 1
    assert out[0]["had_error"] is False


def test_command_cost_collector_caps_at_50_and_sorts_desc():
    c = _CommandCostCollector()
    for i in range(60):
        asst = _rec(kind="assistant", model=_MODEL,
                    tokens={"input": i, "output": 0, "cache_creation": 0, "cache_read": 0})
        c.ingest_interaction(_ix(interaction_id=f"i{i}", responses=[asst]))
    out = c.result()
    assert len(out) == 50
    # Highest token count (i=59) must be first
    assert out[0]["interaction_id"] == "i59"


def test_command_cost_collector_empty():
    assert _CommandCostCollector().result() == []


# ── _ToolCostCollector (§1.3) ────────────────────────────────────────────────

def test_tool_cost_collector_attributes_1_over_n():
    c = _ToolCostCollector()
    # One assistant message invoking 2 distinct tools → each gets 1/2 of the cost
    tokens = {"input": 1_000_000, "output": 0, "cache_creation": 0, "cache_read": 0}
    c.ingest(_rec(
        kind="assistant", model=_MODEL, tokens=tokens,
        tools=[{"name": "Bash", "id": "t1", "input": {}},
               {"name": "Grep", "id": "t2", "input": {}}],
    ))
    out = c.result()
    assert set(out.keys()) == {"Bash", "Grep"}
    assert out["Bash"]["calls"] == 1
    assert out["Grep"]["calls"] == 1
    assert out["Bash"]["cost"] == pytest.approx(out["Grep"]["cost"], rel=1e-9)
    # Sum of shares equals the full message cost
    total = out["Bash"]["cost"] + out["Grep"]["cost"]
    assert total == pytest.approx(3.0, rel=1e-6)  # 1M input × $3/M for sonnet-4


def test_tool_cost_collector_counts_repeated_invocations():
    c = _ToolCostCollector()
    # Same tool invoked twice in one message → only 1 distinct, calls=2
    c.ingest(_rec(kind="assistant", model=_MODEL,
                  tokens={"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
                  tools=[{"name": "Bash", "id": "t1", "input": {}},
                         {"name": "Bash", "id": "t2", "input": {}}]))
    out = c.result()
    assert out["Bash"]["calls"] == 2


def test_tool_cost_collector_skips_user_records():
    c = _ToolCostCollector()
    c.ingest(_rec(kind="user", tools=[{"name": "X", "id": "1", "input": {}}]))
    assert c.result() == {}


def test_tool_cost_collector_empty():
    assert _ToolCostCollector().result() == {}


# ── _TokenCompositionCollector (§1.4) ────────────────────────────────────────

def test_token_composition_shape_and_totals():
    c = _TokenCompositionCollector(tz_offset=0)
    c.ingest(_rec(session_id="s1", timestamp="2026-02-23T10:00:00Z",
                  tokens={"input": 100, "output": 50, "cache_creation": 0, "cache_read": 0}))
    c.ingest(_rec(session_id="s2", timestamp="2026-02-23T12:00:00Z",
                  tokens={"input": 200, "output": 100, "cache_creation": 0, "cache_read": 0}))
    out = c.result()
    assert set(out.keys()) == {"daily", "totals", "per_session"}
    assert out["daily"]["2026-02-23"]["input"] == 300
    assert out["totals"]["output"] == 150
    assert out["per_session"]["s1"]["input"] == 100
    assert out["per_session"]["s2"]["input"] == 200


def test_token_composition_empty():
    r = _TokenCompositionCollector().result()
    assert r == {"daily": {}, "totals": {}, "per_session": {}}


# ── _OutlierCollector (§1.5) ─────────────────────────────────────────────────

def test_outlier_collector_flags_high_tool_and_step():
    c = _OutlierCollector()
    # 21 tools + 16 steps → both
    responses = [_rec(kind="assistant", model=_MODEL) for _ in range(16)]
    c.ingest_interaction(_ix(interaction_id="big", tool_count=21, assistant_steps=16,
                             responses=responses))
    # within thresholds → neither
    c.ingest_interaction(_ix(interaction_id="small", tool_count=5, assistant_steps=3))
    out = c.result()
    assert [e["interaction_id"] for e in out["high_tool_commands"]] == ["big"]
    assert [e["interaction_id"] for e in out["high_step_commands"]] == ["big"]


def test_outlier_collector_empty():
    r = _OutlierCollector().result()
    assert r == {"high_tool_commands": [], "high_step_commands": []}


# ── _RetryCollector (§1.6) ───────────────────────────────────────────────────

def test_retry_collector_detects_failed_retry():
    c = _RetryCollector()
    # Bash invoked 3x, middle call errored with 1000 output tokens
    r1 = _rec(kind="assistant", model=_MODEL, timestamp="2026-02-01T00:00:00Z",
              tokens={"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
              tools=[{"name": "Bash", "id": "t1", "input": {}}])
    r2 = _rec(kind="assistant", model=_MODEL, timestamp="2026-02-01T00:00:10Z",
              tokens={"input": 0, "output": 1000, "cache_creation": 0, "cache_read": 0},
              tools=[{"name": "Bash", "id": "t2", "input": {}}], is_error=True)
    r3 = _rec(kind="assistant", model=_MODEL, timestamp="2026-02-01T00:00:20Z",
              tokens={"input": 0, "output": 0, "cache_creation": 0, "cache_read": 0},
              tools=[{"name": "Bash", "id": "t3", "input": {}}])
    ix = _ix(interaction_id="retry", responses=[r1, r2, r3])
    c.ingest_interaction(ix)
    out = c.result()
    assert len(out) == 1
    s = out[0]
    assert s["tool"] == "Bash"
    assert s["total_invocations"] == 3
    assert s["consecutive_failures"] == 1
    assert s["estimated_wasted_tokens"] == 1000
    assert s["estimated_wasted_cost"] > 0


def test_retry_collector_no_error_no_signal():
    c = _RetryCollector()
    r1 = _rec(kind="assistant", model=_MODEL,
              tools=[{"name": "Bash", "id": "t1", "input": {}}])
    r2 = _rec(kind="assistant", model=_MODEL,
              tools=[{"name": "Bash", "id": "t2", "input": {}}])
    c.ingest_interaction(_ix(responses=[r1, r2]))
    assert c.result() == []


def test_retry_collector_empty():
    assert _RetryCollector().result() == []


# ── _SessionEfficiencyCollector (§1.7) ───────────────────────────────────────

def test_session_efficiency_classifies_edit_heavy():
    c = _SessionEfficiencyCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-02-01T00:00:00Z",
                  tools=[{"name": "Edit", "id": "1", "input": {}},
                         {"name": "Write", "id": "2", "input": {}},
                         {"name": "Read", "id": "3", "input": {}},
                         {"name": "Bash", "id": "4", "input": {}}]))
    out = c.result()
    assert len(out) == 1
    assert out[0]["classification"] == "edit-heavy"
    assert out[0]["edit_ratio"] == 0.5


def test_session_efficiency_classifies_research_heavy():
    c = _SessionEfficiencyCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-02-01T00:00:00Z",
                  tools=[{"name": "Grep", "id": "1", "input": {}},
                         {"name": "Glob", "id": "2", "input": {}},
                         {"name": "Read", "id": "3", "input": {}},
                         {"name": "Bash", "id": "4", "input": {}}]))
    out = c.result()
    # search_ratio (0.5) + read_ratio (0.25) = 0.75 ≥ 0.6, edit_ratio = 0 < 0.1
    assert out[0]["classification"] == "research-heavy"


def test_session_efficiency_classifies_idle_heavy():
    c = _SessionEfficiencyCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-02-01T00:00:00Z"))
    # 100s gap, exceeds both 30s threshold and 40% of 100s duration
    c.ingest(_rec(session_id="s1", timestamp="2026-02-01T00:01:40Z"))
    out = c.result()
    assert out[0]["classification"] == "idle-heavy"
    assert out[0]["idle_gap_total_s"] == 100.0
    assert out[0]["idle_gap_max_s"] == 100.0


def test_session_efficiency_balanced_fallback():
    c = _SessionEfficiencyCollector()
    c.ingest(_rec(session_id="s1", timestamp="2026-02-01T00:00:00Z",
                  tools=[{"name": "Bash", "id": "1", "input": {}},
                         {"name": "Read", "id": "2", "input": {}}]))
    # 15s gap → under idle threshold; only 50% read ratio → not research-heavy
    c.ingest(_rec(session_id="s1", timestamp="2026-02-01T00:00:15Z"))
    out = c.result()
    assert out[0]["classification"] == "balanced"


def test_session_efficiency_empty():
    assert _SessionEfficiencyCollector().result() == []


# ── _ErrorCostCollector (§1.8) ───────────────────────────────────────────────

def test_error_cost_collector_aggregates():
    c = _ErrorCostCollector()
    c.ingest(_rec(kind="assistant", model=_MODEL, is_error=True,
                  tokens={"input": 0, "output": 500, "cache_creation": 0, "cache_read": 0},
                  tools=[{"name": "Bash", "id": "1", "input": {}}]))
    c.ingest(_rec(kind="assistant", model=_MODEL, is_error=True,
                  tokens={"input": 0, "output": 300, "cache_creation": 0, "cache_read": 0},
                  tools=[{"name": "Edit", "id": "2", "input": {}}]))
    c.ingest(_rec(is_error=False))
    out = c.result(interactions=[])
    assert out["total_errors"] == 2
    assert out["errors_by_tool"] == {"Bash": 1, "Edit": 1}
    # avg_output_per_error = (500+300)/2 = 400; total = 400 × 2 = 800
    assert out["estimated_retry_tokens"] == 800
    assert out["estimated_retry_cost"] > 0


def test_error_cost_collector_top_commands():
    c = _ErrorCostCollector()
    c.ingest(_rec(is_error=True, model=_MODEL))
    r_err = _rec(kind="assistant", model=_MODEL, is_error=True)
    ix_with_error = _ix(interaction_id="bad", responses=[r_err])
    ix_without = _ix(interaction_id="good",
                     responses=[_rec(kind="assistant", model=_MODEL, is_error=False)])
    out = c.result(interactions=[ix_with_error, ix_without])
    assert len(out["top_error_commands"]) == 1
    assert out["top_error_commands"][0]["interaction_id"] == "bad"


def test_error_cost_collector_empty():
    out = _ErrorCostCollector().result(interactions=[])
    assert out["total_errors"] == 0
    assert out["estimated_retry_tokens"] == 0
    assert out["errors_by_tool"] == {}
    assert out["top_error_commands"] == []


# ── _trends (§1.9) ───────────────────────────────────────────────────────────

def test_trends_compares_windows():
    # end = 2026-02-15; current = (02-08, 02-15]; prior = (02-01, 02-08]
    records = [_rec(timestamp="2026-02-15T23:59:00Z")]
    asst_cur = _rec(kind="assistant", model=_MODEL,
                    tokens={"input": 1000, "output": 0, "cache_creation": 0, "cache_read": 0})
    asst_prior = _rec(kind="assistant", model=_MODEL,
                      tokens={"input": 500, "output": 0, "cache_creation": 0, "cache_read": 0})
    ix_current = _ix(interaction_id="cur", start_time="2026-02-14T10:00:00Z",
                     responses=[asst_cur])
    ix_prior = _ix(interaction_id="prev", start_time="2026-02-05T10:00:00Z",
                   responses=[asst_prior])
    out = _trends(records, [ix_current, ix_prior], tz_offset=0)
    assert out["current_week"]["commands"] == 1
    assert out["prior_week"]["commands"] == 1
    # delta is %; current has higher cost than prior
    assert out["delta_pct"]["cost"] > 0


def test_trends_empty_records():
    out = _trends([], [], tz_offset=0)
    assert out["current_week"]["commands"] == 0
    assert out["prior_week"]["commands"] == 0
    assert out["delta_pct"]["cost"] == 0.0


def test_trends_prior_empty_under_14d_span():
    # Dataset only 3 days long → nothing in prior week
    records = [_rec(timestamp="2026-02-03T00:00:00Z")]
    ix = _ix(interaction_id="only", start_time="2026-02-02T00:00:00Z",
             responses=[_rec(kind="assistant", model=_MODEL,
                             tokens={"input": 100, "output": 0,
                                     "cache_creation": 0, "cache_read": 0})])
    out = _trends(records, [ix], tz_offset=0)
    assert out["prior_week"]["commands"] == 0
    # Guard: prior zero → delta zero
    assert out["delta_pct"]["cost"] == 0.0


# ── summarise wiring of new sections ─────────────────────────────────────────

def test_summarise_includes_new_sections():
    ds = _ds([_rec(kind="user", timestamp="2026-01-01T12:00:00Z")])
    result = summarise(ds, log_dir="/tmp/test")
    required = {
        "session_costs", "command_costs", "tool_costs", "token_composition",
        "outliers", "retry_signals", "session_efficiency", "error_cost", "trends",
    }
    assert required <= set(result.keys())


def test_summarise_new_sections_empty_fallback():
    ds = _ds([])
    result = summarise(ds, log_dir="/tmp")
    assert result["session_costs"] == []
    assert result["command_costs"] == []
    assert result["tool_costs"] == {}
    assert result["token_composition"] == {"daily": {}, "totals": {}, "per_session": {}}
    assert result["outliers"] == {"high_tool_commands": [], "high_step_commands": []}
    assert result["retry_signals"] == []
    assert result["session_efficiency"] == []
    assert result["error_cost"]["total_errors"] == 0
    assert result["trends"]["current_week"]["commands"] == 0
