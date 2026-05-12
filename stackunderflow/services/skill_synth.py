"""Auto-generate project-specific Claude Code skills from session patterns.

The shipped static skills (``check-prior-work`` / ``find-related-sessions``
/ ``recall-past-decisions``) teach Claude Code *how* to query the store.
They're project-agnostic. This module mines the local store for the
*workflows specific to one project* — "always run ``pytest tests/ -q``
after editing ``stackunderflow/``", "never ``pkill``, use graceful
SIGTERM first", "lint with ``ruff check --fix``" — and emits
``SKILL.md`` files Claude Code can pick up.

Hard guardrails (see ``.notes/specs/02-auto-generated-skills.md``):

* **Project-scoped by default.** :func:`synthesize_skills` requires an
  explicit scope — a single ``project`` slug or an explicit ``projects``
  allowlist. There is *no* implicit "mine every project" path anywhere
  in this module; the CLI never exposes one either.
* **Never packaged.** Generated skills land under
  ``<project>/.claude/skills/auto-*/SKILL.md`` (a user-project artifact,
  ``.gitignore``-d and excluded from the wheel). They are never written
  into the ``stackunderflow`` package tree.
* **Labelled.** Directory prefix ``auto-``, frontmatter
  ``auto_generated: true``, body opens with a generated-by marker.
* **Idempotent.** Re-running overwrites in place (back up the prior file
  to ``SKILL.md.bak`` first); the directory name is derived
  deterministically from the pattern signature, with a hash suffix on
  the rare slug collision so collisions are explicit, never silent.
* **Safe to re-run over a hand-authored skill.** Writing skips any
  target directory whose ``SKILL.md`` is *not* marked
  ``auto_generated: true`` — we never clobber user content.

Pure-regex synthesis only — no LLM call, no network. (An opt-in
``--use-llm`` refinement pass is a possible v2; see the spec's open
questions.)
"""

from __future__ import annotations

import hashlib
import json
import re
import shlex
import shutil
import sqlite3
from collections import Counter
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from stackunderflow.services.discovery import load_messages_for_project, parse_since

__all__ = [
    "SkillCandidate",
    "synthesize_skills",
    "render_skill_md",
    "write_skill_files",
    "list_generated_skills",
    "clean_generated_skills",
    "ALL_PATTERN_KINDS",
    "SKILL_DIR_PREFIX",
    "DEFAULT_MIN_OCCURRENCES",
    "DEFAULT_WINDOW",
]

# ── constants ───────────────────────────────────────────────────────────────

SKILL_DIR_PREFIX = "auto-"
DEFAULT_MIN_OCCURRENCES = 5
DEFAULT_WINDOW = "90d"  # frequency window — see spec open question #2

# Pattern kinds, in priority order (used to break ties when two detectors
# describe the same underlying command — the earlier kind wins).
ALL_PATTERN_KINDS: tuple[str, ...] = (
    "avoids-X",
    "never-touches-paths",
    "canonical-test-command",
    "always-runs-X-after-Y",
    "uses-tool-flag-combo",
)
_PATTERN_PRIORITY = {kind: i for i, kind in enumerate(ALL_PATTERN_KINDS)}

# Executables too generic to be worth a "this project does X" skill — used
# by the after-edit / flag-combo / test detectors to keep candidates clean.
_BORING_EXES = frozenset(
    {
        "cd", "pushd", "popd", "ls", "ll", "la", "cat", "bat", "pwd", "echo",
        "printf", "true", "false", ":", "export", "set", "unset", "source",
        ".", "which", "type", "command", "head", "tail", "less", "more",
        "clear", "history", "git", "sleep", "wait", "kill", "pkill", "killall",
        "pgrep", "jobs", "fg", "bg", "disown",
        "exit", "return", "read", "test", "[", "[[", "mkdir", "rmdir", "touch",
        "cp", "mv", "ln", "chmod", "chown", "find", "grep", "rg", "ag", "sed",
        "awk", "cut", "sort", "uniq", "wc", "tr", "tee", "xargs", "env",
        "open", "code", "vim", "nano", "emacs", "tmux", "screen", "ssh", "scp",
        "curl", "wget", "ping", "host", "dig", "nslookup", "ps", "top", "htop",
        "df", "du", "free", "uname", "whoami", "id", "date", "uptime",
    }
)

# A much smaller skip-set for the "user corrected this" detectors — pure
# navigation only. Things like ``pkill`` / ``rm`` / ``git push --force`` are
# *exactly* what gets corrected, so they must stay in scope here even though
# the workflow detectors above treat them as boring.
_NAV_EXES = frozenset({"cd", "pushd", "popd", "ls", "ll", "la", "pwd"})

# Negation / correction cues — a user message containing one of these,
# right after an assistant action, is treated as steering Claude away.
_NEGATION_PATTERNS = re.compile(
    r"\b("
    r"don'?t|do not|never|stop|instead|avoid|please don'?t|no,|nope|"
    r"not the|not that|don'?t use|don'?t edit|don'?t touch|don'?t run|"
    r"shouldn'?t|should not|won'?t|will not|cannot|can'?t|"
    r"undo|revert|roll ?back|that'?s wrong|wrong (file|approach|command)"
    r")\b",
    re.IGNORECASE,
)

# Test-runner recognition. exe → predicate over the parsed command.
_TEST_EXES = frozenset(
    {"pytest", "py.test", "tox", "nox", "jest", "vitest", "mocha", "ava",
     "phpunit", "rspec", "minitest"}
)

_PATH_LIKE = re.compile(r"[/\\]|\.\w{1,5}$|^\.+$")

_GENERATED_MARKER_PREFIX = "<!-- Generated by stackunderflow skills generate"

