"""Tests for the cross-host ``stackunderflow-memory`` skill + plugin artefacts.

The agent-facing memory skill ships in two forms that MUST stay in lock-step:

* the canonical ``skills/stackunderflow-memory/SKILL.md`` (the single source), and
* a byte-identical mirror bundled inside the plugin at
  ``plugins/stackunderflow-memory/skills/stackunderflow-memory/SKILL.md`` so the
  plugin directory is self-contained and installable on its own.

``scripts/sync_plugin_skills.py`` is the single writer + drift guard for that
mirror. These tests:

* run the sync guard's ``check()`` and assert the mirror is byte-identical to the
  canonical source (an edit to one copy but not the other fails here);
* prove the guard actually bites, by monkeypatching it onto a deliberately-drifted
  pair and asserting it reports the drift, then that ``write()`` reconciles it;
* validate every per-host plugin + marketplace manifest is well-formed JSON with
  the fields its host expects (``name`` kebab-case, ``author`` / ``owner`` objects
  with a ``name``, marketplace ``source`` that resolves to the plugin directory);
* pin the canonical SKILL.md's trigger surface + the spec's behavioural rules
  (prefer text over the large JSON envelope, cite session id + provider, treat the
  local store as private) so a future edit can't quietly drop them.

Shape + contract only -- it does not invoke the ``stax`` / ``stackunderflow`` CLI
(the short ``stax`` alias is added elsewhere this wave), so the suite stays
hermetic.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest

# <repo>/tests/stackunderflow/test_plugin_skill_sync.py -> parents[2] == <repo>.
_REPO_ROOT = Path(__file__).resolve().parents[2]

_SKILL_NAME = "stackunderflow-memory"
_CANONICAL_SKILL = _REPO_ROOT / "skills" / _SKILL_NAME / "SKILL.md"

_PLUGINS_ROOT = _REPO_ROOT / "plugins"
_PLUGIN_DIR = _PLUGINS_ROOT / _SKILL_NAME
_MIRROR_SKILL = _PLUGIN_DIR / "skills" / _SKILL_NAME / "SKILL.md"
_COMMAND = _PLUGIN_DIR / "commands" / "su-memory.md"

_HOSTS = ("claude", "codex", "cursor")
_PLUGIN_MANIFESTS = tuple(
    _PLUGIN_DIR / f".{host}-plugin" / "plugin.json" for host in _HOSTS
)
_MARKETPLACE_MANIFESTS = tuple(
    _PLUGINS_ROOT / f".{host}-plugin" / "marketplace.json" for host in _HOSTS
)


def _load_sync():
    """Import ``scripts/sync_plugin_skills.py`` (a script, not an installed package)."""
    spec = importlib.util.spec_from_file_location(
        "sync_plugin_skills", _REPO_ROOT / "scripts" / "sync_plugin_skills.py"
    )
    assert spec is not None and spec.loader is not None, (
        "could not load scripts/sync_plugin_skills.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


sync = _load_sync()


def _split_frontmatter(text: str) -> tuple[str, str]:
    """Return (frontmatter, body) split on the first two ``---`` lines."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise ValueError("file does not start with `---` frontmatter delimiter")
    try:
        end = lines.index("---", 1)
    except ValueError as exc:
        raise ValueError("frontmatter is not closed by a second `---`") from exc
    return "\n".join(lines[1:end]), "\n".join(lines[end + 1 :])


def _parse_simple_yaml(text: str) -> dict[str, str]:
    """Parse the tiny ``key: value`` scalar subset used in skill frontmatter."""
    out: dict[str, str] = {}
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if ":" not in stripped:
            raise ValueError(f"bad frontmatter line: {line!r}")
        key, _, value = stripped.partition(":")
        out[key.strip()] = value.strip()
    return out


# ── sync guard: canonical <-> plugin mirror ─────────────────────────────────


def test_sync_check_reports_no_drift() -> None:
    """The committed mirror is in sync with the canonical source."""
    assert sync.check() == [], (
        "plugin skill mirror has drifted from the canonical SKILL.md; "
        "run `python scripts/sync_plugin_skills.py --write`"
    )


def test_mirror_is_byte_identical_to_canonical() -> None:
    assert _CANONICAL_SKILL.exists(), f"missing canonical skill: {_CANONICAL_SKILL}"
    assert _MIRROR_SKILL.exists(), f"missing plugin mirror: {_MIRROR_SKILL}"
    assert _CANONICAL_SKILL.read_bytes() == _MIRROR_SKILL.read_bytes()


def test_sync_guard_bites_and_write_reconciles(tmp_path, monkeypatch) -> None:
    """A drifted pair is reported by check(); write() then makes them identical."""
    canonical = tmp_path / "canonical.md"
    mirror = tmp_path / "nested" / "mirror.md"
    canonical.write_text("canonical body\n", encoding="utf-8")
    mirror.parent.mkdir(parents=True)
    mirror.write_text("STALE body\n", encoding="utf-8")

    monkeypatch.setattr(sync, "MIRRORS", ((canonical, mirror),))

    problems = sync.check()
    assert problems, "sync guard failed to detect a drifted mirror"
    assert any("out of sync" in p for p in problems)

    written = sync.write()
    assert written, "write() reported nothing written for a drifted mirror"
    assert sync.check() == []
    assert mirror.read_bytes() == canonical.read_bytes()


def test_sync_reports_missing_mirror(tmp_path, monkeypatch) -> None:
    canonical = tmp_path / "canonical.md"
    canonical.write_text("body\n", encoding="utf-8")
    absent = tmp_path / "does-not-exist.md"
    monkeypatch.setattr(sync, "MIRRORS", ((canonical, absent),))
    problems = sync.check()
    assert any("mirror missing" in p for p in problems)


