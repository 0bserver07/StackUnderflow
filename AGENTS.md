<!-- stackunderflow:guide:start -->
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
read-only — nothing leaves the machine.
<!-- stackunderflow:guide:end -->

## Versioning — HARD RULE, zero tolerance (agents have violated this three times)

**Agents NEVER change a version, anywhere, for any reason.** Off-limits:
`stackunderflow/__version__.py`, `pyproject.toml`, `stackunderflow-ui/package.json`
and `package-lock.json`, `flake.nix`, CHANGELOG `## [N.N.N]` headings, git tags,
GitHub releases, PyPI publishes, `release:` commits. Not even a `-dev` suffix.
*Suggesting* a version number for finished work counts as a violation.

Only the maintainer decides and executes releases. "Wrap this up" / "prepare the
release" / "do the docs" means: put notes under `## [Unreleased]` and STOP.

**The version is FROZEN at 0.9.x.** The 0.8 → 0.9 jump was agent inflation
(commit `bed5923`, 2026-05-15, twelve hours after 0.8.0 shipped) and it is
published on PyPI — a version number there can never be reused, so it is
unrecallable forever. If the maintainer cuts a release, the increment is the
smallest possible (0.9.2 → 0.9.3 → 0.9.4 …). The minor digit never moves again
unless the maintainer types it personally.

This freeze is enforced mechanically: `tests/stackunderflow/test_version_freeze.py`
turns CI red if any version-bearing file leaves 0.9.x. Raising its
`ALLOWED_PREFIX` is a maintainer-only edit.

A version number is not progress. Roadmap issues #86–#104 sat open across four
releases while the number climbed. Never let the version imply verified, closed
work.
