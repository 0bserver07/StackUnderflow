# The decommission report — DRAFT

**Wave 10, item RS-10-004.** The evidence the maintainer reads to decide whether
the Python implementation is retired, kept, or kept in part.

**Snapshot:** branch `rust`, worktree `../StackUnderflow-rust`, **HEAD
`a40db22`**, 2026-08-01. Every count below is either re-derived from the tree at
that commit by a command reproduced here, or quoted from a named proof artifact
with its line. Numbers that could not be traced to either are **flagged, not
copied** — they are listed in Appendix A.

**What this document is not.** It is not a recommendation to flip, and it
contains no version number, tag, or release timing: the flip decision and any
versioning are the maintainer's alone (`docs/specs/rust-port.md` §5, CLAUDE.md).
Section 6 describes what a soak would have to *cover* to close RS-10-002; it
does not schedule one.

**Worktree caveat, stated first because it bounds everything else.** At the time
of writing, the worktree is **dirty with five concurrent in-flight batches**
(`BATCH-SSE-CLAIM.md`, `BATCH-T4-CLAIM.md`, `BATCH-TEAMS-CLAIM.md`,
`BATCH-W7-CLAIM.md`, plus the tranche-2 CLI writers and a tranche-3
`stax-reports` crate extraction). Nothing uncommitted is counted as coverage in
§1. No gate was re-run for this report — a `ci.sh` run against a dirty tree
would not be measuring HEAD, and the campaign's own law (gate *at* the commit)
forbids reporting it as if it were. Every gate result below is **as last
recorded**, with the commit that recorded it.

---

## 0. Method — what was re-verified, and what was quoted

