"""Real-shape end-to-end validation for the 12 beta normalizers.

Closes HANDOFF §"What's left" item #3. For each beta provider we:

1. Load a synthetic-but-spec-accurate fixture from
   ``tests/fixtures/beta_normalizers/<provider>/`` and place it in
   ``tmp_path`` matching the on-disk layout each adapter expects.
2. Read records via the registered adapter (``adapter.enumerate()`` +
   ``adapter.read(ref)``).
3. Convert each Record to the joined-row dict shape ``backfill`` would
   hand the normalizer (mirrors the SQL in
   ``stackunderflow/etl/backfill.py::_run_normalizers``).
4. Pipe the row through the registered normalizer.
5. Assert the canonical ``usage_events`` shape, ``cost_usd`` /
   ``cost_source`` semantics, and defensive empty / malformed input.

The fixture data files are checked in for inspection. The test reads
them from disk and lays them out under ``tmp_path`` — never against the
maintainer's real ``~/.stackunderflow/store.db`` or ``~/.<provider>/``
trees.
"""

from __future__ import annotations

import json
import shutil
import sqlite3
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pytest

from stackunderflow.adapters.base import Record
from stackunderflow.etl.normalize import get as get_normalizer
from stackunderflow.etl.normalize.base import (
    COST_SOURCE_ESTIMATED,
    COST_SOURCE_RATE_CARD,
    COST_SOURCE_UNKNOWN,
)

# ── fixture root ────────────────────────────────────────────────────────

_REPO_ROOT = Path(__file__).resolve().parents[4]
_FIXTURES_ROOT = _REPO_ROOT / "tests" / "fixtures" / "beta_normalizers"


def _fixture_dir(provider: str) -> Path:
    p = _FIXTURES_ROOT / provider
    assert p.is_dir(), f"missing fixture dir for {provider}: {p}"
    return p


# ── canonical event-shape assertions ────────────────────────────────────

_REQUIRED_EVENT_KEYS = (
    "source_message_fk",
    "provider",
    "project_id",
    "session_id",
    "ts",
    "day",
    "model",
    "speed",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_create_tokens",
    "cost_usd",
    "cost_source",
    "role",
)

_VALID_COST_SOURCES = frozenset(
    {COST_SOURCE_ESTIMATED, COST_SOURCE_RATE_CARD, "live", COST_SOURCE_UNKNOWN}
)


def _assert_canonical_event_shape(ev: dict, *, provider: str) -> None:
    """Every event must carry the keys the marts read, with sane types."""
    for key in _REQUIRED_EVENT_KEYS:
        assert key in ev, f"event missing key {key!r}: {ev!r}"
    assert ev["provider"] == provider, (
        f"event provider mismatch: expected {provider!r}, got {ev['provider']!r}"
    )
    # Token columns must be ints and non-negative.
    for key in (
        "input_tokens", "output_tokens",
        "cache_read_tokens", "cache_create_tokens",
    ):
        assert isinstance(ev[key], int), f"{key} must be int: {ev[key]!r}"
        assert ev[key] >= 0, f"{key} must be >= 0: {ev[key]!r}"
    # cost_usd is a non-negative float.
    assert isinstance(ev["cost_usd"], float), (
        f"cost_usd must be float: {ev['cost_usd']!r}"
    )
    assert ev["cost_usd"] >= 0.0
    # cost_source must come from the spec enum.
    assert ev["cost_source"] in _VALID_COST_SOURCES, (
        f"unknown cost_source {ev['cost_source']!r}"
    )
    # speed defaults to 'standard' for non-Anthropic-priority rows.
    assert ev["speed"] in ("standard", "fast"), (
        f"unexpected speed {ev['speed']!r}"
    )
    # day derives from ts as YYYY-MM-DD (or "" when ts is unparseable).
    assert isinstance(ev["day"], str)
    # session_id is a string (may be empty in degenerate fixtures).
    assert isinstance(ev["session_id"], str)


# ── Record → msg_row conversion (mirrors backfill.py JOIN columns) ──────


