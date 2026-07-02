"""Tests for ``stackunderflow worktrees`` — read-only worktree hygiene CLI.

Locks the text table + json render contracts, the ``--project`` scope
passthrough, exit codes, and the attribute command's output. The data
contract itself lives with the shared assembler in ``routes/worktrees.py``
(covered by ``tests/stackunderflow/routes/test_worktrees_route.py``); this
file owns the CLI surface.

``services/worktrees.py`` is owned by a parallel campaign agent (wt-core):
the service entry points are monkeypatched, but fixtures construct REAL
``WorktreeInfo`` objects so contract drift breaks loudly here.
"""

from __future__ import annotations

import json

from click.testing import CliRunner

import stackunderflow.deps as deps
import stackunderflow.routes.worktrees as worktrees_route
import stackunderflow.services.worktrees as worktrees_service
from stackunderflow.cli import cli
from stackunderflow.services.worktrees import WorktreeInfo
from tests.conftest import set_home_env

_USD = {"code": "USD", "symbol": "$", "rate_from_usd": 1.0, "warning": None}


def _info(**overrides) -> WorktreeInfo:
    base = {
        "path": "/repo/.claude/worktrees/agent-abc",
        "branch": "worktree-agent-abc",
        "head": "0123abcd",
        "parent_repo": "/repo",
        "parent_slug": "-repo",
        "dirty_count": 2,
        "unique_commits": 1,
        "age_days": 3.5,
        "verdict": "HAS_UNIQUE_WORK",
        "sessions": 4,
        "cost_usd": 1.25,
        "prune_commands": [
            "git -C /repo worktree remove .claude/worktrees/agent-abc",
            "git -C /repo branch -D worktree-agent-abc",
        ],
    }
    base.update(overrides)
    return WorktreeInfo(**base)


def _patch_env(tmp_path, monkeypatch, infos, *, calls=None):
    """Redirect HOME + store to tmp_path and mock the service layer.

    ``worktrees list`` goes through the shared assembler in
    ``routes/worktrees.py``, so ``list_worktrees`` is patched there;
    the currency helper is pinned to USD so figures are hermetic.
    ``attribute_fragments`` is patched to explode — the list command must
    never write (read-only contract); attribute tests re-patch it.
    """
    set_home_env(monkeypatch, tmp_path / "home")
    monkeypatch.setattr(deps, "store_path", tmp_path / "store.db")

    def fake_list(conn, project_root=None):
        if calls is not None:
            calls.append(project_root)
        return list(infos)

    monkeypatch.setattr(worktrees_route, "list_worktrees", fake_list)
    monkeypatch.setattr(worktrees_route, "active_currency_payload", lambda: dict(_USD))

    def explode(conn):  # pragma: no cover - reaching this IS the failure
        raise AssertionError("worktrees list must never attribute/write")

    monkeypatch.setattr(worktrees_service, "attribute_fragments", explode)


# ── worktrees list — text ─────────────────────────────────────────────────────


class TestListText:
    def test_table_and_summary(self, tmp_path, monkeypatch):
        _patch_env(tmp_path, monkeypatch, [
            _info(),
            _info(path="/repo/.claude/worktrees/agent-xyz",
                  branch="worktree-agent-xyz", verdict="MERGED_SAFE_TO_PRUNE",
                  dirty_count=0, unique_commits=0, sessions=1, cost_usd=0.50),
        ])
        r = CliRunner().invoke(cli, ["worktrees", "list"])
        assert r.exit_code == 0, r.output
        # Header row.
        for col in ("PATH", "BRANCH", "VERDICT", "DIRTY", "UNIQUE", "SESSIONS", "COST"):
            assert col in r.output
        # Row content.
        assert "worktree-agent-abc" in r.output
        assert "HAS_UNIQUE_WORK" in r.output
        assert "MERGED_SAFE_TO_PRUNE" in r.output
        assert "$1.25" in r.output
        assert "$0.50" in r.output
        # Summary line: counts + attributed cost.
        assert "2 worktree(s)" in r.output
        assert "1 safe to prune" in r.output
        assert "1 with unique work" in r.output
        assert "$1.75" in r.output
        # The prune-preview reminder — the CLI never deletes anything.
        assert "preview" in r.output

    def test_default_format_is_text(self, tmp_path, monkeypatch):
        _patch_env(tmp_path, monkeypatch, [_info()])
        r = CliRunner().invoke(cli, ["worktrees", "list"])
        assert r.exit_code == 0, r.output
        assert not r.output.lstrip().startswith("{")

    def test_empty_store(self, tmp_path, monkeypatch):
        _patch_env(tmp_path, monkeypatch, [])
        r = CliRunner().invoke(cli, ["worktrees", "list"])
        assert r.exit_code == 0, r.output
        assert "No worktrees found" in r.output
        assert "store" in r.output

    def test_home_paths_are_tilde_abbreviated(self, tmp_path, monkeypatch):
        wt_path = str(tmp_path / "home" / "repo" / ".claude" / "worktrees" / "agent-1")
        _patch_env(tmp_path, monkeypatch, [_info(path=wt_path)])
        r = CliRunner().invoke(cli, ["worktrees", "list"])
        assert r.exit_code == 0, r.output
        assert "~/repo/.claude/worktrees/agent-1" in r.output
        assert wt_path not in r.output

    def test_long_paths_keep_the_tail(self, tmp_path, monkeypatch):
        long_path = "/very/long/prefix/" + "x" * 60 + "/.claude/worktrees/agent-tail"
        _patch_env(tmp_path, monkeypatch, [_info(path=long_path)])
        r = CliRunner().invoke(cli, ["worktrees", "list"])
        assert r.exit_code == 0, r.output
        # The informative tail survives shortening; the full path does not.
        assert "worktrees/agent-tail" in r.output
        assert long_path not in r.output


