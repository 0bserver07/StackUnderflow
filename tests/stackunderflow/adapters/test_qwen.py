"""Unit tests for the Qwen adapter.

Builds a synthetic ``$QWEN_DATA_DIR/projects/<project>/chats/*.jsonl``
tree under ``tmp_path`` and points the adapter at it. Exercises:

- ``enumerate()`` discovers chats by project directory.
- ``read()`` yields one ``Record`` per ``user`` / ``assistant`` entry.
- Token normalization: ``input = promptTokenCount - cachedContentTokenCount``;
  ``output = candidatesTokenCount + thoughtsTokenCount``;
  ``cache_read = cachedContentTokenCount``; ``cache_creation = 0``.
- ``seq`` is monotonic byte offset, ``since_offset`` resumes mid-file.
- Tool extraction from ``functionCall.name`` blocks.
- The ``AdapterContract`` mixin runs the storage-aware contract test.

Spec: codeburn-catalog §13.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.base import Record, SessionRef
from stackunderflow.adapters.qwen import QwenAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── fixture builders ──────────────────────────────────────────────────


def _user_entry(
    text: str,
    *,
    session_id: str = "qwen-sess-0001",
    timestamp: str = "2026-04-29T10:00:00.000Z",
    uuid: str = "u-1",
) -> dict:
    return {
        "uuid": uuid,
        "sessionId": session_id,
        "timestamp": timestamp,
        "type": "user",
        "message": {
            "role": "user",
            "parts": [{"text": text}],
        },
    }


def _assistant_entry(
    text: str,
    *,
    session_id: str = "qwen-sess-0001",
    timestamp: str = "2026-04-29T10:00:01.000Z",
    uuid: str = "a-1",
    model: str = "qwen-plus",
    prompt_tokens: int = 1200,
    candidates_tokens: int = 350,
    thoughts_tokens: int = 150,
    cached_tokens: int = 200,
    tool_calls: list[dict] | None = None,
) -> dict:
    parts: list[dict] = [{"text": text}]
    if tool_calls:
        parts.extend(tool_calls)
    return {
        "uuid": uuid,
        "sessionId": session_id,
        "timestamp": timestamp,
        "type": "assistant",
        "model": model,
        "message": {
            "role": "assistant",
            "parts": parts,
        },
        "usageMetadata": {
            "promptTokenCount": prompt_tokens,
            "candidatesTokenCount": candidates_tokens,
            "thoughtsTokenCount": thoughts_tokens,
            "cachedContentTokenCount": cached_tokens,
        },
    }


def _write_chat(
    projects_root: Path,
    project_slug: str,
    session_id: str,
    entries: list[dict],
) -> Path:
    project = projects_root / project_slug
    chats = project / "chats"
    chats.mkdir(parents=True, exist_ok=True)
    fp = chats / f"{session_id}.jsonl"
    fp.write_text("\n".join(json.dumps(e) for e in entries) + "\n")
    return fp


@pytest.fixture
def synthetic_projects(tmp_path: Path) -> Path:
    projects_root = tmp_path / "projects"
    projects_root.mkdir()
    _write_chat(
        projects_root,
        "my-project",
        "qwen-sess-0001",
        [
            _user_entry("Refactor this please."),
            _assistant_entry(
                "Here's the refactor.",
                tool_calls=[
                    {"functionCall": {"name": "read_file", "args": {"path": "x.py"}}},
                    {"functionCall": {"name": "shell", "args": {"command": "ls"}}},
                ],
            ),
        ],
    )
    return projects_root


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_yields_one_session_ref(synthetic_projects: Path) -> None:
    adapter = QwenAdapter(projects_root=synthetic_projects)
    refs = list(adapter.enumerate())

    assert len(refs) == 1
    ref = refs[0]
    assert isinstance(ref, SessionRef)
    assert ref.provider == "qwen"
    assert ref.source_kind == "file"
    assert ref.project_slug == "my-project"
    assert ref.session_id == "qwen-sess-0001"
    assert ref.file_path.suffix == ".jsonl"
    assert ref.file_size > 0


def test_enumerate_skips_projects_without_chats(tmp_path: Path) -> None:
    projects_root = tmp_path / "projects"
    projects_root.mkdir()
    _write_chat(projects_root, "valid", "s1", [_user_entry("hi")])
    (projects_root / "no-chats").mkdir()  # project dir with no /chats subdir

    adapter = QwenAdapter(projects_root=projects_root)
    refs = list(adapter.enumerate())
    assert [r.project_slug for r in refs] == ["valid"]


def test_enumerate_returns_nothing_when_root_missing(tmp_path: Path) -> None:
    adapter = QwenAdapter(projects_root=tmp_path / "does-not-exist")
    assert list(adapter.enumerate()) == []


def test_read_yields_user_and_assistant_records(synthetic_projects: Path) -> None:
    adapter = QwenAdapter(projects_root=synthetic_projects)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))

    assert len(records) == 2
    user, asst = records
    assert isinstance(user, Record)
    assert user.role == "user"
    assert "Refactor" in user.content_text
    assert asst.role == "assistant"
    assert "Here's the refactor" in asst.content_text
    assert asst.model == "qwen-plus"


def test_read_normalizes_tokens_per_spec(synthetic_projects: Path) -> None:
    """input = prompt - cached; output = candidates + thoughts; cache_read = cached."""
    adapter = QwenAdapter(projects_root=synthetic_projects)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    asst = next(r for r in records if r.role == "assistant")

    # 1200 prompt - 200 cached = 1000 fresh input
    assert asst.input_tokens == 1000
    # 350 candidates + 150 thoughts = 500 output
    assert asst.output_tokens == 500
    assert asst.cache_read_tokens == 200
    assert asst.cache_create_tokens == 0


def test_read_extracts_tools_from_function_calls(synthetic_projects: Path) -> None:
    adapter = QwenAdapter(projects_root=synthetic_projects)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    asst = next(r for r in records if r.role == "assistant")

    # read_file -> Read; shell -> Bash (per _TOOL_NAME_MAP in the adapter)
    assert "Read" in asst.tools
    assert "Bash" in asst.tools


def test_seq_is_monotonic(synthetic_projects: Path) -> None:
    adapter = QwenAdapter(projects_root=synthetic_projects)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    seqs = [r.seq for r in records]
    assert seqs == sorted(set(seqs))


def test_since_offset_resumes_mid_file(tmp_path: Path) -> None:
    """Multi-turn fixture so we can resume past the first turn's offset."""
    projects_root = tmp_path / "projects"
    projects_root.mkdir()
    _write_chat(
        projects_root,
        "multi",
        "sess-multi",
        [
            _user_entry("first user", uuid="u1"),
            _assistant_entry(
                "first assistant",
                uuid="a1",
                prompt_tokens=100, candidates_tokens=10,
                thoughts_tokens=0, cached_tokens=0,
            ),
            _user_entry("second user", uuid="u2",
                        timestamp="2026-04-29T10:00:02.000Z"),
            _assistant_entry(
                "second assistant",
                uuid="a2",
                timestamp="2026-04-29T10:00:03.000Z",
                prompt_tokens=200, candidates_tokens=20,
                thoughts_tokens=0, cached_tokens=0,
            ),
        ],
    )
    adapter = QwenAdapter(projects_root=projects_root)
    ref = next(iter(adapter.enumerate()))

    full = list(adapter.read(ref))
    assert len(full) == 4

    midpoint = full[1].seq  # byte offset of the first assistant line
    resumed = list(adapter.read(ref, since_offset=midpoint))

    assert all(r.seq > midpoint for r in resumed)
    assert len(resumed) < len(full)
    # Resumed must still include the second turn's content.
    assert any("second" in r.content_text for r in resumed)


