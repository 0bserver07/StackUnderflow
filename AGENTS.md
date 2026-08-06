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

**Versions are maintainer-only. Agents NEVER change a version, anywhere, for any
reason.** Off-limits: `stackunderflow/__version__.py`, `pyproject.toml`,
`stackunderflow-ui/package.json` and `package-lock.json`, `flake.nix`, CHANGELOG
`## [N.N.N]` headings, git tags, GitHub releases, PyPI publishes, `release:`
commits. Not even a `-dev` suffix. *Suggesting* a version number for finished
work counts as a violation.

What the number should be, and when it moves at all, is the maintainer's
decision alone — this rule constrains agents, not the maintainer. "Wrap this
up" / "prepare the release" / "do the docs" means: put notes under
`## [Unreleased]` and STOP.

Why this exists: agents inflated 0.8 → 0.9 without approval (commit `bed5923`,
2026-05-15, twelve hours after 0.8.0 shipped) and executed the v0.9.2 release
commit (`59eb59a`) after being told to stop. PyPI never lets a version number
be reused, so an unapproved bump is permanent damage.

Enforced mechanically: `tests/stackunderflow/test_version_guard.py` pins the
exact version strings in the tree — ANY change to them fails CI unless the
maintainer updates the pin in the same deliberate release commit.

A version number is not progress. Roadmap issues #86–#104 sat open across four
releases while the number climbed. Never let the version imply verified, closed
work.

## Push & history rules — HARD RULE (2026-08-06)

**Origin pushes are maintainer-word-only. Force-pushing a public or shared
branch is banned for agents, always, no exceptions** — including "cleanup",
"squash", or any publication plan. History rewrites happen on a scratch
clone, get verified, and wait for the maintainer's explicit word.

**No infrastructure details in tracked files, ever**: hostnames, tailnet
names, IPs, SSH ports, home paths, spend figures. Operational/fleet state
files are gitignored (see .gitignore) and stay local. Why: on 2026-08-05 a
publication pass found a live tailnet SSH URL and spend totals committed in
campaign records, and the recovery from the resulting history surgery cost a
night. The store is local-first; the repo must be too.
