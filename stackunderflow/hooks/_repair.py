"""``stackunderflow hooks repair`` — heal stale StackUnderflow hook commands.

If a hook entry ever ends up referencing a stale absolute path
(``/old/venv/bin/stackunderflow hooks run …`` after the venv moved), or the
legacy singular ``hook run`` spelling, ``repair`` rewrites just that
``command`` string to the portable canonical form
(``stackunderflow hooks run <id>``, preserving a ``--capture-content`` choice).
It changes *nothing else*: no hooks added or removed, every non-StackUnderflow
entry left byte-for-byte, a per-file backup written before any mutation, and
``--dry-run`` reports without touching anything.

Scope:

* ``project`` (default narrow) — just ``<git-root-of-cwd>/.claude/settings.json``
* ``user``                     — ``~/.claude/settings.json``
* ``all``                      — walk ``$HOME`` for every ``.claude/settings.json``

The ``$HOME`` walk is bounded and conservative: it never follows symlinks, it
is depth-limited (≤ 8 directory levels below ``$HOME``), and it prunes the
usual heavy / irrelevant trees (``node_modules``, ``.git``, ``.npm``,
``.cache``, ``.nvm``, …). It is *only ever run when the user asks for it* —
never from a postinstall, never automatically.
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import dataclass, field
from pathlib import Path

from stackunderflow.hooks import templates
from stackunderflow.hooks._install import (
    _atomic_write_json,
    _backup,
    _entry_is_ours,
    _read_settings,
    count_other_hooks,
    resolve_settings_path,
)

logger = logging.getLogger("stackunderflow.hooks")

_VALID_REPAIR_SCOPES = ("project", "user", "all")

# Directory names we never descend into during the ``$HOME`` walk. The first
# five are the spec's named prune list; the rest are large/irrelevant trees
# that would only slow the scan. ``.claude`` is deliberately NOT here — that's
# the directory we're looking for.
_PRUNE_DIRS = frozenset(
    {
        "node_modules", ".git", ".npm", ".cache", ".nvm",
        ".Trash", ".rustup", ".cargo", ".gradle", ".m2", ".bun", ".deno", ".pnpm-store",
        "__pycache__", ".venv", "venv", "env", ".tox", ".nox",
        ".mypy_cache", ".pytest_cache", ".ruff_cache", ".hatch", ".eggs",
        "build", "dist", "target", ".next", ".nuxt", ".svelte-kit", ".parcel-cache",
        "Library",  # macOS — huge, never a project root
    }
)

_MAX_DEPTH = 8


# ── report ──────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class RepairReport:
    scope: str
    dry_run: bool
    scanned_files: list[str] = field(default_factory=list)   # settings.json files inspected
    repaired: list[dict] = field(default_factory=list)        # [{file, hook_id, old, new}, ...]
    backups: list[str] = field(default_factory=list)          # .bak.<ts> files written
    pruned_dirs: int = 0                                       # directories skipped during the walk (informational)

    @property
    def files_changed(self) -> int:
        return len({entry["file"] for entry in self.repaired})

    def to_dict(self) -> dict:
        return {
            "scope": self.scope,
            "dry_run": self.dry_run,
            "scanned_files": list(self.scanned_files),
            "repaired": [dict(entry) for entry in self.repaired],
            "backups": list(self.backups),
            "pruned_dirs": self.pruned_dirs,
            "files_changed": self.files_changed,
        }


# ── filesystem walk ─────────────────────────────────────────────────────────


def _scan_settings_files(root: Path, *, max_depth: int = _MAX_DEPTH) -> tuple[list[Path], int]:
    """Return ``([<dir>/.claude/settings.json, …], pruned_dir_count)`` under *root*.

    Bounded + symlink-safe: never recurses into a symlinked directory, never
    into a pruned name, never below *max_depth* directory levels under *root*.
    The walk is collected eagerly (it's small — a handful of paths even on a
    busy ``$HOME``) so the pruned count is the *total*, not a snapshot.
    """
    root = root.resolve()
    found: list[Path] = []
    pruned = 0
    # os.walk(followlinks=False) already declines to recurse into symlinked
    # dirs; we additionally strip them (and pruned names, and anything past the
    # depth budget) out of ``dirnames`` in-place so the walk doesn't even stat
    # their contents.
    for dirpath, dirnames, filenames in os.walk(root, topdown=True, followlinks=False):
        depth = len(Path(dirpath).resolve().relative_to(root).parts)
        if depth >= max_depth:
            pruned += len(dirnames)
            dirnames[:] = []
        else:
            kept = []
            for d in dirnames:
                full = os.path.join(dirpath, d)
                if d in _PRUNE_DIRS or os.path.islink(full):
                    pruned += 1
                    continue
                kept.append(d)
            dirnames[:] = kept
        if os.path.basename(dirpath) == ".claude" and "settings.json" in filenames:
            found.append(Path(dirpath) / "settings.json")
    return found, pruned


# ── per-file repair ─────────────────────────────────────────────────────────


def _repaired_command(command: str) -> tuple[str, str] | None:
    """If *command* is a stale StackUnderflow hook command, return ``(hook_id, fixed)``.

    "Stale" = recognisably ours but not byte-equal to the canonical form
    (preserving its ``--capture-content`` choice). ``None`` if it's already
    canonical or not ours.
    """
    parsed = templates.parse_hook_command(command)
    if parsed is None:
        return None
    hook_id, capture_content = parsed
    canon = templates.canonical_command(hook_id, capture_content=capture_content)
    if command.strip() == canon:
        return None  # already fine
    return hook_id, canon


def _repair_settings_obj(settings: dict) -> tuple[dict, list[dict]]:
    """Return ``(new_settings, changes)`` — *changes* describes each rewritten command."""
    new = json.loads(json.dumps(settings))  # plain-JSON deep copy
    changes: list[dict] = []
    hooks = new.get("hooks")
    if not isinstance(hooks, dict):
        return new, changes
    for _event, groups in hooks.items():
        if not isinstance(groups, list):
            continue
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                continue
            for entry in group["hooks"]:
                if not isinstance(entry, dict):
                    continue
                if _entry_is_ours(entry) is None:
                    continue
                cmd = entry.get("command")
                if not isinstance(cmd, str):
                    continue
                fix = _repaired_command(cmd)
                if fix is None:
                    continue
                hook_id, fixed = fix
                changes.append({"hook_id": hook_id, "old": cmd, "new": fixed})
                entry["command"] = fixed
    return new, changes


def _repair_one_file(path: Path, *, dry_run: bool) -> tuple[list[dict], str | None]:
    """Inspect+repair a single ``settings.json``. Returns ``(changes, backup_path)``.

    Skips silently (no changes) if the file is missing or not valid JSON — a
    broken settings file is the user's to fix; we won't touch it.
    """
    if not path.exists():
        return [], None
    try:
        settings = _read_settings(path)
    except ValueError:
        logger.debug("repair: skipping %s (not valid JSON)", path)
        return [], None
    new_settings, changes = _repair_settings_obj(settings)
    if not changes:
        return [], None
    # Sanity: a repair must never change the count of non-ours hooks.
    if count_other_hooks(new_settings) != count_other_hooks(settings):  # pragma: no cover - defensive
        logger.debug("repair: refusing to write %s — other-hook count would change", path)
        return [], None
    backup_path: str | None = None
    if not dry_run:
        backup_path = str(_backup(path))
        _atomic_write_json(path, new_settings)
    return changes, backup_path


# ── public API ──────────────────────────────────────────────────────────────


def repair(
    scope: str = "project",
    *,
    dry_run: bool = False,
    cwd: Path | None = None,
    home: Path | None = None,
) -> RepairReport:
    """Canonicalise stale StackUnderflow hook commands within *scope*.

    *cwd* (project scope) and *home* (``all`` scope) are injectable for
    tests; both default to the real locations.
    """
    if scope not in _VALID_REPAIR_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_REPAIR_SCOPES}, got {scope!r}")

    scanned: list[str] = []
    repaired: list[dict] = []
    backups: list[str] = []
    pruned_dirs = 0

    if scope == "all":
        root = home if home is not None else Path.home()
        settings_files, pruned_dirs = _scan_settings_files(root)
        for settings_path in settings_files:
            scanned.append(str(settings_path))
            changes, backup_path = _repair_one_file(settings_path, dry_run=dry_run)
            for c in changes:
                repaired.append({"file": str(settings_path), **c})
            if backup_path:
                backups.append(backup_path)
    else:
        settings_path = resolve_settings_path(scope, cwd=cwd)
        scanned.append(str(settings_path))
        changes, backup_path = _repair_one_file(settings_path, dry_run=dry_run)
        for c in changes:
            repaired.append({"file": str(settings_path), **c})
        if backup_path:
            backups.append(backup_path)

    return RepairReport(
        scope=scope,
        dry_run=dry_run,
        scanned_files=scanned,
        repaired=repaired,
        backups=backups,
        pruned_dirs=pruned_dirs,
    )