# ── canonical SKILL.md: trigger surface + behavioural rules ──────────────────


def test_canonical_skill_frontmatter_shape() -> None:
    fm_text, body = _split_frontmatter(_CANONICAL_SKILL.read_text(encoding="utf-8"))
    fm = _parse_simple_yaml(fm_text)

    assert fm.get("name") == _SKILL_NAME, (
        f"frontmatter name {fm.get('name')!r} must match directory {_SKILL_NAME!r}"
    )
    description = fm.get("description", "")
    assert description, "frontmatter `description` is empty"
    # Claude Code uses `description` as the auto-trigger surface; short ones do not
    # fire reliably, and it is truncated past a documented combined cap.
    assert 40 <= len(description) <= 1536, (
        f"description length {len(description)} out of the [40, 1536] range"
    )
    assert body.strip(), "skill body is empty after the frontmatter"


def test_description_signals_before_acting() -> None:
    """The trigger surface names the before-action moments it should fire on."""
    fm_text, _ = _split_frontmatter(_CANONICAL_SKILL.read_text(encoding="utf-8"))
    description = _parse_simple_yaml(fm_text)["description"].lower()
    assert "before" in description
    for cue in ("edit", "write", "bash", "decision"):
        assert cue in description, f"description omits the {cue!r} trigger cue"


def test_body_teaches_both_command_forms() -> None:
    _, body = _split_frontmatter(_CANONICAL_SKILL.read_text(encoding="utf-8"))
    assert "stax memory" in body, "skill must teach the short `stax memory` form"
    assert "stackunderflow memory" in body, "skill must note the long form"
    for sub in ("decisions", "file", "worked", "sessions", "ask"):
        assert f"memory {sub}" in body, f"command menu omits `memory {sub}`"


def test_body_states_the_json_and_safety_rules() -> None:
    """Pin the spec's behavioural rules so an edit can't silently drop them."""
    _, body = _split_frontmatter(_CANONICAL_SKILL.read_text(encoding="utf-8"))
    lowered = body.lower()
    # Prefer text; JSON is large and can consume the context window.
    assert "context window" in lowered
    assert "--json" in body
    # Citation rule: session id + provider.
    assert "session id" in lowered
    assert "provider" in lowered
    # Store privacy + read-only.
    assert "~/.stackunderflow" in body
    assert "read-only" in lowered


# ── plugin manifests ─────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "manifest", _PLUGIN_MANIFESTS, ids=[m.parent.name for m in _PLUGIN_MANIFESTS]
)
def test_plugin_manifest_is_valid(manifest: Path) -> None:
    assert manifest.exists(), f"missing plugin manifest: {manifest}"
    data = json.loads(manifest.read_text(encoding="utf-8"))
    assert data.get("name") == _SKILL_NAME
    # kebab-case identifier: lowercase, digits, hyphens only.
    assert data["name"].replace("-", "").isalnum() and data["name"].islower()
    assert data.get("description", "").strip(), "plugin `description` is empty"
    author = data.get("author")
    assert isinstance(author, dict) and author.get("name"), (
        "plugin `author` must be an object with a non-empty `name`"
    )
    # No version string is invented here (versions are maintainer-only); if one is
    # ever present it must at least be a string, never a bare number.
    if "version" in data:
        assert isinstance(data["version"], str)


@pytest.mark.parametrize(
    "manifest", _MARKETPLACE_MANIFESTS, ids=[m.parent.name for m in _MARKETPLACE_MANIFESTS]
)
def test_marketplace_manifest_is_valid(manifest: Path) -> None:
    assert manifest.exists(), f"missing marketplace manifest: {manifest}"
    data = json.loads(manifest.read_text(encoding="utf-8"))
    assert data.get("name"), "marketplace `name` is empty"
    owner = data.get("owner")
    assert isinstance(owner, dict) and owner.get("name"), (
        "marketplace `owner` must be an object with a non-empty `name`"
    )
    plugins = data.get("plugins")
    assert isinstance(plugins, list) and plugins, "marketplace `plugins` must be non-empty"
    entry = next((p for p in plugins if p.get("name") == _SKILL_NAME), None)
    assert entry is not None, f"marketplace does not list the {_SKILL_NAME!r} plugin"
    source = entry.get("source")
    assert isinstance(source, str) and source.startswith("./"), (
        "same-repo plugin `source` should be a relative path from the marketplace root"
    )
    # `source` is relative to the marketplace root (the `plugins/` dir) and must
    # resolve to the real plugin directory holding a plugin manifest.
    resolved = (manifest.parent.parent / source).resolve()
    assert resolved == _PLUGIN_DIR.resolve(), f"source {source!r} does not resolve to the plugin dir"
    assert (resolved / ".claude-plugin" / "plugin.json").exists()


def test_no_version_string_invented_in_manifests() -> None:
    """Versions are maintainer-only; these new manifests must not pin one."""
    for manifest in (*_PLUGIN_MANIFESTS, *_MARKETPLACE_MANIFESTS):
        data = json.loads(manifest.read_text(encoding="utf-8"))
        assert "version" not in data, (
            f"{manifest} pins a version; versions are maintainer-only (AGENTS.md). "
            "Omit it -- the host falls back to the git commit SHA."
        )


# ── command delegator ────────────────────────────────────────────────────────


def test_command_delegates_to_the_skill() -> None:
    assert _COMMAND.exists(), f"missing command delegator: {_COMMAND}"
    text = _COMMAND.read_text(encoding="utf-8")
    fm_text, body = _split_frontmatter(text)
    fm = _parse_simple_yaml(fm_text)
    assert fm.get("description", "").strip(), "command `description` is empty"
    assert _SKILL_NAME in body, "command must point at the stackunderflow-memory skill"
    assert "stax memory" in body or "stackunderflow memory" in body