# ── data shape ──────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class SkillCandidate:
    """One mined workflow pattern, ready to render as a ``SKILL.md``.

    ``name`` doubles as the output directory name (``<out>/<name>/SKILL.md``)
    — it's deterministic from the pattern so a re-run overwrites in place.
    ``pattern_id`` is the hash of the normalized signature; it goes in the
    frontmatter for traceability and disambiguates the rare slug collision.
    ``normalized_command`` (internal) lets the merge step recognise that two
    detectors found the same underlying command.
    """

    pattern_id: str
    name: str
    description: str
    body: str
    evidence_count: int
    last_seen_ts: str
    pattern_kind: str
    project_slug: str | None = None
    example_session_ids: tuple[str, ...] = ()
    normalized_command: str | None = field(default=None, compare=False)

    def to_dict(self) -> dict[str, Any]:
        return {
            "pattern_id": self.pattern_id,
            "name": self.name,
            "description": self.description,
            "body": self.body,
            "evidence_count": self.evidence_count,
            "last_seen_ts": self.last_seen_ts,
            "pattern_kind": self.pattern_kind,
            "project_slug": self.project_slug,
            "example_session_ids": list(self.example_session_ids),
        }


# ── internal in-memory shapes ───────────────────────────────────────────────


@dataclass
class _ToolCall:
    name: str
    args: dict[str, Any]


@dataclass
class _Event:
    seq: int
    ts: str
    role: str
    text: str
    tool_calls: list[_ToolCall]
    is_user_text: bool  # a real user turn (not a tool_result echo)


@dataclass
class _Session:
    session_id: str
    project_slug: str
    last_ts: str
    events: list[_Event]


@dataclass
class _ParsedCmd:
    exe: str
    sub: str | None
    flags: list[str]          # flag *names*, "--tb=short" -> "--tb"
    positionals: list[str]
    raw: str

    @property
    def is_boring(self) -> bool:
        return self.exe in _BORING_EXES or not self.exe

    @property
    def normalized(self) -> str:
        """A stable signature for grouping: exe + meaningful positionals + flags."""
        parts = [self.exe]
        for p in self.positionals:
            if not _PATH_LIKE.search(p):
                parts.append(p)
        parts.extend(sorted(set(self.flags)))
        return " ".join(parts)


# ── raw payload parsing ─────────────────────────────────────────────────────


def _safe_json(raw: str | None) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw)
    except (json.JSONDecodeError, ValueError, TypeError):
        return None


def _tool_calls_from_raw(raw: Any) -> list[_ToolCall]:
    """Extract ``tool_use`` blocks from a verbatim provider payload.

    Handles the Anthropic/Claude shape (``message.content[]`` with
    ``type == "tool_use"``), a flat ``tool_calls`` list (some adapters),
    and the ``[{"name": ..., "input": {...}}]`` dict-list shape used in
    a few tests / normalized payloads. Anything unrecognised yields ``[]``.
    """
    out: list[_ToolCall] = []
    if not isinstance(raw, dict | list):
        return out

    def _add(name: Any, args: Any) -> None:
        if isinstance(name, str) and name:
            out.append(_ToolCall(name=name, args=args if isinstance(args, dict) else {}))

    candidates: list[Any] = []
    if isinstance(raw, dict):
        msg = raw.get("message")
        if isinstance(msg, dict) and isinstance(msg.get("content"), list):
            candidates = msg["content"]
        elif isinstance(raw.get("content"), list):
            candidates = raw["content"]
        if isinstance(raw.get("tool_calls"), list):
            candidates = candidates + raw["tool_calls"]
    elif isinstance(raw, list):
        candidates = raw

    for blk in candidates:
        if not isinstance(blk, dict):
            continue
        btype = blk.get("type")
        if btype in (None, "tool_use", "function", "tool_call"):
            name = blk.get("name") or blk.get("tool")
            args = blk.get("input") or blk.get("arguments") or blk.get("args") or {}
            if name:
                _add(name, args)
    return out


def _is_tool_result_payload(raw: Any) -> bool:
    """True if the message body is (only) tool_result echoes, not real text."""
    if not isinstance(raw, dict):
        return False
    msg = raw.get("message")
    body = msg.get("content") if isinstance(msg, dict) else raw.get("content")
    if not isinstance(body, list) or not body:
        return False
    saw_result = False
    for blk in body:
        if not isinstance(blk, dict):
            return False
        t = blk.get("type")
        if t == "tool_result":
            saw_result = True
        elif t == "text" and (blk.get("text") or "").strip():
            return False
        elif t not in (None, "tool_result"):
            return False
    return saw_result


def _split_command_segments(cmd: str) -> list[str]:
    """Split a shell command on ``&&`` / ``||`` / ``;`` / newlines.

    Pipes (``|``) are left intact — ``a | b`` is one logical command for
    our purposes (we only care about the head executable). Best-effort:
    we don't try to honour quoting around the separators.
    """
    parts = re.split(r"&&|\|\||;|\n", cmd)
    return [p.strip() for p in parts if p.strip()]


_ENV_ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
_LEADING_WRAPPERS = frozenset({"sudo", "time", "nice", "nohup", "env", "command", "exec", "xargs"})


def _parse_command(seg: str) -> _ParsedCmd | None:
    try:
        tokens = shlex.split(seg, comments=True)
    except ValueError:
        tokens = seg.split()
    # strip leading VAR=val and wrapper commands
    while tokens and (_ENV_ASSIGN.match(tokens[0]) or tokens[0] in _LEADING_WRAPPERS):
        tokens = tokens[1:]
    if not tokens:
        return None
    exe = tokens[0]
    if "/" in exe or "\\" in exe:
        exe = Path(exe).name
    rest = tokens[1:]

    # python -m pytest  ->  pytest
    if exe in ("python", "python3", "py") and len(rest) >= 2 and rest[0] == "-m":
        exe = rest[1]
        rest = rest[2:]
    # npx pytest -> treat the run target as the exe
    if exe == "npx" and rest and not rest[0].startswith("-"):
        exe = rest[0]
        rest = rest[1:]

    flags: list[str] = []
    positionals: list[str] = []
    for tok in rest:
        if tok.startswith("-") and tok != "-":
            name = tok.split("=", 1)[0]
            flags.append(name)
        else:
            positionals.append(tok)
    # ``sub`` = a real subcommand word ("check", "run", "test") — *not* a
    # path argument ("tests/", "main.py"). Keeping these distinct is what
    # lets the flag-combo signature match the after-edit signature for the
    # same command, so the dedup step can collapse them.
    sub = positionals[0] if positionals and not _PATH_LIKE.search(positionals[0]) else None
    return _ParsedCmd(exe=exe, sub=sub, flags=flags, positionals=positionals, raw=seg.strip())


