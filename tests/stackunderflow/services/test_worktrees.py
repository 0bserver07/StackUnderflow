"""Tests for ``services.worktrees`` — worktree detect / attribute / preview-prune.

Strategy: REAL fixture repos built with ``git init`` + ``git worktree add``
under ``tmp_path`` (never ``~/.stackunderflow`` / ``~/.claude`` / real repos).
Fixture git calls are config-isolated (``GIT_CONFIG_GLOBAL/SYSTEM`` →
``os.devnull``, explicit ``user.name``/``user.email``, ``commit.gpgsign=false``)
so they behave identically on CI and on a developer machine with exotic
global config. The module under test runs against ambient env — that IS the
production condition.

Pure-logic surfaces (``is_worktree_slug``, ``_path_to_slug``, ``_verdict``)
are table-driven and need no git at all.
"""

from __future__ import annotations

import os
import shutil
import sqlite3
import subprocess
import time
from pathlib import Path

import pytest

from stackunderflow.services import worktrees
from stackunderflow.services.worktrees import (
    WorktreeInfo,
    attribute_fragments,
    is_worktree_slug,
    list_worktrees,
)
from stackunderflow.store import db, schema

_HAS_GIT = shutil.which("git") is not None

requires_git = pytest.mark.skipif(not _HAS_GIT, reason="git binary not available")


# ── fixture helpers ──────────────────────────────────────────────────────────


def _git(cwd: Path, *args: str) -> str:
    """Run git for FIXTURE SETUP, isolated from the machine's global config."""
    env = dict(os.environ)
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_SYSTEM"] = os.devnull
    env.pop("GIT_DIR", None)
    env.pop("GIT_WORK_TREE", None)
    result = subprocess.run(  # noqa: S603 — fixture-controlled argv, never user input
        [  # noqa: S607 — relies on git on PATH, same posture as the module under test
            "git",
            "-C",
            str(cwd),
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            *args,
        ],
        capture_output=True,
        text=True,
        env=env,
        check=True,
    )
    return result.stdout


def _make_repo(base: Path, name: str = "repo") -> Path:
    root = base / name
    root.mkdir()
    _git(root, "init")
    (root / "README.md").write_text("hello\n")
    _git(root, "add", "README.md")
    _git(root, "commit", "-m", "initial commit")
    return root


def _add_worktree(root: Path, name: str, *args: str) -> Path:
    """Add a worktree under the real-world ``.claude/worktrees/<name>`` shape."""
    wt = root / ".claude" / "worktrees" / name
    _git(root, "worktree", "add", *args, str(wt))
    return wt


def _set_age(path: Path, days: float) -> None:
    t = time.time() - days * 86400
    os.utime(path, (t, t))


def _by_name(infos: list[WorktreeInfo]) -> dict[str, WorktreeInfo]:
    return {Path(w.path).name: w for w in infos}


@pytest.fixture
def store(tmp_path: Path):
    conn = db.connect(tmp_path / "store.db")
    schema.apply(conn)
    yield conn
    conn.close()


@pytest.fixture
def fixture_repo(tmp_path: Path) -> Path:
    """One repo, four worktrees covering the verdict matrix.

    * ``merged``  — branched at main HEAD, no commits, clean, 10 days old
    * ``unique``  — one commit that never landed on main, 10 days old
    * ``dirty``   — an uncommitted (untracked) file, 10 days old
    * ``active``  — dirty AND fresh mtime (activity must win)
    """
    root = _make_repo(tmp_path)

    merged = _add_worktree(root, "merged", "-b", "feat-merged")

    unique = _add_worktree(root, "unique", "-b", "feat-unique")
    (unique / "new.txt").write_text("work\n")
    _git(unique, "add", "new.txt")
    _git(unique, "commit", "-m", "unique work never merged")

    dirty = _add_worktree(root, "dirty", "-b", "feat-dirty")
    (dirty / "scratch.txt").write_text("uncommitted\n")

    active = _add_worktree(root, "active", "-b", "feat-active")
    (active / "wip.txt").write_text("hot\n")

    for wt in (merged, unique, dirty):
        _set_age(wt, days=10)
    return root


