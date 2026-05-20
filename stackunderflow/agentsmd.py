"""``stackunderflow guide`` — install the agent-discovery snippet (Move 4).

The CLI is StackUnderflow's agent-facing surface — the ``memory`` commands. For
an agent to *use* it, the agent first has to know it exists. This module writes
a small, marked, idempotent block into the agent instruction file — ``CLAUDE.md``
for Claude Code, ``AGENTS.md`` for Codex — teaching it that the ``memory``
commands are there and when to reach for each. It is how the CLI recovers the
one thing an MCP server got for free: discovery.

The block lives between two HTML-comment markers::

    <!-- stackunderflow:guide:start -->
    ...snippet...
    <!-- stackunderflow:guide:end -->

so the installer is **idempotent and convergent**, modelled on the hooks
installer (``hooks/_install.py``):

* re-running ``install`` replaces the block in place — never a second copy;
* nothing outside the markers is touched;
* a timestamped backup (``<name>.bak.<utc-ts>``) is written before any
  mutation — never on a no-op, never under ``--dry-run``;
* the file itself is never deleted (``uninstall`` strips the block and leaves
  the rest, even if the rest is now empty).

Scope:

* ``project`` (default) — ``./CLAUDE.md`` *and* ``./AGENTS.md`` in the cwd's git
  root, so both Claude Code and Codex pick the snippet up.
* ``user`` — ``~/.claude/CLAUDE.md``.

The CLI command in ``cli.py`` is a thin wrapper over ``install`` / ``uninstall``
/ ``status`` here.
"""

from __future__ import annotations

import logging
import os
import re
import tempfile
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

logger = logging.getLogger("stackunderflow.agentsmd")

_VALID_SCOPES = ("project", "user")

# ── the snippet ─────────────────────────────────────────────────────────────

GUIDE_START = "<!-- stackunderflow:guide:start -->"
GUIDE_END = "<!-- stackunderflow:guide:end -->"

# ~15 lines naming the `memory` commands (Move 1), when to reach for each, and
# the `--json` contract (Move 2). Kept deliberately short — it is a pointer
# into the CLI, not documentation.
_GUIDE_BODY = """\
## StackUnderflow — query your past coding sessions

This machine indexes every past AI coding session locally with StackUnderflow.
Before re-deriving something, check whether the answer is already recorded:

- `stackunderflow memory file <path>` — a file's history: past edits, failure
  modes, and sessions that touched it. Worth a look before a non-trivial edit.
- `stackunderflow memory decisions "<topic>"` — past decisions on a topic.
- `stackunderflow memory worked "<action>"` — past sessions where an action
  succeeded, with evidence.
- `stackunderflow memory sessions` — recent sessions in this project.
- `stackunderflow memory ask "<question>"` — natural-language query over history.

Pass `--json` for a stable, token-bounded envelope (`schema:
stackunderflow.memory/1`) meant for programmatic use. Every query is local and
read-only — nothing leaves the machine."""


def render_block() -> str:
    """The exact marked block ``install`` writes — markers plus snippet body."""
    return f"{GUIDE_START}\n{_GUIDE_BODY}\n{GUIDE_END}"


# A well-formed block: the start marker, anything (non-greedy), the end marker.
_BLOCK_RE = re.compile(
    re.escape(GUIDE_START) + r".*?" + re.escape(GUIDE_END),
    re.DOTALL,
)


# ── reports ─────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class GuideFileResult:
    """What ``install`` / ``uninstall`` did to one target file."""

    path: str
    existed: bool
    created: bool
    changed: bool
    backup_path: str | None
    action: str  # "installed" | "updated" | "removed" | "unchanged" | "absent"

    def to_dict(self) -> dict:
        return {
            "path": self.path,
            "existed": self.existed,
            "created": self.created,
            "changed": self.changed,
            "backup_path": self.backup_path,
            "action": self.action,
        }