def _parsed_commands(tc: _ToolCall) -> list[_ParsedCmd]:
    """All shell sub-commands implied by one Bash-ish tool call."""
    if tc.name not in ("Bash", "Shell", "shell", "run_command", "execute"):
        return []
    cmd = tc.args.get("command") or tc.args.get("cmd") or tc.args.get("script")
    if not isinstance(cmd, str) or not cmd.strip():
        return []
    out: list[_ParsedCmd] = []
    for seg in _split_command_segments(cmd):
        p = _parse_command(seg)
        if p is not None:
            out.append(p)
    return out


_EDIT_TOOLS = frozenset({"Edit", "Write", "MultiEdit", "NotebookEdit", "str_replace_editor", "create_file"})


def _edited_paths(tc: _ToolCall) -> list[str]:
    if tc.name not in _EDIT_TOOLS:
        return []
    out: list[str] = []
    for key in ("file_path", "path", "filename", "notebook_path", "target_file"):
        v = tc.args.get(key)
        if isinstance(v, str) and v.strip():
            out.append(v.strip())
    return out


def _is_test_command(p: _ParsedCmd) -> bool:
    if p.exe in _TEST_EXES:
        return True
    if p.exe in ("npm", "yarn", "pnpm", "bun"):
        if p.sub in ("test", "t"):
            return True
        run_targets = {"test", "tests", "test:unit", "jest", "vitest"}
        if p.sub == "run" and len(p.positionals) >= 2 and p.positionals[1] in run_targets:
            return True
    if p.exe == "cargo" and p.sub == "test":
        return True
    if p.exe == "go" and p.sub == "test":
        return True
    if p.exe in ("make", "just", "task") and any(pos in ("test", "tests", "check") for pos in p.positionals):
        return True
    if p.exe == "unittest":
        return True
    return False


# ── path / slug normalisation ───────────────────────────────────────────────


def _abbrev_home(path: str) -> str:
    try:
        home = str(Path.home())
    except (RuntimeError, OSError):
        home = ""
    if home and path.startswith(home + "/"):
        return "~/" + path[len(home) + 1 :]
    if home and path == home:
        return "~"
    return path


def _slugify(text: str, *, maxlen: int = 48) -> str:
    s = re.sub(r"[^A-Za-z0-9]+", "-", text.strip().lower())
    s = re.sub(r"-{2,}", "-", s).strip("-")
    if len(s) > maxlen:
        s = s[:maxlen].rstrip("-")
    return s or "pattern"


def _hash_signature(signature: str) -> str:
    return hashlib.sha256(signature.encode("utf-8")).hexdigest()[:16]


# ── loading ─────────────────────────────────────────────────────────────────


def _resolve_project_ids(conn: sqlite3.Connection, slug: str) -> list[int]:
    """A slug can map to >1 project row (one per provider) — return all."""
    rows = conn.execute("SELECT id FROM projects WHERE slug = ?", (slug,)).fetchall()
    return [int(r[0] if not hasattr(r, "keys") else r["id"]) for r in rows]


def _load_sessions(
    conn: sqlite3.Connection,
    *,
    project: str | None,
    projects: list[str] | None,
    since: str | None,
) -> list[_Session]:
    """Load + parse messages for the requested scope, grouped per session.

    Exactly one of ``project`` (single slug) or ``projects`` (allowlist of
    slugs) must be set. There is no "all projects" path.
    """
    if projects:
        slugs = list(dict.fromkeys(projects))  # de-dup, keep order
        cross_project = len(slugs) > 1
    elif project:
        slugs = [project]
        cross_project = False
    else:  # pragma: no cover - guarded by synthesize_skills
        raise ValueError("a scope is required: pass project= or projects=")

    pid_to_slug: dict[int, str] = {}
    for slug in slugs:
        for pid in _resolve_project_ids(conn, slug):
            pid_to_slug[pid] = slug
    if not pid_to_slug:
        return []

    by_session: dict[tuple[str, str], _Session] = {}
    for pid, slug in pid_to_slug.items():
        for row in load_messages_for_project(conn, pid, since=since):
            sid = row["session_id"]
            key = (slug, sid)
            sess = by_session.get(key)
            if sess is None:
                sess = _Session(session_id=sid, project_slug=slug, last_ts=row["timestamp"] or "", events=[])
                by_session[key] = sess
            raw = _safe_json(row["raw_json"])
            tcs = _tool_calls_from_raw(raw)
            role = row["role"] or ""
            text = row["content_text"] or ""
            is_user_text = (
                role == "user"
                and bool(text.strip())
                and not _is_tool_result_payload(raw)
            )
            sess.events.append(
                _Event(
                    seq=int(row["seq"] or 0),
                    ts=row["timestamp"] or "",
                    role=role,
                    text=text,
                    tool_calls=tcs,
                    is_user_text=is_user_text,
                )
            )
            ts = row["timestamp"] or ""
            if ts > sess.last_ts:
                sess.last_ts = ts

    sessions = list(by_session.values())
    for s in sessions:
        s.events.sort(key=lambda e: e.seq)
    # Stamp cross-project marker on the slug field of returned candidates by
    # zeroing the per-session slug only when genuinely cross-project — keep it
    # simple: callers (synthesize_skills) decide. Return as-is.
    _ = cross_project
    return sessions