| class | treatment |
|---|---|
| Route/endpoint coverage | **re-derived** at HEAD: Python decorators parsed out of `stackunderflow/routes/*.py` + `server.py`; Rust `.route()` registrations parsed out of `rust/crates/stax-server/src/**`; case rows parsed out of `git show HEAD:rust/parity/endpoint-cases.txt`. Four claimed-absent endpoints confirmed by reading each route module's own status table. |
| CLI coverage | **re-derived** from `rust/parity/CLI-INVENTORY.md`'s master table (status column), then corrected against the actual `Command` enum in `git show HEAD:rust/crates/stax-cli/src/lib.rs`. |
| Case-matrix sizes | **re-counted**: `endpoint-cases.txt` 716 rows / 70 `!` rows; `cases.txt` 202 rows (2 Rust-only); `hook-cases.txt` 80; `sync-cases.txt` 173. |
| Item ledger | **re-counted** by regex over `rust/TASKS-RS.md` (`^- \[.\] RS-[0-9]`). |
| Divergence ledger | **re-parsed**: 224 ids carry a table row; dispositions classified mechanically. |
| Tallies (`648 identical`, `192/192`, `80/80`, `280+4`) | **quoted** from the landing commits / `ARCHITECT-STATE.md` / the `*-DIFFER.md` procedures. Not re-run. |
| Perf rows | **quoted** from `rust/PERF.md`, which carries the command for every row (§5's law). Not re-run. |
| Test counts | **quoted**. `cargo test` was not run; `#[test]` attribute counts are given in §1.6 as a weak cross-check only. |

---

## 1. Coverage map

### 1.1 The CLI surface

The denominator is `rust/parity/CLI-INVENTORY.md`, which is *generated from the
live `click.Command` objects*, not from prose: **105 nodes — 23 groups
(including the root) + 82 leaf commands — and 276 declared parameters**, against
Click 8.4.2 / CPython 3.12.13 / `cli.py` at 6,484 lines.

| status | nodes | source |
|---|---:|---|
| PORTED (wave 1) | 12 | inventory §1.1 |
| TRANCHE-1 (wave 8) | 17 | inventory §1.1 |
| PARTIAL (the root group) | 1 | inventory §1.1 |
| **ported at HEAD, per the inventory** | **30** | |
| `sync` group + 4 leaves — **shipped after the inventory was generated** | **+5** | `stax-cli/src/sync.rs` at HEAD; landed `ce6682e` |
| **ported at HEAD, corrected** | **35 / 105 (33 %)** | |
| UNPORTED | 70 | |

The 35: `memory` + its 5 verbs, `resume`, the 4 `find-*`/`search-past-decisions`
aliases, `cfg` + 6, `config` (hidden compat) + 3, `clear-cache`, `backup`
(`list`, `verify` only), `status`, `sync` + 4, and the root group.

Two Rust verbs have **no Python counterpart** and are therefore outside the
105-node denominator entirely: `stax store` (the schema/row-count reader, renamed
from `status` under DIV-025) and `stax anchor` (the maintainer-ordered
agent-continuity surface, RS-1-029..033). Decommissioning Python does not lose
them; nor does keeping Python provide them.

**Proof pointer.** `rust/parity-cli.sh` (ci.sh gate 4) — 202 case rows, 2 of them
Rust-only self-checks, run against **both** store states (`fresh`, `fts`):
**400 pass + 4 maintainer-accepted, 0 FAIL**, last recorded at `e374fed`. The 4
accepted are `V-dec-limit-huge` and `V-dec-limit-bad` × 2 states (desk ruling 2,
DIV-010's `>u64` clamp residue); the harness names them every run and prints a
*re-examine* warning if one ever starts passing
(`git show HEAD:rust/parity-cli.sh`, lines 336 / 341 / 350).

**Honest gap in that gate:** the Rust side's stdout is normalised on `Usage:` and
`Try '…'` lines only, substituting the program name — scoped deliberately after a
blanket substitution rewrote real store content and produced a false diff
(`ARCHITECT-STATE.md`, "HARNESS LESSON"). Those bytes are therefore *not* under
test on those two line shapes.

**`--help` is contract-clean, not byte-clean.** `rust/help-tree.sh`: **0 / 30
ported nodes byte-identical, 30 / 30 contract-clean** (same summary, same
options, same subcommand list), with eight structural clap-vs-Click template
differences enumerated in `rust/parity/HELP-TREE.md`. This is **DIV-240**, on the
maintainer's desk (rulings request item 7).

### 1.2 The HTTP surface

Re-derived at HEAD:

| measure | count | how |
|---|---:|---|
| Live route+method pairs in Python | **101** | 93 `@router.<method>` + 1 `@router.api_route` expanded ×4 + 4 SPA routes in `server.py` (the `/static` mount is additional) |
| Mounted in the axum router | **97** | `.route()` registrations parsed from `stax-server/src/**` |
| **Not mounted** | **4** | named below |
| Case rows in `parity/endpoint-cases.txt` | **716** | 598 GET / 78 POST / 21 DELETE / 18 PUT / 1 PATCH, across 145 distinct concrete paths and 40 id groups |
| Rows marked `!` (known-open) | **70** | |
| Last recorded verdict (`c1bea2d`, battery-confirmed exit 0) | **648 identical · 0 divergent · 68 known-open** | `ARCHITECT-STATE.md` |

Per-endpoint coverage against that matrix:

| | endpoints | note |
|---|---:|---|
| ≥ 1 **gating** (non-`!`) row | **86** | the endpoint is byte-compared every gate run |
| **only** `!` rows | **3** | `POST /api/meta-agent/chat`, `GET /api/sync/status`, `GET /api/worktrees` |
| **no row at all** | **12** | 10 with a recorded reason, 2 without — see below |

The 10 no-row endpoints with a recorded reason are all writers or network calls
that the shared-home sequencer cannot hold (`DIV-059`, `DIV-078`, `DIV-136`,
`DIV-146` — a `!` row still *issues* the request, and a non-terminating body
exits the run at 2). Each was instead proven by an **isolated procedure**, all
five of which were written *and run*:

| procedure | result |
|---|---|
| `rust/ETL-BACKFILL-DIFFER.md` | green — 5 `422` bodies byte-identical; stores identical after incremental **and** `force=true` (10/10 tables incl. `cost_usd`); the 409 leg passes on both sides |
| `rust/SEARCH-REINDEX-DIFFER.md` | green — `sqlite_master` identical (42 objects, `sql` text included), 2,284 rows, full dump md5 identical **with `id`** |
| `rust/QA-REINDEX-DIFFER.md` | green — 257 rows, dump md5 identical with the SHA-256 content `id` |
| `rust/TAGS-REINDEX-DIFFER.md` | green — `tags.json` byte-identical, 12,101 bytes |
| `rust/PRICING-REFRESH-DIFFER.md` | **split, honestly** — deterministic set 6/6 identical; fetching set 4/4 divergent, as DIV-199 predicted |
| `rust/REFRESH-DIFFER.md` | closed DIV-127 by *probe* — and found the transcribed 422 body one-third wrong |
| `rust/parity/SSE-PROBE-d.md` | `/api/live/stream` probed by hand (status, headers, both frame shapes, 6 s cadence, non-termination) because a row for it would hang the run |

Idempotence was proven on all three reindex writers, both sides
(`messages.id` advances `1..2284` → `2285..4568` identically), satisfying
DIV-146's law.

**The two that have no row, no reason, and no implementation** — this is a gap
the 716-row tally cannot see, filed here as **DIV-341**:

* `POST /api/project` (RS-5-095) — open, needs `infra/discovery.locate_logs`.
* `GET /api/global-stats` (RS-5-101) — open, needs `queries.get_global_stats`.

Both are recorded as `**open**` in `routes/projects.rs`'s own module doc table,
so nobody hid them; but they are absent from the case file entirely, carry no
`!` row and no DIV, and the matrix therefore reports `0 divergent` while two
endpoints answer 404 on the port. The other two unmounted routes *are* visible:
`GET /api/live/stream` (DIV-165 — the mount is one manifest line; a batch is in
flight on it) and `POST /api/meta-agent/chat` (DIV-138, deferred whole with two
`!` rows).

### 1.3 The pipeline stages

Every stage below closed against the maintainer's real data on a snapshot copy
(§5: the live store is read-only to the campaign).

| stage | proof | scale | verdict |
|---|---|---|---|
| Store open / read | `stax status` vs an inline `sqlite3` reference | 52 objects, 383K-row `messages` view | byte-identical, md5 `68e6e552…` ×3 runs (PERF.md §wave 0) |
| Envelopes (`stackunderflow.memory/1`, `.resume/1`) | golden fixture runner | 31/31 | byte-exact (`eaf60c0`) |
| Adapters (enumerate + parse) | `stax-adapter-parity`, ×20 providers | 55,647 live records | cmp-clean (`2cbbf22`) — see Appendix A, DIV-345 |
| Pricing engine | price-book comparison | **22,217 comparisons** | zero divergent, bit-for-bit (`401744f`) |
| Float rendering | `pyjson` / `repr_float` sweep | **11,635,636 doubles** | 0 divergent after DIV-008's half-to-even fix (`56c5fe5`) |
| Normalizers (20) | `stax-normalize-parity pass` + `dump` | **231,718 events / 383,293 messages seen** | `cmp` identical (`2a23be9`) |
| Marts (8 builders) | rebuild-vs-rebuild, one dumper over both sides | **10/10 tables, 131,582 rows, 0 row diffs** | identical **to the bit** — five marts and `usage_events` land on the same `SUM(cost_usd)` bit pattern `0x40e4cef6405c942b` ($42,615.695356645) (`0995d70`) |
| Stats aggregator | `stax-stats-parity` | 18/18 collector blocks, 298 projects | byte-exact (`b23217b`) |
| Ingest (writer, watermarks) | `ingest-parity.sh`, full-row diff of 5 tables | 4 corpora up to **1.02 GB / 216 files / 56,329 messages / 35,633 events** | all IDENTICAL, idempotent both sides (`b2ab3e1`) |
| Ingest (live tail) | `ingest-tail-proof.sh`, 7 quiesced rounds | min 202.1 / median 202.4 / **max 224.9 ms** | PASS on the max against the 400 ms budget; watermark + all 5 tables identical |

**Not ported: the schema itself.** All **29 migration items are open**
(`RS-0-011..055`; 27 SQL + 2 Python data migrations, v001–v030 with v015 absent).
The wave-4 landing note states the consequence plainly: *"the Rust binary cannot
yet create a store, only fill one"* — the gate mints both stores with Python and
copies one seed to each side. `DIV-216` records the same gap on the sync path
(`stax sync` on a store missing the v028/v029 tables raises where Python
self-heals). A wave-7 batch is in flight on this; **it is not in HEAD.**

### 1.4 The sidecars

| sidecar | proof artifact | result |
|---|---|---|
| Hooks (9 entry points) | `rust/hooks-parity.sh`, 80 recorded invocations | **80 identical / 0 divergent / 0 known-open**, plus `captured_events` (22 rows) and `proactive_state.json` diffed between implementations (`2a26efd`) |
| Sync (`stax-sync`) | `rust/sync-parity.sh` — 173 corpus rows + 9 cross-impl `age` interop rows + 10 CLI-verb rows | **192 identical / 0 divergent / 0 skipped** (`ce6682e`); `endpoint-parity.sh --only SY-` 5 identical / 2 known-open, both differing *only* in their own wall-clock stamp |
| Search / QA / tags / bookmarks | endpoint matrix `X-*`, `Q-*`, `T-*`, `B-*` groups + the four reindex procedures | `X-*` re-run on the `fts` state (250,998-row index): **19/19 identical**, real bm25 `rank` floats and real `snippet()` output byte for byte |
| Anchor (Rust-only) | concurrency repro | **192/192 writes** under the failing repro (`c6a9237`) |

Neither the hooks differ nor the sync differ is a `ci.sh` gate, by a recorded
rule: the threshold was 10 s and they measure 36.7 s and higher (`ci.sh`
header). Their per-commit half is `cargo test`.

### 1.5 What is not ported, by design

`docs/specs/rust-port.md` §1: the React UI (it *is* the parity oracle and stays
TypeScript), `docs-site`, skills content, packaging metadata. Add to that list,
from the ledger: the `_install.py` / `_repair.py` hook installers (install-time,
not hook-budget — RS-8-004/005), the `S3ObjectStore` transport (DIV-213), and
`services/support_matrix.py`'s introspection half (Rust's registry is
compile-time).

### 1.6 The honest gaps

**a) In-flight, not in HEAD.** Five batches are mid-air in the worktree: the SSE
mount (DIV-165), CLI tranche 2 (backup writers, `guide`, `hooks` verbs), tranche
3 (a `stax-reports` crate extraction out of `stax-server`), tranche 4 (skills /
docs / recommend), the teams ingest hook (DIV-042 / RS-2-004), and wave 7 (the
migration runner + `init` / `start`). **None of it counts toward any number
above.** A decision taken on this report is a decision about `a40db22`.

**b) Wave 9 is a stub.** `crates/stax-wasm/src/lib.rs` is 11 lines of charter and
`#![forbid(unsafe_code)]`. There are **zero criterion benches anywhere in the
workspace** (verified: no `criterion` dependency in any manifest, no `benches/`
directory). Spec §6 says the criterion gates *replace* the Python suite's 4
load-sensitive perf-budget tests; that replacement does not exist, so retiring
those tests would retire the coverage with them. Filed as **DIV-349**.

**c) 70 `!` rows, and the tally can drift.** A `!` row that happens to agree is
scored `Identical`, not `KnownOpen` (`parity/src/endpoints.rs:179` returns before
the `known_open` check). So the last recorded `648 identical / 68 known-open` of
716 is consistent with 70 `!` markers — two `!` rows currently agree and are
counted as wins. The consequence: the known-open count moves without a case-file
edit, and "identical" includes rows nobody has promoted. **DIV-348.**

**d) Wall-clock rows can never flip.** Batch E's headline: **13 of 28 known-open
rows could not flip, and not one for a porting reason.** Five fields are wall
clocks or set-ordered:

