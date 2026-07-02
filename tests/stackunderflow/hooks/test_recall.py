"""Active-recall hook (campaign #5) — the contract, locked.

Covered here:

* A risky file (failure modes / non-zero risk counts in the ``memory file``
  envelope) yields a **bounded, valid** injection envelope in Claude Code's
  verified ``hookSpecificOutput.additionalContext`` shape; the block is
  token-capped and truncates **oldest first**.
* A clean file, an empty envelope, or a Bash command with no extractable
  path → **empty output**, and for the no-path case no subprocess at all.
* Every CLI failure mode — binary missing, timeout, non-zero exit, garbage
  stdout, wrong envelope schema, unexpected exception — is a silent no-op.
* The Bash path heuristic: extracts file-looking tokens, skips flags / URLs /
  pseudo-files, ranks extension-bearing tokens first, caps the count.
* One shared deadline across multi-path Bash lookups — the tool is never
  delayed beyond the (configurable) timeout.
* ``handlers.run()`` dispatches the recall id to stdout and always exits 0;
  the ``hooks run`` CLI plumbing behaves the same end-to-end.
* install / uninstall / status / repair cover the new template exactly like
  the other hooks (idempotent, convergent, canonicalisable).

Every ``stackunderflow memory file`` call is **subprocess-mocked** — no real
CLI runs, no real store, no real ``~/.claude``.
"""

from __future__ import annotations

import itertools
import json
import subprocess
from pathlib import Path
from types import SimpleNamespace

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.hooks import recall, templates
from stackunderflow.hooks._install import install, status, uninstall
from stackunderflow.hooks._repair import repair
from stackunderflow.hooks.handlers import run

RECALL_ID = "stackunderflow-pretool-recall"


# ── fixture envelopes (the stackunderflow.memory/1 shape) ────────────────────


def _failure_mode(
    ts: str = "2026-05-17T03:00:00Z",
    outcome: str = "failed",
    evidence: str = "that broke the build — please look again",
    session: str = "fail-sess-0001",
) -> dict:
    """One ``memory file`` result row with ``kind == "failure_mode"``."""
    return {
        "session_id": session,
        "project_slug": "-Users-dev-proj",
        "project_path": "/Users/dev/proj",
        "provider": "claude",
        "first_ts": "2026-05-01T00:00:00Z",
        "last_ts": ts,
        "message_count": 22,
        "cost_usd": 1.25,
        "snippet": None,
        "outcome": outcome,
        "outcome_evidence": evidence,
        "outcome_confidence": 0.9,
        "kind": "failure_mode",
    }


def _envelope(
    *,
    path: str = "/Users/dev/proj/risky.py",
    failed: int = 1,
    reverted: int = 0,
    worked: int = 2,
    total: int = 4,
    results: list[dict] | None = None,
) -> dict:
    """A ``stackunderflow memory file --json`` envelope fixture."""
    results = results if results is not None else [_failure_mode()]
    return {
        "schema": "stackunderflow.memory/1",
        "command": "file",
        "query": {"path": path, "project": None, "since": None, "limit": 20},
        "results": results,
        "result_count": len(results),
        "token_estimate": 200,
        "budget": 2000,
        "truncated": False,
        "risk": {
            "path": path,
            "since": None,
            "total_sessions": total,
            "reverted": reverted,
            "failed": failed,
            "worked": worked,
            "recent_session_ids": [],
        },
    }


def _clean_envelope(path: str = "/Users/dev/proj/calm.py") -> dict:
    """A file with history but zero failure signal — touched sessions only."""
    touched = dict(_failure_mode(outcome="worked", evidence=""), kind="touched")
    return _envelope(path=path, failed=0, reverted=0, worked=3, total=3, results=[touched])


# ── subprocess mocking ───────────────────────────────────────────────────────


