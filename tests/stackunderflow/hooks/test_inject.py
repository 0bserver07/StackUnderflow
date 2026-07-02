"""Context-injection hooks (Move 3) — the contract, locked.

Covered here:

* A valid payload against a seeded store yields a **bounded, valid** injection
  envelope in Claude Code's verified ``hookSpecificOutput.additionalContext``
  shape — for each of SessionStart / UserPromptSubmit / PreToolUse.
* Garbage / empty / unknown / capture-id payloads yield **empty output** — the
  never-disrupt invariant.
* Every injected text obeys the per-event token budget.
* ``handlers.run()`` dispatches injection ids to stdout and always exits 0.
* The internal ``hooks run`` CLI plumbing reads stdin and emits the envelope.
* ``install(..., inject=True)`` wires the three hooks; convergence drops them.

The store is always a ``tmp_path`` one wired in via ``deps.store_path`` — the
real ``~/.stackunderflow`` is never touched.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from click.testing import CliRunner

import stackunderflow.deps as deps
from stackunderflow.cli import cli
from stackunderflow.hooks import inject, templates
from stackunderflow.hooks._install import install, uninstall
from stackunderflow.hooks.handlers import run
from stackunderflow.store import db, schema

INJECT_IDS = (
    "stackunderflow-inject-session-start",
    "stackunderflow-inject-user-prompt",
    "stackunderflow-inject-pre-tool-use",
)


# ── seeding ─────────────────────────────────────────────────────────────────


def _seed(store_path: Path, project_dir: Path) -> str:
    """Seed a project (rooted at *project_dir*) with sessions discovery can find.

    Returns the project ``risky.py`` absolute path — the file the PreToolUse
    test pretends is about to be edited. The seeded data covers all three hooks:
    two recent sessions (SessionStart), a past decision mentioning "backoff"
    (UserPromptSubmit), and an edit-then-broke session for ``risky.py``
    (PreToolUse).
    """
    risky = str((project_dir / "risky.py").resolve())
    conn = db.connect(store_path)
    try:
        pid = int(
            conn.execute(
                "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) "
                "VALUES ('claude', ?, ?, 'proj', 0.0, 0.0)",
                (inject._slug_from_cwd(str(project_dir)), str(project_dir.resolve())),
            ).lastrowid
        )

        def _session(session_id: str, last_ts: str, msgs: int) -> int:
            return int(
                conn.execute(
                    "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                    "VALUES (?, ?, '2026-05-01T00:00:00Z', ?, ?)",
                    (pid, session_id, last_ts, msgs),
                ).lastrowid
            )

        def _msg(sfk: int, seq: int, role: str, content: str, tools: str = "[]") -> None:
            conn.execute(
                "INSERT INTO messages (session_fk, seq, timestamp, role, model, input_tokens, "
                "output_tokens, cache_create_tokens, cache_read_tokens, content_text, tools_json, "
                "raw_json, is_sidechain) "
                "VALUES (?, ?, '2026-05-12T00:00:00Z', ?, NULL, 0, 0, 0, 0, ?, ?, '{}', 0)",
                (sfk, seq, role, content, tools),
            )

        # A past decision — UserPromptSubmit searches for a distinctive token.
        s1 = _session("decision-sess", "2026-05-18T03:00:00Z", 40)
        _msg(s1, 0, "user", "we decided to add retry logic with exponential backoff in the client")

        # An edit-then-broke session for risky.py — PreToolUse failure modes.
        s2 = _session("fail-sess", "2026-05-17T03:00:00Z", 22)
        _msg(
            s2, 0, "assistant", "editing the file", tools=json.dumps([{"name": "Edit", "input": {"file_path": risky}}])
        )
        _msg(s2, 1, "user", "that broke the build — please look again")

        conn.commit()
    finally:
        conn.close()
    return risky


@pytest.fixture
def seeded(tmp_path: Path, monkeypatch) -> tuple[Path, str]:
    """A schema'd, seeded store wired in as ``deps.store_path``.

    Returns ``(project_dir, risky_py_path)``.
    """
    store_path = tmp_path / "store.db"
    conn = db.connect(store_path)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_path)
    project_dir = tmp_path / "proj"
    project_dir.mkdir()
    risky = _seed(store_path, project_dir)
    return project_dir, risky


@pytest.fixture
def empty_store(tmp_path: Path, monkeypatch) -> Path:
    """A schema'd but empty store — discovery finds nothing."""
    store_path = tmp_path / "store.db"
    conn = db.connect(store_path)
    schema.apply(conn)
    conn.close()
    monkeypatch.setattr(deps, "store_path", store_path)
    return tmp_path