| row(s) | field | source |
|---|---|---|
| `!LV-stats`, `!LV-stats-tz` | `burn.ts` | `services/live.py:346` |
| `!PT-patterns`, `!PT-patterns-window` | `window.since` (µs) | `reports/patterns.py:653` |
| `!W-worktrees` | `scanned_at` | `routes/worktrees.py:105` |
| `!J-compare` | `diff.tokens` **key order** | `routes/sessions.py:405` |
| `!QL-quality-real` | `graded_at` | `services/grading.py:200` |

Every one was still proven **off the matrix** by its owner: `live` byte-identical
at eight tz offsets on every field but `.burn.ts`; `quality` 331 of 368 bytes
with key order equal; `compare` every byte but the set order, three times;
`forks` byte-identical on the whole store (383,580 messages, 4,949 abandoned
branches); `agent_teams` six payloads byte-identical; `benchmark` 11 CPython
calls identical on first execution. The proof moved; it was not abandoned.

**e) Unported branches inside ported code.** The clearest is **DIV-214**: `sync
pull`'s warnings interpolate CPython exception text, so `str(JSONDecodeError)` is
a byte contract. The `Expecting value` family **is** translated (including
CPython's token-start back-up, where serde reports column 2 and CPython char 0)
and is proven by `E-json-*` / `V-pull-corrupt`. The **delimiter families**
(`Expecting ',' delimiter`, `Unterminated string starting at`, …) are **not** —
reproducing them means reproducing CPython's scanner state machine. They render
with a loud `<unported: …>` marker (`crates/stax-sync/src/pyerr.rs:81`) so a
corpus row that ever crosses one **fails** rather than quietly agreeing on the
wrong string. No corpus row crosses one today, which is exactly why the marker
exists.

**f) `status` ships with its arithmetic unexercised.** The parity states are a
snapshot whose newest events predate the current month, so `today` and `month`
both report `$0.00 (0 msg)`. The rows prove wiring, both output formats, flag
semantics and zero rendering — and nothing else. Closing it needs a fixture store
whose timestamps are generated *relative to the run clock*, because
`parse_period("today")` reads the wall clock.

**g) DIV-042 — teams metadata.** On the 1.02 GB corpus, Python fills team columns
on **41 of 162 sessions**; the port fills **0**. Counted and printed every gate
run (`deferred_hook.txt`), excluded from the `sessions` diff with the reason
stated. Closes with RS-2-004 + RS-5-025 — a batch is in flight.

---

## 2. The divergence ledger — digest

224 of the 226 DIV ids referenced in `rust/TASKS-RS.md` carry a ledger table row
(DIV-083 and DIV-084 are explicitly unused after a renumber; the ranges 060–063
and 219–229 were never allocated).

### 2.1 By disposition

Classified mechanically over the ledger rows:

| disposition | rows | note |
|---|---:|---|
| **no disposition field at all** | **109** | batches C (DIV-085..132) and E (DIV-153..209) use a 3-column `id / item / finding` table. See **DIV-342.** |
| not ported / deferred, with a reason | 27 | |
| recorded / inherited | 25 | |
| fixed-in-rust / fixed-at-source | 16 | |
| bug-for-bug | 15 | |
| **maintainer-pending** (desk / ruled / accepted) | 15 | |
| narrowing, recorded | 12 | |
| **`UNDECIDED-maintainer`** | **5** | the §6b pre-filed five: DIV-001..005 |

