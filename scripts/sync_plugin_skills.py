#!/usr/bin/env python3
"""Keep each plugin's bundled SKILL.md byte-identical to the canonical one.

The canonical agent-facing skill lives at
``skills/stackunderflow-memory/SKILL.md``. Every per-host plugin under
``plugins/`` bundles its own copy so the plugin directory is self-contained and
installable on its own (Claude Code / Codex / Cursor each read a skill from a
conventional ``skills/<name>/SKILL.md`` layout). Those copies must never drift
from the canonical source -- a stale plugin copy would teach an agent something
the canonical skill no longer says.

This script is the single writer AND the drift guard:

    python scripts/sync_plugin_skills.py --check   # assert every mirror == canonical
    python scripts/sync_plugin_skills.py --write    # copy canonical -> every mirror

``--check`` exits non-zero (naming the drifted or missing files) when any mirror
differs from its canonical source; ``--write`` makes them identical. The relation
is byte-for-byte: the plugin copy is a mirror, never a fork. A test wires
``--check`` into CI so an edit to one copy but not the other fails the build.

Stdlib-only so it runs anywhere Python does. ``check`` and ``write`` are
importable with no side effects at import time; the CLI lives under ``__main__``.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# (canonical source, plugin mirror) pairs. Extend this tuple as more synced
# skills / plugins are added -- every entry is guarded by --check and rewritten
# by --write, so a new mirror can never silently drift.
MIRRORS: tuple[tuple[Path, Path], ...] = (
    (
        ROOT / "skills" / "stackunderflow-memory" / "SKILL.md",
        ROOT
        / "plugins"
        / "stackunderflow-memory"
        / "skills"
        / "stackunderflow-memory"
        / "SKILL.md",
    ),
)


def _rel(path: Path) -> str:
    """Repo-relative path for readable messages; absolute if outside the repo."""
    try:
        return str(path.relative_to(ROOT))
    except ValueError:
        return str(path)


def check() -> list[str]:
    """Return a list of problem strings; an empty list means every mirror is in sync."""
    problems: list[str] = []
    for canonical, mirror in MIRRORS:
        if not canonical.exists():
            problems.append(f"canonical source missing: {_rel(canonical)}")
            continue
        if not mirror.exists():
            problems.append(f"mirror missing: {_rel(mirror)} (run --write)")
            continue
        if canonical.read_bytes() != mirror.read_bytes():
            problems.append(
                f"mirror out of sync: {_rel(mirror)} differs from {_rel(canonical)} "
                f"(run --write)"
            )
    return problems


def write() -> list[str]:
    """Copy each canonical source over its mirror. Returns the mirrors changed."""
    written: list[str] = []
    for canonical, mirror in MIRRORS:
        if not canonical.exists():
            raise FileNotFoundError(f"canonical source missing: {_rel(canonical)}")
        data = canonical.read_bytes()
        mirror.parent.mkdir(parents=True, exist_ok=True)
        if not mirror.exists() or mirror.read_bytes() != data:
            mirror.write_bytes(data)
            written.append(_rel(mirror))
    return written


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Sync / verify plugin-bundled SKILL.md copies against the canonical source."
    )
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument(
        "--check",
        action="store_true",
        help="assert every plugin mirror is byte-identical to its canonical source",
    )
    group.add_argument(
        "--write",
        action="store_true",
        help="copy the canonical SKILL.md over every plugin mirror",
    )
    args = parser.parse_args(argv)

    if args.write:
        written = write()
        if written:
            print(f"synced {len(written)} mirror(s):")
            for path in written:
                print(f"  wrote {path}")
        else:
            print("already in sync; nothing written.")
        return 0

    problems = check()
    if problems:
        print(f"FAIL: plugin skill mirror out of sync ({len(problems)} problem(s)):")
        for problem in problems:
            print(f"  - {problem}")
        print("Fix: python scripts/sync_plugin_skills.py --write")
        return 1
    print(f"OK: {len(MIRRORS)} plugin skill mirror(s) byte-identical to canonical.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