class FakeRun:
    """A ``subprocess.run`` stand-in that records calls and replays a script.

    ``script`` entries are either an exception instance (raised) or a
    ``(returncode, stdout)`` tuple (returned as a CompletedProcess-alike).
    The last entry repeats for any extra calls.
    """

    def __init__(self, *script):
        self.script = list(script)
        self.calls: list[dict] = []

    def __call__(self, cmd, **kwargs):
        self.calls.append({"cmd": list(cmd), **kwargs})
        step = self.script[min(len(self.calls), len(self.script)) - 1]
        if isinstance(step, BaseException):
            raise step
        returncode, stdout = step
        return SimpleNamespace(returncode=returncode, stdout=stdout, stderr="")


@pytest.fixture
def fake_run(monkeypatch):
    """Patch ``subprocess.run`` as seen by the recall module; yields the factory."""

    def _install_script(*script) -> FakeRun:
        fake = FakeRun(*script)
        monkeypatch.setattr(recall.subprocess, "run", fake)
        return fake

    return _install_script


def _edit_payload(path: str = "/Users/dev/proj/risky.py") -> dict:
    return {"hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_input": {"file_path": path}}


def _bash_payload(command: str) -> dict:
    return {"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": command}}


def _assert_valid_envelope(raw: str) -> str:
    """The output is one JSON object in the verified injection shape; returns the text."""
    obj = json.loads(raw)
    assert set(obj) == {"hookSpecificOutput"}
    hso = obj["hookSpecificOutput"]
    assert set(hso) == {"hookEventName", "additionalContext"}
    assert hso["hookEventName"] == "PreToolUse"
    text = hso["additionalContext"]
    assert isinstance(text, str) and text.strip()
    assert len(text) <= recall._MAX_CHARS
    return text


# ── (a) risky file → capped injection ────────────────────────────────────────