**This is the single most important structural finding in the ledger.** The
ledger's own declared row format is `| DIV-nnn | what | where | disposition |
evidence | items |`, and §6b permits exactly two end states (`bug-for-bug`,
`fixed-in-rust`) plus a transient the fleet "must not sit on". Half the rows
carry no such field. RS-10-003's gate — *"DIVERGENCE LEDGER has zero
UNDECIDED-maintainer rows, or each is on the maintainer-decision doc"* — cannot
be evaluated mechanically against the file as it stands.

### 2.2 The five pre-filed §6b divergences — still `UNDECIDED-maintainer`

All five are **implemented bug-for-bug and proven so**; what remains is whether
Python should be *fixed*.

| id | what | evidence at HEAD |
|---|---|---|
| DIV-001 | Frozen mart costs (`daily_mart.cost_usd` freezes the rate card at normalisation) | structurally out of the mart gate's way — the rebuild reads `usage_events.cost_usd`; `price_book` (105 rows: 32 live / 20 manifest / 53 rate_card) and `SUM(usage_events.cost_usd)` fingerprinted before and after on both sides, unchanged |
| DIV-002 | Classifier fall-through dims (`stats/classifier.py:174` → `"assistant"`) | reproduced exactly: **12,878** messages reach the fall-through, **7,343** with `role='user'`; **57 of 244** events-backed `project_mart` rows carry `total_commands = 0` (§6b predicted "57 of 243") |
| DIV-003 | `<synthetic>` folding into `by_model["N/A"]` | mart-gated summary path only |
| DIV-004 | Sub-second `until`-edge asymmetry | no occurrence in the real store; fixtures must not mint one |
| DIV-005 | Sign-inverted tz offsets from the React callers | **exercised and inherited faithfully** — `D-stats-tz` (−480) and `D-stats-tz-inverted` (+480, what the React callers actually send) are both byte-identical, so the port reproduces the wrong bucketing exactly until the frontend fix lands and both flip together |

### 2.3 The maintainer-pending list, cross-referenced

`rust/MAINTAINER-RULINGS-REQUEST.md` (created at HEAD, `a40db22`) consolidates
**16 decisions**. Mapping it to the ledger, and to what each unblocks:

| # in the request | DIV | what it unblocks | ledger cross-ref |
|---:|---|---|---|
| 1 | DIV-133 | trailing-slash 307 on all 93 endpoints; one `lib.rs` fallback | ✅ **collision resolved (DIV-340, closing pass)** — DIV-133 is now unambiguously the app-level row at `TASKS-RS.md:1518`; the `/api/sync/status` row is **DIV-358** |
| 2 | DIV-168 | `%2F` in path params — reproduced on three endpoints that are **green today** | `TASKS-RS.md:1946` |
| 3 | DIV-050 | CORS layer, currently unported | `:1302` |
| 4 | DIV-107 | `qs::opt_int` stricter than pydantic (`?at=3036.0` → 422 here, 200 there) | `:1492`, `:1553` |
| 5 | DIV-055 | the `/api/stats` memo — the only place the reference is faster | `:1316`, PERF.md §wave 5 |
| 6 | DIV-091 | `/api/plan` memo is **not** latency-pure (`date.today()` missing from the key) | `:1476`, `:1536` |
| 7 | DIV-240 | `--help`: accept contract-clean as the ruled bar | `:2090`, `HELP-TREE.md` |
| 8 | DIV-079 | `?per_page=0` → live `ZeroDivisionError` 500 in Python | `:1326` |
| 9 | DIV-067 | `/api/commands` prices every provider as `anthropic` | `:1313` |
| 10 | DIV-040 | $617.41 phantom add-only mart cost — **both implementations agree; the live store is wrong** | `:1290` |
| 11 | DIV-044 | per-partition `UNIQUE` duplicates on a month-moving rotation | `:1296` |
| 12 | DIV-042 | teams metadata 41/162 → 0/162 (code, in flight) | `:1294` |
| 13 | DIV-200 (hooks) | inject hooks open the store **read-write** from a path whose docstring says read-only | `:1796` — ✅ **collision resolved (DIV-340, closing pass)**: batch E's row at `:1978` is now **DIV-350** |
| 14 | — | re-confirm the 4 `>u64` clamp acceptances | `parity-cli.sh:418-434` |
| 15 | — | re-confirm depth ceiling 1024 | desk ruling 5 |
| 16 | DIV-195 | `endpoint-parity.sh` does not export `PYTHONHASHSEED` | `:1973`, §5 below |

**Additional maintainer-facing rows that are *not* in the 16** and would
otherwise be decided by silence:

* **DIV-016** — the primed-vs-unprimed price-book seam. Ruled bug-for-bug for the
  port; the real fix is **filed post-soak for both implementations** (desk ruling
  7).
* **DIV-201 (hooks)** — the capture hooks write the live store on **every**
  recorded fire, ported bug-for-bug. Measured cost on the agent's critical path:
  5.4 ms on tmpfs, **142 ms – 4.4 s on this host's `/media` spindle**. The
  question the ledger asks is whether a synchronous `INSERT` belongs on the hot
  path at all.
* **DIV-204 (hooks)** — the recall hook's 1.5 s deadline is a wall-clock race; on
  a loaded box it silently drops real warnings. Reproduced once in the differ
  under concurrent load.
* **DIV-215** — `_replicate_backup` drops `ConnectTimeout=10` from its `-e` ssh
  string, so `backup create --to` an unreachable host hangs where `sync push`
  fails in ten seconds; and the same function `shlex.quote`s the mkdir target but
  hands rsync a bare `host:dir/`, so a sync root with a space works for one leg
  and breaks for the other. Bug-for-bug, both halves. A one-line Python fix.
* **DIV-149 / DIV-150 / DIV-152** — three "identical helper" collapses the dedup
  pass deliberately did **not** make because the copies disagree (byte-slicing vs
  code-point slicing; truncating vs half-to-even microseconds; `days_in_month`
  returning 30 in one module and 0 in another for an out-of-range month, across
  ~30 sites in five crates).

### 2.4 Divergences the port *fixed*, i.e. where the port is no longer a drop-in

Worth naming because "drop-in" is the gate's own language and these are the
places it is deliberately false:

DIV-014 (deterministic enumerate order), DIV-023 (×3), DIV-043 (the watcher's
reader-generated-event filter — without it a spurious cycle raced an append and
**lost a record**), DIV-048 (`dumps_http`), DIV-049 (ryu kept out of the response
path), DIV-056 (the price-book seam — a silent **2.0 %** cost gap:
`568.59588725` vs `557.33358795` on `/api/stats`), DIV-057 (`sum([])` is `int 0`
— 6 of 14 red cases in one run), DIV-058 (`mimetypes` is a *resolution*, and the
host is an input), DIV-127 (the `/api/refresh` 422, closed by probe),
DIV-153 (the harness's own env asymmetry), DIV-200/hooks (read-only inject),
DIV-354/batchE (`GET /api/qa/reindex` 404 leg), DIV-234 (`backup verify --name
''`).

---

## 3. Performance — every number with its command

All rows below are quoted from `rust/PERF.md`, which carries the exact command
for each (§5: *a number without a command is not a number*). **Host:** tmos-hq,
i7-6700K @ 4.00 GHz, 8 threads, 46 GiB, Linux 5.4.0-216, rustc/cargo 1.97.1,
CPython 3.12.13. **Method:** `hyperfine 1.20.0 --warmup 3 --min-runs 20`, page
cache warm, Rust `--release` (`lto = true`, `codegen-units = 1`), medians.

### 3.1 The process floor — the structural result

| row | Rust | Python | ratio | command |
|---|---:|---:|---:|---|
| `status`, whole run | **14.67 ms** | 38.95 ms | 2.65× | `hyperfine … "$RS status"` vs the inline `sqlite3` reference reproduced verbatim in PERF.md |
| Process floor | **0.94 ms** | 166.23 ms | **176×** | `$RS --version` vs `stackunderflow --help` |
| CPython interpreter floor | — | 14.34 ms | — | `$PY -c pass` |

`stax status` (14.67 ms) costs about what CPython charges to start and do
*nothing* (14.34 ms). PERF.md states the comparison is deliberately generous to
Python: the reference is a bare `sqlite3` script that never imports the package,
paying the 14 ms interpreter floor instead of the 166 ms CLI floor.

### 3.2 Read paths (waves 1–3 landings; commands in the landing commits and `PARITY-*.md`)

| surface | Rust | Python | ratio |
|---|---:|---:|---:|
| `memory sessions` (20 rows) | 4.6 ms | 207.7 ms | 45× |
| `memory decisions` / `worked` (scan-bound) | 468–509 ms | 677–684 ms | 1.3–1.45× |
| `memory file` | 1.92 s | 2.35 s | 1.22× |
| `memory ask` (hybrid demo) | 580 ms | 1.108 s | 1.91× |
| `memory ask` intent gate | 1.2 ms | 162 ms | 131× |
| `resume` | 3.7 ms | 183.9 ms | 49× |
| adapters enumerate (211 sessions) | 2.0 ms | 77.9 ms | 39× |
| adapters full parse (55.7 K records) | 2.77 s / 46 MB | 4.78 s / 111 MB | 1.7× / 2.4× |

PERF.md's own honesty note: *"the 1.2–1.9× there is real; the 35–131× is the
process floor."*

### 3.3 Batch paths — where the port is *not* faster, and why

| pass | Python | Rust | ratio |
|---|---:|---:|---:|
| Normalize, full store (231,718 events) | 137.8 s | 107.3 / 108.8 s | 1.28× |
| `usage_events` dump (231,718 × 18 cols) | 1.87 s | 0.51 s | 3.7× |
| **Full mart rebuild, all 8** | **4,183.8 s** | **4,172.7 s** | **1.003×** |
| — `project` (the only host-side mart) | 27.4 s | 11.7 s | 2.34× |
| — `tool` | 870.0 s | 867.7 s | 1.00× |
| — `command` | 1,336.4 s | 1,361.2 s | 0.98× |
| — `daily` | 413.7 s | 450.2 s | **0.92× (slower)** |

**Measurement caveat, quoted because it changes how to read the table:** the two
mart runs were *concurrent on one box*, single-threaded each, same disk.
Contention was symmetric so the ratios are usable; **the absolute seconds are not
a clean-box number.** PERF.md's reading: *"a full mart rebuild is SQLite-bound,
not language-bound … Nobody should quote 1.003× as 'Rust is not faster' either —
it is the price of porting the query shapes faithfully (§6b), which is the whole
point."*

Re-measured after the stats-enricher change: `project` 11.7 → **11.9 s** (+0.2 s
= the enricher's whole bill), tables **10/10 identical** to the banked Python
dump, per-column sums identical to the bit.

### 3.4 Ingest

| corpus | files | messages | events | Python | Rust | ratio |
|---|---:|---:|---:|---:|---:|---:|
| fixtures only | 5 | 238 | 125 | 229 ms | 296 ms | **0.77× (slower)** |
| + 6 real projects | 12 | 365 | 201 | 356 ms | 208 ms | 1.71× |
| + 25 real projects | 88 | 17,241 | 10,714 | 144.6 s | 100.4 s | 1.44× |
| + 60 real projects (1.02 GB) | 216 | 56,317 | 35,627 | 765.8 s | 461.1 s | 1.66× |

Re-ingest of an unchanged tree is categorical: **1.7 ms for 88 files, 3.4 ms for
216** (the fast path is one `SELECT mtime, size` per ref).

Live tail, seven quiesced rounds: **min 202.1 / median 202.4 / max 224.9 ms**
against the 400 ms budget — PASS on the max with 175 ms of headroom. The floor is
a constant the design chose (`DEFAULT_DEBOUNCE = 200`), not an implementation
limit. PERF.md records that the *first* measurement was wrong (155 ms, below the
debounce floor — the append had landed inside a window an earlier event opened)
and that the harness now enforces quiescence per round and reports the max.

### 3.5 Server

`GET /api/projects`, warm server, median of 11 `curl` `time_total`:

| request | Python (uvicorn) | Rust (axum) | speed-up |
|---|---:|---:|---:|
| `?limit=20` | 11.29 ms | 5.29 ms | 2.13× |
| `?include_stats=true&limit=20` | 11.26 ms | 6.61 ms | 1.70× |
| `?include_stats=1&include_worktrees=1&limit=50` | 12.35 ms | 6.98 ms | 1.77× |

*Modest on purpose*: this endpoint was already fixed by the July campaign
(180 s hang → 12 ms), and the port reproduces those SQL shapes rather than
improving on them. PERF.md: *"A larger number here would mean the port had
changed a query shape — which is the thing §6b forbids."*

`GET /api/stats` — **the one place the reference wins**:

| request | Python | Rust | |
|---|---:|---:|---|
| first call (both cold) | 140 ms | **68 ms** | 2.1× the port's way |
| `?timezone_offset=-480` (both cold) | 102 ms | **43 ms** | 2.4× the port's way |
| `?days=7` (memo **warm**) | **6 ms** | 43 ms | **7× the reference's way** |
| `?details=true` (warm) | **12 ms** | 44 ms | 3.7× the reference's way |
| `?include=overview` (warm) | **5 ms** | 46 ms | 9× the reference's way |

The port is twice as fast at the *work* and several times slower at the
*request*, because Python's `_project_stats_cached` is an 8-entry cross-request
LRU. On the dashboard — which repeat-calls with the same tz — the user-visible
number is the warm one. **DIV-055**, rulings request item 5. Gate 6 proves the
payloads are byte-identical either way, so this is purely latency vs memory.

### 3.6 Hooks — the budget the port exists for

`hyperfine --warmup 3 --min-runs 20 --input <payload>.json`, whole process,
spawn to exit:

| hook fire | Python | Rust | ratio |
|---|---:|---:|---:|
| `inject-session-start` | 406.4 ms | **4.1 ms** | **98×** |
| `inject-pre-tool-use` | 250.0 ms | **3.6 ms** | **69×** |
| `posttool-nudge` | 256.6 ms | **2.1 ms** | **124×** |
| `stop` (WRITE path, tmpfs) | 250.5 ms | **5.4 ms** | 46× |
| `stop` (WRITE path, `/media` spindle) | 2.64 s ± 1.51 | 901 ms ± 1214 | **2.9× — the disk, not the code** |
| `pretool-recall` | 392.3 ms | 189.0 ms | **2.1×** — both spawn the *same* CPython child by design (DIV-205) |
| floor (unknown id) | 186.8 ms | **1.2 ms** | **159×** |

Spec §4's gate is *"hook end-to-end < 15 ms"*. The three read hooks land at
**2.1–4.1 ms**. Python's *floor alone* is 186.8 ms — **12× the whole budget,
before a single row is read** — which is why this is the one surface where the
port is not an optimisation but an enablement.

**Negative control, run and recorded:** changing `inject::SNIPPET_CHARS` from 140
to 139 flipped three cases to DIVERGENT. Its *first* attempt passed, because no
corpus row had a snippet longer than 140 characters — the truncator was dead
corpus. Generalised in the ledger: **every constant a port copies needs a corpus
row that crosses it.**

### 3.7 CLI tranche 1

`/usr/bin/time -f %e`, 5 runs after 3 warm-ups, on the `fts` state:

| command | Python | Rust | speed-up |
|---|---:|---:|---:|
| `status --no-auto-ingest` | 0.26–0.27 s | 0.11–0.13 s | 2.2× |
| `cfg ls` | 0.14 s | < 0.01 s | **> 14×** |

`cfg ls` reads one file under 300 bytes and does 22 dictionary lookups, so the
0.14 s **is** `import stackunderflow.cli`, measured. Every one of the **82 leaf
commands** pays it before doing anything. (PERF.md's prose says "79", the spec's
founding estimate; the generated inventory's 82 is the measured figure — DRIFT-3
in `TASKS-RS.md`.)

**Deliberately not measured:** `status` *without* `--no-auto-ingest`. The
maintainer's store is stale by the reference's own 6-hour threshold, so the
default invocation runs a full ingest+backfill on the Python side — a number that
measures the ETL and a run that writes to the store (DIV-238).

---

## 4. The flip plan

### 4.1 The entry-point collision, exactly as it stands

`pyproject.toml:66-68` declares **two** console scripts, both pointing at the
Python CLI:

```toml
[project.scripts]
stackunderflow = "stackunderflow.cli:cli"
stax = "stackunderflow.cli:cli"
```

The Rust binary is also `stax` (`crates/stax-cli/Cargo.toml:8-10`). This was
ruled a **wave-10 concern** on purpose: *"Python's pyproject still owns the
installed `stax` entry point until the wave-10 decommission decision — install-time
collision is a wave-10 concern, the built binary name is not"*
(`ARCHITECT-STATE.md`, naming bullet). Whichever of the two is installed last
wins the name today.

The decision is therefore not "rename something" but **which of these the
maintainer wants**, stated here as options with their consequences, not as a
recommendation:

| option | consequence |
|---|---|
| Python keeps both names | the Rust binary needs a third name; every proof in this report was taken against a binary called `stax` and the harness normalises that name on `Usage:` lines |
| Python keeps `stackunderflow`, Rust takes `stax` | matches the built artifacts today; **but** 70 of 105 CLI nodes have no Rust implementation at HEAD, so `stax <verb>` becomes a partial surface unless it falls through |
| Python retires both | requires §4.3's blockers closed first |

### 4.2 What runs where, at HEAD

The port is **three binaries**, not one:

| binary | crate | covers |
|---|---|---|
| `stax` | `stax-cli` | 35 of 105 CLI nodes + `store` + `anchor` |
| `stax-server` | `stax-server` | 97 of 101 routes + the SPA + `/static` |
| `stax-hooks` | `stax-hooks` | all 9 hook ids, `hooks run <id> [--capture-content]` |

Note that `stax hooks run` is **not** in `stax-cli` at HEAD (RS-8-021/054..058
open); the hook contract is answered by the separate `stax-hooks` binary with the
reference's argv and stdin contract.

**A hard constraint on renaming, recorded in the wave-6 findings:**
`templates.canonical_command` writes the literal `stackunderflow hooks run <id>`
into a user's `settings.json`, and `parse_hook_command` is what decides whether an
existing entry is *ours* — a false positive **deletes another tool's hook**. The
port keeps the literal `stackunderflow`. Any rename must be paired with a
migration for already-installed `settings.json` entries, and that migration is
not written.

### 4.3 Packaging implications — three data dependencies on the Python tree

This is the part a "delete the Python package" reading gets wrong. The Rust
binaries read files that live **inside** `stackunderflow/`:

| artifact | binding | evidence |
|---|---|---|
| `stackunderflow/data/models.toml` | **build-time** `include_str!` in `stax-etl`'s stats layer — reading the same file the reference reads rather than transcribing it (spec §2.4) | `ci.sh` gate 0's extraction set explicitly includes `stackunderflow/data/` because a clean checkout without it fails to compile: `error: couldn't read …/models.toml (os error 2)` |
| `stackunderflow/adapters/capabilities.json` | **runtime** — injected path, `STACKUNDERFLOW_CAPABILITIES` env, repo-relative default `stackunderflow/adapters/capabilities.json` | `crates/stax-adapters/src/capabilities.rs:36-41`; deliberately *not* `include_str!`, because a build-time copy would let the two implementations disagree |
| `stackunderflow/static/react/` | **runtime** — `AppState::static_dir()` is `package_dir.join("static")` | `crates/stax-server/src/state.rs:184-185`; the React bundle is the parity oracle and stays TypeScript (spec §1) |

