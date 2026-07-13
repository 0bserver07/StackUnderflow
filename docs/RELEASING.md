# Releasing

One command, one number you type. Nothing picks the version but you.

```sh
scripts/release.py X.Y.Z             # dry run — validates, prints the plan, changes nothing
scripts/release.py X.Y.Z --execute   # makes the atomic release commit + tag, locally only
```

That's the whole flow. You never hand-edit the five version files, the guard
test's `PINNED` dict, or the CHANGELOG heading — the script does all of it in
one commit so they can never drift out of sync.

## Why it can't inflate the version

The version is **maintainer-only** (see `AGENTS.md` — agents changed it without
approval three times, and PyPI never lets a number be reused). This script makes
that mechanical rather than a matter of trust:

- **It has no code path that invents a version.** No argument → it refuses. It
  never defaults, computes, or "bumps" — the number only ever comes from what
  you type.
- **It refuses anything not strictly greater** than the latest released tag, and
  refuses a tag that already exists. A typo that goes backward, sideways, or
  re-uses a burned number is rejected before anything changes.
- **Dry run by default.** Without `--execute` it validates and prints; it touches
  nothing.
- **It stops before anything irreversible.** `--execute` makes a *local* commit
  and tag only — it does not push and does not publish.

## The last two steps are yours, on purpose

PyPI upload is triggered by *publishing a GitHub Release*
(`.github/workflows/publish.yml`), so the irreversible action stays a deliberate
human click. After `--execute`, the script prints exactly what remains:

```sh
git push origin main && git push origin vX.Y.Z
gh release create vX.Y.Z --title vX.Y.Z --notes-from-tag --draft
```

The release starts as a **draft**. Nothing reaches PyPI until you publish it.
Before you push, everything is reversible:

```sh
git tag -d vX.Y.Z && git reset --hard HEAD~1
```

## Between releases

Finished work goes under `## [Unreleased]` in `CHANGELOG.md` and stops there —
that is the staging area the release step consumes. Writing notes is not
releasing; the version only moves when you run the command above with a number.
