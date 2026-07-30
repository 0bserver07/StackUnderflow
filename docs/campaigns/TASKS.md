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
| `/api/projects?include_stats=true` | **0.042–0.059s** | — | was >180s HANG; fixed `8a83ccb` (scoped fallback, list-subquery shape), measured on a fresh instance, HTTP 200, all 303 rows with stats |

**Finding worth a task: `/api/stats` costs ~4s on every single call and does not
cache**, while `/api/cost-data` drops to 84ms warm. On the biggest project this is
the slowest thing in the dashboard and the most obvious perf win available.
Not yet investigated; not yet filed as an issue.

Caveat: these are Ubuntu-20.04-on-`/dev/sda2` numbers. They have **not** been
compared against the same calls on the Mac, so read them as absolute, not as a
regression.

---

## 4b. P0 wedge — mart-gap root cause (measured 2026-07-29, read-only)

Live: 334 projects, 243 mart rows, 91 uncovered. Marts are NOT lagging
(watermark == max event id). Coverage ⟺ has-usage_events, perfectly:

- **62** claude `legacy-` history pseudo-sessions — `adapters/claude.py:129-206`
  yields user-role-only records from `history.jsonl`; structurally unbillable
  but 37 carry real prompt text (5,244 user messages total).
- **13** antigravity — deliberately exempt (`capabilities.json
  emits_usage_events:false`), no normalizer; all 412 messages role=user.
- **12** codex ghost projects — **silent data loss**: rollouts with no `cwd`
  get `codex-<uuid>` slugs, `ingest/writer.py:108` upserts project/session
  BEFORE reading records, `:177` marks zero-record files fully processed,
  `ingest/__init__.py:53` never revisits. 60–130KB files → 0 messages, forever.
- **4** normalizer-guard drops (`etl/normalize/claude.py:44,48,59-70` silent
  bare returns — synthetic model / zero tokens).

Fix direction (in progress): seed `project_mart` from the `projects` table
(zero-rows for unbillable projects) in `etl/marts/project.py` refresh +
rebuild, guarding `affected` so the seed never re-runs dims for covered rows;
surface `projects_without_mart` in `etl/status.py`; writer upserts only after
the first yielded record. NOTE: `etl backfill --force` is UNSAFE with a live
server (no lock fencing; deletes all events+marts); safe entrypoints are
no-force CLI backfill or `POST /api/etl/backfill` (single-job slot).

Pipeline discoveries (listed, not fixed — future candidates):
- `messages` is a UNION ALL view over 16 partition tables; scoped predicates
  are NOT pushed down (scoped bulk helper still 0.86s vs 0.95s unscoped, vs
  mart read at 0.001s). Scoping alone cannot reach the <500ms fallback target.
- `_refresh_message_dims` re-runs classifier+enricher+command analysis over
  EVERY message of each affected project on every ingested file.
- `marts/project.py:87` INSERT OR REPLACE writes 13/25 columns then restores
  dims in a second statement outside the ingest transaction — real window
  where dashboards read zeroed dims.
- `watermark.refresh_all_marts` has no per-mart try/except; one failure aborts
  the rest and the caller logs at DEBUG.
- 12 ghost projects still count toward `total_count` in `/api/projects`;
  purging existing ghost rows is a maintainer decision (fix only stops new ones).

## 4c. Cost-integrity findings from the wedge fix (2026-07-29, recorded not fixed)

- **`daily_mart` and `bulk_project_cost` disagree on unknown models**:
  `claude-opus-5` (411 msgs) / `claude-sonnet-5` (41) sit at $0 in the mart
  (`cost_source='unknown'`) while the bulk path prices the same rows at the
  Anthropic manifest fallback — a project's cost changes with mart coverage.
  Same for `grok-*`. Needs one policy, probably the mart's honest-$0.
- **No `grok` pricer exists** — `get_pricer("grok")` silently returns the
  Anthropic pricer (reads $0 today only because the grok normalizer emits
  zero tokens).
- `build_enriched_dataset` (`queries.py`) and `get_project_messages_page`
  still hardcode `provider or "anthropic"` into `Record`s — pipeline-path
  vendor-vs-tool confusion, untouched by `8a83ccb`.
- The 91 uncovered projects contain **zero priced messages** (scoped
  provider×model cross-tab returns 0 rows) — the old full scan bought
  literally nothing.
- tmos-hq: the two hook latency-budget tests
  (`test_handlers.py::TestLatencyBudget`) are bimodal under IO load / cold
  caches — 618–1012ms p99 cold vs <50ms warm, reproduced on pre-change HEAD.
  Environment floor on this box, like the git-2.25.1 worktree pair.

## 5. In flight / uncommitted

- `docs/specs/agent-remotes.md` — untracked; federation proposal dropped by
  the Mac agent 2026-07-29 (query/observe/message other machines' stores in
  place). Provider-general delivery-tier addendum discussed, not yet written.
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
- Project `.claude/settings.json` hooks run bare `stackunderflow hooks run …` —
  measured 2026-07-29: they DO fire on this box (the earlier "won't fire on
  Linux" note was wrong), resolving to a **global** install at
  `~/.local/bin/stackunderflow` (0.9.2-dev.003) with no `STACKUNDERFLOW_HOME`.
  The SessionStart hook therefore recreated `~/.stackunderflow/store.db`
  (22:04:29) even though the server runs `--data-dir`. Until the hooks are
  repointed at the venv binary with `STACKUNDERFLOW_HOME=<dataset>` exported,
  the "`$HOME/.stackunderflow` must not exist" invariant cannot hold here.
- The untracked root-level `TASKS.md` (byte-identical duplicate of this file)
  present at session start on 2026-07-29 vanished from disk ~22:20 the same
  evening; no session command deleted it and no test references it. Content
  preserved here. Deleter unidentified — flagged to the maintainer.
