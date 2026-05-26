"""Unit tests for the Antigravity adapter.

Builds a synthetic ``~/.gemini/antigravity/`` tree under ``tmp_path``
with a hand-encoded ``agyhub_summaries_proto.pb`` and a JSONL CLI
history. The adapter is pointed at it via the ``gemini_home``
constructor override.

Exercises:

* ``enumerate()`` produces one SessionRef per unique conversation UUID
  across summary + CLI history (no duplicates).
* Workspace fallback: summary's workspace wins; missing summary
  workspace falls back to the CLI history entry.
* ``read()`` emits one synthetic title marker per titled conversation
  plus one Record per matching CLI prompt.
* All Records carry ``raw["cost_source"] = "encrypted"`` and zero
  tokens — the adapter never invents numbers off encrypted content.
* Resumable reads via ``since_offset`` follow the seq-based contract.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

import pytest

from stackunderflow.adapters.antigravity import AntigravityAdapter
from tests.stackunderflow.adapters.contract import AdapterContract


# ── tiny protobuf encoder (test-only) ─────────────────────────────────


def _encode_varint(v: int) -> bytes:
    out = bytearray()
    while True:
        if v < 0x80:
            out.append(v)
            return bytes(out)
        out.append((v & 0x7F) | 0x80)
        v >>= 7


def _tag(field: int, wire: int) -> bytes:
    return _encode_varint((field << 3) | wire)


def _len_delim(field: int, payload: bytes) -> bytes:
    return _tag(field, 2) + _encode_varint(len(payload)) + payload


def _varint_field(field: int, v: int) -> bytes:
    return _tag(field, 0) + _encode_varint(v)


def _string_field(field: int, s: str) -> bytes:
    return _len_delim(field, s.encode("utf-8"))


def _timestamp_msg(seconds: int) -> bytes:
    """google.protobuf.Timestamp submessage payload."""
    return _varint_field(1, seconds)


def _conversation_summary(
    uuid: str,
    title: str,
    started_at: int,
    last_at: int,
    workspace_path: str | None,
) -> bytes:
    """Build one entry matching the real ``ConversationSummary`` field layout
    enough that the adapter's parser sees what it needs.
    """
    data_payload = b""
    data_payload += _string_field(1, title)  # title
    data_payload += _len_delim(7, _timestamp_msg(started_at))  # started
    data_payload += _len_delim(10, _timestamp_msg(last_at))  # last_activity

    if workspace_path is not None:
        uri = f"file://{workspace_path}"
        ws_payload = b""
        ws_payload += _string_field(1, uri)
        ws_payload += _string_field(2, uri)
        # GitInfo
        git_payload = b""
        git_payload += _string_field(1, "owner/repo")
        git_payload += _string_field(2, "https://example.com/owner/repo.git")
        ws_payload += _len_delim(3, git_payload)
        ws_payload += _string_field(4, "main")
        data_payload += _len_delim(9, ws_payload)

    entry = _string_field(1, uuid) + _len_delim(2, data_payload)
    return entry


def _build_summary_file(entries: list[bytes]) -> bytes:
    """Wrap entries in the top-level ``repeated ConversationSummary = 1``."""
    return b"".join(_len_delim(1, e) for e in entries)


# ── fixture builders ──────────────────────────────────────────────────


def _build_home(
    tmp_path: Path,
    *,
    summary_entries: list[bytes] | None = None,
    history_entries: list[dict] | None = None,
) -> Path:
    home = tmp_path / ".gemini"
    (home / "antigravity").mkdir(parents=True)
    (home / "antigravity-cli").mkdir(parents=True)

    if summary_entries is not None:
        (home / "antigravity" / "agyhub_summaries_proto.pb").write_bytes(
            _build_summary_file(summary_entries)
        )
    if history_entries is not None:
        history_path = home / "antigravity-cli" / "history.jsonl"
        history_path.write_text(
            "\n".join(json.dumps(e) for e in history_entries) + "\n"
        )
    return home


@pytest.fixture
def synthetic_home(tmp_path: Path) -> Path:
    return _build_home(
        tmp_path,
        summary_entries=[
            _conversation_summary(
                "uuid-ide-001",
                "Project Capabilities Inquiry",
                started_at=1_779_000_000,
                last_at=1_779_001_000,
                workspace_path="/Users/x/projects/alpha",
            ),
            _conversation_summary(
                "uuid-shared-002",
                "CLI Conversation Title",
                started_at=1_779_002_000,
                last_at=1_779_003_000,
                workspace_path=None,  # missing — should fall back to history
            ),
        ],
        history_entries=[
            {
                "display": "first prompt",
                "timestamp": 1_779_002_500_000,
                "workspace": "/Users/x/projects/beta",
                "conversationId": "uuid-shared-002",
            },
            {
                "display": "second prompt",
                "timestamp": 1_779_002_600_000,
                "workspace": "/Users/x/projects/beta",
                "conversationId": "uuid-shared-002",
            },
            {
                "display": "cli-only prompt",
                "timestamp": 1_779_004_000_000,
                "workspace": "/Users/x/projects/gamma",
                "conversationId": "uuid-cli-only-003",
            },
        ],
    )


# ── tests ─────────────────────────────────────────────────────────────


def test_enumerate_dedupes_across_sources(synthetic_home: Path) -> None:
    adapter = AntigravityAdapter(gemini_home=synthetic_home)
    refs = list(adapter.enumerate())
    uuids = [r.session_id for r in refs]
    assert uuids == ["uuid-ide-001", "uuid-shared-002", "uuid-cli-only-003"]
    # No duplicates even though uuid-shared-002 appears in both surfaces.
    assert len(set(uuids)) == len(uuids)


def test_workspace_fallback_to_cli_history(synthetic_home: Path) -> None:
    adapter = AntigravityAdapter(gemini_home=synthetic_home)
    refs = {r.session_id: r for r in adapter.enumerate()}
    # Summary's workspace wins for uuid-ide-001.
    assert refs["uuid-ide-001"].project_slug == "-Users-x-projects-alpha"
    # uuid-shared-002 has no summary workspace → falls back to history's.
    assert refs["uuid-shared-002"].project_slug == "-Users-x-projects-beta"
    # uuid-cli-only-003 has no summary entry at all → comes from history.
    assert refs["uuid-cli-only-003"].project_slug == "-Users-x-projects-gamma"


def test_read_emits_title_marker_plus_cli_prompts(synthetic_home: Path) -> None:
    adapter = AntigravityAdapter(gemini_home=synthetic_home)
    refs = {r.session_id: r for r in adapter.enumerate()}
    recs = list(adapter.read(refs["uuid-shared-002"]))
    # 1 title marker + 2 matching CLI prompts.
    assert len(recs) == 3
    assert recs[0].content_text.startswith("[antigravity title]")
    assert recs[1].content_text == "first prompt"
    assert recs[2].content_text == "second prompt"


def test_records_are_zero_token_and_marked_encrypted(synthetic_home: Path) -> None:
    adapter = AntigravityAdapter(gemini_home=synthetic_home)
    refs = list(adapter.enumerate())
    for ref in refs:
        for rec in adapter.read(ref):
            assert rec.input_tokens == 0
            assert rec.output_tokens == 0
            assert rec.cache_create_tokens == 0
            assert rec.cache_read_tokens == 0
            assert rec.raw["cost_source"] == "encrypted"
            assert rec.role == "user"  # everything we have is user-side


def test_cli_only_session_has_no_title_marker(synthetic_home: Path) -> None:
    """A conversation with only CLI history (no summary entry) skips the
    title marker — there's no title to render."""
    adapter = AntigravityAdapter(gemini_home=synthetic_home)
    refs = {r.session_id: r for r in adapter.enumerate()}
    recs = list(adapter.read(refs["uuid-cli-only-003"]))
    assert len(recs) == 1
    assert recs[0].content_text == "cli-only prompt"


