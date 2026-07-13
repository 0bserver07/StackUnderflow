#!/usr/bin/env python3
"""Cut a StackUnderflow release — the ONE sanctioned path.

    scripts/release.py X.Y.Z             # dry run: validate + show the plan, change nothing
    scripts/release.py X.Y.Z --execute   # make the atomic release commit + tag, locally

Why this exists
---------------
Releases used to be a hand-typed dance across five version files, the guard
test's ``PINNED`` dict, and the CHANGELOG — easy to get wrong, and easy for an
over-eager agent to inflate. This script makes the safety MECHANICAL instead of
trust-based:

* **It never picks or defaults a number.** No argument → it refuses and exits.
  The version can only ever come from a human typing it here. (That is the whole
  anti-inflation guarantee: there is no code path that invents a version.)
* **It refuses anything that isn't strictly greater** than the latest released
  git tag, and refuses a tag that already exists — so a typo that goes backward,
  sideways, or re-uses a burned PyPI number is rejected before anything changes.
* **Dry run by default.** Without ``--execute`` it validates, prints the exact
  plan + diff, and touches nothing.
* **It stops before anything irreversible.** ``--execute`` makes a LOCAL commit +
  annotated tag only. It does not push and does not publish. PyPI upload is
  triggered by *publishing a GitHub Release* (see .github/workflows/publish.yml),
  which stays a deliberate human action — the script just prints the remaining
  commands.

Agents: you do not run this. The maintainer runs it, with a number they chose.
See AGENTS.md — versions are maintainer-only.
"""

from __future__ import annotations

import argparse
import datetime
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
GUARD = REPO / "tests" / "stackunderflow" / "test_version_guard.py"
CHANGELOG = REPO / "CHANGELOG.md"

# Files carrying the project version. Each entry: (path, how-many occurrences of
# the OLD version string we expect to replace). The count is asserted so a
# layout change (a new file, a moved string) fails loudly rather than silently
# leaving a stale version behind.
VERSION_FILES: tuple[tuple[str, int], ...] = (
    ("stackunderflow/__version__.py", 1),
    ("pyproject.toml", 1),
    ("stackunderflow-ui/package.json", 1),
    ("stackunderflow-ui/package-lock.json", 2),
    ("flake.nix", 2),
)

RELEASE_RE = re.compile(r"^\d+\.\d+\.\d+$")


class Abort(SystemExit):
    def __init__(self, msg: str) -> None:
        super().__init__(f"release: {msg}")


def _run(*args: str, check: bool = True) -> str:
    r = subprocess.run(args, cwd=REPO, capture_output=True, text=True)
    if check and r.returncode != 0:
        raise Abort(f"`{' '.join(args)}` failed: {r.stderr.strip() or r.stdout.strip()}")
    return r.stdout.strip()


def _parse(v: str) -> tuple[int, int, int]:
    if not RELEASE_RE.match(v):
        raise Abort(
            f"{v!r} is not a clean release version X.Y.Z (no 'v', no '-dev' suffix). "
            "Pre-release/dev strings are not published from here."
        )
    a, b, c = v.split(".")
    return int(a), int(b), int(c)


def _current_pinned() -> str:
    """The version the tree currently carries, per the guard's PINNED dict —
    the single source of truth for 'what is in the tree right now'."""
    text = GUARD.read_text()
    values = set(re.findall(r'":\s*"([^"]+)"', text))
    # PINNED values are the version strings; they must all agree.
    pins = {v for v in values if re.match(r"^\d+\.\d+\.\d+", v)}
    if len(pins) != 1:
        raise Abort(
            f"guard PINNED does not carry a single agreed version (found {sorted(pins)}); "
            "the tree is inconsistent — fix that before releasing."
        )
    return next(iter(pins))


def _latest_tag() -> str | None:
    out = _run("git", "tag", "--list", "v*", "--sort=-v:refname", check=False)
    for line in out.splitlines():
        line = line.strip()
        if re.match(r"^v\d+\.\d+\.\d+$", line):
            return line[1:]
    return None