def test_malformed_json_line_is_skipped(tmp_path: Path) -> None:
    projects_root = tmp_path / "projects"
    chats = projects_root / "p" / "chats"
    chats.mkdir(parents=True)
    fp = chats / "sess.jsonl"
    fp.write_text(
        json.dumps(_user_entry("before bad")) + "\n"
        + "{not-json}\n"
        + json.dumps(_assistant_entry("after bad")) + "\n"
    )
    adapter = QwenAdapter(projects_root=projects_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))  # must not raise
    # Bad line is dropped; surrounding records survive.
    assert len(records) == 2
    assert records[0].role == "user"
    assert records[1].role == "assistant"


def test_default_model_when_missing(tmp_path: Path) -> None:
    projects_root = tmp_path / "projects"
    chats = projects_root / "p" / "chats"
    chats.mkdir(parents=True)
    entry = _assistant_entry("hi", model="")
    # blank string -> falsy -> default
    del entry["model"]
    fp = chats / "sess.jsonl"
    fp.write_text(json.dumps(entry) + "\n")
    adapter = QwenAdapter(projects_root=projects_root)
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert records[0].model == "qwen-auto"


# ── shared adapter contract ───────────────────────────────────────────


class TestQwenAdapterContract(unittest.TestCase, AdapterContract):
    """Runs the shared AdapterContract invariants against a synthetic chat."""

    def setUp(self):
        import tempfile

        self._tmp = tempfile.TemporaryDirectory()
        projects_root = Path(self._tmp.name) / "projects"
        projects_root.mkdir()
        # Multi-turn so the contract's storage-aware test has a midpoint.
        _write_chat(
            projects_root,
            "contract-project",
            "contract-sess",
            [
                _user_entry("hi", uuid="u1"),
                _assistant_entry(
                    "hello",
                    uuid="a1",
                    prompt_tokens=100, candidates_tokens=10,
                    thoughts_tokens=0, cached_tokens=0,
                ),
                _user_entry(
                    "again", uuid="u2",
                    timestamp="2026-04-29T10:00:02.000Z",
                ),
                _assistant_entry(
                    "ok",
                    uuid="a2",
                    timestamp="2026-04-29T10:00:03.000Z",
                    prompt_tokens=200, candidates_tokens=20,
                    thoughts_tokens=0, cached_tokens=0,
                ),
            ],
        )
        self.adapter = QwenAdapter(projects_root=projects_root)

    def tearDown(self):
        self._tmp.cleanup()


# ── malformed-input hardening (ingest-surface sweep, 2026-07) ─────────


def test_non_string_model_falls_back(tmp_path: Path) -> None:
    """A dict/number ``model`` must not leak into the Record (it would
    poison the store write) — assistant falls back to ``qwen-auto``."""
    chats = tmp_path / "projects" / "proj-a" / "chats"
    chats.mkdir(parents=True)
    (chats / "chat1.jsonl").write_text(
        json.dumps({
            "uuid": "u1",
            "sessionId": "s1",
            "timestamp": "2026-05-01T10:00:00Z",
            "type": "assistant",
            "model": {"bad": 1},
            "message": {"role": "assistant", "parts": [{"text": "hello"}]},
            "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 2},
        })
        + "\n"
        + json.dumps({
            "uuid": "u2",
            "sessionId": "s1",
            "timestamp": "2026-05-01T10:00:01Z",
            "type": "user",
            "model": 42,
            "message": {"role": "user", "parts": [{"text": "hi"}]},
        })
        + "\n"
    )
    adapter = QwenAdapter(projects_root=tmp_path / "projects")
    ref = next(iter(adapter.enumerate()))
    records = list(adapter.read(ref))
    assert len(records) == 2
    assert records[0].model == "qwen-auto"
    assert records[1].model is None
