"""VERSION FREEZE guard — the version stays at 0.9.x. Full rule: CLAUDE.md.

The 0.8 -> 0.9 minor bump was made by a coding agent without approval
(commit bed5923, 2026-05-15) and is published on PyPI, where a version
number can never be reused. The maintainer has frozen the minor digit:
releases are maintainer-only and increment the patch component
(0.9.2 -> 0.9.3 -> ...).

This test makes the freeze mechanical: any commit that moves a version
string in any version-bearing file past 0.9.x turns CI red. Patch bumps
pass without touching this file. Raising ``ALLOWED_PREFIX`` is a
maintainer-only act, done knowingly in the same commit as a deliberate
release decision — never by an agent.
"""

import json
import re
import tomllib
from pathlib import Path

import pytest

ALLOWED_PREFIX = "0.9."

REPO_ROOT = Path(__file__).resolve().parents[2]


def _assert_frozen(value: str, source: str) -> None:
    assert value.startswith(ALLOWED_PREFIX), (
        f"{source} carries version {value!r}, outside the {ALLOWED_PREFIX}x freeze. "
        "Versions are maintainer-only (see CLAUDE.md). If you are an agent: revert "
        "this. If you are the maintainer cutting a deliberate release: bump "
        "ALLOWED_PREFIX in this test in the same commit."
    )


def test_python_package_version_frozen():
    text = (REPO_ROOT / "stackunderflow" / "__version__.py").read_text()
    match = re.search(r'__version__\s*=\s*"([^"]+)"', text)
    assert match, "__version__.py no longer defines __version__ as a string"
    _assert_frozen(match.group(1), "stackunderflow/__version__.py")


def test_pyproject_version_frozen():
    data = tomllib.loads((REPO_ROOT / "pyproject.toml").read_text())
    _assert_frozen(data["project"]["version"], "pyproject.toml")


@pytest.mark.parametrize("filename", ["package.json", "package-lock.json"])
def test_frontend_versions_frozen(filename):
    data = json.loads((REPO_ROOT / "stackunderflow-ui" / filename).read_text())
    _assert_frozen(data["version"], f"stackunderflow-ui/{filename}")
    # package-lock.json repeats the version under the root packages entry.
    root_pkg = data.get("packages", {}).get("", {})
    if "version" in root_pkg:
        _assert_frozen(root_pkg["version"], f'stackunderflow-ui/{filename} packages[""]')


def test_flake_versions_frozen():
    text = (REPO_ROOT / "flake.nix").read_text()
    versions = re.findall(r'version\s*=\s*"([^"]+)"', text)
    assert versions, "flake.nix no longer pins a project version"
    for value in versions:
        _assert_frozen(value, "flake.nix")