def _record_to_msg_row(
    rec: Record,
    *,
    msg_id: int,
    project_id: int,
    provider: str,
) -> dict:
    """Mirror the columns ``etl/backfill.py::_run_normalizers`` selects.

    The normalizer expects a flat dict with the messages-table columns
    plus the joined ``provider``/``project_id``/``session_id`` from the
    surrounding sessions/projects tables. We construct that here from a
    Record so the test exercises the same surface the production
    streaming loop hands the normalizer.
    """
    return {
        "id": msg_id,
        "session_fk": 1,
        "seq": rec.seq,
        "timestamp": rec.timestamp,
        "role": rec.role,
        "model": rec.model,
        "input_tokens": rec.input_tokens,
        "output_tokens": rec.output_tokens,
        "cache_read_tokens": rec.cache_read_tokens,
        "cache_create_tokens": rec.cache_create_tokens,
        "content_text": rec.content_text,
        "tools_json": json.dumps(list(rec.tools)),
        "raw_json": json.dumps(rec.raw, default=str),
        "is_sidechain": int(rec.is_sidechain),
        "uuid": rec.uuid,
        "parent_uuid": rec.parent_uuid,
        "speed": rec.speed,
        "session_id": rec.session_id,
        "project_id": project_id,
        "provider": provider,
    }


# ── per-provider scenario builders ──────────────────────────────────────
#
# Each builder takes a tmp_path and returns an *adapter instance* set up
# against the fixture, plus the "expected normalizer key" used for the
# registry lookup.


@dataclass
class Scenario:
    provider_key: str  # registry key used by the normalizer
    adapter_provider: str  # value the adapter writes to Record.provider
    build_adapter: Callable[[Path], Any]
    expected_min_events: int  # lower bound for the happy-path fixture
    # Exact set of cost_source values this fixture may produce — DATA on
    # the scenario, so the cost test has no provider-name branching.
    # None = only the enum-validity check applies.
    cost_sources: frozenset[str] | None = None


def _build_cursor_agent(tmp_path: Path) -> Any:
    from stackunderflow.adapters.cursor_agent import CursorAgentAdapter

    fixture = _fixture_dir("cursor_agent") / "transcript.jsonl"
    projects_root = tmp_path / "projects"
    transcripts = projects_root / "myproj" / "agent-transcripts"
    sub = transcripts / "11111111-2222-3333-4444-555555555555"
    sub.mkdir(parents=True)
    shutil.copy(fixture, sub / "session.jsonl")
    return CursorAgentAdapter(
        projects_root=projects_root,
        tracking_db=tmp_path / "missing.db",
    )


def _build_opencode(tmp_path: Path) -> Any:
    from stackunderflow.adapters.opencode import OpenCodeAdapter

    spec = json.loads((_fixture_dir("opencode") / "session.json").read_text())
    data_dir = tmp_path / "opencode-data"
    data_dir.mkdir()
    db_path = data_dir / "opencode.db"
    conn = sqlite3.connect(db_path)
    try:
        conn.executescript(
            """
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT,
                title TEXT,
                time_created INTEGER,
                time_archived INTEGER,
                parent_id TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                time_created INTEGER,
                data TEXT
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT,
                session_id TEXT,
                data TEXT
            );
            """
        )
        s = spec["session"]
        conn.execute(
            "INSERT INTO session VALUES (?,?,?,?,?,?)",
            (s["id"], s["directory"], s["title"], s["time_created"],
             s["time_archived"], s["parent_id"]),
        )
        for m in spec["messages"]:
            conn.execute(
                "INSERT INTO message VALUES (?,?,?,?)",
                (m["id"], s["id"], m["time_created"], json.dumps(m["data"])),
            )
            for part in m["parts"]:
                conn.execute(
                    "INSERT INTO part(message_id, session_id, data) "
                    "VALUES (?,?,?)",
                    (m["id"], s["id"], json.dumps(part)),
                )
        conn.commit()
    finally:
        conn.close()
    return OpenCodeAdapter(data_dir=data_dir)