def test_since_offset_skips_records_before_midpoint(synthetic_home: Path) -> None:
    adapter = AntigravityAdapter(gemini_home=synthetic_home)
    refs = {r.session_id: r for r in adapter.enumerate()}
    full = list(adapter.read(refs["uuid-shared-002"]))
    assert len(full) >= 3
    mid = full[1].seq  # skip first two
    resumed = list(adapter.read(refs["uuid-shared-002"], since_offset=mid))
    assert all(r.seq > mid for r in resumed)
    assert len(resumed) == len(full) - 2


def test_empty_gemini_home_is_silent(tmp_path: Path) -> None:
    """No ~/.gemini at all → enumerate yields nothing, never raises."""
    adapter = AntigravityAdapter(gemini_home=tmp_path / "nonexistent")
    assert list(adapter.enumerate()) == []


def test_history_without_summary(tmp_path: Path) -> None:
    """A machine with only the CLI installed still works."""
    home = _build_home(
        tmp_path,
        history_entries=[
            {
                "display": "only prompt",
                "timestamp": 1_779_000_000_000,
                "workspace": "/Users/x/projects/standalone",
                "conversationId": "uuid-cli-7",
            },
        ],
    )
    adapter = AntigravityAdapter(gemini_home=home)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    assert refs[0].project_slug == "-Users-x-projects-standalone"
    recs = list(adapter.read(refs[0]))
    assert len(recs) == 1
    assert recs[0].content_text == "only prompt"