def _insert_project(conn: sqlite3.Connection, provider: str, slug: str) -> int:
    conn.execute(
        "INSERT INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES (?, ?, ?, 0, 0)",
        (provider, slug, slug),
    )
    return int(
        conn.execute(
            "SELECT id FROM projects WHERE provider = ? AND slug = ?", (provider, slug)
        ).fetchone()[0]
    )


def _insert_session_with_cwd(
    conn: sqlite3.Connection, *, slug: str, session_id: str, cwd: str, ts: str
) -> None:
    """Project + session + one cwd-bearing message (the claude.py raw shape)."""
    import json

    conn.execute(
        "INSERT OR IGNORE INTO projects (provider, slug, display_name, first_seen, last_modified) "
        "VALUES ('claude', ?, ?, 0, 0)",
        (slug, slug),
    )
    project_id = conn.execute(
        "SELECT id FROM projects WHERE slug = ?", (slug,)
    ).fetchone()[0]
    conn.execute(
        "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
        "VALUES (?, ?, ?, ?, 1)",
        (project_id, session_id, ts, ts),
    )
    session_fk = conn.execute(
        "SELECT id FROM sessions WHERE session_id = ?", (session_id,)
    ).fetchone()[0]
    conn.execute(
        "INSERT INTO messages (session_fk, seq, timestamp, role, raw_json) "
        "VALUES (?, 0, ?, 'user', ?)",
        (session_fk, ts, json.dumps({"cwd": cwd, "type": "user"})),
    )
    conn.commit()


# ── verdicts against real worktrees ─────────────────────────────────────────


