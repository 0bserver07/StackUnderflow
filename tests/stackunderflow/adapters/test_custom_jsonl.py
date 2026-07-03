"""Tests for the stackunderflow-history-jsonl-v1 stream contract, manifest,
and the guarded subprocess runner (spec #12)."""

from __future__ import annotations

import json
import sys
from pathlib import Path

import pytest

from stackunderflow.adapters import custom_jsonl as cj


# ── stream parsing / validation ──────────────────────────────────────────────


def _line(obj: dict) -> str:
    return json.dumps(obj)


def test_parse_valid_stream():
    stream = "\n".join([
        _line({"type": "session", "session_id": "s1", "project": "p"}),
        _line({"type": "message", "session_id": "s1", "seq": 0, "role": "user",
               "content": "hi", "timestamp": "2026-06-01T00:00:00+00:00"}),
        _line({"type": "message", "session_id": "s1", "seq": 1, "role": "assistant",
               "content": "yo", "model": "m", "input_tokens": 5, "output_tokens": 2,
               "tools": ["Edit"]}),
        _line({"type": "file_touch", "session_id": "s1", "seq": 2,
               "path": "/x/y.py", "operation": "edit"}),
        _line({"type": "cursor", "cursor": "C1"}),
    ])
    ps = cj.parse_stream(stream)
    assert list(ps.sessions) == ["s1"]
    assert len(ps.messages) == 2
    assert ps.messages[1].tools == ("Edit",)
    assert ps.messages[1].input_tokens == 5
    assert len(ps.file_touches) == 1
    assert ps.file_touches[0].path == "/x/y.py"
    assert ps.next_cursor == "C1"


def test_blank_lines_are_skipped():
    stream = "\n\n" + _line({"type": "cursor", "cursor": "C"}) + "\n\n"
    ps = cj.parse_stream(stream)
    assert ps.next_cursor == "C"


def test_bytes_input_is_decoded():
    ps = cj.parse_stream(_line({"type": "cursor", "cursor": "C"}).encode("utf-8"))
    assert ps.next_cursor == "C"


def test_invalid_utf8_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(b"\xff\xfe not utf8")


def test_malformed_json_line_raises_with_line_number():
    stream = "\n".join([
        _line({"type": "message", "session_id": "s1", "seq": 0, "role": "user"}),
        "{ this is broken",
    ])
    with pytest.raises(cj.StreamValidationError) as exc:
        cj.parse_stream(stream)
    assert exc.value.line_no == 2


def test_non_object_line_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream("[1, 2, 3]")


def test_unknown_record_type_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "banana", "session_id": "s"}))


def test_message_bad_role_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "message", "session_id": "s", "seq": 0,
                               "role": "wizard", "content": "x"}))


def test_message_missing_seq_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "message", "session_id": "s",
                               "role": "user", "content": "x"}))


def test_message_negative_seq_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "message", "session_id": "s", "seq": -1,
                               "role": "user", "content": "x"}))


def test_message_bool_seq_rejected():
    # bool is an int subclass — must not sneak through as seq 0/1.
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "message", "session_id": "s", "seq": True,
                               "role": "user", "content": "x"}))


def test_message_bad_tokens_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "message", "session_id": "s", "seq": 0,
                               "role": "user", "content": "x", "input_tokens": -3}))


def test_message_bad_tools_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "message", "session_id": "s", "seq": 0,
                               "role": "user", "content": "x", "tools": [1, 2]}))


def test_file_touch_missing_path_raises():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "file_touch", "session_id": "s", "seq": 0}))


def test_file_touch_default_operation():
    ps = cj.parse_stream(_line({"type": "file_touch", "session_id": "s",
                                "seq": 0, "path": "/a"}))
    assert ps.file_touches[0].operation == "edit"


def test_duplicate_seq_in_session_raises():
    stream = "\n".join([
        _line({"type": "message", "session_id": "s1", "seq": 0, "role": "user", "content": "a"}),
        _line({"type": "file_touch", "session_id": "s1", "seq": 0, "path": "/a"}),
    ])
    with pytest.raises(cj.StreamValidationError) as exc:
        cj.parse_stream(stream)
    assert "duplicate seq" in str(exc.value)