def _scope_label(sessions_used: int, *, project: str | None, projects: list[str] | None) -> tuple[str, str | None]:
    """Return (``generated_from`` clause, ``project_slug`` for the candidate)."""
    if projects and len(dict.fromkeys(projects)) > 1:
        n = len(dict.fromkeys(projects))
        return f"{sessions_used} sessions across {n} projects", None
    slug = (projects[0] if projects else project) or "this project"
    return f"{sessions_used} sessions in {slug}", (slug if slug != "this project" else None)


# ── detectors ───────────────────────────────────────────────────────────────
#
# Each detector takes the per-session in-memory model + ``min_occurrences``
# and returns a list of candidates (0..N) of its own ``pattern_kind``. A
# candidate is only emitted when its session-count meets the threshold. The
# unit of "occurrence" is *distinct sessions exhibiting the pattern* — the
# same number that lands in ``evidence_count`` — so the threshold means the
# same thing for every detector.


def _top_session_ids(sid_to_ts: dict[str, str], k: int = 3) -> tuple[str, ...]:
    return tuple(sid for sid, _ in sorted(sid_to_ts.items(), key=lambda kv: kv[1], reverse=True)[:k])


def _detect_canonical_test_command(sessions: list[_Session], *, min_occurrences: int) -> list[SkillCandidate]:
    sessions_with_sig: dict[str, set[str]] = {}        # normalized sig -> session ids
    literal_counts: dict[str, Counter[str]] = {}       # sig -> Counter(raw command)
    last_seen: dict[str, dict[str, str]] = {}          # sig -> {sid: ts}
    for s in sessions:
        for ev in s.events:
            for tc in ev.tool_calls:
                for p in _parsed_commands(tc):
                    if not _is_test_command(p):
                        continue
                    sig = p.normalized
                    sessions_with_sig.setdefault(sig, set()).add(s.session_id)
                    literal_counts.setdefault(sig, Counter())[p.raw] += 1
                    seen_map = last_seen.setdefault(sig, {})
                    seen_map[s.session_id] = max(seen_map.get(s.session_id, ""), ev.ts)
    if not sessions_with_sig:
        return []
    best_sig = max(sessions_with_sig, key=lambda sig: (len(sessions_with_sig[sig]), sum(literal_counts[sig].values())))
    n = len(sessions_with_sig[best_sig])
    if n < min_occurrences:
        return []
    literal = literal_counts[best_sig].most_common(1)[0][0]
    examples = _top_session_ids(last_seen[best_sig])
    last_ts = max(last_seen[best_sig].values(), default="")
    title = f"Run this project's tests with `{literal}`"
    body = _build_body(
        title=title,
        explanation=(
            f"This project's test suite is invoked as:\n\n```bash\n{literal}\n```\n\n"
            f"Run it before claiming a task is complete. Learned from {n} sessions in this "
            f"project that used this invocation."
        ),
        action_hint=literal,
        examples=examples,
    )
    return [
        SkillCandidate(
            pattern_id=_hash_signature(f"canonical-test-command::{best_sig}"),
            name=f"{SKILL_DIR_PREFIX}canonical-test-command",
            description=(
                f"Triggers when running or verifying tests in this project. The canonical test "
                f"command is `{literal}` — use it rather than guessing a runner or scope."
            ),
            body=body,
            evidence_count=n,
            last_seen_ts=last_ts,
            pattern_kind="canonical-test-command",
            example_session_ids=examples,
            normalized_command=best_sig,
        )
    ]


def _detect_always_runs_after_edit(sessions: list[_Session], *, min_occurrences: int) -> list[SkillCandidate]:
    # Sessions that contain >=1 edit — the denominator for "reliably follows".
    edit_sessions: set[str] = set()
    sig_sessions: dict[str, set[str]] = {}
    literal_counts: dict[str, Counter[str]] = {}
    last_seen: dict[str, dict[str, str]] = {}
    edited_dirs: dict[str, Counter[str]] = {}
    for s in sessions:
        seen_edit = False
        dirs_this_session: list[str] = []
        per_sig_here: set[str] = set()
        for ev in s.events:
            for tc in ev.tool_calls:
                eps = _edited_paths(tc)
                if eps:
                    seen_edit = True
                    for ep in eps:
                        parent = _abbrev_home(str(Path(ep).parent))
                        dirs_this_session.append(parent)
                if not seen_edit:
                    continue
                for p in _parsed_commands(tc):
                    if p.is_boring:
                        continue
                    sig = p.normalized
                    per_sig_here.add(sig)
                    literal_counts.setdefault(sig, Counter())[p.raw] += 1
                    prev = last_seen.setdefault(sig, {}).get(s.session_id, "")
                    last_seen[sig][s.session_id] = max(prev, ev.ts)
        if seen_edit:
            edit_sessions.add(s.session_id)
        for sig in per_sig_here:
            sig_sessions.setdefault(sig, set()).add(s.session_id)
            for d in dirs_this_session:
                edited_dirs.setdefault(sig, Counter())[d] += 1

    if len(edit_sessions) < min_occurrences:
        return []
    ranked = sorted(sig_sessions.items(), key=lambda kv: len(kv[1]), reverse=True)
    out: list[SkillCandidate] = []
    for sig, sids in ranked:
        n = len(sids)
        if n < min_occurrences:
            continue
        # require it to *reliably* follow edits, not just appear once
        if n / max(1, len(edit_sessions)) < 0.5:
            continue
        literal = literal_counts[sig].most_common(1)[0][0]
        dir_hint = ""
        if sig in edited_dirs and edited_dirs[sig]:
            top_dir, _ = edited_dirs[sig].most_common(1)[0]
            if top_dir and top_dir not in (".", "~"):
                dir_hint = f" in `{top_dir}/`"
        examples = _top_session_ids(last_seen[sig])
        last_ts = max(last_seen[sig].values(), default="")
        title = f"Run `{literal}` after editing files{dir_hint}"
        body = _build_body(
            title=title,
            explanation=(
                f"After editing files{dir_hint or ' in this project'}, run:\n\n```bash\n{literal}\n```\n\n"
                f"before claiming the task is complete. Learned from {n} of {len(edit_sessions)} "
                f"sessions that edited files and then ran this."
            ),
            action_hint=literal,
            examples=examples,
        )
        out.append(
            SkillCandidate(
                pattern_id=_hash_signature(f"always-runs-after-edit::{sig}"),
                name=f"{SKILL_DIR_PREFIX}run-{_slugify(sig)}-after-edits",
                description=(
                    f"Triggers after editing files{dir_hint or ' in this project'}. Run `{literal}` to "
                    f"verify the change before reporting the task done."
                ),
                body=body,
                evidence_count=n,
                last_seen_ts=last_ts,
                pattern_kind="always-runs-X-after-Y",
                example_session_ids=examples,
                normalized_command=sig,
            )
        )
        if len(out) >= 3:
            break
    return out