@requires_git
class TestVerdicts:
    def test_verdict_truth_table_on_real_worktrees(self, fixture_repo: Path, store) -> None:
        infos = list_worktrees(store, project_root=str(fixture_repo))
        by = _by_name(infos)
        assert set(by) == {"merged", "unique", "dirty", "active"}

        merged = by["merged"]
        assert merged.verdict == "MERGED_SAFE_TO_PRUNE"
        assert merged.unique_commits == 0
        assert merged.dirty_count == 0
        assert merged.branch == "feat-merged"
        assert merged.note is None
        assert merged.age_days is not None and merged.age_days > 2.0

        unique = by["unique"]
        assert unique.verdict == "HAS_UNIQUE_WORK"
        assert unique.unique_commits == 1
        assert unique.dirty_count == 0

        dirty = by["dirty"]
        assert dirty.verdict == "HAS_UNIQUE_WORK"
        assert dirty.unique_commits == 0
        assert dirty.dirty_count >= 1

        active = by["active"]  # dirty AND fresh — activity wins
        assert active.verdict == "ACTIVE"
        assert active.age_days is not None and active.age_days < 2.0

    def test_worktree_metadata_and_slug_round_trip(self, fixture_repo: Path, store) -> None:
        infos = list_worktrees(store, project_root=str(fixture_repo))
        assert len(infos) == 4
        for w in infos:
            assert w.parent_repo is not None
            assert Path(w.parent_repo).resolve() == fixture_repo.resolve()
            assert w.head is not None and len(w.head) in (40, 64)
            assert w.parent_slug == worktrees._path_to_slug(w.parent_repo)
            # the real worktree path mangles to a slug the pure shape test
            # maps straight back to the real parent slug
            assert is_worktree_slug(worktrees._path_to_slug(w.path)) == w.parent_slug
        # deterministic ordering
        assert [w.path for w in infos] == sorted(w.path for w in infos)

    def test_deleted_worktree_dir_degrades_conservative_never_safe(
        self, tmp_path: Path, store
    ) -> None:
        root = _make_repo(tmp_path)
        wt = root / "wt-gone"
        _git(root, "worktree", "add", "-b", "feat-gone", str(wt))
        shutil.rmtree(wt)  # break it: registered in git, directory gone

        (w,) = list_worktrees(store, project_root=str(root))
        assert w.verdict == "HAS_UNIQUE_WORK"  # error → conservative, never SAFE
        assert w.note is not None and "conservative" in w.note
        assert w.age_days is None

    def test_non_repo_and_missing_roots_return_empty(self, tmp_path: Path, store) -> None:
        plain = tmp_path / "plain"
        plain.mkdir()
        assert list_worktrees(store, project_root=str(plain)) == []
        assert list_worktrees(store, project_root=str(tmp_path / "does-not-exist")) == []

    def test_repo_without_linked_worktrees_returns_empty(self, tmp_path: Path, store) -> None:
        root = _make_repo(tmp_path)
        assert list_worktrees(store, project_root=str(root)) == []

    def test_missing_git_binary_degrades_to_empty(
        self, tmp_path: Path, store, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        root = _make_repo(tmp_path)

        def _no_git(*_a, **_k):
            raise FileNotFoundError("git not found")

        monkeypatch.setattr(worktrees.subprocess, "run", _no_git)
        assert list_worktrees(store, project_root=str(root)) == []

    def test_default_branch_master_fallback(self, tmp_path: Path, store) -> None:
        """No origin, no main — a master-named repo still gets verdicts."""
        root = tmp_path / "legacy"
        root.mkdir()
        _git(root, "init")
        _git(root, "checkout", "-b", "master")
        (root / "a.txt").write_text("a\n")
        _git(root, "add", "a.txt")
        _git(root, "commit", "-m", "initial")
        wt = root / "wt-old"
        _git(root, "worktree", "add", "-b", "feat", str(wt))
        _set_age(wt, days=10)

        (w,) = list_worktrees(store, project_root=str(root))
        assert w.verdict == "MERGED_SAFE_TO_PRUNE"
        assert w.unique_commits == 0

    def test_default_branch_from_origin_head(self, tmp_path: Path, store) -> None:
        """A clone carries refs/remotes/origin/HEAD — the preferred comparison base."""
        upstream = _make_repo(tmp_path, "upstream")
        clone = tmp_path / "clone"
        _git(tmp_path, "clone", str(upstream), str(clone))
        assert worktrees._default_branch(str(clone)) == "origin/main"

        wt = clone / "wt-feat"
        _git(clone, "worktree", "add", "-b", "feat-x", str(wt))
        (wt / "x.txt").write_text("x\n")
        _git(wt, "add", "x.txt")
        _git(wt, "commit", "-m", "unlanded")
        _set_age(wt, days=10)

        (w,) = list_worktrees(store, project_root=str(clone))
        assert w.verdict == "HAS_UNIQUE_WORK"
        assert w.unique_commits == 1


# ── prune previews (never executed) ─────────────────────────────────────────


@requires_git
class TestPrunePreviews:
    def test_prune_commands_are_previews_and_nothing_is_executed(
        self, fixture_repo: Path, store
    ) -> None:
        infos = list_worktrees(store, project_root=str(fixture_repo))
        merged = _by_name(infos)["merged"]
        assert merged.prune_commands == [
            f"git worktree remove {merged.path}",
            "git branch -D feat-merged",
        ]
        # nothing was executed: dir still there, git still lists main + 4
        assert Path(merged.path).is_dir()
        listing = _git(fixture_repo, "worktree", "list", "--porcelain")
        assert listing.count("worktree ") == 5
        branches = _git(fixture_repo, "branch", "--list", "feat-merged")
        assert "feat-merged" in branches

    def test_detached_worktree_previews_no_branch_delete(
        self, tmp_path: Path, store
    ) -> None:
        root = _make_repo(tmp_path)
        wt = root / "wt-detached"
        _git(root, "worktree", "add", "--detach", str(wt))
        _set_age(wt, days=10)

        (w,) = list_worktrees(store, project_root=str(root))
        assert w.branch is None
        assert w.prune_commands == [f"git worktree remove {w.path}"]
        # detached at main HEAD, clean → still safely classifiable
        assert w.verdict == "MERGED_SAFE_TO_PRUNE"

    def test_worktree_on_default_branch_never_previews_branch_delete(
        self, tmp_path: Path, store
    ) -> None:
        root = _make_repo(tmp_path)
        _git(root, "checkout", "-b", "dev")  # free 'main' for the worktree
        wt = root / "wt-main"
        _git(root, "worktree", "add", str(wt), "main")
        _set_age(wt, days=10)

        (w,) = list_worktrees(store, project_root=str(root))
        assert w.branch == "main"
        assert w.prune_commands == [f"git worktree remove {w.path}"]


# ── read-only guarantee ─────────────────────────────────────────────────────


@requires_git
class TestReadOnly:
    def test_only_allowlisted_readonly_git_calls_are_made(
        self, fixture_repo: Path, store, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        recorded: list[tuple[str, ...]] = []
        real_run_git = worktrees._run_git

        def recording(cwd: str, args):
            recorded.append(tuple(args))
            return real_run_git(cwd, args)

        monkeypatch.setattr(worktrees, "_run_git", recording)
        list_worktrees(store, project_root=str(fixture_repo))

        assert recorded, "expected at least one git invocation"
        allowed_heads = {"worktree", "status", "cherry", "rev-parse", "symbolic-ref"}
        for args in recorded:
            assert args[0] in allowed_heads, f"non-read-only git call: {args}"
            if args[0] == "worktree":
                assert args[1] == "list"
            if args[0] == "status":
                assert args[1] == "--porcelain"

    def test_run_git_chokepoint_refuses_mutating_subcommands(
        self, tmp_path: Path
    ) -> None:
        root = _make_repo(tmp_path)
        for argv in (
            ["worktree", "remove", "x"],
            ["worktree", "prune"],
            ["branch", "-D", "x"],
            ["checkout", "main"],
            ["reset", "--hard"],
            ["clean", "-fd"],
            ["gc"],
            ["status"],  # even status without --porcelain is outside the allowlist
        ):
            assert worktrees._run_git(str(root), argv) is None


# ── store integration: discovery + sessions/cost attribution ────────────────


@requires_git
class TestStoreIntegration:
    def test_roots_discovered_from_store_session_cwds(
        self, fixture_repo: Path, store
    ) -> None:
        merged_wt = fixture_repo / ".claude" / "worktrees" / "merged"
        # two sessions in the SAME repo (one in the root, one in a worktree)
        # must produce ONE listing — dedupe by git common dir
        _insert_session_with_cwd(
            store, slug="root-proj", session_id="s-root",
            cwd=str(fixture_repo), ts="2026-07-01T10:00:00Z",
        )
        _insert_session_with_cwd(
            store, slug="wt-proj", session_id="s-wt",
            cwd=str(merged_wt), ts="2026-07-01T11:00:00Z",
        )
        infos = list_worktrees(store)  # no project_root → store-driven
        assert {Path(w.path).name for w in infos} == {"merged", "unique", "dirty", "active"}
        assert len(infos) == 4  # no duplicates from the two cwds

    def test_store_without_matching_cwds_yields_empty(self, store) -> None:
        assert list_worktrees(store) == []

    def test_sessions_and_cost_attributed_via_fragment_slug(
        self, fixture_repo: Path, store
    ) -> None:
        # First pass to learn the path exactly as git reports it, then seed
        # the fragment project under the mangled slug of that path.
        first = _by_name(list_worktrees(store, project_root=str(fixture_repo)))["merged"]
        frag_slug = worktrees._path_to_slug(first.path)

        project_id = _insert_project(store, "claude", frag_slug)
        for sid in ("sess-a", "sess-b"):
            store.execute(
                "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
                "VALUES (?, ?, '2026-06-01T00:00:00Z', '2026-06-01T01:00:00Z', 1)",
                (project_id, sid),
            )
        store.execute(
            "INSERT INTO project_mart (project_id, provider, slug, display_name, total_cost_usd) "
            "VALUES (?, 'claude', ?, ?, 12.5)",
            (project_id, frag_slug, frag_slug),
        )
        store.commit()

        infos = list_worktrees(store, project_root=str(fixture_repo))
        by = _by_name(infos)
        assert by["merged"].sessions == 2
        assert by["merged"].cost_usd == pytest.approx(12.5)
        for name in ("unique", "dirty", "active"):
            assert by[name].sessions == 0
            assert by[name].cost_usd == 0.0

    def test_cost_falls_back_to_usage_events_when_mart_has_no_row(
        self, fixture_repo: Path, store
    ) -> None:
        first = _by_name(list_worktrees(store, project_root=str(fixture_repo)))["merged"]
        frag_slug = worktrees._path_to_slug(first.path)

        project_id = _insert_project(store, "claude", frag_slug)
        store.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) "
            "VALUES (?, 'sess-a', '2026-06-01T00:00:00Z', '2026-06-01T01:00:00Z', 1)",
            (project_id,),
        )
        for fk, cost in ((1, 1.25), (2, 2.0)):
            store.execute(
                "INSERT INTO usage_events "
                "(source_message_fk, provider, project_id, session_id, ts, day, model, role, cost_usd) "
                "VALUES (?, 'claude', ?, 'sess-a', '2026-06-01T00:00:00Z', '2026-06-01', 'm', 'assistant', ?)",
                (fk, project_id, cost),
            )
        store.commit()

        by = _by_name(list_worktrees(store, project_root=str(fixture_repo)))
        assert by["merged"].sessions == 1
        assert by["merged"].cost_usd == pytest.approx(3.25)


