from pathlib import Path

import pytest

from stackunderflow.adapters.claude import ClaudeAdapter


@pytest.fixture
def fake_home(tmp_path: Path, monkeypatch) -> Path:
    monkeypatch.setenv("HOME", str(tmp_path))
    return tmp_path


def test_enumerate_empty_claude_dir(fake_home: Path) -> None:
    a = ClaudeAdapter()
    assert list(a.enumerate()) == []


def test_enumerate_finds_jsonl_files(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-Users-me-app"
    project_dir.mkdir(parents=True)
    (project_dir / "abc.jsonl").write_text('{"sessionId":"abc","timestamp":"2026-01-01T00:00:00Z","type":"user"}\n')
    a = ClaudeAdapter()
    refs = list(a.enumerate())
    assert len(refs) == 1
    assert refs[0].provider == "claude"
    assert refs[0].project_slug == "-Users-me-app"
    assert refs[0].session_id == "abc"


def test_enumerate_legacy_project_from_history(fake_home: Path, monkeypatch) -> None:
    # Legacy: empty project dir with .continuation_cache.json + history.jsonl entry
    project_dir = fake_home / ".claude" / "projects" / "-Users-me-legacy"
    project_dir.mkdir(parents=True)
    (project_dir / ".continuation_cache.json").write_text("{}")
    history = fake_home / ".claude" / "history.jsonl"
    history.write_text(
        '{"display":"hi","timestamp":1704067200000,"project":"/Users/me/legacy"}\n'
    )
    a = ClaudeAdapter()
    refs = list(a.enumerate())
    assert any(r.project_slug == "-Users-me-legacy" for r in refs)