def _detect_uses_tool_flag_combo(sessions: list[_Session], *, min_occurrences: int) -> list[SkillCandidate]:
    combo_sessions: dict[tuple[str, str, tuple[str, ...]], set[str]] = {}
    combo_raw: dict[tuple[str, str, tuple[str, ...]], Counter[str]] = {}
    combo_count: Counter[tuple[str, str, tuple[str, ...]]] = Counter()
    last_seen: dict[tuple[str, str, tuple[str, ...]], dict[str, str]] = {}
    for s in sessions:
        for ev in s.events:
            for tc in ev.tool_calls:
                for p in _parsed_commands(tc):
                    if p.is_boring or not p.flags:
                        continue
                    key = (p.exe, p.sub or "", tuple(sorted(set(p.flags))))
                    combo_sessions.setdefault(key, set()).add(s.session_id)
                    combo_raw.setdefault(key, Counter())[p.raw] += 1
                    combo_count[key] += 1
                    prev = last_seen.setdefault(key, {}).get(s.session_id, "")
                    last_seen[key][s.session_id] = max(prev, ev.ts)
    ranked = sorted(combo_sessions.items(), key=lambda kv: (len(kv[1]), combo_count[kv[0]]), reverse=True)
    out: list[SkillCandidate] = []
    for key, sids in ranked:
        n = len(sids)
        if n < min_occurrences:
            continue
        exe, sub, flagtup = key
        base = f"{exe} {sub}".strip()
        literal = combo_raw[key].most_common(1)[0][0]
        flags_str = " ".join(flagtup)
        examples = _top_session_ids(last_seen[key])
        last_ts = max(last_seen[key].values(), default="")
        title = f"Use `{literal}` — keep the `{flags_str}` flag(s)"
        body = _build_body(
            title=title,
            explanation=(
                f"When you run `{base}` in this project, include the `{flags_str}` flag(s):\n\n"
                f"```bash\n{literal}\n```\n\nLearned from {n} sessions ({combo_count[key]} invocations) "
                f"that used this combination."
            ),
            action_hint=literal,
            examples=examples,
        )
        out.append(
            SkillCandidate(
                pattern_id=_hash_signature(f"flag-combo::{exe}::{sub}::{','.join(flagtup)}"),
                name=f"{SKILL_DIR_PREFIX}flags-{_slugify(base)}",
                description=(
                    f"Triggers when running `{base}` in this project. Use `{literal}` — the "
                    f"`{flags_str}` flag(s) are the established convention here."
                ),
                body=body,
                evidence_count=n,
                last_seen_ts=last_ts,
                pattern_kind="uses-tool-flag-combo",
                example_session_ids=examples,
                normalized_command=" ".join([exe, *(p for p in [sub] if p), *flagtup]),
            )
        )
        if len(out) >= 3:
            break
    return out


def _recent_assistant_tool_calls(events: list[_Event], upto_idx: int, *, lookback: int = 3) -> list[_ToolCall]:
    out: list[_ToolCall] = []
    i = upto_idx - 1
    seen_assistant = 0
    while i >= 0 and seen_assistant < lookback:
        ev = events[i]
        if ev.tool_calls:
            out.extend(ev.tool_calls)
            seen_assistant += 1
        i -= 1
    return out


def _detect_avoids_command(sessions: list[_Session], *, min_occurrences: int) -> list[SkillCandidate]:
    exe_sessions: dict[str, set[str]] = {}
    exe_examples: dict[str, dict[str, str]] = {}
    exe_count: Counter[str] = Counter()
    for s in sessions:
        for idx, ev in enumerate(s.events):
            if not ev.is_user_text or not _NEGATION_PATTERNS.search(ev.text):
                continue
            recent = _recent_assistant_tool_calls(s.events, idx)
            lowered = ev.text.lower()
            for tc in recent:
                for p in _parsed_commands(tc):
                    if not p.exe or p.exe in _NAV_EXES:
                        continue
                    if re.search(rf"\b{re.escape(p.exe)}\b", lowered):
                        exe_sessions.setdefault(p.exe, set()).add(s.session_id)
                        exe_count[p.exe] += 1
                        exe_examples.setdefault(p.exe, {})[s.session_id] = max(
                            exe_examples.get(p.exe, {}).get(s.session_id, ""), ev.ts
                        )
    out: list[SkillCandidate] = []
    for exe, sids in sorted(exe_sessions.items(), key=lambda kv: len(kv[1]), reverse=True):
        n = len(sids)
        if n < min_occurrences:
            continue
        examples = _top_session_ids(exe_examples[exe])
        last_ts = max(exe_examples[exe].values(), default="")
        title = f"Avoid `{exe}` in this project"
        body = _build_body(
            title=title,
            explanation=(
                f"Across {n} sessions the user has steered Claude away from running `{exe}`. "
                f"Don't reach for it by default — check `CLAUDE.md` for the preferred approach, "
                f"or ask the user before using `{exe}`."
            ),
            action_hint=None,
            examples=examples,
        )
        out.append(
            SkillCandidate(
                pattern_id=_hash_signature(f"avoids::{exe}"),
                name=f"{SKILL_DIR_PREFIX}avoid-{_slugify(exe)}",
                description=(
                    f"Triggers when about to run `{exe}`. The user has repeatedly corrected this in "
                    f"this project — prefer the established alternative or ask first."
                ),
                body=body,
                evidence_count=n,
                last_seen_ts=last_ts,
                pattern_kind="avoids-X",
                example_session_ids=examples,
                normalized_command=exe,
            )
        )
        if len(out) >= 3:
            break
    return out