# ── is_worktree_slug: pure, table-driven ────────────────────────────────────


@pytest.mark.parametrize(
    ("slug", "expected"),
    [
        # the two real-world shapes from this machine (contract-mandated)
        (
            "-Users-yadkonrad-dev-dev-year26-feb26-chimera--worktrees-all-issues",
            "-Users-yadkonrad-dev-dev-year26-feb26-chimera",
        ),
        (
            "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow--claude-worktrees-todo-cleanup",
            "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
        ),
        (
            "-Users-yadkonrad-dev-dev-year26-feb26-chimera--worktrees-remaining-features",
            "-Users-yadkonrad-dev-dev-year26-feb26-chimera",
        ),
        ("-home-u-proj--claude-worktrees-agent-a2a90f25e55f4489", "-home-u-proj"),
        # nested worktrees: the RIGHTMOST marker wins → immediate parent
        ("-p--worktrees-a--claude-worktrees-b", "-p--worktrees-a"),
        ("-p--claude-worktrees-a--worktrees-b", "-p--claude-worktrees-a"),
        # non-matches: 'worktrees' as a genuine directory name (single dash)
        ("-Users-x-worktrees-app", None),
        ("-Users-x-git-worktrees-app", None),
        ("-Users-x-my-project", None),
        # non-matches: degenerate shapes
        ("-Users-x-proj--worktrees-", None),  # empty worktree name
        ("-Users-x-proj--claude-worktrees-", None),  # empty worktree name
        ("--worktrees-foo", None),  # empty parent
        ("--claude-worktrees-foo", None),  # empty parent
        ("-Users-x-proj--worktrees", None),  # marker requires its trailing dash
        ("worktrees", None),
        ("", None),
    ],
)
def test_is_worktree_slug(slug: str, expected: str | None) -> None:
    assert is_worktree_slug(slug) == expected