def test_same_seq_different_sessions_is_ok():
    stream = "\n".join([
        _line({"type": "message", "session_id": "s1", "seq": 0, "role": "user", "content": "a"}),
        _line({"type": "message", "session_id": "s2", "seq": 0, "role": "user", "content": "b"}),
    ])
    ps = cj.parse_stream(stream)
    assert {m.session_id for m in ps.messages} == {"s1", "s2"}


def test_cursor_record_last_wins():
    stream = "\n".join([
        _line({"type": "cursor", "cursor": "C1"}),
        _line({"type": "cursor", "cursor": "C2"}),
    ])
    assert cj.parse_stream(stream).next_cursor == "C2"


def test_cursor_record_requires_string():
    with pytest.raises(cj.StreamValidationError):
        cj.parse_stream(_line({"type": "cursor", "cursor": 5}))


def test_session_ids_includes_message_only_sessions():
    # A message that references a session with no explicit session line still
    # surfaces the session id.
    ps = cj.parse_stream(_line({"type": "message", "session_id": "orphan",
                                "seq": 0, "role": "user", "content": "x"}))
    assert ps.session_ids() == ["orphan"]


# ── manifest ─────────────────────────────────────────────────────────────────


def test_parse_manifest_minimal():
    m = cj.parse_manifest({"source_id": "amp", "command": ["amp-export"]})
    assert m.source_id == "amp"
    assert m.command == ("amp-export",)
    assert m.timeout_seconds == cj._DEFAULT_TIMEOUT_SECONDS
    assert m.max_output_bytes == cj._DEFAULT_MAX_OUTPUT_BYTES
    assert m.env_passthrough == ()


def test_parse_manifest_full():
    m = cj.parse_manifest({
        "schema": cj.SCHEMA, "source_id": "amp-1", "command": ["x", "--y"],
        "cursor": "seed", "timeout_seconds": 30, "max_output_bytes": 1024,
        "env_passthrough": ["AMP_TOKEN"],
    })
    assert m.cursor == "seed"
    assert m.timeout_seconds == 30
    assert m.max_output_bytes == 1024
    assert m.env_passthrough == ("AMP_TOKEN",)


def test_manifest_rejects_wrong_schema():
    with pytest.raises(cj.ManifestError):
        cj.parse_manifest({"schema": "something-else", "source_id": "a",
                           "command": ["x"]})


@pytest.mark.parametrize("source_id", ["", "has space", "a/b", "..", ".", "x" * 200])
def test_manifest_rejects_unsafe_source_id(source_id):
    with pytest.raises(cj.ManifestError):
        cj.parse_manifest({"source_id": source_id, "command": ["x"]})


@pytest.mark.parametrize("command", [[], "notalist", [""], [1, 2], None])
def test_manifest_rejects_bad_command(command):
    with pytest.raises(cj.ManifestError):
        cj.parse_manifest({"source_id": "a", "command": command})


def test_manifest_rejects_bad_cursor_type():
    with pytest.raises(cj.ManifestError):
        cj.parse_manifest({"source_id": "a", "command": ["x"], "cursor": 5})


def test_manifest_rejects_non_positive_timeout():
    with pytest.raises(cj.ManifestError):
        cj.parse_manifest({"source_id": "a", "command": ["x"], "timeout_seconds": 0})


def test_manifest_caps_timeout_and_output():
    m = cj.parse_manifest({
        "source_id": "a", "command": ["x"],
        "timeout_seconds": 10 ** 9, "max_output_bytes": 10 ** 12,
    })
    assert m.timeout_seconds == cj._MAX_TIMEOUT_SECONDS
    assert m.max_output_bytes == cj._HARD_MAX_OUTPUT_BYTES


def test_manifest_rejects_bad_env_passthrough():
    with pytest.raises(cj.ManifestError):
        cj.parse_manifest({"source_id": "a", "command": ["x"], "env_passthrough": [1]})


def test_load_manifest_from_file(tmp_path: Path):
    p = tmp_path / cj.MANIFEST_FILENAME
    p.write_text(json.dumps({"source_id": "amp", "command": ["amp-export"]}))
    m = cj.load_manifest(p)
    assert m.source_id == "amp"
    assert m.path == p


def test_load_manifest_from_directory(tmp_path: Path):
    (tmp_path / cj.MANIFEST_FILENAME).write_text(
        json.dumps({"source_id": "amp", "command": ["amp-export"]})
    )
    m = cj.load_manifest(tmp_path)
    assert m.source_id == "amp"