def _detect_never_touches_paths(sessions: list[_Session], *, min_occurrences: int) -> list[SkillCandidate]:
    path_sessions: dict[str, set[str]] = {}
    path_examples: dict[str, dict[str, str]] = {}
    path_count: Counter[str] = Counter()
    for s in sessions:
        for idx, ev in enumerate(s.events):
            if not ev.is_user_text or not _NEGATION_PATTERNS.search(ev.text):
                continue
            recent = _recent_assistant_tool_calls(s.events, idx)
            lowered = ev.text.lower()
            for tc in recent:
                for ep in _edited_paths(tc):
                    norm = _abbrev_home(ep)
                    base = Path(ep).name
                    if not base:
                        continue
                    if base.lower() in lowered or norm.lower() in lowered or ep.lower() in lowered:
                        path_sessions.setdefault(norm, set()).add(s.session_id)
                        path_count[norm] += 1
                        path_examples.setdefault(norm, {})[s.session_id] = max(
                            path_examples.get(norm, {}).get(s.session_id, ""), ev.ts
                        )
    out: list[SkillCandidate] = []
    for path, sids in sorted(path_sessions.items(), key=lambda kv: len(kv[1]), reverse=True):
        n = len(sids)
        if n < min_occurrences:
            continue
        examples = _top_session_ids(path_examples[path])
        last_ts = max(path_examples[path].values(), default="")
        title = f"Never modify `{path}`"
        body = _build_body(
            title=title,
            explanation=(
                f"Across {n} sessions the user has corrected Claude away from editing `{path}`. "
                f"Treat it as off-limits — if a change seems to require touching it, stop and ask, "
                f"or look for the right file (tests typically use a temp copy / fixture)."
            ),
            action_hint=None,
            examples=examples,
        )
        out.append(
            SkillCandidate(
                pattern_id=_hash_signature(f"never-touches::{path}"),
                name=f"{SKILL_DIR_PREFIX}never-touch-{_slugify(Path(path).name)}",
                description=(
                    f"Triggers when about to edit `{path}` (or anything that resolves to it). The "
                    f"user has repeatedly steered Claude away from modifying it in this project."
                ),
                body=body,
                evidence_count=n,
                last_seen_ts=last_ts,
                pattern_kind="never-touches-paths",
                example_session_ids=examples,
                normalized_command=None,
            )
        )
        if len(out) >= 3:
            break
    return out


_DETECTORS: dict[str, Any] = {
    "canonical-test-command": _detect_canonical_test_command,
    "always-runs-X-after-Y": _detect_always_runs_after_edit,
    "uses-tool-flag-combo": _detect_uses_tool_flag_combo,
    "avoids-X": _detect_avoids_command,
    "never-touches-paths": _detect_never_touches_paths,
}


# ── body rendering ──────────────────────────────────────────────────────────


def _build_body(*, title: str, explanation: str, action_hint: str | None, examples: tuple[str, ...]) -> str:
    lines = [f"# {title}", "", explanation.strip(), "", "## Evidence", ""]
    if examples:
        lines.append("Most-recent example sessions: " + ", ".join(examples))
    else:
        lines.append("Most-recent example sessions: (none recorded)")
    lines.append("")
    if action_hint:
        lines.append(
            f"(view via `stackunderflow find-sessions-touching-file <path>` or "
            f"`stackunderflow search-past-decisions {action_hint!r} --project this`)"
        )
    else:
        lines.append(
            "(view via `stackunderflow search-past-decisions \"<term>\" --project this`)"
        )
    return "\n".join(lines).rstrip() + "\n"


def render_skill_md(candidate: SkillCandidate, *, generated_at: datetime | None = None) -> str:
    """Render a full ``SKILL.md`` (frontmatter + body) for ``candidate``.

    ``generated_at`` defaults to "now" (UTC); pass an explicit value so a
    whole ``generate`` run shares one timestamp (and tests can pin it).
    """
    ts = (generated_at or datetime.now(UTC)).astimezone(UTC).replace(microsecond=0).isoformat()
    n = candidate.evidence_count
    if candidate.project_slug:
        generated_from = f"{n} sessions in {candidate.project_slug}"
    else:
        generated_from = f"{n} sessions"
    fm = [
        "---",
        f"name: {candidate.name}",
        f"description: {candidate.description}",
        "auto_generated: true",
        f"generated_at: {ts}",
        f"generated_from: {generated_from}",
        f"pattern_kind: {candidate.pattern_kind}",
        f"pattern_id: {candidate.pattern_id}",
        f"evidence_count: {n}",
        "---",
        "",
        f"<!-- Generated by stackunderflow skills generate at {ts} from {n} sessions"
        " — do not edit manually; regenerate to update -->",
        "",
        "",
    ]
    return "\n".join(fm) + candidate.body


# ── public synthesis API ────────────────────────────────────────────────────