@pytest.mark.parametrize(
    ("path", "expected"),
    [
        (
            "/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow",
            "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
        ),
        (
            "/Users/x/proj/.claude/worktrees/todo-cleanup",
            "-Users-x-proj--claude-worktrees-todo-cleanup",
        ),
        ("/Users/x/proj/.worktrees/all-issues", "-Users-x-proj--worktrees-all-issues"),
        ("/Users/x/proj/", "-Users-x-proj"),  # trailing separator ignored
    ],
)
def test_path_to_slug_matches_claude_code_mangling(path: str, expected: str) -> None:
    assert worktrees._path_to_slug(path) == expected


# ── verdict rule: pure truth table ──────────────────────────────────────────


@pytest.mark.parametrize(
    ("age_days", "unique", "dirty", "expected"),
    [
        # activity (mtime ≤ 48h) wins over everything, including errors
        (0.5, 5, 5, "ACTIVE"),
        (1.9, 0, 0, "ACTIVE"),
        (2.0, 0, 0, "ACTIVE"),  # boundary: exactly 48h
        (0.1, None, None, "ACTIVE"),
        # unique work / dirt → HAS_UNIQUE_WORK
        (10.0, 1, 0, "HAS_UNIQUE_WORK"),
        (10.0, 0, 3, "HAS_UNIQUE_WORK"),
        (10.0, 4, 2, "HAS_UNIQUE_WORK"),
        # any probe error (None) → conservative, never SAFE
        (10.0, None, 0, "HAS_UNIQUE_WORK"),
        (10.0, 0, None, "HAS_UNIQUE_WORK"),
        (10.0, None, None, "HAS_UNIQUE_WORK"),
        # SAFE only when both probes succeeded AND both are zero
        (10.0, 0, 0, "MERGED_SAFE_TO_PRUNE"),
        (2.1, 0, 0, "MERGED_SAFE_TO_PRUNE"),
        # unreadable mtime alone (git probes fine) does not block SAFE
        (None, 0, 0, "MERGED_SAFE_TO_PRUNE"),
        (None, 1, 0, "HAS_UNIQUE_WORK"),
    ],
)
def test_verdict_rule(
    age_days: float | None, unique: int | None, dirty: int | None, expected: str
) -> None:
    assert (
        worktrees._verdict(age_days=age_days, unique_commits=unique, dirty_count=dirty)
        == expected
    )