class TestInjectsOnRisk:
    def test_edit_of_risky_file_injects_envelope(self, fake_run) -> None:
        fake = fake_run((0, json.dumps(_envelope())))
        raw = recall.build_recall(RECALL_ID, _edit_payload())
        text = _assert_valid_envelope(raw)
        assert "risky.py" in text
        assert "failed" in text
        assert "that broke the build" in text
        assert "stackunderflow memory file /Users/dev/proj/risky.py --json" in text
        # Exactly one lookup, the documented command shape.
        assert len(fake.calls) == 1
        assert fake.calls[0]["cmd"] == ["stackunderflow", "memory", "file", "/Users/dev/proj/risky.py", "--json"]

    def test_write_payload_also_recalls(self, fake_run) -> None:
        fake_run((0, json.dumps(_envelope())))
        payload = {"tool_name": "Write", "tool_input": {"file_path": "/Users/dev/proj/risky.py"}}
        assert _assert_valid_envelope(recall.build_recall(RECALL_ID, payload))

    def test_risk_counts_without_failure_rows_still_warn(self, fake_run) -> None:
        # The risk block says "2 failed" but the packed results carry none
        # (e.g. budget-dropped by the CLI) — the header alone is the warning.
        fake_run((0, json.dumps(_envelope(failed=2, results=[]))))
        text = _assert_valid_envelope(recall.build_recall(RECALL_ID, _edit_payload()))
        assert "2 failed" in text

    def test_bash_command_with_risky_path_injects(self, fake_run) -> None:
        fake = fake_run((0, json.dumps(_envelope(path="/Users/dev/proj/tests/test_auth.py"))))
        raw = recall.build_recall(RECALL_ID, _bash_payload("pytest tests/test_auth.py -q"))
        text = _assert_valid_envelope(raw)
        assert "test_auth.py" in text
        assert fake.calls[0]["cmd"][:3] == ["stackunderflow", "memory", "file"]
        assert fake.calls[0]["cmd"][3] == "tests/test_auth.py"

    def test_block_is_capped_and_truncates_oldest_first(self, fake_run) -> None:
        rows = [
            _failure_mode(
                ts=f"2026-05-{day:02d}T00:00:00Z",
                evidence=f"day-{day:02d} " + "x" * 200,
                session=f"s-{day}",
            )
            for day in range(1, 21)
        ]
        fake_run((0, json.dumps(_envelope(results=rows))))
        text = _assert_valid_envelope(recall.build_recall(RECALL_ID, _edit_payload()))
        assert "2026-05-20" in text  # newest survives
        assert "2026-05-01" not in text  # oldest dropped first
        assert len(text) <= recall._MAX_CHARS

    def test_assemble_drops_oldest_lines_to_fit_budget(self) -> None:
        # Direct unit for the char-budget loop: bullets arrive newest-first,
        # so the drop-from-the-end loop discards the oldest lines.
        bullets = [f"  • line-{i} " + "x" * 400 for i in range(10)]  # ~4KB total
        text = recall._assemble("header", bullets, "footer")
        assert len(text) <= recall._MAX_CHARS
        assert "line-0" in text  # newest survives
        assert "line-9" not in text  # oldest dropped
        assert text.startswith("header") and text.endswith("footer")

    def test_assemble_hard_clips_even_without_bullets(self) -> None:
        text = recall._assemble("H" * (recall._MAX_CHARS * 2), [], "footer")
        assert len(text) <= recall._MAX_CHARS
        assert text.endswith("…")

    def test_multiple_bash_paths_are_merged(self, fake_run) -> None:
        fake = fake_run(
            (0, json.dumps(_envelope(path="/p/a.py", results=[_failure_mode(ts="2026-05-18T00:00:00Z")]))),
            (0, json.dumps(_envelope(path="/p/b.py", results=[_failure_mode(ts="2026-05-19T00:00:00Z")]))),
        )
        raw = recall.build_recall(RECALL_ID, _bash_payload("python a.py b.py"))
        text = _assert_valid_envelope(raw)
        assert "a.py" in text and "b.py" in text
        assert len(fake.calls) == 2

    def test_subprocess_call_is_hardened(self, fake_run) -> None:
        # stdin detached, output captured as text, a finite timeout, cwd from
        # the payload when it exists.
        fake = fake_run((0, json.dumps(_envelope())))
        payload = dict(_edit_payload(), cwd="/")  # "/" always exists
        recall.build_recall(RECALL_ID, payload)
        call = fake.calls[0]
        assert call["stdin"] is subprocess.DEVNULL
        assert call["capture_output"] is True and call["text"] is True
        assert 0 < call["timeout"] <= recall._DEFAULT_TIMEOUT_S
        assert call["cwd"] == "/"

    def test_missing_cwd_passes_none(self, fake_run) -> None:
        fake = fake_run((0, json.dumps(_envelope())))
        payload = dict(_edit_payload(), cwd="/definitely/not/a/dir-xyz")
        recall.build_recall(RECALL_ID, payload)
        assert fake.calls[0]["cwd"] is None


# ── (b) clean file → silent ─────────────────────────────────────────────────


class TestSilentWhenClean:
    def test_clean_file_yields_empty(self, fake_run) -> None:
        fake_run((0, json.dumps(_clean_envelope())))
        assert recall.build_recall(RECALL_ID, _edit_payload("/Users/dev/proj/calm.py")) == ""

    def test_unknown_file_empty_envelope_yields_empty(self, fake_run) -> None:
        fake_run((0, json.dumps(_envelope(failed=0, reverted=0, worked=0, total=0, results=[]))))
        assert recall.build_recall(RECALL_ID, _edit_payload("/Users/dev/proj/new.py")) == ""


# ── (c) CLI failure modes → silent ──────────────────────────────────────────


