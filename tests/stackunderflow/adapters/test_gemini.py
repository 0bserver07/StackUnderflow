"""Unit tests for the Gemini adapter.

Gemini ships two on-disk formats; both are covered:

- **CLI ≤0.38 (single JSON)**: a single top-level object with a
  ``messages: [...]`` array. ``seq`` is the message index. Resume
  semantics are "skip records at-or-before index N" (same pattern as
  the Cline adapter).
- **CLI ≥0.39 (JSONL)**: one metadata line followed by one message
  line per entry. ``seq`` is the byte offset of the line start (same as
  Codex / Qwen).

Token normalization (cached subtracted from input, thoughts folded into
output) is exercised against both variants so a future
breaking change in the upstream shape can't quietly miscount.

Spec: codeburn-catalog §7.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.gemini import GeminiAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _user_msg(
    text: str,
    *,
    msg_id: str = "u1",
    timestamp: str = "2026-04-29T10:00:00.000Z",
) -> dict:
    return {
        "id": msg_id,
        "timestamp": timestamp,
        "type": "user",
        "content": text,
    }


def _gemini_msg(
    text: str,
    *,
    msg_id: str = "g1",
    timestamp: str = "2026-04-29T10:00:01.000Z",
    model: str = "gemini-2.5-pro",
    raw_input: int = 1000,
    raw_output: int = 400,
    cached: int = 200,
    thoughts: int = 100,
    tool_calls: list[dict] | None = None,
) -> dict:
    return {
        "id": msg_id,
        "timestamp": timestamp,
        "type": "gemini",
        "content": [{"text": text}],
        "tokens": {
            "input": raw_input,
            "output": raw_output,
            "cached": cached,
            "thoughts": thoughts,
            "tool": 0,
            "total": raw_input + raw_output + thoughts,
        },
        "model": model,
        "toolCalls": tool_calls or [],
    }


def _write_single_json(
    projects_root: Path,
    project_slug: str,
    session_id: str,
    messages: list[dict],
) -> Path:
    chats = projects_root / project_slug / "chats"
    chats.mkdir(parents=True, exist_ok=True)
    fp = chats / f"session-{session_id}.json"
    doc = {
        "sessionId": session_id,
        "startTime": "2026-04-29T10:00:00.000Z",
        "messages": messages,
    }
    fp.write_text(json.dumps(doc))
    return fp


def _write_jsonl(
    projects_root: Path,
    project_slug: str,
    session_id: str,
    messages: list[dict],
    *,
    metadata: dict | None = None,
) -> Path:
    chats = projects_root / project_slug / "chats"
    chats.mkdir(parents=True, exist_ok=True)
    fp = chats / f"session-{session_id}.jsonl"
    meta = metadata or {
        "sessionId": session_id,
        "startTime": "2026-04-29T10:00:00.000Z",
        "kind": "session_metadata",
    }
    lines = [json.dumps(meta)] + [json.dumps(m) for m in messages]
    fp.write_text("\n".join(lines) + "\n")
    return fp


# ── single-JSON (CLI ≤0.38) ──────────────────────────────────────────


@pytest.fixture
def synthetic_single_json(tmp_path: Path) -> Path:
    projects_root = tmp_path / "tmp"
    projects_root.mkdir()
    _write_single_json(
        projects_root,
        "my-project",
        "abc-001",
        [
            _user_msg("Refactor please."),
            _gemini_msg(
                "Sure, here's the refactor.",
                tool_calls=[
                    {"id": "tc1", "name": "read_file", "args": {"path": "x.py"}},
                    {"id": "tc2", "name": "shell", "args": {"command": "ls"}},
                ],
            ),
        ],
    )
    return projects_root


def test_single_json_enumerate(synthetic_single_json: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_single_json)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "gemini"
    assert ref.source_kind == "file"
    assert ref.project_slug == "my-project"
    assert ref.file_path.suffix == ".json"


def test_single_json_read_yields_records(synthetic_single_json: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_single_json)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 2
    user, asst = records
    assert isinstance(user, Record)
    assert user.role == "user"
    assert user.content_text == "Refactor please."
    assert asst.role == "assistant"
    assert "refactor" in asst.content_text.lower()
    assert asst.model == "gemini-2.5-pro"
    # session_id picked up from the document, not the filename
    assert asst.session_id == "abc-001"


def test_single_json_normalizes_tokens(synthetic_single_json: Path) -> None:
    """input = raw input - cached; output = raw output + thoughts; cache_read = cached."""
    adapter = GeminiAdapter(projects_root=synthetic_single_json)
    ref = next(iter(adapter.enumerate()))
    asst = next(r for r in adapter.read(ref) if r.role == "assistant")

    # 1000 - 200 = 800 fresh input
    assert asst.input_tokens == 800
    # 400 + 100 = 500 output
    assert asst.output_tokens == 500
    assert asst.cache_read_tokens == 200
    assert asst.cache_create_tokens == 0


def test_single_json_extracts_tools(synthetic_single_json: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_single_json)
    ref = next(iter(adapter.enumerate()))
    asst = next(r for r in adapter.read(ref) if r.role == "assistant")
    assert "Read" in asst.tools
    assert "Bash" in asst.tools


def test_single_json_seq_is_message_index(synthetic_single_json: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_single_json)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    # Single-JSON seq is the messages-array index.
    assert [r.seq for r in records] == [0, 1]


def test_single_json_since_offset_resumes(tmp_path: Path) -> None:
    projects_root = tmp_path / "tmp"
    projects_root.mkdir()
    _write_single_json(
        projects_root,
        "p",
        "multi",
        [
            _user_msg("first", msg_id="u1"),
            _gemini_msg("first asst", msg_id="g1",
                        raw_input=100, raw_output=10, cached=0, thoughts=0),
            _user_msg("second", msg_id="u2"),
            _gemini_msg("second asst", msg_id="g2",
                        raw_input=200, raw_output=20, cached=0, thoughts=0),
        ],
    )
    adapter = GeminiAdapter(projects_root=projects_root)
    ref = next(iter(adapter.enumerate()))
    full = list(adapter.read(ref))
    assert len(full) == 4

    midpoint = full[1].seq  # = 1
    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)
    assert any("second" in r.content_text for r in resumed)


# ── JSONL (CLI ≥0.39) ────────────────────────────────────────────────


@pytest.fixture
def synthetic_jsonl(tmp_path: Path) -> Path:
    projects_root = tmp_path / "tmp"
    projects_root.mkdir()
    _write_jsonl(
        projects_root,
        "my-project",
        "xyz-002",
        [
            _user_msg("Hello"),
            _gemini_msg("Hi back"),
        ],
    )
    return projects_root


def test_jsonl_enumerate(synthetic_jsonl: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_jsonl)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    ref = refs[0]
    assert ref.file_path.suffix == ".jsonl"
    assert ref.source_hint == {"format": "jsonl"}


def test_jsonl_read_yields_records(synthetic_jsonl: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_jsonl)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert [r.role for r in records] == ["user", "assistant"]
    # session_id refined from the metadata line
    assert records[0].session_id == "xyz-002"


def test_jsonl_normalizes_tokens(synthetic_jsonl: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_jsonl)
    ref = next(iter(adapter.enumerate()))
    asst = next(r for r in adapter.read(ref) if r.role == "assistant")
    # Same numbers as the single-JSON fixture (default _gemini_msg)
    assert asst.input_tokens == 800
    assert asst.output_tokens == 500
    assert asst.cache_read_tokens == 200
    assert asst.cache_create_tokens == 0


def test_jsonl_seq_is_byte_offset(synthetic_jsonl: Path) -> None:
    adapter = GeminiAdapter(projects_root=synthetic_jsonl)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    # Byte offsets must be > 0 for every record (line 0 was metadata).
    assert all(r.seq > 0 for r in records)
    assert records[0].seq < records[1].seq


def test_jsonl_since_offset_resumes_mid_file(tmp_path: Path) -> None:
    projects_root = tmp_path / "tmp"
    projects_root.mkdir()
    _write_jsonl(
        projects_root,
        "p",
        "multi",
        [
            _user_msg("first", msg_id="u1"),
            _gemini_msg("first asst", msg_id="g1",
                        raw_input=100, raw_output=10, cached=0, thoughts=0),
            _user_msg("second", msg_id="u2"),
            _gemini_msg("second asst", msg_id="g2",
                        raw_input=200, raw_output=20, cached=0, thoughts=0),
        ],
    )
    adapter = GeminiAdapter(projects_root=projects_root)
    ref = next(iter(adapter.enumerate()))

    full = list(adapter.read(ref))
    assert len(full) == 4

    midpoint = full[1].seq
    resumed = list(adapter.read(ref, since_offset=midpoint))
    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)


def test_jsonl_malformed_line_is_skipped(tmp_path: Path) -> None:
    projects_root = tmp_path / "tmp"
    chats = projects_root / "p" / "chats"
    chats.mkdir(parents=True)
    fp = chats / "session-bad.jsonl"
    fp.write_text(
        json.dumps({"sessionId": "bad", "kind": "session_metadata"}) + "\n"
        + json.dumps(_user_msg("hello")) + "\n"
        + "{not-json}\n"
        + json.dumps(_gemini_msg("world")) + "\n"
    )
    adapter = GeminiAdapter(projects_root=projects_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))  # must not raise
    assert len(records) == 2
    assert records[0].role == "user"
    assert records[1].role == "assistant"


# ── shared adapter contract — single JSON variant ────────────────────


class TestGeminiAdapterContractSingleJson(unittest.TestCase, AdapterContract):
    """Runs the shared AdapterContract invariants against a single-JSON fixture."""

    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        projects_root = Path(self._tmp.name) / "tmp"
        projects_root.mkdir()
        _write_single_json(
            projects_root,
            "contract-project",
            "contract-sess",
            [
                _user_msg("hi", msg_id="u1"),
                _gemini_msg(
                    "hello",
                    msg_id="g1",
                    raw_input=100, raw_output=10, cached=0, thoughts=0,
                ),
                _user_msg(
                    "again", msg_id="u2",
                    timestamp="2026-04-29T10:00:02.000Z",
                ),
                _gemini_msg(
                    "ok",
                    msg_id="g2",
                    timestamp="2026-04-29T10:00:03.000Z",
                    raw_input=200, raw_output=20, cached=0, thoughts=0,
                ),
            ],
        )
        self.adapter = GeminiAdapter(projects_root=projects_root)

    def tearDown(self):
        self._tmp.cleanup()


# ── shared adapter contract — JSONL variant ──────────────────────────


class TestGeminiAdapterContractJsonl(unittest.TestCase, AdapterContract):
    """Runs the shared AdapterContract invariants against a JSONL fixture."""

    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        projects_root = Path(self._tmp.name) / "tmp"
        projects_root.mkdir()
        _write_jsonl(
            projects_root,
            "contract-project",
            "contract-sess",
            [
                _user_msg("hi", msg_id="u1"),
                _gemini_msg(
                    "hello",
                    msg_id="g1",
                    raw_input=100, raw_output=10, cached=0, thoughts=0,
                ),
                _user_msg(
                    "again", msg_id="u2",
                    timestamp="2026-04-29T10:00:02.000Z",
                ),
                _gemini_msg(
                    "ok",
                    msg_id="g2",
                    timestamp="2026-04-29T10:00:03.000Z",
                    raw_input=200, raw_output=20, cached=0, thoughts=0,
                ),
            ],
        )
        self.adapter = GeminiAdapter(projects_root=projects_root)

    def tearDown(self):
        self._tmp.cleanup()


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_non_string_model_falls_back(tmp_path: Path) -> None:
    """A dict/number ``model`` must not leak into the Record (it would
    poison the store write) — assistant falls back to ``gemini-auto``."""
    chats = tmp_path / "tmp" / "proj-a" / "chats"
    chats.mkdir(parents=True)
    (chats / "session-1.json").write_text(json.dumps({
        "sessionId": "s1",
        "messages": [
            {
                "id": "m1",
                "timestamp": "2026-05-01T10:00:00Z",
                "type": "gemini",
                "content": "hello",
                "model": {"bad": 1},
                "tokens": {"input": 4, "output": 2},
            },
            {
                "id": "m2",
                "timestamp": "2026-05-01T10:00:01Z",
                "type": "user",
                "content": "hi",
                "model": 42,
            },
        ],
    }))
    adapter = GeminiAdapter(projects_root=tmp_path / "tmp")
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 2
    assert records[0].model == "gemini-auto"
    assert records[1].model is None