def synthesize_skills(
    conn: sqlite3.Connection,
    *,
    project: str | None = None,
    projects: list[str] | None = None,
    min_occurrences: int = DEFAULT_MIN_OCCURRENCES,
    pattern_kinds: list[str] | None = None,
    since: str | None = DEFAULT_WINDOW,
) -> list[SkillCandidate]:
    """Mine ``messages`` for repeated workflow patterns and return candidates.

    Scope is *mandatory and explicit*: pass either ``project`` (one slug)
    or ``projects`` (an explicit allowlist of slugs). There is no implicit
    "all projects" mode — calling with neither raises ``ValueError``. This
    is the structural guarantee behind the spec's project-scoped-only rule.

    Parameters
    ----------
    conn:
        Main store connection.
    project:
        A single ``projects.slug`` to scope to.
    projects:
        An explicit list of slugs (cross-project mining). When this has
        more than one entry, candidates come back with ``project_slug=None``
        (the pattern spans projects).
    min_occurrences:
        A pattern must show up in at least this many *distinct sessions* to
        become a candidate. Default :data:`DEFAULT_MIN_OCCURRENCES`.
    pattern_kinds:
        Restrict to these detector kinds (subset of
        :data:`ALL_PATTERN_KINDS`). Default: all.
    since:
        Frequency window — only sessions with activity newer than this are
        considered. Accepts ``"90d"`` / ``"1w"`` / ISO / ``None`` (no
        bound). Default :data:`DEFAULT_WINDOW`.

    Returns
    -------
    A list of :class:`SkillCandidate`, sorted by ``evidence_count``
    descending, de-duplicated so two detectors describing the same
    underlying command yield a single (highest-priority) candidate.
    """
    if not project and not projects:
        raise ValueError(
            "synthesize_skills requires a scope: pass project=<slug> or "
            "projects=[<slug>, ...]. Mining every project is intentionally "
            "not supported."
        )
    if min_occurrences < 1:
        raise ValueError("min_occurrences must be >= 1")
    kinds = list(pattern_kinds) if pattern_kinds else list(ALL_PATTERN_KINDS)
    unknown = set(kinds) - set(ALL_PATTERN_KINDS)
    if unknown:
        raise ValueError(f"unknown pattern kind(s): {sorted(unknown)}")
    # normalise/validate ``since`` early so a bad value fails fast
    parse_since(since)

    sessions = _load_sessions(conn, project=project, projects=projects, since=since)
    if not sessions:
        return []
    used_session_ids = {s.session_id for s in sessions}
    generated_from, candidate_slug = _scope_label(len(used_session_ids), project=project, projects=projects)
    _ = generated_from  # render computes its own from candidate fields

    raw_candidates: list[SkillCandidate] = []
    for kind in kinds:
        detector = _DETECTORS[kind]
        for cand in detector(sessions, min_occurrences=min_occurrences):
            # stamp the resolved scope slug onto the candidate
            raw_candidates.append(
                SkillCandidate(
                    pattern_id=cand.pattern_id,
                    name=cand.name,
                    description=cand.description,
                    body=cand.body,
                    evidence_count=cand.evidence_count,
                    last_seen_ts=cand.last_seen_ts,
                    pattern_kind=cand.pattern_kind,
                    project_slug=candidate_slug,
                    example_session_ids=cand.example_session_ids,
                    normalized_command=cand.normalized_command,
                )
            )
    return _merge_and_dedup(raw_candidates)


def _merge_and_dedup(candidates: list[SkillCandidate]) -> list[SkillCandidate]:
    """Collapse equivalent candidates; sort by evidence desc.

    Two collapse rules:

    * **Same directory name** — keep the one with the higher
      ``evidence_count`` (then higher pattern priority).
    * **Same underlying normalized command across different kinds** — keep
      the highest-priority kind (``avoids`` / ``never-touches`` / canonical
      test > after-edit > flag-combo). This is what makes "the test command
      detector and the after-edit detector both fired on ``pytest -q``"
      resolve to one skill, not two.
    """
    def _rank(c: SkillCandidate) -> tuple[int, int]:
        return (c.evidence_count, -_PATTERN_PRIORITY.get(c.pattern_kind, 99))

    by_name: dict[str, SkillCandidate] = {}
    for c in candidates:
        cur = by_name.get(c.name)
        if cur is None or _rank(c) > _rank(cur):
            by_name[c.name] = c

    by_cmd: dict[str, SkillCandidate] = {}
    leftovers: list[SkillCandidate] = []
    for c in by_name.values():
        if not c.normalized_command:
            leftovers.append(c)
            continue
        cur = by_cmd.get(c.normalized_command)
        if cur is None:
            by_cmd[c.normalized_command] = c
            continue
        # prefer higher pattern priority (lower index); tie-break on evidence
        if (_PATTERN_PRIORITY.get(c.pattern_kind, 99), -c.evidence_count) < (
            _PATTERN_PRIORITY.get(cur.pattern_kind, 99),
            -cur.evidence_count,
        ):
            by_cmd[c.normalized_command] = c

    merged = leftovers + list(by_cmd.values())
    merged.sort(key=lambda c: (c.evidence_count, -_PATTERN_PRIORITY.get(c.pattern_kind, 99)), reverse=True)
    return merged


# ── filesystem: write / list / clean ────────────────────────────────────────


def _frontmatter(text: str) -> dict[str, str]:
    """Parse the leading ``--- ... ---`` block into a flat ``key: value`` dict.

    Tiny, dependency-free; only the scalar subset our SKILL.md files use.
    Returns ``{}`` if the text doesn't start with a frontmatter block.
    """
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return {}
    try:
        end = lines.index("---", 1)
    except ValueError:
        return {}
    out: dict[str, str] = {}
    for line in lines[1:end]:
        s = line.strip()
        if not s or s.startswith("#") or ":" not in s:
            continue
        k, _, v = s.partition(":")
        out[k.strip()] = v.strip()
    return out


def _is_generated_skill_md(path: Path) -> dict[str, str] | None:
    """Return parsed frontmatter iff ``path`` is one of *our* generated files."""
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None
    fm = _frontmatter(text)
    if str(fm.get("auto_generated", "")).strip().lower() == "true":
        return fm
    if _GENERATED_MARKER_PREFIX in text and fm:
        return fm
    return None


@dataclass
class WriteResult:
    name: str
    path: Path
    action: str  # created | updated | unchanged | skipped-user-authored | would-create | would-update