class TestSilentOnFailure:
    def test_cli_missing_yields_empty(self, fake_run) -> None:
        fake_run(FileNotFoundError("stackunderflow: not found"))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    def test_timeout_yields_empty(self, fake_run) -> None:
        fake_run(subprocess.TimeoutExpired(cmd="stackunderflow", timeout=1.5))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    def test_nonzero_exit_yields_empty(self, fake_run) -> None:
        error_envelope = {"schema": "stackunderflow.memory/1", "command": "file", "query": {}, "error": "boom"}
        fake_run((1, json.dumps(error_envelope)))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    def test_malformed_stdout_yields_empty(self, fake_run) -> None:
        fake_run((0, "<<<not json>>>"))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    def test_empty_stdout_yields_empty(self, fake_run) -> None:
        fake_run((0, ""))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    def test_non_dict_json_yields_empty(self, fake_run) -> None:
        fake_run((0, json.dumps(["not", "an", "envelope"])))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    def test_unknown_schema_major_yields_empty(self, fake_run) -> None:
        env = dict(_envelope(), schema="stackunderflow.memory/2")
        fake_run((0, json.dumps(env)))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    def test_unexpected_exception_yields_empty(self, fake_run) -> None:
        fake_run(RuntimeError("anything at all"))
        assert recall.build_recall(RECALL_ID, _edit_payload()) == ""

    @pytest.mark.parametrize("bad", [None, "not a dict", 42, [], {"tool_input": "nope"}])
    def test_garbage_payload_yields_empty_without_subprocess(self, fake_run, bad) -> None:
        fake = fake_run((0, json.dumps(_envelope())))
        assert recall.build_recall(RECALL_ID, bad) == ""
        assert fake.calls == []

    def test_unknown_hook_id_yields_empty_without_subprocess(self, fake_run) -> None:
        fake = fake_run((0, json.dumps(_envelope())))
        assert recall.build_recall("stackunderflow-bogus", _edit_payload()) == ""
        assert recall.build_recall("stackunderflow-inject-pre-tool-use", _edit_payload()) == ""
        assert fake.calls == []


# ── (d) the Bash path heuristic ─────────────────────────────────────────────


class TestBashPathExtraction:
    def test_no_extractable_paths_is_silent_and_never_shells(self, fake_run) -> None:
        fake = fake_run((0, json.dumps(_envelope())))
        for cmd in ("npm run build", "git status", "echo hello world", "ls -la"):
            assert recall.build_recall(RECALL_ID, _bash_payload(cmd)) == ""
        assert fake.calls == []

    def test_extracts_paths_and_ranks_extensions_first(self) -> None:
        got = recall._paths_from_command("/usr/bin/env python scripts/run.py --out build/")
        assert got[0] == "scripts/run.py"
        assert "/usr/bin/env" in got

    def test_skips_flags_urls_and_pseudo_files(self) -> None:
        cmd = "curl -o /dev/null https://example.com/x.py --retry 3"
        assert recall._paths_from_command(cmd) == []

    def test_flag_equals_value_yields_the_value(self) -> None:
        assert recall._paths_from_command("mytool --file=src/app.py") == ["src/app.py"]

    def test_caps_at_max_paths(self) -> None:
        cmd = "cat a.py b.py c.py d.py e.py"
        assert len(recall._paths_from_command(cmd)) == recall._MAX_BASH_PATHS

    def test_unbalanced_quotes_fall_back_to_split(self) -> None:
        assert recall._paths_from_command('echo "unclosed src/x.py') == ["src/x.py"]

    def test_version_numbers_are_not_files(self) -> None:
        assert recall._paths_from_command("pyenv local 3.12.9") == []

    def test_edit_payload_ignores_bash_heuristic(self) -> None:
        # A file-tool payload takes its path from tool_input, never the heuristic.
        payload = {"tool_name": "Edit", "tool_input": {"file_path": "  /p/x.py  "}}
        assert recall._candidate_paths(payload) == ["/p/x.py"]

    def test_bash_payload_without_command_yields_nothing(self) -> None:
        assert recall._candidate_paths({"tool_name": "Bash", "tool_input": {}}) == []
        assert recall._candidate_paths({"tool_name": "Bash", "tool_input": {"command": 42}}) == []