Plus, in flight and not at HEAD: the wave-7 migration runner pulls each `.sql`
body out of `stackunderflow/store/migrations/` with `include_str!`.

**Consequence, stated plainly:** at HEAD, a machine running only the Rust
binaries still needs the `stackunderflow` package directory on disk for
`capabilities.json` and the React bundle. Making the binaries standalone is a
packaging item nobody has taken — the adapters ruling explicitly deferred it
(*"the installed-binary distribution question is a wave-8 packaging item"*), and
no wave-8 tranche claimed it.

### 4.4 The rollback story

Rollback is **structurally cheap and this is the campaign's best-defended
property**: the SQLite store is the compatibility boundary (spec §2.1). Both
implementations read and write the same `store.db`, schema v030, and this is not
an assertion —

* the mart gate rebuilt from scratch on both sides and diffed **to the bit**
  (10/10 tables, 131,582 rows, identical `SUM(cost_usd)` bit patterns);
* the ingest gate diffed five tables **full-row** at four corpus sizes up to
  1.02 GB, and proved idempotence on both sides;
* the writer's pragmas byte-match `store/db.py` (WAL / `synchronous=NORMAL` /
  `foreign_keys=ON`, test-pinned — desk ruling 3);
* the three reindex writers produce byte-identical sidecars including the
  SHA-256 content ids and the `sqlite_master` DDL text.