# ── envelope shape helpers ──────────────────────────────────────────────────


def _assert_valid_envelope(raw: str, event: str, hook_id: str) -> str:
    """The output is a single valid JSON object in the verified injection shape."""
    obj = json.loads(raw)
    assert set(obj) == {"hookSpecificOutput"}, f"unexpected top-level keys: {set(obj)}"
    hso = obj["hookSpecificOutput"]
    assert set(hso) == {"hookEventName", "additionalContext"}, f"unexpected keys: {set(hso)}"
    assert hso["hookEventName"] == event
    text = hso["additionalContext"]
    assert isinstance(text, str) and text.strip()
    # Token-bounded: chars/4 estimate must stay within the per-event budget.
    budget = inject._TOKEN_BUDGET[hook_id]
    assert len(text) <= budget * inject._CHARS_PER_TOKEN, (
        f"{hook_id}: {len(text)} chars exceeds {budget * inject._CHARS_PER_TOKEN}"
    )
    return text


# ── happy path: a valid payload yields a bounded valid envelope ─────────────


class TestSessionStart:
    def test_valid_payload_yields_bounded_envelope(self, seeded) -> None:
        project_dir, _ = seeded
        raw = inject.build_injection("stackunderflow-inject-session-start", {"cwd": str(project_dir)})
        text = _assert_valid_envelope(raw, "SessionStart", "stackunderflow-inject-session-start")
        assert "StackUnderflow memory" in text
        assert "stackunderflow memory" in text  # points the agent at the CLI

    def test_no_sessions_yields_empty(self, empty_store: Path) -> None:
        assert inject.build_injection("stackunderflow-inject-session-start", {"cwd": str(empty_store)}) == ""

    def test_missing_cwd_yields_empty(self, seeded) -> None:
        assert inject.build_injection("stackunderflow-inject-session-start", {}) == ""


class TestUserPromptSubmit:
    def test_prompt_matching_a_decision_yields_envelope(self, seeded) -> None:
        project_dir, _ = seeded
        raw = inject.build_injection(
            "stackunderflow-inject-user-prompt",
            {"cwd": str(project_dir), "prompt": "remind me how the backoff was set up"},
        )
        text = _assert_valid_envelope(raw, "UserPromptSubmit", "stackunderflow-inject-user-prompt")
        assert "backoff" in text

    def test_generic_prompt_yields_empty(self, seeded) -> None:
        # Nothing distinctive to search on → inject nothing rather than noise.
        project_dir, _ = seeded
        assert (
            inject.build_injection(
                "stackunderflow-inject-user-prompt",
                {"cwd": str(project_dir), "prompt": "fix it"},
            )
            == ""
        )

    def test_no_match_yields_empty(self, seeded) -> None:
        project_dir, _ = seeded
        assert (
            inject.build_injection(
                "stackunderflow-inject-user-prompt",
                {"cwd": str(project_dir), "prompt": "investigate the quux frobnicator"},
            )
            == ""
        )

    def test_missing_prompt_yields_empty(self, seeded) -> None:
        project_dir, _ = seeded
        assert inject.build_injection("stackunderflow-inject-user-prompt", {"cwd": str(project_dir)}) == ""