def write_skill_files(
    candidates: list[SkillCandidate],
    out_dir: str | Path,
    *,
    generated_at: datetime | None = None,
    dry_run: bool = False,
) -> list[WriteResult]:
    """Render + write each candidate to ``<out_dir>/<name>/SKILL.md``.

    Guarantees:

    * Creates ``out_dir`` (and per-skill dirs) as needed.
    * Never overwrites a ``SKILL.md`` that isn't marked
      ``auto_generated: true`` — that's a user-authored skill; we
      ``skipped-user-authored`` it.
    * Before overwriting one of our own files, copies it to
      ``SKILL.md.bak``.
    * On the rare case where two distinct patterns slug to the same
      directory name, the second gets a ``-<hash6>`` suffix so the
      collision is explicit rather than silently clobbering.
    * ``dry_run`` computes the actions without touching the filesystem.

    Returns one :class:`WriteResult` per candidate, in input order.
    """
    out_dir = Path(out_dir)
    gen_at = generated_at or datetime.now(UTC)
    results: list[WriteResult] = []
    used_dirs: set[str] = set()
    for cand in candidates:
        name = cand.name
        target_dir = out_dir / name
        # collision: a *different* pattern already claimed this dir name
        if name in used_dirs or (
            target_dir.exists()
            and (existing := _is_generated_skill_md(target_dir / "SKILL.md")) is not None
            and existing.get("pattern_id") not in (None, "", cand.pattern_id)
        ):
            name = f"{cand.name}-{cand.pattern_id[:6]}"
            target_dir = out_dir / name
        used_dirs.add(name)
        skill_md = target_dir / "SKILL.md"
        rendered = render_skill_md(cand, generated_at=gen_at)

        if skill_md.exists():
            gen_fm = _is_generated_skill_md(skill_md)
            if gen_fm is None:
                results.append(WriteResult(name=name, path=skill_md, action="skipped-user-authored"))
                continue
            prior = skill_md.read_text(encoding="utf-8")
            # ignore the generated_at line when deciding "unchanged"
            if _strip_volatile(prior) == _strip_volatile(rendered):
                results.append(WriteResult(name=name, path=skill_md, action="unchanged"))
                continue
            if dry_run:
                results.append(WriteResult(name=name, path=skill_md, action="would-update"))
                continue
            shutil.copy2(skill_md, skill_md.with_suffix(".md.bak"))
            target_dir.mkdir(parents=True, exist_ok=True)
            skill_md.write_text(rendered, encoding="utf-8")
            results.append(WriteResult(name=name, path=skill_md, action="updated"))
        else:
            if dry_run:
                results.append(WriteResult(name=name, path=skill_md, action="would-create"))
                continue
            target_dir.mkdir(parents=True, exist_ok=True)
            skill_md.write_text(rendered, encoding="utf-8")
            results.append(WriteResult(name=name, path=skill_md, action="created"))
    return results


_VOLATILE_LINE = re.compile(r"^(generated_at:|<!-- Generated by stackunderflow skills generate at ).*", re.MULTILINE)


def _strip_volatile(text: str) -> str:
    return _VOLATILE_LINE.sub("<volatile>", text)


def list_generated_skills(skills_dir: str | Path) -> list[dict[str, Any]]:
    """Introspect ``<skills_dir>/auto-*/`` for *our* generated skills.

    Only directories whose ``SKILL.md`` is marked ``auto_generated: true``
    are reported — a hand-authored ``auto-foo/`` won't show up.
    """
    skills_dir = Path(skills_dir)
    if not skills_dir.is_dir():
        return []
    out: list[dict[str, Any]] = []
    for child in sorted(skills_dir.iterdir()):
        if not child.is_dir() or not child.name.startswith(SKILL_DIR_PREFIX):
            continue
        skill_md = child / "SKILL.md"
        fm = _is_generated_skill_md(skill_md)
        if fm is None:
            continue
        try:
            ev = int(fm.get("evidence_count", "0"))
        except ValueError:
            ev = 0
        out.append(
            {
                "name": fm.get("name", child.name),
                "path": str(skill_md),
                "pattern_kind": fm.get("pattern_kind", ""),
                "pattern_id": fm.get("pattern_id", ""),
                "evidence_count": ev,
                "generated_at": fm.get("generated_at", ""),
                "generated_from": fm.get("generated_from", ""),
                "description": fm.get("description", ""),
            }
        )
    return out


def clean_generated_skills(
    skills_dir: str | Path,
    *,
    older_than: str | None = None,
    dry_run: bool = False,
) -> list[Path]:
    """Remove generated skill directories under ``skills_dir``.

    Only ``auto-*/`` directories whose ``SKILL.md`` is marked
    ``auto_generated: true`` are eligible — a user's hand-written skill is
    never removed, nor is a non-``auto-`` directory.

    ``older_than`` (``"30d"`` / ``"2w"`` / ISO / ``None``) keeps anything
    generated within that window; ``None`` removes all eligible dirs.
    ``dry_run`` returns what *would* be removed without deleting.

    Returns the list of removed (or would-be-removed) directory paths.
    """
    skills_dir = Path(skills_dir)
    if not skills_dir.is_dir():
        return []
    cutoff = parse_since(older_than) if older_than else None
    removed: list[Path] = []
    for child in sorted(skills_dir.iterdir()):
        if not child.is_dir() or not child.name.startswith(SKILL_DIR_PREFIX):
            continue
        fm = _is_generated_skill_md(child / "SKILL.md")
        if fm is None:
            continue
        if cutoff is not None:
            gen_at = (fm.get("generated_at") or "").strip()
            # keep if generated within the window (>= cutoff); only remove older
            if gen_at and gen_at >= cutoff:
                continue
            if not gen_at:
                # no timestamp to judge by — be conservative, keep it
                continue
        removed.append(child)
        if not dry_run:
            shutil.rmtree(child)
    return removed