So a flip is reversible by pointing the old entry point back at the same store,
with **three caveats**:

1. **The port never migrates.** `schema.apply()` is unported (29 open migration
   items). Rolling *forward* into Python is safe (Python self-heals); rolling
   *back* from a store Python has migrated is safe only while the port's read
   paths tolerate the new schema. The port raises rather than guessing (DIV-216),
   which is the loud failure mode, not the silent one.
2. **The port does not create data files it was not handed** — no store on
   `status` (DIV-239), no sidecar creation (DIV-077), no price-book priming. A
   rollback onto a home the port created may find files Python expects missing.
3. **Capture hooks write on every fire in both implementations** (DIV-201), so
   the two can be run side by side without either losing capture rows — the
   `UNIQUE (ts, hook_id, session_id)` key with microsecond `ts` is what makes a
   re-fire idempotent.

---

## 5. Risk register — what is *not* proven

| # | risk | status | evidence |
|---|---|---|---|
| R1 | **`PYTHONHASHSEED` is not exported by `endpoint-parity.sh`** — CPython randomises `str` hashing per process, so any reference payload built by iterating a `set` has a per-boot key order. `compare` measured **three different `diff.tokens` orders in three runs with every other byte identical.** | **OPEN — DIV-195.** Verified 2026-08-01: `parity-cli.sh`, `sync-parity.sh` and `hooks-parity.sh` all export it; `endpoint-parity.sh` does not. | This is a **latent flake under every green row in the matrix whose Python side iterates a set, and the matrix has never been run twice in a row to find out how many that is.** The one-line fix moves the reference's own bytes and requires a full re-measure. |
| R2 | **S3 transport is not ported and cannot be differed here** — `S3ObjectStore` is a `boto3` client and `boto3` is not installed on the parity host. | **DEFERRED with the reason.** What *is* ported and unit-pinned: `_full`, the `list` prefix strip, `parse_bucket_url`, `scheme_of`, `requires_boto3`, `store_from_url`'s dispatch. | `stax sync push` to an `s3://` destination prints `_SYNC_INSTALL_HINT` and exits 1 — **byte-identical to what Python does on this host**, because `_sync_missing_deps` also finds no `boto3`. That is *parity by agreement on the facts*, not parity of the transport. Closing it needs an S3 client **and** a host with `boto3`. |
| R3 | **The 4 `>u64` clamp cases** — `V-dec-limit-huge` / `V-dec-limit-bad` × 2 states, accepted by desk ruling 2. | **ACCEPTED**, re-confirmation requested (rulings item 14). | Named every run with `ACPT` lines and a tally section; a case that starts *passing* prints a re-examine warning (`HEAD:rust/parity-cli.sh:336/341/350`). |
| R4 | **Streams are probed, not differed.** `/api/live/stream` has **no case row, ever** — a `Verdict::Error` (socket timeout) is not downgraded by `!` and ranks *above* a divergence at exit 2, so one SSE row would take every other verdict with it. | **PROBE ONLY** (DIV-136). Frames diff identical against batch D's recording after timestamp normalisation; cadence re-measured 6.031 s vs 6.03 s. **The handler cannot mount at HEAD** (DIV-165 — `http_body` / `futures_core` / `tokio_stream` are `E0463`; one manifest line). | `rust/parity/SSE-PROBE-d.md`. A batch is in flight on the mount; it is not in HEAD. |
| R5 | **The port cannot create a store.** 29 migration items open. | **OPEN** (RS-0-011..055). | Wave-4 finding 3; DIV-216. A wave-7 batch is in flight. |
| R6 | **No regression gate exists for perf.** Zero criterion benches in the workspace; spec §6 designated them as the replacement for the Python suite's 4 load-sensitive perf-budget tests. | **OPEN — DIV-349.** | Verified: no `criterion` in any manifest, no `benches/` directory. |
| R7 | **`422` body bytes are pinned to a fastapi/pydantic release, not to a contract.** Four shapes were measured byte-identical (`missing`, `bool_parsing`, `int_parsing`, `string_type`) and a fifth (the two-entry `missing` list) later. What stays unpinned is the wording across a bump. | **RECORDED** (DIV-053, narrowed twice). | Cross-reference the known CI drift note: CI installs unpinned latest fastapi. |
| R8 | **Four `{"detail":"<field>"}` 422s survive** in `routes/commands.rs` ×2, `routes/cost.rs`, `routes/budgets.rs` — the same latent bug batch C fixed elsewhere. **None has a case row**, so nothing has ever measured them. | **NAMED, NOT FIXED** (DIV-151). Now `json::validation_422_field_only` with a test pinning the wrong-but-current bytes. | The batch's own generalised lesson: *a validation path with no case row is an unported branch wearing a green tick.* |
| R9 | **`math.erf` is unreachable under `#![forbid(unsafe_code)]`**; the fdlibm transcription differs from glibc 2.31 on **2.520 % of 220,042 points, always exactly 1 ULP**. | **OFF the case-row path** (`var<=0` short-circuits every p-value on this store) — DIV-176. One manifest/`lib.rs` line. | Not reachable today; reachable the moment a store produces a positive variance on that path. |
| R10 | **No TLS crate in the workspace at all**, so `_fetch_from_litellm` (HTTPS) cannot be ported. Outbound HTTPS *works* on this host (`200 1670646`) — the port is pinned to the reference's fetch-failure leg **by a dependency gap, not by the network**. | **RECORDED** (DIV-199, DIV-065). | `PRICING-REFRESH-DIFFER.md`: fetching set 4/4 divergent, as predicted. |
| R11 | **Two ported endpoints run an LLM / network call on the reference and would become writers if the service is up.** `/api/…/quality` grades via Ollama and `INSERT OR REPLACE`s; if :11434 is ever open, Python runs first and the port would serve **Python's freshly-written grade back** — an accidental near-match hiding a fabricated row. | **PROCEDURAL GUARD ONLY** (DIV-170, DIV-352) — a banner in the case file. A mechanical guard is an `endpoint-parity.sh` change. | The determinism of `M-ollama` is *a property of the machine, not the code*. |
| R12 | **The differ is a sequencer over one shared home**, so two whole endpoint classes fall outside it: a body that never ends (R4) and a request whose second execution observably differs from its first (DIV-146 — `DELETE /api/cfg/model-aliases` is 200 on the Python leg and a correct 404 on the Rust leg; **no ordering fixes it**). | **STRUCTURAL** — both are written into the case-file header rules. | This bounds what "648 identical" can ever mean. |
| R13 | **A fixture produced by the code under test can be green by vacuum.** `build_sync_state.py` seeded `merged.db` by running `runner.push`, which wrote `sync_outbox` rows into the store it pushed *from* — so every later `push`/`pull` case was a no-op both sides agreed on. The differ reported green **on empty buckets.** | **FIXED** (push from a scratch copy), and generalised into a law. | The same class as the hooks corpus finding: the maintainer's own `tools_json` is names-only, so the **active-recall feature cannot fire on his own data**, and a differ run against the real store would have been green by vacuum on four of nine hooks. |
| R14 | **The live store's marts are stale against a from-scratch rebuild** — both implementations agree; the *store* is wrong. `command_mart` carries **959 events that no longer exist and $617.41 (1.45 %) of cost that is not in `usage_events`**; `tool_mart` 32 events / $19.25; `message_tool_mart.byte_count` 30,541 low. | **NOT a port divergence** (DIV-040) — a property of the Python mart layer, reproduced exactly. Needs a one-time backfill decision. | Rulings item 10. |
| R15 | **A watermark hazard is structural on the watcher path.** `count_added == 0 → processed_offset = file_size` means a cycle that reads a partially-flushed line advances the watermark **past** it and the record is lost permanently. DIV-043's event filter removed the pressure that made it reachable; the branch is unchanged **on both sides**. | **OPEN, maintainer's desk** (wave-4 finding 1). | Seven consecutive appends now land clean; the hazard is structural, not incidental. |
| R16 | **DIV-214's delimiter families are unported** (see §1.6e), and **DIV-137 / DIV-197 / DIV-357** are the same shape: handler messages that interpolate CPython's own exception text, which rusqlite/serde cannot reproduce. | **BUG-FOR-BUG IMPOSSIBLE**, marked loudly rather than faked. | A corpus row that crosses one fails; none does today. |
| R17 | **Ledger integrity.** 9 DIV ids are double-allocated; 109 of 224 rows carry no disposition; TASKS-RS ticks lag the landings by a wide margin. | **OPEN — Appendix A**, DIV-340 / DIV-342 / DIV-343 / DIV-344. | This is a risk to the *decision*, not to the code: it is what makes "every diff closed or documented" (the wave-10 gate) unverifiable by machine today. |