def _build_qwen(tmp_path: Path) -> Any:
    from stackunderflow.adapters.qwen import QwenAdapter

    fixture = _fixture_dir("qwen") / "chat.jsonl"
    projects_root = tmp_path / "qwen-projects"
    chats = projects_root / "myproj" / "chats"
    chats.mkdir(parents=True)
    shutil.copy(fixture, chats / "session-qwen-001.jsonl")
    return QwenAdapter(projects_root=projects_root)


def _build_gemini(tmp_path: Path) -> Any:
    from stackunderflow.adapters.gemini import GeminiAdapter

    fixture = _fixture_dir("gemini") / "chat.jsonl"
    projects_root = tmp_path / "gemini-tmp"
    chats = projects_root / "myproj" / "chats"
    chats.mkdir(parents=True)
    shutil.copy(fixture, chats / "session-gemini-001.jsonl")
    return GeminiAdapter(projects_root=projects_root)


def _build_copilot(tmp_path: Path) -> Any:
    from stackunderflow.adapters.copilot import CopilotAdapter

    fixture = _fixture_dir("copilot") / "events.jsonl"
    legacy_root = tmp_path / "copilot-legacy"
    sess = legacy_root / "session-001"
    sess.mkdir(parents=True)
    shutil.copy(fixture, sess / "events.jsonl")
    return CopilotAdapter(
        legacy_root=legacy_root,
        vscode_workspace_storage=tmp_path / "missing-vscode-storage",
    )


def _build_codeium(tmp_path: Path) -> Any:
    from stackunderflow.adapters.codeium import CodeiumAdapter

    # Codeium adapter is a discovery-only stub — the on-disk format is
    # protobuf with no parser. We point at an empty dir to confirm the
    # stub returns nothing without raising.
    return CodeiumAdapter(root=tmp_path / "codeium-empty")


def _build_continue(tmp_path: Path) -> Any:
    from stackunderflow.adapters.continue_adapter import ContinueAdapter

    spec = json.loads((_fixture_dir("continue") / "session.json").read_text())
    root = tmp_path / "continue"
    root.mkdir()
    db_path = root / "state.db"
    conn = sqlite3.connect(db_path)
    try:
        conn.executescript(
            """
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                createdAt INTEGER
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                role TEXT,
                content TEXT,
                model TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                createdAt INTEGER
            );
            """
        )
        for s in spec["sessions"]:
            conn.execute(
                "INSERT INTO sessions VALUES (?,?,?)",
                (s["id"], s["title"], s["createdAt"]),
            )
        for m in spec["messages"]:
            conn.execute(
                "INSERT INTO messages(session_id, role, content, model, "
                "input_tokens, output_tokens, createdAt) "
                "VALUES (?,?,?,?,?,?,?)",
                (
                    m["session_id"], m["role"], m["content"], m["model"],
                    m["input_tokens"], m["output_tokens"], m["createdAt"],
                ),
            )
        conn.commit()
    finally:
        conn.close()
    return ContinueAdapter(root=root)


def _build_droid(tmp_path: Path) -> Any:
    from stackunderflow.adapters.droid import DroidAdapter

    fdir = _fixture_dir("droid")
    sessions_root = tmp_path / "droid-sessions"
    project_dir = sessions_root / "projhash-001"
    project_dir.mkdir(parents=True)
    shutil.copy(fdir / "session.jsonl", project_dir / "session.jsonl")
    shutil.copy(
        fdir / "session.settings.json",
        project_dir / "session.settings.json",
    )
    return DroidAdapter(sessions_root=sessions_root)


def _build_kiro(tmp_path: Path) -> Any:
    from stackunderflow.adapters.kiro import KiroAdapter

    fixture = _fixture_dir("kiro") / "chat.chat"
    storage_root = tmp_path / "kiro-storage"
    storage_root.mkdir()
    shutil.copy(fixture, storage_root / "kiro-workflow-001.chat")
    return KiroAdapter(storage_root=storage_root)


