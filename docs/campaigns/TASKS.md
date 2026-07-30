# Central task index

The one list. Sessions come and go and the in-session TaskCreate list gets wiped
(it happened 2026-06-15); this file is the durable aggregate.

**Three task surfaces exist. This file is the index over all of them:**

| Surface | Holds | Authority |
|---|---|---|
| GitHub issues | roadmap specs, waves, sizes | public plan of record |
| `docs/campaigns/*.md` | per-campaign specs + handoff detail | working detail |
| this file | status of everything, and what to do next | **start here** |

Rule: when a session finishes work, update this file. When it starts, read it.
Never trust the ephemeral task list to survive.

---

## 1. GitHub issue reconciliation — 6 of 9 open issues look shipped

Nine issues are open. Cross-checking each against `CHANGELOG.md [Unreleased]` and
the actual tree found implementations on disk for most of them. This is the
pathology the repo's own AGENTS.md warns about: *"Roadmap issues #86–#104 sat open
across four releases while the number climbed."*

**Evidence is "implementation exists," not "spec satisfied."** Each row below needs
a close-out pass — read the spec, prove it against the code, then close with that
evidence. Do not bulk-close.

| Issue | Spec | Evidence found | Verdict |
|---|---|---|---|
| **#100** | Spec 28: multi-device sync | `CHANGELOG [Unreleased]` credits it explicitly: *"Multi-device sync (issue #100, opt-in)"*, `v028`/`v029` migrations | **close with evidence** |
| **#97** | Spec 27: active-surfacing / proactive nudges | `CHANGELOG` credits it: *"Proactive nudges (issue #97, opt-in, default-off)"* | **close with evidence** |
| **#98** | Spec 25: fork mode | `stackunderflow/routes/forks.py` exists; `/api/forks` referenced in changelog | verify → likely close |
| **#99** | Spec 26: comparative benchmark engine | `routes/benchmark.py`, `services/benchmark_stats.py`, `tests/…/test_benchmark_route.py`, `docs/specs/benchmark-engine.md` | verify → likely close |
| **#95** | Spec 23: LLM-graded session quality | rubric in `cli.py`, `services/benchmark_stats.py`, `reports/benchmark.py`, `docs/specs/benchmark-rubric-v1.md` | verify → likely close |
| **#102** | Spec 30: beta-normalizer fixtures | `tests/fixtures/beta_normalizers/` has per-provider packs; changelog: *"loads every provider's fixture from checked-in data packs"* | verify → likely close |
| **#101** | Spec 29: Windows test-fixture port | `sys.platform` tests exist; `docs/windows-support.md` present. Partial — the *matrix* port is the ask | **genuinely open** |
| **#94** | Spec 22: outcome attribution v2 (sessions → commits → PRs → CI) | git-log correlation exists for productive/abandoned, but PR/CI/downstream legs not found | **genuinely open** |
| **#103** | Roadmap umbrella: 14 specs in 6 waves | — | keep open until the rest close |

Next action: close-out pass on #100 and #97 first (changelog already names them),
then verify #98/#99/#95/#102.

---

## 2. Active campaigns

| Campaign | Doc | Status |
|---|---|---|
| Brand & site → `stackunderflow.run` | `docs/specs/brand-and-site.md` | research + decisions done; **nothing built** |
| Intelligence layer | `docs/campaigns/intelligence-layer.md` | committed/durable; foundation landed |
| Cost audit | `docs/campaigns/cost-audit.md` | untracked working doc |
| UI perf audit | `docs/campaigns/ui-perf-audit.md` | untracked working doc |
| Perf hot path r2 | `docs/campaigns/perf-hot-path-round-2.md` | branch `perf/hot-path-round-2` exists |

### Brand & site — next steps (from `docs/specs/brand-and-site.md` §7)

1. Positioning propagation — one canonical line into README, docs-site `index.md`,
   `astro.config.mjs`, og tags, GitHub description; fix 17 → 20 and the duplicated
   `<title>`. Cheap, unblocked.