---

## 6. What a soak would have to cover

RS-10-002 asks for *"both stacks side-by-side on the live store; every
response/mart/envelope diff either closed or filed."* The campaign's own findings
say a soak measured in *days* proves less than a soak measured in **events
crossed**. What follows is the coverage list, not a schedule; the decision to run
it, and for how long, is the maintainer's.

### 6.1 Events the soak must cross, each with the divergence it would settle

| event to cross | what it settles | why a short soak misses it |
|---|---|---|
| **A local-midnight boundary with no intervening ingest** | DIV-091 — `_SPEND_CACHE` is keyed `(store_path, period_start, period_end)` and validated on `store.db`'s mtime, but `date.today()` is **not in the key**. Python serves yesterday's `daily_costs`, one element short, moving the projection denominator, the weighted-7d tail, `days_to_limit` and `alert`. The port is *more correct* there. | unreachable by the differ at all; needs a running pair over midnight |
| **A month boundary with a log rotation** | DIV-044 — `UNIQUE (session_fk, seq)` is per-partition, so a reparse whose month moved lands a **second** row | the same-month absorption is pinned by a test; the cross-month case is documented, not exercised |
| **A rate-card edit** | DIV-001 — every rate-card edit reopens the frozen-mart-cost divergence (±0.001 % on `all`, **−65 %** on a dirty project's `week`) | nothing crosses it unless prices move |
| **`status` / `plan` on a window with data in it** | §1.6f — the parity states' newest events predate the current month, so `today` and `month` render `$0.00 (0 msg)` and the arithmetic is untested | a static seed cannot cross `parse_period("today")`, which reads the wall clock |
| **A second consecutive full matrix run** | R1 / DIV-195 — the size of the `PYTHONHASHSEED` flake is **unknown because the matrix has never been run twice in a row** | one run cannot show it |
| **A cross-origin caller** | DIV-050 — CORS is unported and nothing measures it | the differ is same-origin by construction |
| **A `%2F` or trailing-slash request from a real client** | DIV-168 / DIV-133 — reproduced on three endpoints that are **green today**, invisible because no row sends one | no case row sends one |
| **A non-USD configured currency** | DIV-052 — the port answers an error rather than inventing a rate | both harness states are USD |
| **A model alias being set** | DIV-147 — `crate::pricing::engine` does not consult `settings.model_aliases` but `infra.costs.compute_cost` does on every call; **no divergence today only because the alias map is empty on every state the campaign tests** | needs a user action |
| **An Ollama instance actually running** | R11 / DIV-170 / DIV-352 — the quality endpoint becomes a writer and the port could serve Python's fabricated grade back | :11434 is closed on the parity host |
| **A hook fire on a store whose `tools_json` carries arguments** | the hooks finding — on the maintainer's names-only data, four of nine hooks only ever exercise their silent branch | the synthetic corpus proves the code; only real data proves the feature |

### 6.2 What to run in parallel, mechanically

Both stacks against the same `store.db` is safe by construction — that is what
every gate in this campaign already does — with three procedural rules the
campaign learned the expensive way:

1. **Never point the soak at :8095.** That is the maintainer's live server; both
   the driver and the differ refuse it outright. The port serves :8096, the
   reference :8097.
2. **Any endpoint whose side effect is another endpoint's input must not share a
   home.** One `GET /api/pricing` appended **24 `source='live'` rows to Python's
   `price_book`** and wrote `cache/pricing.json`, which is
   `read_cache_status()`'s input — five clean cases became five divergences from
   a *case-file edit with no code change* (DIV-059, caught in the act as
   DIV-350/batchE). One `POST /api/search/reindex` rebuilt a parity home's index
   from 53 KB to **520 MB in 192 s** and starved 47 cases (DIV-078).
3. **Writers get case-local homes**, the mechanism tranche 1 built: each side
   gets its own fresh copy of a seed **at the same path** (because `backup list`
   prints its own directory), and the two trees are diffed — so a writer is
   proven on three axes: stdout, exit code, and the bytes left on disk.

### 6.3 What the soak cannot close

No soak closes: the 5 pre-filed `UNDECIDED-maintainer` rows (they are decisions,
not measurements), DIV-240 (`--help` byte parity is a second help engine),
DIV-213 (needs an S3 client *and* a `boto3` host), DIV-205 (recall's CPython
child is deliberate), the 13 wall-clock known-open rows, or the 29 open migration
items. Those close by ruling or by code, and §2.3 maps each to its owner.

---

## Appendix A — reconciliation: where the artifacts disagree with each other

Ten discrepancies were found while cross-checking `TASKS-RS.md`'s ticks and
tallies against the proof artifacts. New ids are allocated from **DIV-340** per
the campaign's numbering handoff.

**Count: 10 (DIV-340 … DIV-349).**

| id | discrepancy | evidence | why it matters |
|---|---|---|---|
| **DIV-340** | **Nine DIV ids are double-allocated.** `DIV-200`…`DIV-207` name *both* the wave-6 hooks findings (`TASKS-RS.md:1796-1803`) *and* wave-5 batch E's findings (`:1978-1985`) — 16 distinct findings under 8 ids. `DIV-133` names *both* the app-level trailing-slash 307 (`:1518`) and `/api/sync/status`'s deferral (`:1604`). | line numbers above | **Live consequence:** the sync landing note says *"DIV-133 is CLOSED"* while `MAINTAINER-RULINGS-REQUEST.md` item 1 asks the maintainer to rule on DIV-133 as still open — two different findings, one id. Likewise `ARCHITECT-STATE.md` records *"DIV-203 tokio::time fixed (c1bea2d)"* while DIV-203 also names the hooks' ASCII-digit narrowing, which is not fixed and was never meant to be. A one-word answer to a ruling request could land on the wrong finding. **RESOLVED 2026-08-01 (closing pass):** batch E's eight moved to `DIV-350..357`, batch D's `/api/sync/status` moved to `DIV-358`; the hooks findings and batch C's trailing-slash row keep their ids. Mapping table: `rust/TASKS-RS.md` § *DIV renumbering (closing pass)*. `DIV-359` left unallocated as the seam; new findings start at `DIV-360`. |
| **DIV-341** | **Two endpoints are unported *and* unrepresented in the matrix.** `POST /api/project` (RS-5-095) and `GET /api/global-stats` (RS-5-101) have no axum registration, **no case row — not even a `!` row —** and no ledger entry. | re-derived at HEAD; `routes/projects.rs:5,11` record both as `**open**` | The matrix reports `0 divergent` over 716 rows while two endpoints 404 on the port. The campaign's own rule is *"unported endpoints belong here, in the file, not missing from it — 'we know' beats 'nobody looked'"* (`endpoint-cases.txt` header). These two are the exception nobody filed. |
| **DIV-342** | **109 of 224 ledger rows carry no disposition field.** Batches C (DIV-085..132) and E (DIV-153..209) use a 3-column `id / item / finding` table; the ledger's declared format has six columns including `disposition`, and §6b permits exactly two end states. | column-count histogram: 153 rows × 4 cols, 63 × 3 cols, 5 × 6 cols, 2 × 5, 1 × 7 | RS-10-003's gate — *"zero UNDECIDED-maintainer rows, or each is on the maintainer-decision doc"* — cannot be evaluated mechanically. "Every diff closed or documented" is currently a human reading, not a check. |
| **DIV-343** | **TASKS-RS ticks lag the landings by a wide margin.** 146 of 551 item lines are `[x]`; 405 are `[ ]`. By wave: wave 2 is **GATED** with 2 of 24 `adapter:` items ticked; wave 5's server surface is reported **ported** with 13 of 80 `endpoint:` items ticked; wave 3 is 34/87. | `grep -cE '^- \[x\] RS-[0-9]' TASKS-RS.md` → 146; per-wave breakdown re-derived | The file calls itself *"the campaign's single progress authority"* and *"the memory"*. A maintainer reading item state would conclude the port is ~26 % done; the proof artifacts say otherwise. The **narrative sections are ahead of the ledger**, and the reconciliation note at `:184` shows this has already happened once in the other direction (30+ items were pre-ticked with no implementation and reverted). |
| **DIV-344** | **17 duplicate RS item ids**, against the grammar's *"IDs are stable — never renumber"* and the coverage rule's one-item-per-unit invariant: `RS-5-122`, `RS-6-025`, `RS-8-003..010`, `RS-8-021`, `RS-8-081..086` each appear twice (the wave-6 hooks block re-lists wave-8 items to tick them). | re-derived | The header states **499** items, the reconciliation note says **505**, the wave-8 tranche says wave 8 is a **113-item** wave, and the raw line count is **551** (534 distinct). Four different denominators for "how much is left". |
| **DIV-345** | **Numeric disagreements on the same runs.** (a) The 1.02 GB ingest corpus: `TASKS-RS.md` and `ARCHITECT-STATE.md` say **56,329 messages / 35,633 events**; `PERF.md` §wave 4 says **56,317 / 35,627** for the same 216-file corpus. (b) Adapters: `ARCHITECT-STATE.md`'s proof totals say **55,795 live records**, its own fleet roster and commit `2cbbf22` say **55,647**. (c) Wall-clock rows: `ARCHITECT-STATE.md` says *"the 6 wall-clock rows proven off-matrix"*; batch E's table lists **7 rows across 5 fields**. | cited files | Small, but these are the numbers a decommission decision quotes. The likely cause in (a) and (b) is two runs against a corpus drawn from a live tree; either way, one of each pair is unreproducible from its stated command. |
| **DIV-346** | **`CLI-INVENTORY.md`'s status column is stale.** The five `sync` nodes are marked `UNPORTED · W8-T2`, but `stax sync {init,push,pull,status}` ships at HEAD (`crates/stax-cli/src/sync.rs`, landed `ce6682e`, proven by `sync-parity.sh`'s 10 CLI-verb rows). The inventory was generated at `e374fed`, one commit earlier. | `git show HEAD:rust/crates/stax-cli/src/lib.rs`; inventory rows 95–99 | The inventory is the *generated* denominator this report and wave 8 both count against. It is generated, so it is correct at generation time and silently wrong afterwards — it needs regenerating at the wave gate, not at the tranche start. |
| **DIV-347** | **DIV-025's ledger row still reads `OPEN — rename blocker, maintainer`** (`TASKS-RS.md:1068`) although the desk-rulings block (`:1364`) records it RESOLVED and executed (binary renamed to `stax`, verb `status` → `store`, swept everywhere, harness re-run green). | both lines | A reader grepping the ledger for open blockers finds one that was closed a wave ago. |
| **DIV-348** | **The `!`-row count and the known-open tally disagree by design.** 70 rows carry `!`; the last recorded verdict is 68 known-open. A `!` row that agrees is scored `Identical` (`parity/src/endpoints.rs:179` returns before the `known_open` branch). | re-counted; source read | Two rows are known-open markers currently counted as wins, and the known-open number moves without a case-file edit. Neither figure alone tells the maintainer how much of the matrix is not gating. |
| **DIV-349** | **Wave 9 does not exist yet**: `stax-wasm` is an 11-line charter, and there are **zero criterion benches** in the workspace. | `crates/stax-wasm/src/lib.rs`; no `criterion` in any manifest, no `benches/` | Spec §6 designates the criterion gates as the replacement for the Python suite's 4 load-sensitive perf-budget tests. Retiring the Python suite at HEAD retires that coverage with nothing behind it, and PERF.md has no regression gate — it is a scoreboard, not a check. |