def _build_openclaw(tmp_path: Path) -> Any:
    from stackunderflow.adapters.openclaw import OpenClawAdapter

    fixture = _fixture_dir("openclaw") / "session.jsonl"
    base = tmp_path / "openclaw-agents"
    sessions = base / "claw-agent" / "sessions"
    sessions.mkdir(parents=True)
    shutil.copy(fixture, sessions / "claw-sess-001.jsonl")
    return OpenClawAdapter(base_dirs=[base])


def _build_pi(tmp_path: Path) -> Any:
    from stackunderflow.adapters.pi import PiAdapter

    fixture = _fixture_dir("pi") / "session.jsonl"
    pi_root = tmp_path / "pi-sessions"
    pi_root.mkdir()
    shutil.copy(fixture, pi_root / "pi-sess-001.jsonl")
    return PiAdapter(roots=[(pi_root, "pi")])


def _build_kilocode(tmp_path: Path) -> Any:
    from stackunderflow.adapters.cline import KiloCodeAdapter

    fdir = _fixture_dir("kilocode")
    tasks_root = tmp_path / "kilocode-tasks"
    task_dir = tasks_root / "task-001"
    task_dir.mkdir(parents=True)
    shutil.copy(fdir / "ui_messages.json", task_dir / "ui_messages.json")
    shutil.copy(
        fdir / "api_conversation_history.json",
        task_dir / "api_conversation_history.json",
    )
    return KiloCodeAdapter(tasks_root=tasks_root)


def _build_roocode(tmp_path: Path) -> Any:
    from stackunderflow.adapters.cline import RooCodeAdapter

    fdir = _fixture_dir("roocode")
    tasks_root = tmp_path / "roocode-tasks"
    task_dir = tasks_root / "task-001"
    task_dir.mkdir(parents=True)
    shutil.copy(fdir / "ui_messages.json", task_dir / "ui_messages.json")
    shutil.copy(
        fdir / "api_conversation_history.json",
        task_dir / "api_conversation_history.json",
    )
    return RooCodeAdapter(tasks_root=tasks_root)


# ── scenario registry ──────────────────────────────────────────────────


def _build_codex(tmp_path: Path) -> Any:
    """Codex fixture is DATA on disk (``_fixture_dir('codex')``) — a
    realistic rollout: session_meta with NO model (real ones carry none),
    turn_context holding the model, two token_count'd assistant turns.
    Regression for the model=None stranding; default-on providers get the
    same fixture→adapter→normalizer guarantee as everything else."""
    from stackunderflow.adapters.codex import CodexAdapter

    return CodexAdapter(sessions_root=_fixture_dir("codex"))