# ── the shared deadline ─────────────────────────────────────────────────────


class TestDeadline:
    def test_timeout_env_var_tunes_the_deadline(self, fake_run, monkeypatch) -> None:
        monkeypatch.setenv(recall._TIMEOUT_ENV, "0.3")
        fake = fake_run((0, json.dumps(_envelope())))
        recall.build_recall(RECALL_ID, _edit_payload())
        assert 0 < fake.calls[0]["timeout"] <= 0.3

    @pytest.mark.parametrize("bad", ["banana", "-2", "0", ""])
    def test_garbage_timeout_env_falls_back_to_default(self, bad, monkeypatch) -> None:
        monkeypatch.setenv(recall._TIMEOUT_ENV, bad)
        assert recall._timeout_seconds() == recall._DEFAULT_TIMEOUT_S

    def test_huge_timeout_env_is_clamped(self, monkeypatch) -> None:
        monkeypatch.setenv(recall._TIMEOUT_ENV, "1500")  # a stray "milliseconds" value
        assert recall._timeout_seconds() == recall._MAX_TIMEOUT_S

    def test_deadline_is_shared_across_bash_paths(self, fake_run, monkeypatch) -> None:
        # A fake clock that jumps 1s per reading: the deadline (1.5s) admits
        # exactly one lookup for a three-path command — total wall time can
        # never exceed the single configured timeout.
        ticks = itertools.count()
        monkeypatch.setattr(recall.time, "monotonic", lambda: float(next(ticks)))
        fake = fake_run((0, json.dumps(_clean_envelope())))
        assert recall.build_recall(RECALL_ID, _bash_payload("cat a.py b.py c.py")) == ""
        assert len(fake.calls) == 1


# ── handlers.run() dispatch + CLI plumbing ──────────────────────────────────


class TestDispatch:
    def test_run_writes_envelope_to_stdout_and_exits_zero(self, fake_run, capsys) -> None:
        fake_run((0, json.dumps(_envelope())))
        assert run(RECALL_ID, _edit_payload()) == 0
        out = capsys.readouterr().out
        assert "risky.py" in _assert_valid_envelope(out.strip())

    def test_run_silent_cases_write_nothing_exit_zero(self, fake_run, capsys) -> None:
        fake_run(subprocess.TimeoutExpired(cmd="stackunderflow", timeout=1.5))
        assert run(RECALL_ID, _edit_payload()) == 0
        assert capsys.readouterr().out == ""

    def test_run_never_records_a_captured_event(self, fake_run, tmp_path: Path, monkeypatch) -> None:
        # Recall reads memory; it must not write anything — not even the store file.
        monkeypatch.setattr(deps, "store_path", tmp_path / "store.db")
        fake_run((0, json.dumps(_envelope())))
        assert run(RECALL_ID, _edit_payload()) == 0
        assert not (tmp_path / "store.db").exists()

    def test_cli_hooks_run_emits_envelope(self, fake_run) -> None:
        fake_run((0, json.dumps(_envelope())))
        res = CliRunner().invoke(cli, ["hooks", "run", RECALL_ID], input=json.dumps(_edit_payload()))
        assert res.exit_code == 0
        _assert_valid_envelope(res.output.strip())

    def test_cli_hooks_run_garbage_stdin_exits_zero_silent(self, fake_run) -> None:
        fake = fake_run((0, json.dumps(_envelope())))
        res = CliRunner().invoke(cli, ["hooks", "run", RECALL_ID], input="<<<not json>>>")
        assert res.exit_code == 0
        assert res.output.strip() == ""
        assert fake.calls == []


# ── (e) install / uninstall / status / repair for the new template ──────────


@pytest.fixture
def project_root(tmp_path: Path, monkeypatch) -> Path:
    """A git-rooted tmp dir, store pointed away from the real one."""
    (tmp_path / ".git").mkdir()
    monkeypatch.setattr(deps, "store_path", tmp_path / "store.db")
    return tmp_path