2. Identity — commit the verified token set, redraw the mark as SVG, unify the two
   favicons, pick the display face *(needs the maintainer's eye)*.
3. Site scaffold — Astro + Starlight at `/docs/`, Netlify, DNS for the domain.
4. Landing page, component per section.
5. The ⌘K memory-query palette — the differentiating build.
6. `llms.txt`, changelog route, sitemap, Umami.

---

## 3. Carried-over engineering backlog

From the durable roadmap in the project memory dir (`roadmap.md`, dated
2026-06-18/19). Several items may have landed in the large `[Unreleased]` block —
**this section needs a reconciliation pass of its own** before anyone works from it.

- **Pricing spine** — collapse `RATE_CARD` / `_CANONICAL_IDS` / manifest / overlay
  into one effective-dated price book in `store.db`. Partly done: manifest is now
  authoritative in several paths, `pricing doctor` exists. Known remaining edge,
  flagged in the changelog: undated `rate_card` book rows outrank dated manifest
  rows on the store-backed path, so server-primed pricing of newly ingested
  pre-boundary events uses the current rate.
- **Grade fabrication cleanup** — the GET-write / fabricated-5.0 bug is fixed and
  tagged with `grade_source`, but **pre-existing all-5.0 rows were never purged**
  and are indistinguishable without a legacy source marker. One-time purge or a
  legacy column. Still open.
- **Adapter defensive coverage** — 27 malformed-fixture regression tests landed
  across 10 adapters. Verify whether the remaining adapters are covered.
- **Perf** — see §4; measured numbers below supersede the roadmap's claims.

---

## 4. Measured performance (tmos-hq, 2026-07-29)

Against the real store: 348,452 messages / 3,396 sessions / 305 projects /
208,905 usage_events. Server bound to the tailnet IP on port 8095.

| Endpoint | cold | warm | note |
|---|---|---|---|
| `/` | 1.5ms | — | |
| `/api/projects` | 33ms | 13ms | 305 projects |
| `POST /api/project-by-dir` | 16ms | — | |
| `/api/search?q=…` | 6–14ms | — | FTS5 across all 348K messages |
| `/api/cost-data` | 4.23s | **0.084s** | cache works — 50× |
| `/api/stats` | 4.31s | **4.03s** | **no warm benefit** |
| `/api/stats?days=30` | 4.05s | 4.17s | capping days doesn't help |

**Finding worth a task: `/api/stats` costs ~4s on every single call and does not
cache**, while `/api/cost-data` drops to 84ms warm. On the biggest project this is
the slowest thing in the dashboard and the most obvious perf win available.
Not yet investigated; not yet filed as an issue.

Caveat: these are Ubuntu-20.04-on-`/dev/sda2` numbers. They have **not** been
compared against the same calls on the Mac, so read them as absolute, not as a
regression.

---

## 5. In flight / uncommitted

- `docs/specs/brand-and-site.md` — untracked
- `docs/specs/agent-egress-audit.md` — untracked, predates the brand work (unread)
- `docs/private/brand-site-research.md` — gitignored by design
- `.gitignore` — one line (`docs/private/`)
- `stash@{0}` — `session-leftover-contamination`, orphaned from the deleted
  `feat/drop-playback-agents-beta` branch. 15 files, +74/−639; deletes a stale
  built asset and touches 11 adapter defensive tests. **Triage or drop it.**
- `main` is in sync with `origin/main`; nothing unpushed.

## 6. Environment notes

- Dev now also runs on **tmos-hq** at
  `/media/tmos-bumblebe/dev_dev/year26/jul26/StackUnderflow`, dataset symlinked
  from `~/.stackunderflow` to the media volume.
- That box needs **git ≥ 2.28** to pass the full suite — `init.defaultBranch`
  didn't exist before it, so 2 `test_worktrees.py` tests fail on Ubuntu 20.04's
  git 2.25.1. Currently 4135 passed / 2 failed / 13 skipped.
- History there is filed under the **old** `jan26` slug; pass
  `--project '-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow'` to reach it.
- Project `.claude/settings.json` carries macOS-absolute hook paths — hooks will
  not fire on Linux until repointed.