class TestPreToolUse:
    def test_editing_a_known_bad_file_yields_envelope(self, seeded) -> None:
        _, risky = seeded
        raw = inject.build_injection(
            "stackunderflow-inject-pre-tool-use",
            {"tool_name": "Edit", "tool_input": {"file_path": risky}},
        )
        text = _assert_valid_envelope(raw, "PreToolUse", "stackunderflow-inject-pre-tool-use")
        assert "risky.py" in text

    def test_editing_an_unknown_file_yields_empty(self, seeded) -> None:
        project_dir, _ = seeded
        assert (
            inject.build_injection(
                "stackunderflow-inject-pre-tool-use",
                {"tool_name": "Edit", "tool_input": {"file_path": str(project_dir / "calm.py")}},
            )
            == ""
        )

    def test_missing_file_path_yields_empty(self, seeded) -> None:
        assert (
            inject.build_injection("stackunderflow-inject-pre-tool-use", {"tool_name": "Edit", "tool_input": {}}) == ""
        )


# ── never disrupt the agent: garbage / unknown → empty ──────────────────────


class TestNeverDisrupt:
    @pytest.mark.parametrize("hook_id", INJECT_IDS)
    @pytest.mark.parametrize("bad", [None, "not a dict", 42, [], {"weird": object()}])
    def test_garbage_payload_yields_empty(self, hook_id: str, bad, seeded) -> None:
        assert inject.build_injection(hook_id, bad) == ""

    @pytest.mark.parametrize("hook_id", INJECT_IDS)
    def test_empty_payload_yields_empty(self, hook_id: str, seeded) -> None:
        assert inject.build_injection(hook_id, {}) == ""

    def test_unknown_hook_id_yields_empty(self, seeded) -> None:
        assert inject.build_injection("stackunderflow-bogus", {"cwd": "/x"}) == ""

    def test_capture_hook_id_yields_empty(self, seeded) -> None:
        # A capture id is not an injection id — build_injection declines it.
        assert inject.build_injection("stackunderflow-stop", {"cwd": "/x"}) == ""

    def test_no_store_yields_empty(self, tmp_path: Path, monkeypatch) -> None:
        monkeypatch.setattr(deps, "store_path", tmp_path / "absent.db")
        for hook_id in INJECT_IDS:
            assert (
                inject.build_injection(
                    hook_id, {"cwd": str(tmp_path), "prompt": "retry backoff", "tool_input": {"file_path": "x.py"}}
                )
                == ""
            )
        assert not (tmp_path / "absent.db").exists()  # never created the file


# ── handlers.run() dispatch ─────────────────────────────────────────────────


class TestRunDispatch:
    def test_run_injection_writes_envelope_to_stdout(self, seeded, capsys) -> None:
        project_dir, _ = seeded
        rc = run("stackunderflow-inject-session-start", {"cwd": str(project_dir)})
        assert rc == 0
        out = capsys.readouterr().out
        _assert_valid_envelope(out.strip(), "SessionStart", "stackunderflow-inject-session-start")

    def test_run_injection_empty_payload_writes_nothing(self, seeded, capsys) -> None:
        assert run("stackunderflow-inject-pre-tool-use", {}) == 0
        assert capsys.readouterr().out == ""

    @pytest.mark.parametrize("bad", [None, "garbage", []])
    def test_run_injection_garbage_writes_nothing_exits_zero(self, seeded, capsys, bad) -> None:
        assert run("stackunderflow-inject-user-prompt", bad) == 0
        assert capsys.readouterr().out == ""


# ── the `hooks run` CLI plumbing ────────────────────────────────────────────


class TestHooksRunCli:
    def test_cli_run_injection_emits_envelope(self, seeded) -> None:
        project_dir, _ = seeded
        payload = {"hook_event_name": "SessionStart", "cwd": str(project_dir)}
        res = CliRunner().invoke(
            cli, ["hooks", "run", "stackunderflow-inject-session-start"], input=json.dumps(payload)
        )
        assert res.exit_code == 0
        _assert_valid_envelope(res.output.strip(), "SessionStart", "stackunderflow-inject-session-start")

    def test_cli_run_injection_garbage_stdin_empty_exit_zero(self, seeded) -> None:
        res = CliRunner().invoke(cli, ["hooks", "run", "stackunderflow-inject-session-start"], input="<<<not json>>>")
        assert res.exit_code == 0
        assert res.output.strip() == ""

    def test_cli_run_injection_empty_stdin_empty_exit_zero(self, seeded) -> None:
        res = CliRunner().invoke(cli, ["hooks", "run", "stackunderflow-inject-pre-tool-use"], input="")
        assert res.exit_code == 0
        assert res.output.strip() == ""


