"""Install / uninstall / status for the StackUnderflow Claude Code hooks.

Every mutation in here obeys the spec's hard constraints:

* **Opt-in only** — nothing calls ``install()`` except the user, via
  ``stackunderflow hooks install``.
* **Backup before mutation** — an existing ``settings.json`` is copied to
  ``settings.json.bak.<utc-ts>`` *before* it is rewritten (never on a
  no-op re-install, never under ``--dry-run``).
* **Never delete other hooks** — we add a self-contained matcher-group per
  event; ``uninstall`` removes only entries whose command we positively
  recognise as ours (see ``templates.parse_hook_command``). Every other
  hook entry — and the file itself — is left exactly as found.
* **Scope is explicit** — ``project`` (``.claude/settings.json`` in cwd's
  git root) or ``user`` (``~/.claude/settings.json``). No implicit
  broadening; ``--scope all`` only exists on ``repair``.
* **Portable commands** — see ``templates.canonical_command``.

``install`` is idempotent *and* convergent: it strips any pre-existing
StackUnderflow hook entries (stale absolute paths, an older
``--capture-content`` choice, the legacy ``hook run`` spelling) and writes
the canonical block fresh, so re-running it always lands on exactly the
config the current flags describe — never a duplicate, never a leftover.
"""

from __future__ import annotations

import json
import logging
import os
import tempfile
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

from stackunderflow.hooks import templates

logger = logging.getLogger("stackunderflow.hooks")

_VALID_SCOPES = ("project", "user")


# ── reports ─────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class InstallReport:
    scope: str
    settings_path: str
    dry_run: bool
    capture_content: bool
    changed: bool                                  # did the file content change (or would it, under --dry-run)?
    created_file: bool                             # was settings.json absent before?
    backup_path: str | None                        # the .bak.<ts> written (None on no-op / dry-run / fresh file)
    hooks_installed: list[str] = field(default_factory=list)        # hook ids in the resulting config
    stale_entries_replaced: list[str] = field(default_factory=list) # hook ids whose stale entry we rewrote
    other_hooks_preserved: int = 0                 # count of non-StackUnderflow hook entries left untouched
    captured_events_table_ready: bool = False      # did we ensure the captured_events table exists?

    def to_dict(self) -> dict:
        return {
            "scope": self.scope,
            "settings_path": self.settings_path,
            "dry_run": self.dry_run,
            "capture_content": self.capture_content,
            "changed": self.changed,
            "created_file": self.created_file,
            "backup_path": self.backup_path,
            "hooks_installed": list(self.hooks_installed),
            "stale_entries_replaced": list(self.stale_entries_replaced),
            "other_hooks_preserved": self.other_hooks_preserved,
            "captured_events_table_ready": self.captured_events_table_ready,
        }


@dataclass(frozen=True)
class UninstallReport:
    scope: str
    settings_path: str
    file_existed: bool
    changed: bool
    backup_path: str | None
    hooks_removed: list[str] = field(default_factory=list)
    other_hooks_preserved: int = 0

    def to_dict(self) -> dict:
        return {
            "scope": self.scope,
            "settings_path": self.settings_path,
            "file_existed": self.file_existed,
            "changed": self.changed,
            "backup_path": self.backup_path,
            "hooks_removed": list(self.hooks_removed),
            "other_hooks_preserved": self.other_hooks_preserved,
        }


# ── path resolution ─────────────────────────────────────────────────────────


def _git_root(start: Path) -> Path:
    """Nearest ancestor of *start* (inclusive) containing ``.git``; else *start*.

    ``.git`` may be a directory (normal clone) or a file (worktree / submodule)
    — both count.
    """
    start = start.resolve()
    for candidate in (start, *start.parents):
        if (candidate / ".git").exists():
            return candidate
    return start


def resolve_settings_path(scope: str, *, cwd: Path | None = None) -> Path:
    """Map *scope* to the ``settings.json`` it governs.

    ``project`` → ``<git-root-of-cwd>/.claude/settings.json``
    ``user``    → ``~/.claude/settings.json``
    """
    if scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES}, got {scope!r}")
    if scope == "user":
        return Path.home() / ".claude" / "settings.json"
    base = _git_root(cwd if cwd is not None else Path.cwd())
    return base / ".claude" / "settings.json"


# ── settings.json IO ────────────────────────────────────────────────────────


def _read_settings(path: Path) -> dict:
    """Load *path* as a JSON object; ``{}`` if absent.

    Raises ``ValueError`` if the file exists but isn't a JSON object — we
    refuse to merge into something we can't safely round-trip rather than
    risk clobbering it.
    """
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        raise ValueError(f"{path} is not valid JSON ({exc}); fix or remove it before installing hooks") from exc
    if not isinstance(data, dict):
        raise ValueError(f"{path} must contain a JSON object, found {type(data).__name__}")
    return data


def _backup_path_for(path: Path) -> Path:
    ts = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    return path.with_name(f"{path.name}.bak.{ts}")