def _settings(root: Path) -> dict:
    return json.loads((root / ".claude" / "settings.json").read_text())


class TestInstallRepair:
    def test_plain_install_omits_recall(self, project_root: Path) -> None:
        install("project", cwd=project_root)
        data = _settings(project_root)
        assert "PreToolUse" not in data["hooks"]

    def test_inject_install_adds_recall_group(self, project_root: Path) -> None:
        report = install("project", cwd=project_root, inject=True)
        assert RECALL_ID in report.hooks_installed
        groups = _settings(project_root)["hooks"]["PreToolUse"]
        recall_groups = [
            g for g in groups
            if any(RECALL_ID in e["command"] for e in g["hooks"])
        ]
        assert recall_groups == [
            {
                "matcher": "Edit|Write|Bash",
                "hooks": [{"type": "command", "command": f"stackunderflow hooks run {RECALL_ID}"}],
            }
        ]

    def test_inject_install_is_idempotent(self, project_root: Path) -> None:
        install("project", cwd=project_root, inject=True)
        report2 = install("project", cwd=project_root, inject=True)
        assert report2.changed is False
        groups = _settings(project_root)["hooks"]["PreToolUse"]
        ours = [g for g in groups if any(RECALL_ID in e["command"] for e in g["hooks"])]
        assert len(ours) == 1  # replaced, never stacked

    def test_plain_reinstall_converges_recall_away(self, project_root: Path) -> None:
        install("project", cwd=project_root, inject=True)
        install("project", cwd=project_root)
        data = _settings(project_root)
        assert "PreToolUse" not in data["hooks"]

    def test_uninstall_removes_recall(self, project_root: Path) -> None:
        install("project", cwd=project_root, inject=True)
        report = uninstall("project", cwd=project_root)
        assert RECALL_ID in report.hooks_removed
        assert "PreToolUse" not in _settings(project_root).get("hooks", {})

    def test_status_reports_recall(self, project_root: Path) -> None:
        install("project", cwd=project_root, inject=True)
        st = status("project", cwd=project_root)
        assert RECALL_ID in st["project"]["hooks"]
        assert st["project"]["hooks"][RECALL_ID] is False  # never --capture-content
        assert RECALL_ID not in st["project"]["stale"]

    def test_repair_canonicalises_stale_recall_command(self, project_root: Path) -> None:
        p = project_root / ".claude" / "settings.json"
        p.parent.mkdir(parents=True)
        stale = f"/old/venv/bin/stackunderflow hooks run {RECALL_ID}"
        p.write_text(json.dumps(
            {"hooks": {"PreToolUse": [{"matcher": "Edit|Write|Bash",
                                       "hooks": [{"type": "command", "command": stale}]}]}}
        ))
        report = repair("project", cwd=project_root)
        assert [c["hook_id"] for c in report.repaired] == [RECALL_ID]
        cmd = _settings(project_root)["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        assert cmd == f"stackunderflow hooks run {RECALL_ID}"

    def test_parse_and_canonical_round_trip(self) -> None:
        cmd = templates.canonical_command(RECALL_ID)
        assert cmd == f"stackunderflow hooks run {RECALL_ID}"
        assert templates.parse_hook_command(cmd) == (RECALL_ID, False)
        assert templates.is_canonical(cmd, capture_content=False)
        assert templates.HOOK_ID_EVENTS[RECALL_ID] == "PreToolUse"
        assert RECALL_ID in templates.ALL_HOOK_IDS

    def test_canonical_hooks_block_includes_recall_only_with_inject(self) -> None:
        plain = templates.canonical_hooks_block()
        assert "PreToolUse" not in plain
        with_inject = templates.canonical_hooks_block(inject=True)
        cmds = [e["command"] for g in with_inject["PreToolUse"] for e in g["hooks"]]
        assert f"stackunderflow hooks run {RECALL_ID}" in cmds
