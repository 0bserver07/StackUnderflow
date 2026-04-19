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


def test_read_modern_jsonl_yields_records(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-a"
    project_dir.mkdir(parents=True)
    fp = project_dir / "abc.jsonl"
    fp.write_text(
        '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z",'
        '"uuid":"u1","message":{"role":"user","content":"hello"}}\n'
        '{"sessionId":"abc","type":"assistant","timestamp":"2026-01-01T00:00:01Z",'
        '"uuid":"u2","parentUuid":"u1",'
        '"message":{"role":"assistant","model":"claude-sonnet-4-6",'
        '"content":[{"type":"text","text":"hi"}],'
        '"usage":{"input_tokens":5,"output_tokens":2}}}\n'
    )
    a = ClaudeAdapter()
    ref = list(a.enumerate())[0]
    records = list(a.read(ref))
    assert len(records) == 2
    assert records[0].role == "user"
    assert records[0].content_text == "hello"
    assert records[1].role == "assistant"
    assert records[1].input_tokens == 5
    assert records[1].output_tokens == 2
    assert records[1].model == "claude-sonnet-4-6"
    assert records[0].seq < records[1].seq


def test_read_respects_since_offset(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-a"
    project_dir.mkdir(parents=True)
    fp = project_dir / "abc.jsonl"
    line1 = '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u1","message":{"role":"user","content":"a"}}\n'
    line2 = '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:01Z","uuid":"u2","message":{"role":"user","content":"b"}}\n'
    fp.write_text(line1 + line2)

    a = ClaudeAdapter()
    ref = list(a.enumerate())[0]
    records = list(a.read(ref, since_offset=len(line1.encode())))
    assert len(records) == 1
    assert records[0].content_text == "b"


def test_read_skips_malformed_lines(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-a"
    project_dir.mkdir(parents=True)
    fp = project_dir / "abc.jsonl"
    fp.write_text(
        'not-json\n'
        '{"sessionId":"abc","type":"user","timestamp":"2026-01-01T00:00:00Z","uuid":"u","message":{"role":"user","content":"hello"}}\n'
    )
    a = ClaudeAdapter()
    ref = list(a.enumerate())[0]
    records = list(a.read(ref))
    assert len(records) == 1


def test_read_legacy_history_yields_records(fake_home: Path) -> None:
    project_dir = fake_home / ".claude" / "projects" / "-Users-me-legacy"
    project_dir.mkdir(parents=True)
    (project_dir / ".continuation_cache.json").write_text("{}")
    history = fake_home / ".claude" / "history.jsonl"
    history.write_text(
        '{"display":"msg1","timestamp":1704067200000,"project":"/Users/me/legacy"}\n'
        '{"display":"msg2","timestamp":1704067260000,"project":"/Users/me/legacy","sessionId":"s-real"}\n'
        '{"display":"other","timestamp":1704067200000,"project":"/Users/me/other"}\n'
    )
    a = ClaudeAdapter()
    ref = [r for r in a.enumerate() if r.project_slug == "-Users-me-legacy"][0]
    recs = list(a.read(ref))
    assert len(recs) == 2
    assert recs[0].content_text == "msg1"
    assert recs[1].content_text == "msg2"
    assert all(r.role == "user" for r in recs)
    assert recs[0].timestamp.startswith("2024-01-01")
