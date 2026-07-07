"""Version integrity guard — versions are maintainer-only. Full rule: AGENTS.md.

Agents changed version numbers without approval twice (commit bed5923
took 0.8 -> 0.9; commit 59eb59a executed the v0.9.2 release after a
recorded stop directive), and PyPI never lets a version number be
reused, so an unapproved bump is permanent damage.

This test pins the exact version strings currently in the tree: ANY
change to them fails CI unless ``PINNED`` below is updated in the same
commit. Updating ``PINNED`` is part of a deliberate release the
maintainer decided on — never an agent's edit.

The test takes no position on what the next version should be or when
it moves. That is the maintainer's decision alone.
"""

import json
import re
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]

# Maintainer-only: updated as part of a deliberate release commit.
PINNED = {
    "stackunderflow/__version__.py": "0.9.3",
    "pyproject.toml": "0.9.3",
    "stackunderflow-ui/package.json": "0.9.3",
    "stackunderflow-ui/package-lock.json": "0.9.3",
    "flake.nix": "0.9.3",
}


def _assert_pinned(actual: str, source: str) -> None:
    expected = PINNED[source]
    assert actual == expected, (
        f"{source} carries version {actual!r}; the pinned value is {expected!r}. "
        "Versions are maintainer-only (AGENTS.md). If you are an agent: revert this "
        "change. If you are the maintainer cutting a release: update PINNED in this "
        "test in the same commit."
    )


def test_python_package_version():
    text = (REPO_ROOT / "stackunderflow" / "__version__.py").read_text()
    match = re.search(r'__version__\s*=\s*"([^"]+)"', text)
    assert match, "__version__.py no longer defines __version__ as a string"
    _assert_pinned(match.group(1), "stackunderflow/__version__.py")


def test_pyproject_version():
    data = tomllib.loads((REPO_ROOT / "pyproject.toml").read_text())
    _assert_pinned(data["project"]["version"], "pyproject.toml")


def test_frontend_package_version():
    data = json.loads((REPO_ROOT / "stackunderflow-ui" / "package.json").read_text())
    _assert_pinned(data["version"], "stackunderflow-ui/package.json")


def test_frontend_lockfile_version():
    data = json.loads(
        (REPO_ROOT / "stackunderflow-ui" / "package-lock.json").read_text()
    )
    _assert_pinned(data["version"], "stackunderflow-ui/package-lock.json")
    # The lockfile repeats the version under the root packages entry.
    root_pkg = data.get("packages", {}).get("", {})
    if "version" in root_pkg:
        _assert_pinned(root_pkg["version"], "stackunderflow-ui/package-lock.json")


def test_flake_versions():
    text = (REPO_ROOT / "flake.nix").read_text()
    versions = re.findall(r'version\s*=\s*"([^"]+)"', text)
    assert versions, "flake.nix no longer pins a project version"
    for value in versions:
        _assert_pinned(value, "flake.nix")