@dataclass(frozen=True)
class GuideReport:
    """The outcome of one ``install`` / ``uninstall`` call across its target files."""

    scope: str
    operation: str  # "install" | "uninstall"
    dry_run: bool
    files: list[GuideFileResult] = field(default_factory=list)

    @property
    def changed(self) -> bool:
        return any(f.changed for f in self.files)

    def to_dict(self) -> dict:
        return {
            "scope": self.scope,
            "operation": self.operation,
            "dry_run": self.dry_run,
            "changed": self.changed,
            "files": [f.to_dict() for f in self.files],
        }


# ── path resolution ─────────────────────────────────────────────────────────


def _git_root(start: Path) -> Path:
    """Nearest ancestor of *start* (inclusive) containing ``.git``; else *start*.

    A standalone copy of ``hooks/_install._git_root`` — the guide installer is
    not a hook, so it does not depend on the hooks package.
    """
    start = start.resolve()
    for candidate in (start, *start.parents):
        if (candidate / ".git").exists():
            return candidate
    return start


def target_paths(scope: str, *, cwd: Path | None = None) -> list[Path]:
    """The instruction file(s) *scope* governs.

    ``project`` → ``<git-root-of-cwd>/CLAUDE.md`` *and* ``.../AGENTS.md``.
    ``user``    → ``~/.claude/CLAUDE.md``.
    """
    if scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES}, got {scope!r}")
    if scope == "user":
        return [Path.home() / ".claude" / "CLAUDE.md"]
    base = _git_root(cwd if cwd is not None else Path.cwd())
    return [base / "CLAUDE.md", base / "AGENTS.md"]


# ── file IO ─────────────────────────────────────────────────────────────────


def _read_text(path: Path) -> str:
    """Read *path* as UTF-8 text.

    Raises ``ValueError`` if it exists but is not decodable — we refuse to
    mutate a file we cannot safely round-trip, mirroring the hooks installer's
    stance on non-JSON ``settings.json``.
    """
    try:
        return path.read_text(encoding="utf-8")
    except UnicodeDecodeError as exc:
        raise ValueError(f"{path} is not UTF-8 text ({exc}); fix or remove it before installing the guide") from exc


def _backup_path_for(path: Path) -> Path:
    ts = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    return path.with_name(f"{path.name}.bak.{ts}")


def _backup(path: Path) -> Path:
    """Copy *path* to ``<path>.bak.<utc-ts>`` and return the backup path.

    A numeric suffix is appended on a sub-second timestamp collision so a
    backup is never overwritten.
    """
    dest = _backup_path_for(path)
    n = 1
    while dest.exists():
        dest = dest.with_name(f"{dest.name}.{n}")
        n += 1
    dest.write_bytes(path.read_bytes())
    return dest