# ── worktrees list — json ─────────────────────────────────────────────────────


class TestListJson:
    def test_json_is_the_route_payload_shape(self, tmp_path, monkeypatch):
        wt = _info()
        _patch_env(tmp_path, monkeypatch, [wt])
        r = CliRunner().invoke(cli, ["worktrees", "list", "--format", "json"])
        assert r.exit_code == 0, r.output
        body = json.loads(r.output)
        assert set(body.keys()) == {"scope", "worktrees", "summary", "scanned_at", "currency"}
        assert body["scope"] == "store"
        assert body["worktrees"] == [wt.to_dict()]
        assert body["summary"]["total"] == 1
        assert body["summary"]["has_unique_work"] == 1
        assert body["summary"]["attributed_cost_usd"] == 1.25
        # Prune commands ship in the json payload — as a preview only.
        assert body["worktrees"][0]["prune_commands"] == wt.prune_commands

    def test_project_scope_passthrough(self, tmp_path, monkeypatch):
        calls: list = []
        _patch_env(tmp_path, monkeypatch, [], calls=calls)
        r = CliRunner().invoke(
            cli, ["worktrees", "list", "--project", "/logs/demo", "--format", "json"]
        )
        assert r.exit_code == 0, r.output
        assert calls == ["/logs/demo"]
        assert json.loads(r.output)["scope"] == "/logs/demo"

    def test_no_project_scans_all_known_roots(self, tmp_path, monkeypatch):
        calls: list = []
        _patch_env(tmp_path, monkeypatch, [], calls=calls)
        r = CliRunner().invoke(cli, ["worktrees", "list", "--format", "json"])
        assert r.exit_code == 0, r.output
        assert calls == [None]


# ── worktrees attribute ───────────────────────────────────────────────────────


class TestAttribute:
    def test_prints_rows_updated(self, tmp_path, monkeypatch):
        _patch_env(tmp_path, monkeypatch, [])
        monkeypatch.setattr(worktrees_service, "attribute_fragments", lambda conn: 3)
        r = CliRunner().invoke(cli, ["worktrees", "attribute"])
        assert r.exit_code == 0, r.output
        assert "Attributed 3 worktree session fragment(s)" in r.output

    def test_idempotent_second_run_reports_zero(self, tmp_path, monkeypatch):
        _patch_env(tmp_path, monkeypatch, [])
        results = iter([2, 0])
        monkeypatch.setattr(
            worktrees_service, "attribute_fragments", lambda conn: next(results)
        )
        runner = CliRunner()
        first = runner.invoke(cli, ["worktrees", "attribute"])
        second = runner.invoke(cli, ["worktrees", "attribute"])
        assert first.exit_code == 0 and "Attributed 2" in first.output
        assert second.exit_code == 0 and "Attributed 0" in second.output


# ── exit codes ────────────────────────────────────────────────────────────────


class TestExitCodes:
    def test_list_rejects_unknown_format(self, tmp_path, monkeypatch):
        _patch_env(tmp_path, monkeypatch, [])
        r = CliRunner().invoke(cli, ["worktrees", "list", "--format", "yaml"])
        assert r.exit_code != 0

    def test_list_service_error_exits_nonzero(self, tmp_path, monkeypatch):
        _patch_env(tmp_path, monkeypatch, [])

        def boom(conn, project_root=None):
            raise RuntimeError("git exploded")

        monkeypatch.setattr(worktrees_route, "list_worktrees", boom)
        r = CliRunner().invoke(cli, ["worktrees", "list"])
        assert r.exit_code != 0