def _preflight(new: str) -> tuple[str, str]:
    # 1. tree must be clean, on main, in sync with origin.
    if _run("git", "status", "--porcelain"):
        raise Abort("working tree is dirty — commit or stash first.")
    branch = _run("git", "rev-parse", "--abbrev-ref", "HEAD")
    if branch != "main":
        raise Abort(f"on branch {branch!r}, not main.")
    _run("git", "fetch", "--quiet", "origin", "main", check=False)
    local = _run("git", "rev-parse", "HEAD")
    remote = _run("git", "rev-parse", "origin/main", check=False)
    if remote and local != remote:
        raise Abort("local main is not in sync with origin/main — push/pull first.")

    # 2. the new version must be a clean release, strictly greater, not-yet-tagged.
    new_t = _parse(new)
    tag = f"v{new}"
    if tag in _run("git", "tag", "--list", tag, check=False).splitlines():
        raise Abort(f"tag {tag} already exists — that version is spoken for.")
    latest = _latest_tag()
    if latest and new_t <= _parse(latest):
        raise Abort(
            f"{new} is not greater than the latest release {latest}. "
            "Releases only ever go forward — pick a higher number."
        )

    # 3. every version file must actually carry the current pinned string.
    old = _current_pinned()
    for rel, count in VERSION_FILES:
        text = (REPO / rel).read_text()
        found = text.count(old)
        if found != count:
            raise Abort(
                f"{rel}: expected {count} occurrence(s) of the current version "
                f"{old!r}, found {found}. Refusing to guess."
            )

    # 4. there must be something to release.
    body = CHANGELOG.read_text()
    m = re.search(r"## \[Unreleased\]\s*(.*?)(?=\n## \[|\Z)", body, re.S)
    if not m or not m.group(1).strip():
        raise Abort("CHANGELOG '## [Unreleased]' is empty — nothing to release.")

    return old, latest or "(none)"


def _apply(old: str, new: str) -> None:
    # Version files: replace the exact old string, asserting the count again.
    for rel, count in VERSION_FILES:
        p = REPO / rel
        text = p.read_text()
        assert text.count(old) == count
        p.write_text(text.replace(old, new))

    # Guard PINNED: rewrite every pinned version value to the new one.
    g = GUARD.read_text()
    g = re.sub(r'(":\s*")' + re.escape(old) + r'(")', r"\g<1>" + new + r"\g<2>", g)
    GUARD.write_text(g)

    # CHANGELOG: date the released notes, open a fresh empty Unreleased above.
    today = datetime.date.today().isoformat()
    body = CHANGELOG.read_text()
    body = body.replace(
        "## [Unreleased]",
        f"## [Unreleased]\n\n## [{new}] - {today}",
        1,
    )
    CHANGELOG.write_text(body)


def main() -> int:
    ap = argparse.ArgumentParser(
        prog="release.py",
        description="Cut a release. Requires an explicit version — never picks one.",
    )
    ap.add_argument("version", help="the exact release version X.Y.Z you are cutting")
    ap.add_argument(
        "--execute",
        action="store_true",
        help="make the local release commit + tag (default is a dry run that changes nothing)",
    )
    args = ap.parse_args()

    old, latest = _preflight(args.version)
    new = args.version
    tag = f"v{new}"

    print(f"  current tree version : {old}")
    print(f"  latest released tag  : {latest}")
    print(f"  releasing as         : {new}   (tag {tag})")
    print(f"  files to bump        : {', '.join(p for p, _ in VERSION_FILES)}, "
          f"+ guard PINNED, + CHANGELOG heading")

    if not args.execute:
        print("\n  DRY RUN — nothing changed. Re-run with --execute to make the commit.")
        return 0

    _apply(old, new)

    # Prove coherence before the commit stands: the guard test must pass on the
    # new pins, and the lint gate CI runs must be clean.
    print("\n  applied. verifying...")
    _run(sys.executable, "-m", "pytest", "tests/stackunderflow/test_version_guard.py", "-q")
    _run("ruff", "check", "stackunderflow/", "--select", "E,F", "--exclude", "*/build.py")

    _run("git", "add", "-A")
    _run("git", "commit", "-m", f"release: {new}")
    _run("git", "tag", "-a", tag, "-m", f"StackUnderflow {new}")

    print(f"\n  committed + tagged {tag} LOCALLY. Nothing pushed, nothing published.")
    print("  review it:")
    print(f"      git show {tag}")
    print("  then, when you're ready (these are the only remaining, deliberate steps):")
    print(f"      git push origin main && git push origin {tag}")
    print(f"      gh release create {tag} --title {tag} --notes-from-tag --draft")
    print("  the release starts as a DRAFT — publishing it is what triggers the")
    print("  PyPI upload (permanent). Publish the draft only when you're sure.")
    print("  to undo everything before you push:")
    print(f"      git tag -d {tag} && git reset --hard HEAD~1")
    return 0


if __name__ == "__main__":
    sys.exit(main())