### A.1 Where TASKS-RS ticks *were* confirmable

The reconciliation is not one-directional. Every tick that was spot-checked
against an artifact held: wave-3 marts (RS-3-004..013 + RS-3-082) against the
10-table bit-exact dump; wave-4 ingest (RS-4-002/005/006/007) against the
five-table full-row diff, with RS-4-001 and RS-4-004 **left open with recorded
reasons** rather than swept; wave-6 hooks (RS-8-003/006..010, RS-8-081..086)
against the 80-invocation differ, with RS-8-004/005 left open for a stated reason
(install-time, not hook-budget). The lag in DIV-343 is a **bookkeeping** lag, not
a claimed-but-absent-work lag — which is precisely why it needs closing before a
decision, since the two are indistinguishable from the file alone.

---

## Appendix B — evidence index

| artifact | what it proves |
|---|---|
| `rust/ci.sh` | the seven gates: clean-checkout build, fmt, clippy, test, CLI byte-parity, ingest parity, endpoint byte-parity |
| `rust/parity-cli.sh` + `parity/cases.txt` | gate 4 — 202 rows × 2 store states |
| `rust/endpoint-parity.sh` + `parity/endpoint-cases.txt` | gate 6 — 716 rows |
| `rust/ingest-parity.sh`, `rust/ingest-tail-proof.sh` | gate 5 + the live tail |
| `rust/hooks-parity.sh` + `parity/hook-cases.txt` | 80 hook invocations, five surfaces |
| `rust/sync-parity.sh` + `parity/sync-cases.txt` | 192 sync rows incl. cross-impl `age` interop |
| `rust/help-tree.sh` → `parity/HELP-TREE.md` | the `--help` contract, 30/30 |
| `rust/parity/CLI-INVENTORY.md` | the generated 105-node / 276-param CLI map |
| `rust/{ETL-BACKFILL,PRICING-REFRESH,SEARCH-REINDEX,QA-REINDEX,TAGS-REINDEX}-DIFFER.md`, `REFRESH-DIFFER.md` | the six isolated writer procedures |
| `rust/parity/SSE-PROBE-d.md` | the stream, probed |
| `rust/parity/DIV-c-*.md` (8), `DIV-e-*.md` (12) | per-member divergence write-ups |
| `rust/PARITY-wave1-*.md` (4) | wave-1 dated runs |
| `rust/PERF.md` | every perf row with its command |
| `rust/TASKS-RS.md` | the item ledger + the divergence ledger |
| `rust/MAINTAINER-RULINGS-REQUEST.md` | the 16 open decisions |
| `rust/ARCHITECT-STATE.md` | wave state, findings ledger, fleet roster |
| `rust/BATCH-{A,B,C,D,E,SSE,T4,TEAMS,W7}-CLAIM.md` | batch fences (the last four are in flight, uncommitted) |