def test_worktreeinfo_to_dict_field_contract() -> None:
    info = WorktreeInfo(
        path="/x",
        branch="b",
        head="h",
        parent_repo="/p",
        parent_slug="-p",
        dirty_count=0,
        unique_commits=0,
        age_days=1.0,
        verdict="ACTIVE",
        sessions=2,
        cost_usd=1.5,
        prune_commands=["git worktree remove /x"],
    )
    d = info.to_dict()
    assert set(d) == {
        "path",
        "branch",
        "head",
        "parent_repo",
        "parent_slug",
        "dirty_count",
        "unique_commits",
        "age_days",
        "verdict",
        "sessions",
        "cost_usd",
        "prune_commands",
        "note",
    }
    assert d["prune_commands"] == ["git worktree remove /x"]
    assert d["note"] is None


# ── attribute_fragments ─────────────────────────────────────────────────────


class TestAttributeFragments:
    def test_attributes_fragments_and_is_idempotent(self, store) -> None:
        _insert_project(store, "claude", "-Users-x-app")
        _insert_project(store, "claude", "-Users-x-app--worktrees-fix")
        _insert_project(store, "claude", "-Users-x-app--claude-worktrees-alpha")
        _insert_project(store, "claude", "-Users-x-other")

        assert attribute_fragments(store) == 2

        rows = dict(store.execute("SELECT slug, worktree_of FROM projects").fetchall())
        assert rows["-Users-x-app--worktrees-fix"] == "-Users-x-app"
        assert rows["-Users-x-app--claude-worktrees-alpha"] == "-Users-x-app"
        assert rows["-Users-x-app"] is None
        assert rows["-Users-x-other"] is None

        # idempotent: a second run changes nothing and reports 0
        assert attribute_fragments(store) == 0

    def test_orphan_fragment_is_still_attributed(self, store) -> None:
        """The parent slug is recorded even when no parent project row exists."""
        _insert_project(store, "claude", "-gone-parent--worktrees-x")
        assert attribute_fragments(store) == 1
        row = store.execute(
            "SELECT worktree_of FROM projects WHERE slug = '-gone-parent--worktrees-x'"
        ).fetchone()
        assert row[0] == "-gone-parent"

    def test_same_shape_under_two_providers_both_attributed(self, store) -> None:
        _insert_project(store, "claude", "-p--worktrees-x")
        _insert_project(store, "codex", "-p--worktrees-x")
        assert attribute_fragments(store) == 2
        rows = store.execute(
            "SELECT worktree_of FROM projects WHERE slug = '-p--worktrees-x'"
        ).fetchall()
        assert [r[0] for r in rows] == ["-p", "-p"]

    def test_pre_v027_store_degrades_to_zero(self, tmp_path: Path) -> None:
        """A store without the worktree_of column returns 0, never raises."""
        conn = sqlite3.connect(tmp_path / "old.db")
        try:
            conn.execute(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, provider TEXT, slug TEXT)"
            )
            conn.execute(
                "INSERT INTO projects (provider, slug) VALUES ('claude', '-p--worktrees-x')"
            )
            assert attribute_fragments(conn) == 0
        finally:
            conn.close()