def test_summary_without_history(tmp_path: Path) -> None:
    """A machine with only the IDE installed still works."""
    home = _build_home(
        tmp_path,
        summary_entries=[
            _conversation_summary(
                "uuid-ide-only",
                "Some Title",
                started_at=1_779_000_000,
                last_at=1_779_000_500,
                workspace_path="/Users/x/projects/ide",
            ),
        ],
    )
    adapter = AntigravityAdapter(gemini_home=home)
    refs = list(adapter.enumerate())
    assert len(refs) == 1
    recs = list(adapter.read(refs[0]))
    assert len(recs) == 1  # title marker only
    assert "[antigravity title]" in recs[0].content_text


def test_watch_paths_returns_three_roots(synthetic_home: Path) -> None:
    adapter = AntigravityAdapter(gemini_home=synthetic_home)
    paths = adapter.watch_paths()
    names = sorted(p.name for p in paths)
    assert names == ["antigravity", "antigravity-cli", "antigravity-ide"]


# ── contract tests ────────────────────────────────────────────────────


class TestAntigravityContract(unittest.TestCase, AdapterContract):
    def setUp(self) -> None:
        # The contract suite drives enumerate/read and checks invariants;
        # build a small fixture inline.
        import tempfile
        self._tmp = tempfile.TemporaryDirectory()
        tmp = Path(self._tmp.name)
        home = _build_home(
            tmp,
            summary_entries=[
                _conversation_summary(
                    "uuid-contract-1",
                    "Contract Session",
                    started_at=1_779_000_000,
                    last_at=1_779_001_000,
                    workspace_path="/Users/c/projects/contract",
                ),
            ],
            history_entries=[
                {
                    "display": "contract prompt one",
                    "timestamp": 1_779_000_100_000,
                    "workspace": "/Users/c/projects/contract",
                    "conversationId": "uuid-contract-1",
                },
                {
                    "display": "contract prompt two",
                    "timestamp": 1_779_000_200_000,
                    "workspace": "/Users/c/projects/contract",
                    "conversationId": "uuid-contract-1",
                },
            ],
        )
        self.adapter = AntigravityAdapter(gemini_home=home)

    def tearDown(self) -> None:
        self._tmp.cleanup()