def _atomic_write_text(path: Path, text: str) -> None:
    """Write *text* to *path* atomically (temp file + rename)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fh.write(text)
        os.replace(tmp_name, path)
    except BaseException:
        try:
            os.unlink(tmp_name)
        except OSError:
            logger.debug("could not remove temp file %s", tmp_name)
        raise


# ── block surgery (pure str→str) ────────────────────────────────────────────


def _strip_block(text: str) -> str:
    """Remove our marked block from *text*, leaving everything else intact.

    Convergent: a well-formed block is removed whole; a half-written file with
    only one orphan marker has that stray line dropped too, so a re-install
    always lands clean. Trailing whitespace is left for the composer to tidy.
    """
    cleaned = _BLOCK_RE.sub("", text)
    if GUIDE_START not in cleaned and GUIDE_END not in cleaned:
        return cleaned
    # Orphan marker line(s) from a malformed prior state — drop them.
    kept = [ln for ln in cleaned.splitlines() if ln.strip() not in (GUIDE_START, GUIDE_END)]
    return "\n".join(kept)


def _compose_install(original: str) -> str:
    """The file content ``install`` wants: *original* minus any old block, plus a fresh one."""
    rest = _strip_block(original).rstrip()
    block = render_block()
    if rest:
        return f"{rest}\n\n{block}\n"
    return f"{block}\n"


def _compose_uninstall(original: str) -> str:
    """The file content ``uninstall`` wants: *original* minus our block."""
    rest = _strip_block(original).rstrip()
    return f"{rest}\n" if rest else ""


# ── per-file operations ─────────────────────────────────────────────────────


def _install_one(path: Path, *, dry_run: bool) -> GuideFileResult:
    existed = path.exists()
    original = _read_text(path) if existed else ""
    had_block = _BLOCK_RE.search(original) is not None
    new = _compose_install(original)
    changed = new != original

    backup: str | None = None
    if changed and not dry_run:
        if existed:
            backup = str(_backup(path))
        _atomic_write_text(path, new)

    if not changed:
        action = "unchanged"
    elif had_block:
        action = "updated"
    else:
        action = "installed"
    return GuideFileResult(
        path=str(path),
        existed=existed,
        created=changed and not existed and not dry_run,
        changed=changed,
        backup_path=backup,
        action=action,
    )


def _uninstall_one(path: Path, *, dry_run: bool) -> GuideFileResult:
    if not path.exists():
        return GuideFileResult(
            path=str(path),
            existed=False,
            created=False,
            changed=False,
            backup_path=None,
            action="absent",
        )
    original = _read_text(path)
    new = _compose_uninstall(original)
    changed = new != original

    backup: str | None = None
    if changed and not dry_run:
        backup = str(_backup(path))
        _atomic_write_text(path, new)

    return GuideFileResult(
        path=str(path),
        existed=True,
        created=False,
        changed=changed,
        backup_path=backup,
        action="removed" if changed else "unchanged",
    )


def _status_one(path: Path) -> dict:
    entry: dict = {"path": str(path), "exists": path.exists(), "installed": False, "up_to_date": False}
    if not path.exists():
        return entry
    try:
        text = _read_text(path)
    except ValueError:
        entry["valid"] = False
        return entry
    match = _BLOCK_RE.search(text)
    entry["installed"] = match is not None
    entry["up_to_date"] = match is not None and match.group(0).strip() == render_block().strip()
    return entry


# ── public API ──────────────────────────────────────────────────────────────


def install(scope: str = "project", *, dry_run: bool = False, cwd: Path | None = None) -> GuideReport:
    """Write the discovery snippet into *scope*'s instruction file(s).

    Idempotent and convergent — see the module docstring. ``--dry-run`` computes
    the result and writes nothing.
    """
    if scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES}, got {scope!r}")
    files = [_install_one(p, dry_run=dry_run) for p in target_paths(scope, cwd=cwd)]
    return GuideReport(scope=scope, operation="install", dry_run=dry_run, files=files)


def uninstall(scope: str = "project", *, dry_run: bool = False, cwd: Path | None = None) -> GuideReport:
    """Strip the discovery snippet from *scope*'s instruction file(s).

    Removes only our marked block; never deletes the file or touches anything
    outside the markers. A backup is written first iff a file actually changes.
    """
    if scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES}, got {scope!r}")
    files = [_uninstall_one(p, dry_run=dry_run) for p in target_paths(scope, cwd=cwd)]
    return GuideReport(scope=scope, operation="uninstall", dry_run=dry_run, files=files)


def status(scope: str | None = None, *, cwd: Path | None = None) -> dict:
    """Describe where the snippet is installed.

    With *scope* ``None`` (default) inspects both ``project`` and ``user``.
    Each scope maps to a list of per-file dicts:
    ``{path, exists, installed, up_to_date}``.
    """
    if scope is not None and scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES} or None, got {scope!r}")
    scopes = _VALID_SCOPES if scope is None else (scope,)
    return {sc: [_status_one(p) for p in target_paths(sc, cwd=cwd)] for sc in scopes}
