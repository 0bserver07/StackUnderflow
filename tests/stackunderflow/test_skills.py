"""Smoke tests for the shipped Claude Code skills.

The skills under ``stackunderflow/skills/<name>/SKILL.md`` are markdown
files with YAML frontmatter that Claude Code reads to decide when to
invoke a capability. These tests verify each shipped skill:

* Lives at the expected path layout (``<name>/SKILL.md`` directory).
* Has valid YAML frontmatter delimited by ``---`` lines.
* Frontmatter declares a ``name`` matching the directory name.
* Frontmatter declares a non-empty ``description`` (the trigger surface
  Claude Code uses to pick the skill).
* Body after the frontmatter is non-empty.

This is shape-only — it doesn't validate the content quality of the
descriptions or bodies, just that the files parse and aren't empty.
"""

from __future__ import annotations

from pathlib import Path

import pytest

SKILLS_DIR = Path(__file__).resolve().parents[2] / "stackunderflow" / "skills"

EXPECTED_SKILLS = (
    "check-prior-work",
    "find-related-sessions",
    "recall-past-decisions",
)


def _split_frontmatter(text: str) -> tuple[str, str]:
    """Return (frontmatter, body) split on the first two ``---`` lines.

    Raises ``ValueError`` if the file doesn't begin with frontmatter.
    """
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        raise ValueError("file does not start with `---` frontmatter delimiter")
    try:
        end = lines.index("---", 1)
    except ValueError as e:
        raise ValueError("frontmatter is not closed by a second `---`") from e
    return "\n".join(lines[1:end]), "\n".join(lines[end + 1 :])


def _parse_simple_yaml(text: str) -> dict[str, str]:
    """Parse the tiny key: value subset of YAML used in skill frontmatter.

    Skills only declare scalar string keys (``name``, ``description``).
    A real YAML parser would be overkill and would add a runtime dep.
    """
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


@pytest.mark.parametrize("skill_name", EXPECTED_SKILLS)
def test_skill_directory_exists(skill_name: str) -> None:
    """Each shipped skill lives at ``stackunderflow/skills/<name>/SKILL.md``."""
    skill_md = SKILLS_DIR / skill_name / "SKILL.md"
    assert skill_md.exists(), f"missing skill file: {skill_md}"


@pytest.mark.parametrize("skill_name", EXPECTED_SKILLS)
def test_skill_has_valid_frontmatter(skill_name: str) -> None:
    """Frontmatter declares non-empty ``name`` matching dir + ``description``."""
    skill_md = SKILLS_DIR / skill_name / "SKILL.md"
    text = skill_md.read_text(encoding="utf-8")
    fm_text, body = _split_frontmatter(text)
    fm = _parse_simple_yaml(fm_text)

    assert fm.get("name") == skill_name, (
        f"frontmatter `name` ({fm.get('name')!r}) does not match "
        f"directory name ({skill_name!r})"
    )

    description = fm.get("description", "")
    assert description, "frontmatter `description` is empty"
    assert len(description) >= 40, (
        f"description for {skill_name!r} is suspiciously short "
        f"({len(description)} chars) — Claude Code uses this as the trigger "
        f"surface and short descriptions don't fire reliably."
    )

    assert body.strip(), f"skill body for {skill_name!r} is empty after frontmatter"


@pytest.mark.parametrize("skill_name", EXPECTED_SKILLS)
def test_skill_body_cites_a_cli_command(skill_name: str) -> None:
    """Each skill body references the ``stackunderflow`` CLI it triggers."""
    skill_md = SKILLS_DIR / skill_name / "SKILL.md"
    text = skill_md.read_text(encoding="utf-8")
    _, body = _split_frontmatter(text)
    assert "stackunderflow " in body, (
        f"skill {skill_name!r} body does not cite a `stackunderflow` CLI command"
    )


def test_no_unexpected_skills_shipped() -> None:
    """Catch accidental sibling SKILL.md files that weren't reviewed."""
    if not SKILLS_DIR.exists():
        pytest.skip("stackunderflow/skills/ does not exist")
    seen = {p.name for p in SKILLS_DIR.iterdir() if p.is_dir()}
    expected = set(EXPECTED_SKILLS)
    extras = seen - expected
    assert not extras, (
        f"unexpected skill directories shipped: {sorted(extras)}. "
        f"Add to EXPECTED_SKILLS or remove."
    )