def test_load_manifest_missing_raises(tmp_path: Path):
    with pytest.raises(cj.ManifestError):
        cj.load_manifest(tmp_path / "nope.json")


def test_load_manifest_bad_json_raises(tmp_path: Path):
    p = tmp_path / cj.MANIFEST_FILENAME
    p.write_text("{ not json")
    with pytest.raises(cj.ManifestError):
        cj.load_manifest(p)


@pytest.mark.parametrize("sid,ok", [
    ("amp", True), ("amp-1", True), ("a.b_c", True), ("", False),
    ("a b", False), ("a/b", False), ("..", False),
])
def test_is_safe_source_id(sid, ok):
    assert cj.is_safe_source_id(sid) is ok


# ── env allowlist ────────────────────────────────────────────────────────────


def test_build_child_env_allowlists_and_adds_cursor():
    m = cj.parse_manifest({"source_id": "a", "command": ["x"],
                           "env_passthrough": ["AMP_TOKEN"]})
    parent = {"PATH": "/bin", "HOME": "/home/u", "AMP_TOKEN": "secret",
              "SHOULD_DROP": "leak"}
    env = cj.build_child_env(m, cursor="CUR", parent_env=parent)
    assert env["PATH"] == "/bin"
    assert env["HOME"] == "/home/u"
    assert env["AMP_TOKEN"] == "secret"          # explicit passthrough kept
    assert "SHOULD_DROP" not in env               # everything else dropped
    assert env[cj.CURSOR_ENV_VAR] == "CUR"


def test_build_child_env_none_cursor_is_empty_string():
    m = cj.parse_manifest({"source_id": "a", "command": ["x"]})
    env = cj.build_child_env(m, cursor=None, parent_env={})
    assert env[cj.CURSOR_ENV_VAR] == ""


# ── guarded subprocess runner ────────────────────────────────────────────────


def _manifest(command, **kw):
    data = {"source_id": "t", "command": command}
    data.update(kw)
    return cj.parse_manifest(data)


def test_run_export_captures_stdout():
    m = _manifest([sys.executable, "-c", "print('hello-stream')"])
    out = cj.run_export(m, cursor=None)
    assert out.strip() == b"hello-stream"


def test_run_export_nonzero_exit_raises():
    m = _manifest([sys.executable, "-c",
                   "import sys; sys.stderr.write('boom'); sys.exit(2)"])
    with pytest.raises(cj.ExportCommandError) as exc:
        cj.run_export(m, cursor=None)
    assert "exited 2" in str(exc.value)
    assert "boom" in str(exc.value)


def test_run_export_timeout_raises():
    m = _manifest([sys.executable, "-c", "import time; time.sleep(30)"],
                  timeout_seconds=0.5)
    with pytest.raises(cj.ExportCommandError) as exc:
        cj.run_export(m, cursor=None)
    assert "timed out" in str(exc.value)


def test_run_export_output_cap_raises():
    m = _manifest([sys.executable, "-c", "import sys; sys.stdout.write('x'*100000)"],
                  max_output_bytes=1000)
    with pytest.raises(cj.ExportCommandError) as exc:
        cj.run_export(m, cursor=None)
    assert "more than" in str(exc.value)


def test_run_export_spawn_failure_raises():
    m = _manifest(["this-binary-does-not-exist-xyzzy"])
    with pytest.raises(cj.ExportCommandError) as exc:
        cj.run_export(m, cursor=None)
    assert "could not launch" in str(exc.value)


def test_run_export_passes_cursor_via_env():
    m = _manifest([sys.executable, "-c",
                   "import os,sys; sys.stdout.write(os.environ['STACKUNDERFLOW_HISTORY_CURSOR'])"])
    out = cj.run_export(m, cursor="cursor-XYZ")
    assert out == b"cursor-XYZ"


def test_run_export_no_shell_interpretation():
    # If argv were run through a shell, the '$(...)' would be substituted. With
    # shell=False it is a literal argument echoed back verbatim.
    payload = "$(echo pwned)"
    m = _manifest([sys.executable, "-c", "import sys; sys.stdout.write(sys.argv[1])", payload])
    out = cj.run_export(m, cursor=None)
    assert out.decode() == payload