# ── install --inject wiring ─────────────────────────────────────────────────


@pytest.fixture
def project_root(tmp_path: Path, monkeypatch) -> Path:
    """A git-rooted tmp dir, with the store pointed at tmp (away from the real one)."""
    (tmp_path / ".git").mkdir()
    monkeypatch.setattr(deps, "store_path", tmp_path / "store.db")
    return tmp_path


def _settings(root: Path) -> dict:
    return json.loads((root / ".claude" / "settings.json").read_text())


class TestInstallInject:
    def test_plain_install_omits_injection_hooks(self, project_root: Path) -> None:
        report = install("project", cwd=project_root)
        assert report.inject is False
        data = _settings(project_root)
        assert set(data["hooks"]) == {"PostToolUse", "UserPromptSubmit", "Stop", "PreCompact"}
        assert "SessionStart" not in data["hooks"]
        assert "PreToolUse" not in data["hooks"]

    def test_inject_install_adds_injection_and_recall_hooks(self, project_root: Path) -> None:
        report = install("project", cwd=project_root, inject=True)
        assert report.inject is True
        assert set(report.hooks_installed) == set(templates.ALL_HOOK_IDS)
        data = _settings(project_root)
        # SessionStart + PreToolUse are injection-only events.
        assert data["hooks"]["SessionStart"][0]["hooks"][0]["command"] == (
            "stackunderflow hooks run stackunderflow-inject-session-start"
        )
        # PreToolUse carries the in-process injection group AND the recall group.
        ptu = data["hooks"]["PreToolUse"][0]
        assert ptu["matcher"] == "Edit|Write|MultiEdit"
        assert ptu["hooks"][0]["command"] == "stackunderflow hooks run stackunderflow-inject-pre-tool-use"
        recall_grp = data["hooks"]["PreToolUse"][1]
        assert recall_grp["matcher"] == "Edit|Write|Bash"
        assert recall_grp["hooks"][0]["command"] == "stackunderflow hooks run stackunderflow-pretool-recall"
        # UserPromptSubmit carries BOTH a capture and an injection group.
        ups_cmds = {e["command"] for g in data["hooks"]["UserPromptSubmit"] for e in g["hooks"]}
        assert ups_cmds == {
            "stackunderflow hooks run stackunderflow-user-prompt",
            "stackunderflow hooks run stackunderflow-inject-user-prompt",
        }

    def test_inject_then_plain_install_converges_to_capture_only(self, project_root: Path) -> None:
        install("project", cwd=project_root, inject=True)
        install("project", cwd=project_root)  # convergent re-install drops injection
        data = _settings(project_root)
        assert set(data["hooks"]) == {"PostToolUse", "UserPromptSubmit", "Stop", "PreCompact"}
        ups_cmds = {e["command"] for g in data["hooks"]["UserPromptSubmit"] for e in g["hooks"]}
        assert ups_cmds == {"stackunderflow hooks run stackunderflow-user-prompt"}

    def test_inject_install_is_idempotent(self, project_root: Path) -> None:
        install("project", cwd=project_root, inject=True)
        report2 = install("project", cwd=project_root, inject=True)
        assert report2.changed is False

    def test_uninstall_removes_injection_hooks_too(self, project_root: Path) -> None:
        install("project", cwd=project_root, inject=True)
        report = uninstall("project", cwd=project_root)
        assert set(report.hooks_removed) == set(templates.ALL_HOOK_IDS)
        data = _settings(project_root)
        # Only our hooks were present → all our events are gone.
        assert "SessionStart" not in data.get("hooks", {})
        assert "PreToolUse" not in data.get("hooks", {})

    def test_dry_run_inject_writes_nothing(self, project_root: Path) -> None:
        report = install("project", cwd=project_root, inject=True, dry_run=True)
        assert report.changed is True
        assert not (project_root / ".claude" / "settings.json").exists()