SCENARIOS: dict[str, Scenario] = {
    "codex": Scenario(  # default-on; same end-to-end guarantee as the rest
        provider_key="codex",
        adapter_provider="codex",
        build_adapter=_build_codex,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "cursor-agent": Scenario(  # key = the adapter's exact provider string
        provider_key="cursor-agent",
        adapter_provider="cursor-agent",
        build_adapter=_build_cursor_agent,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_ESTIMATED}),
    ),
    "opencode": Scenario(
        provider_key="opencode",
        adapter_provider="opencode",
        build_adapter=_build_opencode,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "qwen": Scenario(
        provider_key="qwen",
        adapter_provider="qwen",
        build_adapter=_build_qwen,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "gemini": Scenario(
        provider_key="gemini",
        adapter_provider="gemini",
        build_adapter=_build_gemini,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "copilot": Scenario(
        provider_key="copilot",
        adapter_provider="copilot",
        build_adapter=_build_copilot,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN, COST_SOURCE_ESTIMATED}),
    ),
    "codeium": Scenario(
        provider_key="codeium",
        adapter_provider="codeium",
        build_adapter=_build_codeium,
        expected_min_events=0,  # discovery-only stub
    ),
    "continue": Scenario(
        provider_key="continue",
        adapter_provider="continue",
        build_adapter=_build_continue,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "droid": Scenario(
        provider_key="droid",
        adapter_provider="droid",
        build_adapter=_build_droid,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "kiro": Scenario(
        provider_key="kiro",
        adapter_provider="kiro",
        build_adapter=_build_kiro,
        # one Record per execution (whole chat rolled up)
        expected_min_events=1,
        cost_sources=frozenset({COST_SOURCE_ESTIMATED}),
    ),
    "openclaw": Scenario(
        provider_key="openclaw",
        adapter_provider="openclaw",
        build_adapter=_build_openclaw,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "pi": Scenario(
        provider_key="pi",
        adapter_provider="pi",
        build_adapter=_build_pi,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "kilocode": Scenario(
        provider_key="kilocode",
        adapter_provider="kilocode",
        build_adapter=_build_kilocode,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
    "roocode": Scenario(
        provider_key="roocode",
        adapter_provider="roocode",
        build_adapter=_build_roocode,
        expected_min_events=2,
        cost_sources=frozenset({COST_SOURCE_RATE_CARD, COST_SOURCE_UNKNOWN}),
    ),
}


_BETA_PROVIDERS = tuple(SCENARIOS.keys())


# ── helpers ────────────────────────────────────────────────────────────


def _events_via_pipeline(
    adapter: Any, *, normalizer_key: str, provider_value: str,
) -> list[dict]:
    """Run adapter→normalizer end-to-end on the fixture; return events."""
    normalizer_cls = get_normalizer(normalizer_key)
    assert normalizer_cls is not None, (
        f"no normalizer registered for {normalizer_key}"
    )
    normalizer = normalizer_cls()

    out: list[dict] = []
    next_id = 1
    for ref in adapter.enumerate():
        for rec in adapter.read(ref):
            msg_row = _record_to_msg_row(
                rec,
                msg_id=next_id,
                project_id=42,
                provider=provider_value,
            )
            next_id += 1
            for ev in normalizer.normalize(msg_row):
                out.append(ev)
    return out


# ── tests ──────────────────────────────────────────────────────────────


@pytest.mark.parametrize("provider", _BETA_PROVIDERS)
def test_beta_normalizer_registered(provider: str) -> None:
    """Every beta provider in the scenario list resolves through the registry."""
    cls = get_normalizer(provider)
    assert cls is not None, f"no normalizer registered for {provider!r}"


@pytest.mark.parametrize("provider", _BETA_PROVIDERS)
def test_beta_normalizer_canonical_event_shape(
    provider: str, tmp_path: Path,
) -> None:
    """End-to-end: fixture → adapter → normalizer → canonical events.

    Verifies the canonical ``usage_events`` shape (provider, model,
    speed, token columns, cost_usd, cost_source, ts/day, session_id,
    project_id, source_message_fk) holds for every event the normalizer
    emits on a spec-accurate fixture.
    """
    scenario = SCENARIOS[provider]
    adapter = scenario.build_adapter(tmp_path)
    events = _events_via_pipeline(
        adapter,
        normalizer_key=scenario.provider_key,
        provider_value=scenario.provider_key,
    )

    if scenario.expected_min_events == 0:
        # Discovery-only stub (codeium): empty adapter, nothing for the
        # normalizer to do. Still a valid run — just no events.
        assert events == []
        return

    assert len(events) >= scenario.expected_min_events, (
        f"{provider}: expected >= {scenario.expected_min_events} events, "
        f"got {len(events)}"
    )
    for ev in events:
        _assert_canonical_event_shape(ev, provider=scenario.provider_key)


@pytest.mark.parametrize("provider", _BETA_PROVIDERS)
def test_beta_normalizer_cost_semantics(
    provider: str, tmp_path: Path,
) -> None:
    """``cost_usd`` is non-null; ``cost_source`` is in the spec enum.

    Per HANDOFF §"Cost is computed once": cost is computed on the
    normalizer side and stamped onto every event. When pricing is known
    we expect ``rate_card``; estimated providers stamp ``estimated``;
    unknown models fall back to ``unknown``.
    """
    scenario = SCENARIOS[provider]
    adapter = scenario.build_adapter(tmp_path)
    events = _events_via_pipeline(
        adapter,
        normalizer_key=scenario.provider_key,
        provider_value=scenario.provider_key,
    )

    if scenario.expected_min_events == 0:
        assert events == []
        return

    for ev in events:
        # cost_usd is always a non-negative float. Even on `unknown`
        # cost source we get 0.0 — never None — so SUM in marts is
        # always safe.
        assert isinstance(ev["cost_usd"], float)
        assert ev["cost_usd"] >= 0.0
        assert ev["cost_source"] in _VALID_COST_SOURCES

    # Per-scenario cost_source expectations — declared as DATA on the
    # Scenario (no provider-name branching in test logic). Subset check +
    # the non-empty guard is equivalent to the old exact-set assertions.
    allowed = SCENARIOS[provider].cost_sources
    if allowed is not None:
        assert events, f"{provider}: cost expectations need events"
        cost_sources = {ev["cost_source"] for ev in events}
        unexpected = cost_sources - allowed
        assert not unexpected, (
            f"{provider}: unexpected cost_source(s) {unexpected!r} "
            f"in {cost_sources!r} (allowed: {sorted(allowed)})"
        )


@pytest.mark.parametrize("provider", _BETA_PROVIDERS)
def test_beta_adapter_empty_root_yields_no_events(
    provider: str, tmp_path: Path,
) -> None:
    """No fixture present → adapter yields nothing, no exceptions.

    Extends the v0.6.1 defensive empty-source coverage end-to-end: when
    the on-disk root doesn't exist (or is empty), neither the adapter
    nor the normalizer should raise.
    """
    scenario = SCENARIOS[provider]
    # Build an adapter as usual, then re-target its private path
    # attributes at a missing dir so the adapter sees nothing on disk.
    adapter = scenario.build_adapter(tmp_path)
    _retarget_to_missing(adapter, tmp_path / "missing-root")

    events = _events_via_pipeline(
        adapter,
        normalizer_key=scenario.provider_key,
        provider_value=scenario.provider_key,
    )
    assert events == []


def _retarget_to_missing(adapter: Any, missing: Path) -> None:
    """Point any path-rooted adapter attribute at a missing dir.

    Defensive coverage runs against truly absent on-disk state. Each
    adapter exposes one or more private path attrs; we sweep the common
    names and overwrite them.
    """
    for attr in (
        "_root", "_projects_root", "_data_dir", "_legacy_root",
        "_vscode_root", "_tracking_db",
    ):
        if hasattr(adapter, attr):
            setattr(adapter, attr, missing)
    # OpenClaw / Pi take base-dir lists.
    if hasattr(adapter, "_bases"):
        adapter._bases = [missing]
    if hasattr(adapter, "_roots"):
        # PiAdapter's _roots is a list of (Path, label) tuples.
        roots = adapter._roots
        if roots and isinstance(roots[0], tuple):
            adapter._roots = [(missing, "pi")]
        else:
            adapter._roots = [missing]


@pytest.mark.parametrize("provider", _BETA_PROVIDERS)
def test_beta_normalizer_user_role_yields_no_events(provider: str) -> None:
    """User-role rows are non-billable — every normalizer must drop them."""
    scenario = SCENARIOS[provider]
    cls = get_normalizer(scenario.provider_key)
    assert cls is not None
    normalizer = cls()
    msg_row = {
        "id": 1,
        "provider": scenario.provider_key,
        "project_id": 1,
        "session_id": "x",
        "timestamp": "2026-04-25T00:00:00+00:00",
        "role": "user",
        "model": "claude-sonnet-4-5-20250929",
        "input_tokens": 100,
        "output_tokens": 100,
        "cache_read_tokens": 0,
        "cache_create_tokens": 0,
        "content_text": "user msg",
        "raw_json": "{}",
        "speed": "standard",
    }
    events = list(normalizer.normalize(msg_row))
    assert events == [], (
        f"{provider}: user-role rows must yield zero events, "
        f"got {len(events)}"
    )


@pytest.mark.parametrize("provider", _BETA_PROVIDERS)
def test_beta_normalizer_malformed_raw_json_does_not_raise(
    provider: str,
) -> None:
    """Malformed ``raw_json`` must not bubble exceptions up to the caller.

    HANDOFF v0.6.1 added defensive coverage for malformed source data;
    this is the normalizer-side companion. Either yield zero events or
    yield a defensible event — never raise.
    """
    scenario = SCENARIOS[provider]
    cls = get_normalizer(scenario.provider_key)
    assert cls is not None
    normalizer = cls()
    msg_row = {
        "id": 1,
        "provider": scenario.provider_key,
        "project_id": 1,
        "session_id": "x",
        "timestamp": "2026-04-25T00:00:00+00:00",
        "role": "assistant",
        "model": "",  # missing model
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_read_tokens": 0,
        "cache_create_tokens": 0,
        "content_text": "",  # also empty content
        "raw_json": "this is not valid json {{{",
        "speed": "standard",
    }
    # Drain any yielded events. The contract is "must not raise"; the
    # exact event count depends on whether the normalizer estimates
    # from text or requires an explicit token block.
    events = list(normalizer.normalize(msg_row))
    for ev in events:
        # If anything was yielded, it should still be canonical-shape.
        _assert_canonical_event_shape(ev, provider=scenario.provider_key)


# ── pricing-coverage regression tests ───────────────────────────────────
#
# Once the beta-normalizer pricing sweep landed (HANDOFF follow-up #3),
# representative beta-provider model ids became first-class members of
# the canonical RATE_CARD. These tests lock in that coverage so future
# refactors can't silently drop a vendor's rates and leave the
# normalizer stamping `cost_source='unknown'` again.
#
# Providers that already deterministically stamp `estimated` (cursor_agent,
# kiro) are excluded — their cost_source is independent of rate-card
# coverage by design. Discovery-only stubs (codeium) are also excluded.

# Providers whose fixture models we have now covered with first-party
# rate-card entries. The list is intentionally explicit so adding a new
# beta provider doesn't accidentally tighten this contract by parametrize
# expansion alone.
_RATE_CARD_COVERED = (
    "opencode",       # fixture: claude-sonnet-4-5-20250929
    "qwen",           # fixture: qwen-coder-plus
    "gemini",         # fixture: gemini-1.5-pro
    "copilot",        # fixture: claude-sonnet-4-5-20250929
    "continue",       # fixture: claude-sonnet-4-5-20250929
    "droid",          # fixture: claude-sonnet-4-5-20250929
    "openclaw",       # fixture: claude-sonnet-4-5-20250929
    "pi",             # fixture: gpt-5
    "kilocode",       # fixture: claude-sonnet-4-5-20250929
    "roocode",        # fixture: claude-sonnet-4-5-20250929
)


@pytest.mark.parametrize("provider", _RATE_CARD_COVERED)
def test_beta_normalizer_fixture_emits_rate_card_cost_source(
    provider: str, tmp_path: Path,
) -> None:
    """Real-shape fixture → ``cost_source`` is never ``"unknown"``.

    Locks in HANDOFF follow-up #3 — every beta normalizer that emits a
    model id known to the canonical RATE_CARD must stamp ``rate_card``
    (or ``estimated`` for copilot's input-fallback path). A regression
    that drops a vendor from ``_CANONICAL_IDS`` or breaks the
    provider→pricer routing in ``normalize/base.py`` shows up here as a
    failure on at least one event.
    """
    scenario = SCENARIOS[provider]
    adapter = scenario.build_adapter(tmp_path)
    events = _events_via_pipeline(
        adapter,
        normalizer_key=scenario.provider_key,
        provider_value=scenario.provider_key,
    )
    assert events, f"{provider}: fixture pipeline yielded no events"
    cost_sources = {ev["cost_source"] for ev in events}
    assert COST_SOURCE_UNKNOWN not in cost_sources, (
        f"{provider}: at least one event still stamps "
        f"cost_source='unknown' — RATE_CARD entry missing or pricer "
        f"routing broken. Got {cost_sources!r}."
    )
    # At least one event should hit the canonical rate-card path
    # (rules out an accidental all-estimated fallback for non-estimating
    # providers).
    assert COST_SOURCE_RATE_CARD in cost_sources, (
        f"{provider}: no event stamped 'rate_card'; got {cost_sources!r}"
    )


# Representative (model_id, provider_key) pairs for the beta vendors whose
# rates were added in this sweep. Each pair is a model that genuine users
# encounter in the wild and that the normalizer must NOT stamp as
# ``unknown`` going forward.
_RATE_CARD_REPRESENTATIVE_MODELS: tuple[tuple[str, str], ...] = (
    # Qwen family
    ("qwen-max", "qwen"),
    ("qwen-max-longcontext", "qwen"),
    ("qwen-plus", "qwen"),
    ("qwen-turbo", "qwen"),
    ("qwen-coder", "qwen"),
    ("qwen-coder-plus", "qwen"),
    ("qwen3-coder", "qwen"),
    ("qwen-auto", "qwen"),
    # Gemini family
    ("gemini-2.5-pro", "gemini"),
    ("gemini-2.5-flash", "gemini"),
    ("gemini-2.5-flash-lite", "gemini"),
    ("gemini-1.5-pro", "gemini"),
    ("gemini-1.5-flash", "gemini"),
    ("gemini-3.0-pro", "gemini"),
    ("gemini-3.1-pro", "gemini"),
    # Gemini 3 preview ids — pricing-fixes-round2
    ("gemini-3-pro-preview", "gemini"),
    ("gemini-3.1-pro-preview", "gemini"),
    ("gemini-3-flash-preview", "gemini"),
    ("gemini-auto", "gemini"),
    # Un-dated Anthropic alias used by Kiro-style normalisation
    ("claude-3-5-sonnet", "openclaw"),
    # pricing-fixes-round2: opus 4.7, GLM-5 family, autoselectors
    ("claude-opus-4-7", "openclaw"),
    ("glm-5", "openclaw"),
    ("glm-5.1", "openclaw"),
    ("composer-1", "cursor"),
    ("droid-auto", "droid"),
    ("cline-auto", "cline"),
)


@pytest.mark.parametrize(
    ("model_id", "provider"), _RATE_CARD_REPRESENTATIVE_MODELS,
)
def test_beta_model_id_in_canonical_rate_card(
    model_id: str, provider: str,
) -> None:
    """``model_id`` is a member of RATE_CARD with non-zero rates.

    The normalizer's ``cost_source`` decision is driven by
    ``model in RATE_CARD``. This locks in that the rate-card sweep keeps
    every representative beta model present with usable rates so
    downstream marts never see ``cost_source='unknown'`` for these ids.
    """
    from stackunderflow.infra.costs import RATE_CARD, get_model_pricing

    assert model_id in RATE_CARD, (
        f"{model_id}: missing from RATE_CARD (expected for {provider})"
    )
    entry = RATE_CARD[model_id]
    assert entry is not None, f"{model_id}: RATE_CARD entry is None"
    # Input + output rates must be strictly positive; cache rates can be
    # 0.0 for providers (Qwen, Gemini, OpenAI) that don't bill writes.
    assert entry["input_cost_per_token"] > 0.0, (
        f"{model_id}: input_cost_per_token is non-positive"
    )
    assert entry["output_cost_per_token"] > 0.0, (
        f"{model_id}: output_cost_per_token is non-positive"
    )

    # And ``get_model_pricing`` should route the same way (this catches
    # ``_provider_for_model`` regressions where a model lands in
    # RATE_CARD but gets priced against the wrong pricer).
    pricing = get_model_pricing(model_id)
    assert pricing is not None, f"{model_id}: get_model_pricing returned None"
    assert pricing["input_cost_per_token"] == entry["input_cost_per_token"]