def _backup(path: Path) -> Path:
    """Copy *path* to ``<path>.bak.<utc-ts>`` and return the backup path.

    If the timestamp collides with an existing backup (sub-second double
    call) a numeric suffix is appended so we never overwrite a backup.
    """
    dest = _backup_path_for(path)
    n = 1
    while dest.exists():
        dest = dest.with_name(f"{dest.name}.{n}")
        n += 1
    dest.write_bytes(path.read_bytes())
    return dest


def _atomic_write_json(path: Path, data: dict) -> None:
    """Write *data* as pretty JSON to *path* atomically (temp file + rename)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(data, indent=2) + "\n"
    fd, tmp_name = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp")
    try:
        with os.fdopen(fd, "w") as fh:
            fh.write(text)
        os.replace(tmp_name, path)
    except BaseException:
        # Best-effort cleanup; don't mask the original error.
        try:
            os.unlink(tmp_name)
        except OSError:
            logger.debug("could not remove temp file %s", tmp_name)
        raise


# ── hook-block surgery (pure dict→dict) ─────────────────────────────────────


def _iter_hook_entries(settings: dict):
    """Yield ``(event, group_index, entry_index, entry)`` for every hook entry.

    Tolerant of malformed shapes — anything that isn't the expected
    list/dict nesting is skipped rather than raising.
    """
    hooks = settings.get("hooks")
    if not isinstance(hooks, dict):
        return
    for event, groups in hooks.items():
        if not isinstance(groups, list):
            continue
        for gi, group in enumerate(groups):
            if not isinstance(group, dict):
                continue
            entries = group.get("hooks")
            if not isinstance(entries, list):
                continue
            for ei, entry in enumerate(entries):
                if isinstance(entry, dict):
                    yield event, gi, ei, entry


def _entry_is_ours(entry: dict) -> tuple[str, bool] | None:
    cmd = entry.get("command")
    if not isinstance(cmd, str):
        return None
    if entry.get("type", "command") != "command":
        return None
    return templates.parse_hook_command(cmd)


def count_other_hooks(settings: dict) -> int:
    """Count hook entries that are *not* ours (the invariant ``uninstall`` protects)."""
    return sum(1 for _e, _g, _i, entry in _iter_hook_entries(settings) if _entry_is_ours(entry) is None)


def detect_our_hooks(settings: dict) -> dict[str, list[tuple[str, bool, bool]]]:
    """Map event → list of ``(hook_id, capture_content, is_canonical)`` we already have."""
    found: dict[str, list[tuple[str, bool, bool]]] = {}
    for event, _gi, _ei, entry in _iter_hook_entries(settings):
        parsed = _entry_is_ours(entry)
        if parsed is None:
            continue
        hook_id, capture_content = parsed
        canon = templates.is_canonical(entry["command"], capture_content=capture_content)
        found.setdefault(event, []).append((hook_id, capture_content, canon))
    return found


def _strip_our_hooks(settings: dict) -> tuple[dict, list[str]]:
    """Return a deep-ish copy of *settings* with every StackUnderflow hook entry removed.

    Empties cascade: a matcher-group whose ``hooks`` list goes empty is
    dropped; an event whose group list goes empty is dropped; an empty
    ``hooks`` mapping is dropped. Everything else is preserved verbatim.
    Returns ``(new_settings, removed_hook_ids)``.
    """
    new = json.loads(json.dumps(settings))  # cheap deep copy of plain JSON
    removed: list[str] = []
    hooks = new.get("hooks")
    if not isinstance(hooks, dict):
        return new, removed
    for event in list(hooks.keys()):
        groups = hooks[event]
        if not isinstance(groups, list):
            continue
        kept_groups = []
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                kept_groups.append(group)
                continue
            kept_entries = []
            for entry in group["hooks"]:
                parsed = _entry_is_ours(entry) if isinstance(entry, dict) else None
                if parsed is not None:
                    removed.append(parsed[0])
                else:
                    kept_entries.append(entry)
            if kept_entries:
                group = {**group, "hooks": kept_entries}
                kept_groups.append(group)
            # else: group became empty → drop it
        if kept_groups:
            hooks[event] = kept_groups
        else:
            del hooks[event]
    if not hooks:
        del new["hooks"]
    return new, removed


def _add_our_hooks(settings: dict, *, capture_content: bool) -> dict:
    """Append our canonical matcher-group to each event array (creating as needed)."""
    new = json.loads(json.dumps(settings))
    hooks = new.setdefault("hooks", {})
    if not isinstance(hooks, dict):  # pragma: no cover - defensive; caller passes a sane dict
        raise ValueError("settings['hooks'] must be a JSON object")
    for event in templates.EVENT_HOOK_IDS:
        arr = hooks.setdefault(event, [])
        if not isinstance(arr, list):  # pragma: no cover - defensive
            raise ValueError(f"settings['hooks'][{event!r}] must be a JSON array")
        arr.append(templates.matcher_group(event, capture_content=capture_content))
    return new


# ── public API ──────────────────────────────────────────────────────────────


def install(
    scope: str = "project",
    *,
    dry_run: bool = False,
    capture_content: bool = False,
    cwd: Path | None = None,
) -> InstallReport:
    """Register the StackUnderflow hooks in the *scope*'s ``settings.json``.

    Idempotent and convergent — see the module docstring. With
    ``capture_content=True`` the installed hook commands carry
    ``--capture-content`` so handlers store the full (unsanitised) payload
    (default: metadata + tool name + exit code only).
    """
    if scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES}, got {scope!r}")
    path = resolve_settings_path(scope, cwd=cwd)
    existed = path.exists()
    original = _read_settings(path)

    stripped, replaced = _strip_our_hooks(original)
    desired = _add_our_hooks(stripped, capture_content=capture_content)

    changed = json.dumps(desired, sort_keys=True) != json.dumps(original, sort_keys=True)
    other_count = count_other_hooks(original)

    backup_path: Path | None = None
    if changed and not dry_run:
        if existed:
            backup_path = _backup(path)
        _atomic_write_json(path, desired)

    table_ready = False
    if not dry_run:
        table_ready = _ensure_captured_events_table_quiet()

    return InstallReport(
        scope=scope,
        settings_path=str(path),
        dry_run=dry_run,
        capture_content=capture_content,
        changed=changed,
        created_file=changed and not existed and not dry_run,
        backup_path=str(backup_path) if backup_path else None,
        hooks_installed=list(templates.HOOK_IDS),
        stale_entries_replaced=replaced,
        other_hooks_preserved=other_count,
        captured_events_table_ready=table_ready,
    )


def uninstall(scope: str = "project", *, cwd: Path | None = None) -> UninstallReport:
    """Remove the StackUnderflow hooks from the *scope*'s ``settings.json``.

    Removes *only* entries we recognise as ours; never deletes the file or
    any other hook. A backup is written first iff the file actually changes.
    """
    if scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES}, got {scope!r}")
    path = resolve_settings_path(scope, cwd=cwd)
    if not path.exists():
        return UninstallReport(
            scope=scope,
            settings_path=str(path),
            file_existed=False,
            changed=False,
            backup_path=None,
            hooks_removed=[],
            other_hooks_preserved=0,
        )
    original = _read_settings(path)
    stripped, removed = _strip_our_hooks(original)
    changed = json.dumps(stripped, sort_keys=True) != json.dumps(original, sort_keys=True)

    backup_path: Path | None = None
    if changed:
        backup_path = _backup(path)
        _atomic_write_json(path, stripped)

    return UninstallReport(
        scope=scope,
        settings_path=str(path),
        file_existed=True,
        changed=changed,
        backup_path=str(backup_path) if backup_path else None,
        hooks_removed=removed,
        other_hooks_preserved=count_other_hooks(stripped),
    )


def status(scope: str | None = None, *, cwd: Path | None = None) -> dict:
    """Describe what's installed where.

    With *scope* ``None`` (default) inspects both ``project`` and ``user``.
    Each entry: ``{settings_path, exists, valid_json, hooks (id→capture_content),
    stale (ids whose entry isn't canonical), other_hook_count}``.
    """
    scopes = _VALID_SCOPES if scope is None else (scope,)
    if scope is not None and scope not in _VALID_SCOPES:
        raise ValueError(f"scope must be one of {_VALID_SCOPES} or None, got {scope!r}")
    out: dict = {}
    for sc in scopes:
        path = resolve_settings_path(sc, cwd=cwd)
        entry: dict = {"settings_path": str(path), "exists": path.exists(), "valid_json": True}
        if not path.exists():
            entry.update({"hooks": {}, "stale": [], "other_hook_count": 0})
            out[sc] = entry
            continue
        try:
            settings = _read_settings(path)
        except ValueError:
            entry.update({"valid_json": False, "hooks": {}, "stale": [], "other_hook_count": 0})
            out[sc] = entry
            continue
        found = detect_our_hooks(settings)
        hooks_map: dict[str, bool] = {}
        stale: list[str] = []
        for _event, items in found.items():
            for hook_id, capture_content, canon in items:
                hooks_map[hook_id] = capture_content
                if not canon:
                    stale.append(hook_id)
        entry.update(
            {
                "hooks": hooks_map,
                "stale": sorted(set(stale)),
                "other_hook_count": count_other_hooks(settings),
            }
        )
        out[sc] = entry
    return out


# ── store table bootstrap ───────────────────────────────────────────────────


def _ensure_captured_events_table_quiet() -> bool:
    """Best-effort: make sure ``captured_events`` exists in the real store.

    Lets the very first hook fire after ``install`` have somewhere to write
    without waiting for the dashboard's ``schema.apply``. Failure here is
    non-fatal — the handler runs the same ``CREATE TABLE IF NOT EXISTS`` on
    every invocation, so a flaky store at install time self-heals.
    """
    try:
        import stackunderflow.deps as deps
        from stackunderflow.hooks.handlers import ensure_captured_events_table
        from stackunderflow.store import db

        conn = db.connect(deps.store_path)
        try:
            ensure_captured_events_table(conn)
        finally:
            conn.close()
        return True
    except Exception:  # noqa: BLE001 - never let store bootstrap break `install`
        logger.debug("could not pre-create captured_events table at install time", exc_info=True)
        return False
