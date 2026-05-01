"""Unit tests for the Copilot (legacy + VS Code transcript) adapter.

Builds two synthetic JSONL fixtures under ``tmp_path``:

  - Legacy: ``legacy/{sessionId}/events.jsonl`` with model_change +
    user.message + assistant.message events. Asserts model carries
    forward across events.
  - VS Code transcript:
    ``workspaceStorage/{hash}/GitHub.copilot-chat/transcripts/{id}.jsonl``
    with a ``session.start`` header + alternating user/assistant events.
    Asserts tool-call-id-prefix model inference (``toolu_bdrk_*`` →
    ``claude-auto``, ``call_*`` → ``gpt-auto``) when no explicit model
    is set.

Also exercises the ``cost_source="estimated"`` flag on rows where output
tokens are missing and inherits ``AdapterContract`` for the shared
invariants (monotonic seq, ISO timestamps, resumable read).
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record
from stackunderflow.adapters.copilot import CopilotAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _write_jsonl(path: Path, events: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for ev in events:
            fh.write(json.dumps(ev) + "\n")


def _legacy_session(
    legacy_root: Path,
    session_id: str,
    *,
    events: list[dict] | None = None,
    workspace_cwd: str | None = "/Users/test/code/proj",
) -> Path:
    session_dir = legacy_root / session_id
    if events is None:
        events = [
            {"type": "session.model_change", "model": "claude-3-5-sonnet"},
            {
                "type": "user.message",
                "content": "Add tests for the parser.",
                "timestamp": "2026-04-29T10:00:00Z",
            },
            {
                "type": "assistant.message",
                "model": "claude-3-5-sonnet",
                "content": "Here's a test plan…",
                "outputTokens": 240,
                "inputTokens": 30,
                "timestamp": "2026-04-29T10:00:01Z",
            },
        ]
    _write_jsonl(session_dir / "events.jsonl", events)
    if workspace_cwd is not None:
        (session_dir / "workspace.json").write_text(
            json.dumps({"cwd": workspace_cwd})
        )
    return session_dir


def _vscode_transcript(
    workspace_storage: Path,
    workspace_hash: str,
    transcript_id: str,
    *,
    events: list[dict] | None = None,
) -> Path:
    transcripts_dir = (
        workspace_storage / workspace_hash / "GitHub.copilot-chat" / "transcripts"
    )
    if events is None:
        events = [
            {
                "type": "session.start",
                "data": {"producer": "copilot-agent"},
                "timestamp": "2026-04-29T11:00:00Z",
            },
            {
                "type": "user.message",
                "data": {"content": "Refactor get_user."},
                "timestamp": "2026-04-29T11:00:01Z",
            },
            {
                "type": "assistant.message",
                # No explicit model — adapter must infer from the
                # tool-call id prefix.
                "data": {
                    "content": "I'll edit the file.",
                    "toolCalls": [
                        {"id": "toolu_bdrk_01ABCDEFG", "name": "edit_file"},
                    ],
                    "outputTokens": 180,
                },
                "timestamp": "2026-04-29T11:00:02Z",
            },
        ]
    _write_jsonl(transcripts_dir / f"{transcript_id}.jsonl", events)
    return transcripts_dir / f"{transcript_id}.jsonl"


@pytest.fixture
def copilot_dirs(tmp_path: Path) -> tuple[Path, Path]:
    legacy_root = tmp_path / "legacy"
    workspace_storage = tmp_path / "workspaceStorage"
    legacy_root.mkdir()
    workspace_storage.mkdir()
    _legacy_session(legacy_root, "sess-legacy-001")
    _vscode_transcript(workspace_storage, "ws-hash-aaa", "transcript-001")
    return legacy_root, workspace_storage


# ── enumeration ───────────────────────────────────────────────────────


def test_enumerate_yields_one_ref_per_session(copilot_dirs: tuple[Path, Path]) -> None:
    legacy, vscode = copilot_dirs
    adapter = CopilotAdapter(legacy_root=legacy, vscode_workspace_storage=vscode)
    refs = list(adapter.enumerate())
    assert len(refs) == 2
    by_id = {r.session_id: r for r in refs}
    assert "sess-legacy-001" in by_id
    assert "transcript-001" in by_id

    legacy_ref = by_id["sess-legacy-001"]
    assert legacy_ref.provider == "copilot"
    assert legacy_ref.source_kind == "file"
    assert (legacy_ref.source_hint or {}).get("format") == "legacy"
    # workspace.json carried cwd → slugified into project_slug
    assert legacy_ref.project_slug == "Users-test-code-proj"

    vscode_ref = by_id["transcript-001"]
    assert vscode_ref.source_kind == "file"
    hint = vscode_ref.source_hint or {}
    assert hint.get("format") == "vscode-transcript"
    assert hint.get("workspace_hash") == "ws-hash-aaa"
    assert "ws-hash-aaa" in vscode_ref.project_slug


def test_enumerate_returns_nothing_when_dirs_missing(tmp_path: Path) -> None:
    adapter = CopilotAdapter(
        legacy_root=tmp_path / "missing-legacy",
        vscode_workspace_storage=tmp_path / "missing-ws",
    )
    assert list(adapter.enumerate()) == []


# ── reading ──────────────────────────────────────────────────────────


def test_read_legacy_emits_assistant_record_with_explicit_tokens(
    copilot_dirs: tuple[Path, Path],
) -> None:
    legacy, vscode = copilot_dirs
    adapter = CopilotAdapter(legacy_root=legacy, vscode_workspace_storage=vscode)
    refs = [r for r in adapter.enumerate() if r.session_id == "sess-legacy-001"]
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    assert isinstance(rec, Record)
    assert rec.provider == "copilot"
    assert rec.role == "assistant"
    assert rec.model == "claude-3-5-sonnet"
    assert rec.input_tokens == 30
    assert rec.output_tokens == 240
    # Explicit counts → not estimated.
    assert rec.raw.get("cost_source") != "estimated"


def test_read_vscode_infers_model_from_tool_call_id(
    copilot_dirs: tuple[Path, Path],
) -> None:
    legacy, vscode = copilot_dirs
    adapter = CopilotAdapter(legacy_root=legacy, vscode_workspace_storage=vscode)
    refs = [r for r in adapter.enumerate() if r.session_id == "transcript-001"]
    records = list(adapter.read(refs[0]))
    assert len(records) == 1
    rec = records[0]
    # `toolu_bdrk_*` prefix → claude-auto (pricer routes to Anthropic).
    assert rec.model == "claude-auto"
    assert rec.input_tokens > 0  # estimated from preceding user.message
    assert rec.output_tokens == 180
    assert rec.raw.get("cost_source") == "estimated"
    # The "tools" tuple should pick up the tool name.
    assert "edit_file" in rec.tools


def test_read_infers_openai_from_call_prefix(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    legacy.mkdir()
    _legacy_session(
        legacy,
        "sess-openai",
        events=[
            {"type": "user.message", "content": "list files"},
            {
                "type": "assistant.message",
                "content": "running ls",
                "toolCalls": [{"id": "call_8888", "name": "shell"}],
                "outputTokens": 50,
            },
        ],
        workspace_cwd=None,
    )
    adapter = CopilotAdapter(
        legacy_root=legacy,
        vscode_workspace_storage=tmp_path / "ws-missing",
    )
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].model == "gpt-auto"


def test_read_estimates_output_tokens_when_missing(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    legacy.mkdir()
    body = "abcd" * 50  # 200 chars → 50 tokens at len/4
    _legacy_session(
        legacy,
        "sess-estimated",
        events=[
            {"type": "session.model_change", "model": "claude-3-5-sonnet"},
            {"type": "user.message", "content": "ping"},
            {
                "type": "assistant.message",
                "content": body,
                # outputTokens deliberately omitted.
            },
        ],
        workspace_cwd=None,
    )
    adapter = CopilotAdapter(
        legacy_root=legacy,
        vscode_workspace_storage=tmp_path / "ws-missing",
    )
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    rec = records[0]
    assert rec.output_tokens == len(body) // 4
    assert rec.raw.get("cost_source") == "estimated"


def test_read_skips_assistant_with_zero_output_and_empty_text(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    legacy.mkdir()
    _legacy_session(
        legacy,
        "sess-empty",
        events=[
            {"type": "user.message", "content": "go"},
            {
                "type": "assistant.message",
                "content": "",
                "outputTokens": 0,
            },
        ],
        workspace_cwd=None,
    )
    adapter = CopilotAdapter(
        legacy_root=legacy,
        vscode_workspace_storage=tmp_path / "ws-missing",
    )
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert records == []


def test_read_resume_with_since_offset_skips_earlier_records(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    legacy.mkdir()
    _legacy_session(
        legacy,
        "sess-resume",
        events=[
            {"type": "session.model_change", "model": "claude-3-5-sonnet"},
            {"type": "user.message", "content": "first"},
            {"type": "assistant.message", "content": "first reply", "outputTokens": 10},
            {"type": "user.message", "content": "second"},
            {"type": "assistant.message", "content": "second reply", "outputTokens": 20},
        ],
        workspace_cwd=None,
    )
    adapter = CopilotAdapter(
        legacy_root=legacy,
        vscode_workspace_storage=tmp_path / "ws-missing",
    )
    ref = next(iter(adapter.enumerate()))
    full = list(adapter.read(ref))
    assert len(full) == 2
    midpoint = full[0].seq
    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


def test_read_tolerates_malformed_json_lines(tmp_path: Path) -> None:
    legacy = tmp_path / "legacy"
    session_dir = legacy / "sess-bad"
    session_dir.mkdir(parents=True)
    events_path = session_dir / "events.jsonl"
    events_path.write_text(
        '{"type":"user.message","content":"go"}\n'
        "this is not json\n"
        '{"type":"assistant.message","content":"ok","outputTokens":5,'
        '"model":"claude-3-5-sonnet"}\n'
    )
    adapter = CopilotAdapter(
        legacy_root=legacy,
        vscode_workspace_storage=tmp_path / "ws-missing",
    )
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 1
    assert records[0].output_tokens == 5


# ── shared adapter contract ───────────────────────────────────────────


class TestCopilotAdapterContract(unittest.TestCase, AdapterContract):
    """Run the shared invariants against a populated legacy fixture."""

    def setUp(self) -> None:
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        legacy = Path(self._tmp.name) / "legacy"
        legacy.mkdir()
        # Multi-turn legacy session so resume tests exercise a midpoint.
        _legacy_session(
            legacy,
            "contract-session",
            events=[
                {"type": "session.model_change", "model": "claude-3-5-sonnet"},
                {"type": "user.message", "content": "one"},
                {
                    "type": "assistant.message",
                    "content": "first reply",
                    "outputTokens": 10,
                    "timestamp": "2026-04-29T10:00:01Z",
                },
                {"type": "user.message", "content": "two"},
                {
                    "type": "assistant.message",
                    "content": "second reply",
                    "outputTokens": 12,
                    "timestamp": "2026-04-29T10:00:02Z",
                },
            ],
            workspace_cwd=None,
        )
        self.adapter = CopilotAdapter(
            legacy_root=legacy,
            vscode_workspace_storage=Path(self._tmp.name) / "ws-missing",
        )

    def tearDown(self) -> None:
        self._tmp.cleanup()
