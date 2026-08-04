# The decommission report — DRAFT

**Wave 10, item RS-10-004.** The evidence the maintainer reads to decide whether
the Python implementation is retired, kept, or kept in part.

**Snapshot: branch `rust`, worktree `../StackUnderflow-rust`, HEAD `e42b71a`,
2026-08-04.** This is a **refresh** of the 2026-08-01 draft, which was taken
against `a40db22` — twenty commits and five whole legs earlier. Every count below
is either re-derived from the tree at **this** HEAD by a command reproduced here,
or quoted from a named proof artifact with its line. Numbers that could not be
traced to either are **flagged, not copied**.

**What this document is not.** It is not a recommendation to flip, and it
contains no version number, tag, or release timing: the flip decision and any
versioning are the maintainer's alone (`docs/specs/rust-port.md` §5, CLAUDE.md).
Section 6 describes what a soak would have to *cover* to close RS-10-002; it
does not schedule one. The flip that is executing (§4) is an entry-point change
the maintainer ordered, and this document neither authorises nor times it.

**Tree condition, stated first because the previous draft's caveat was the
loudest thing in it and it is now false.** The five concurrent in-flight batches
that bounded the `a40db22` draft — SSE, tranche 2, tranche 3, tranche 4, TEAMS,
wave 7 — **have all landed and are committed**. There is no `*-CLAIM.md` batch
in flight and no uncommitted crate work from any leg. What the working tree does
carry at the moment of writing, named so nobody reads "clean" as more than it is:

* `rust/.stax-anchors.db` — the campaign's own dogfood anchor database, modified
  by every anchor write (committed by decision, so it shows as dirty by design);
* `rust/DIRECTIVE-RULINGS-AND-FLIP.md` — the directive this refresh executes,
  untracked and deleted on acknowledgement per its own rule;
* **the ruled work landing in `stax-server` as this is written** — a new
  `cors.rs` (ruling 3, DIV-050), a new `path_semantics.rs` and edits to
  `lib.rs` / `method_semantics.rs` / `qs.rs` / `spa.rs` and four route modules
  (rulings 1, 1b, 2, 4), plus **five new rows in `parity/endpoint-cases.txt`**
  (763 at HEAD → 768 in the worktree). All uncommitted, therefore **counted
  nowhere below**: every endpoint number in this document is the HEAD number,
  re-derived with `git show HEAD:…`, and a reader who greps the working tree
  will legitimately see five more rows than §1.2 reports;
* one untracked React build asset under `stackunderflow/static/`.

No `ci.sh` run is reported against that tree — the campaign's law is to gate *at*
the commit. Every gate result below is **as last recorded**, with the commit or
artifact that recorded it; every *count* is re-derived at HEAD.

---

## 0. Method — what was re-verified at `e42b71a`, and what was quoted

| class | treatment |
|---|---|
| Route/endpoint coverage | **re-derived** at HEAD: Python decorators parsed out of `stackunderflow/routes/*.py` + `server.py` (101 route+method pairs); Rust `.route()` path literals parsed out of `rust/crates/stax-server/src/**` and set-compared against them after normalising `{param}` / `{param:path}` → `{*}`. The single unmatched pair was confirmed by reading its route module's own status table. |
| CLI coverage | **re-derived** from `rust/parity/HELP-TREE.md`, which is *generated* by `rust/parity/tools/help_tree.py` and — since the closing pass — carries `verify_against_binary()`: every node's status is the shipped `rust/target/release/stax <path> --help` exit code, not prose. That is the "inventory's generator" this report counts against. |
| Case-matrix sizes | **re-counted at HEAD**: `endpoint-cases.txt` **763** rows / **71** `!` rows; `cases.txt` **552** rows → **1,104** cases across the two store states; `hook-cases.txt` 80; `sync-cases.txt` 173. |
| Item ledger | **re-counted** by regex over `rust/TASKS-RS.md` (`^- \[.\] RS-[0-9]`): **542** item lines, **321** `[x]`, **221** `[ ]`, **8** duplicate ids. |
| Divergence ledger | **re-parsed at HEAD**: **360** table rows over **351** distinct ids (9 of them id-remap rows carrying no finding). Every row carries an explicit `disposition:` field — see §2.1; this is DIV-342 closed. |
| Tallies (`692 identical`, `1,100 pass + 4 accepted`, `192/192`, `80/80`, `37/37`) | **quoted** from the landing notes in `TASKS-RS.md` / `ARCHITECT-STATE.md` / the `*-DIFFER.md` procedures. Not re-run for this refresh. |
| Perf rows | **quoted** from `rust/PERF.md`, which carries the command for every row (§5's law). Not re-run. §3 is unchanged from the `a40db22` draft for that reason. |
| Test counts | **quoted**; `#[test]` / `#[tokio::test]` attribute counts re-derived at HEAD as a weak cross-check only (§1.6). `cargo test` was not run. |

---

## 1. Coverage map

### 1.1 The CLI surface

The denominator is the Python command tree as the generator sees it: **105 nodes
— 23 groups (including the root) + 82 leaf commands — and 276 declared
parameters**, against Click 8.4.2 / CPython 3.12.13 / `cli.py` at 6,484 lines.

**The status column is no longer prose, and that is the change since the draft.**
`rust/parity/tools/help_tree.py` now calls `verify_against_binary()`: every node
is asked of the **shipped** `rust/target/release/stax <path> --help` and scored
on its exit code, which is the only source that sees hidden nodes and cannot be
out of date with itself. The last regeneration reports **zero disagreements**
between the document and the binary.

| status | nodes | source |
|---|---:|---|
| **exists in the Rust binary** | **100 / 105 (95 %)** | `HELP-TREE.md` verdict, binary-verified |
| of those, contract-clean (same summary, same options, same subcommand list) | **93 / 100** | `HELP-TREE.md` per-node table |
| of those, contract-**divergent** | **7** | named below |
| byte-identical after the scoped program-name substitution | **0 / 100** | DIV-240, ruled |
| **unported** | **5** | named below |

**The 5 unported nodes, named rather than counted** (the generator lists them so
a differ that walked only what exists cannot claim a clean tree):
`analyze`, `analyze backfill`, `analyze quality`, `analyze session` — deferred on
an honest blocker: `services/static_analysis/` (1,560 ln) **and** Playback v2
**and** six forked tools (`radon`/`mypy`/`tsc`/`eslint`/`go`/`gocyclo`), none of
which is on this box, so both sides would take the "missing tool → warn and skip"
path and the differ would compare two identical warnings; and `ingest github`,
parked on **DIV-199** (no TLS crate in the workspace). Python covers both
surfaces.

**The 7 contract-divergent nodes,** all on the *summary* fact except the root:
`(root)` (subcommand list — the port carries the two Rust-only verbs and `msg`,
and excludes the unported `analyze`), `context-budget` and `yield` (the port's
`about` is the docstring's first sentence where Click prints the whole
docstring — DIV-241's class, at two nodes the transcription pass did not reach),
and `sync`, `sync init`, `sync pull`, `sync push` (same shape).

Three Rust verbs have **no Python counterpart** and are therefore outside the
105-node denominator entirely: `stax store` (the schema/row-count reader, renamed
from `status` under DIV-025), `stax anchor` (the maintainer-ordered
agent-continuity surface, RS-1-029..033), and `stax msg` (the agent-to-agent
inbox, RS-T-001..003). Decommissioning Python does not lose them; nor does
keeping Python provide them.

**Proof pointer.** `rust/parity-cli.sh` (ci.sh gate 4) — **552 case rows** run
against **both** store states (`fresh`, `fts`) = **1,104 cases**: **1,100 pass +
4 maintainer-accepted, 0 FAIL, RC=0**, last recorded at the import leg's close,
**run twice consecutively with the same tally**. The 4 accepted are
`V-dec-limit-huge` and `V-dec-limit-bad` × 2 states (desk ruling 2 / rulings item
14, DIV-010's `>u64` clamp residue); the harness names them every run and prints
a *re-examine* warning if one ever starts passing (`rust/parity-cli.sh`, lines
336 / 341 / 350).

**Honest gap in that gate:** the Rust side's stdout is normalised on `Usage:` and
`Try '…'` lines only, substituting the program name — scoped deliberately after a
blanket substitution rewrote real store content and produced a false diff
(`ARCHITECT-STATE.md`, "HARNESS LESSON"). Those bytes are therefore *not* under
test on those two line shapes.

**`--help` is contract-clean, not byte-clean, and that is now the ruled bar.**
`rust/help-tree.sh`: **0 / 100 byte-identical, 93 / 100 contract-clean**, with
eight structural clap-vs-Click template differences enumerated in
`rust/parity/HELP-TREE.md`. **DIV-240 is RULED (item 7): contract-clean is the
bar.** The 7 divergent nodes above are therefore work items against a settled
standard, not an open question.

### 1.2 The HTTP surface

Re-derived at HEAD:

| measure | count | how |
|---|---:|---|
| Live route+method pairs in Python | **101** | 93 `@router.<method>` (66 GET / 21 POST / 4 DELETE / 2 PUT) + 1 `@router.api_route` expanded ×4 + 4 SPA routes in `server.py` (the `/static` mount is additional) |
| **Mounted in the axum router** | **100 / 101** | `.route()` path literals parsed from `stax-server/src/**` and set-compared against the Python pairs after normalising path params |
| **Not mounted** | **1** | `POST /api/meta-agent/chat` — DIV-138, deferred whole with two `!` rows (a 1,410-line tool-call executor against a live LLM over `application/x-ndjson`) |
| Case rows in `parity/endpoint-cases.txt` | **763** | re-counted at HEAD (`grep -cvE '^\s*(#\|$)'`) |
| Rows marked `!` (known-open) | **71** | re-counted at HEAD |
| Last recorded verdict (wave-10 item 2c, re-run before **and** after the embedding change) | **692 identical · 0 divergent · 71 known-open · 0 flip-candidate · RC=0**, **verdict-diff EMPTY** | `TASKS-RS.md` DIV-400; `ARCHITECT-STATE.md` follow-up pass |

**What closed since the draft, in matrix terms.** `a40db22` reported 648/0/68 of
716 with **four** routes unmounted. The four were: the two DIV-341 endpoints
(`POST /api/project`, `GET /api/global-stats` — closed 2026-08-01, both
byte-identical, one of them off-matrix on a second store path, DIV-366),
`GET /api/live/stream` (**MOUNTED** — DIV-320 closed DIV-165 with one manifest
line), and `POST /api/meta-agent/chat`, which is the one that remains. The
matrix grew 716 → 735 → 763 rows and stayed at **0 divergent** throughout; the
verdict-diff over the common rows moved only where a `!` was deliberately
flipped, which is each pass's own gate.

Per-endpoint coverage against that matrix:

| | endpoints | note |
|---|---:|---|
| ≥ 1 **gating** (non-`!`) row | **86** | the endpoint is byte-compared every gate run |
| **only** `!` rows | **3** | `POST /api/meta-agent/chat`, `GET /api/sync/status`, `GET /api/worktrees` — the last two differ *only* in their own wall-clock stamp, proven off-matrix |
| **no row at all** | **10** | all with a recorded reason (the draft's "2 without" were DIV-341, closed) |

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
the 716-row tally cannot see, filed here as **DIV-341**. *(CLOSED 2026-08-01 —
both ported and byte-identical; see the note after this list.)*

* `POST /api/project` (RS-5-095) — open, needs `infra/discovery.locate_logs`.
* `GET /api/global-stats` (RS-5-101) — open, needs `queries.get_global_stats`.

**DIV-341 closed, 2026-08-01.** `RS-5-101` is byte-identical at 38,146 bytes —
the figure the closing pass had measured on the reference before the port
existed — on both of `get_global_stats`'s store paths (the mart fast path in
gate 6, and `_global_stats_raw_scan` on a `daily_mart`-emptied copy, off-matrix:
DIV-366). `RS-5-095`'s three error legs are ordinary gate-6 rows and its success
leg is `rust/project-set-differ.sh`, **6/6 byte-identical on two homes**. The
matrix went 657 → **662 identical, 76 → 71 known-open, still 0 divergent of
735**, and `H-head-project` — the free completeness check this report predicted
— went green with it. One incidental defect fell out: **DIV-365**, `has_jsonl`
was `Path::extension().eq_ignore_ascii_case`, which is neither of the two things
`Path.glob("*.jsonl")` does.

Both were recorded as `**open**` in `routes/projects.rs`'s own module doc table,
so nobody hid them; but they were absent from the case file entirely, carried no
`!` row and no DIV, and the matrix therefore reported `0 divergent` while two
endpoints answered 404 on the port. **Both are closed at this HEAD.** Of the
other two routes the draft listed as unmounted, `GET /api/live/stream` is
**MOUNTED** (DIV-320: `futures-core = "0.3.33"`, a crate `axum-core` already
depended on — the `Cargo.lock` did not move; cadence re-measured at **+34.2 ms
per tick, +0.57 %**, DIV-321) and still carries **no case row, ever**, by
DIV-136's rule. `POST /api/meta-agent/chat` (DIV-138) is the single remaining
unmounted pair.

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

**The schema is ported — this is the largest single reversal since the draft.**
The draft read *"All 29 migration items are open … the Rust binary cannot yet
create a store, only fill one"*. At this HEAD **all 30 migration-tagged items are
`[x]`** (`grep -cE '^- \[.\] RS-0-0[0-9]+ .*migration'` → 30, none open),
including the runner itself (RS-0-025). The `.sql` bodies are `include_str!`d
from `stackunderflow/store/migrations/` rather than transcribed — deliberately,
because 27 transcribed heredocs is 27 chances at a silent character-level drift
(DIV-300), and they are compiled in as of wave-10 item 2c. Proof:
`rust/schema-differ.sh`, **37 states, 37 identical / 0 divergent** — every
`user_version` step applied by both implementations and the resulting
`sqlite_master` compared verbatim. Two consequences the draft's caveats named
are therefore also closed: **DIV-216** (`stax sync` now self-heals a store
missing the v028/v029 tables, exactly as `cli.py::_open_store` does) and
**DIV-239 / DIV-291** for the reports family (**DIV-374**: two empty homes, both
bootstrapped a 528,384-byte v030 store with 37 tables, stdout identical, exit 0
both). One divergence is inherited rather than fixed: **DIV-302** — `v008` reads
the wall clock into the schema, so two stores created either side of a UTC month
boundary have legitimately different partition names; `schema-differ.sh` aborts
on a rollover instead of reporting a divergence it caused itself.

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

**a) Nothing is in flight. — CLOSED.** The draft's six mid-air batches (SSE,
tranche 2, tranche 3, tranche 4, TEAMS, wave 7) have all landed and are
committed, and so have four legs that did not exist when it was written: tranche
5, tranche 6 (`doctor` / `risk`), T2v3 (the T2 remainder), the telephone leg, and
the `import` leg. A decision taken on this report is a decision about `e42b71a`.
The only uncommitted code in the tree is ruling 3's CORS layer, landing as this
is written and counted nowhere above.

**b) Wave 9 exists; the perf gate still does not, and it is the Python-retirement
gate.** `crates/stax-wasm` is a real crate now — the browser demo is built and
provably offline (DIV-330..337), with the verb assembly duplicated because
`stax-cli` cannot be a wasm32 dependency. But there are still **zero criterion
benches anywhere in the workspace** (no `criterion` in any manifest, no
`benches/` directory). Spec §6 designates the criterion gates as the
*replacement* for the Python suite's 4 load-sensitive perf-budget tests.
**DIV-349 is RULED (2026-08-04): build the criterion gates DURING the soak;
Python's 4 perf tests are not retired until they exist. Python RETIREMENT is
gated on this. The FLIP is not.** That is the one line in this section a
decision-maker needs: the flip and the retirement are separable, and only the
second waits on DIV-349.

**c) 71 `!` rows, and the tally can drift.** A `!` row that happens to agree is
scored `Identical`, not `KnownOpen` (`parity/src/endpoints.rs:179` returns before
the `known_open` check). The last recorded verdict is `692 identical / 71
known-open` of 763 against **71** `!` markers, so the two now agree — but they
agree by arithmetic, not by construction, and the count still moves without a
case-file edit. **DIV-348**, and the closing pass narrowed it: a `!` row that
passes is no longer scored as a win in the *flip-candidate* column (0 at HEAD),
which is what makes the agreement visible.

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

**f) `status`'s arithmetic is exercised now — CLOSED (DIV-280).** The draft's
gap was real: the parity states are a snapshot whose newest events predate the
current month, so `today` / `month` / `report -p today` rendered `$0.00 (0 msg)`
and the rows proved wiring and nothing else. Tranche 3 built the missing thing —
a fixture store whose timestamps are generated **relative to the run clock**
(`build_clock_state.py`) — and every window-on-the-run-clock verb is gated
against it. The fixture's deliberate boundary row landed on a real inherited
seam and it is filed rather than smoothed: **DIV-281**, an event stamped at
exactly local midnight is *excluded* from `today`. One method lesson came with
it (**DIV-282**): the first fixture schema was hand-written and failed three
times against the reference, so it now comes from `schema.apply` — *a
hand-written fixture schema tests the hand-writing.*

**g) DIV-042 — teams metadata — CLOSED (batch TEAMS, 2026-08-01).** The draft's
gap was 41 of 162 sessions filled by Python and 0 by the port. RS-2-004 landed
as `stax-etl`'s `ingest/teams.rs` (in `stax-etl` and not `stax-adapters`,
because adapters stay storage-free — DIV-310, the architect's own binding
ruling) and RS-5-025's ingest half as `ingest/outcomes.rs`. **Rulings item 12
ratifies it as noted-and-now-closed.** What the closure paid for is recorded:
DIV-311 (the chain fallback iterated a `frozenset`; the port uses a `BTreeSet`
so the lowest uuid wins deterministically), DIV-313 (a traversal order that is
*semantic* and therefore matched rather than improved), and DIV-318 (the hook
re-reads the whole `projects/` tree on every ingest pass **on both sides** —
inherited and quantified, not fixed).

---

## 2. The divergence ledger — digest

**360 table rows over 351 distinct ids** in `rust/TASKS-RS.md` (9 of them id-remap
rows carrying no finding of their own; two are the sweep's own new findings,
DIV-480 / DIV-481). The draft counted 224 rows over 226 ids;
the growth is five landed legs, the DIV-340 renumbering, and the audit's own
`DIV-340..349` block, which lives in Appendix A of this document rather than in
the ledger. `DIV-083` / `DIV-084` are explicitly unused after a renumber, the
ranges `060–063` and `219–229` were never allocated, and `DIV-359` is
deliberately left unallocated as the seam between the audit block and the
renumbering.

### 2.1 By disposition — DIV-342 is CLOSED

**The draft's single most important structural finding was that 109 of 224 rows
carried no disposition field at all, so RS-10-003's gate could not be evaluated
by machine.** That is fixed. Every one of the 360 rows now carries an explicit
`**disposition:**` field, appended to its last cell so no existing text moved and
no id was renumbered (`rust/TASKS-RS.md` § *Disposition sweep*, 2026-08-04).

| disposition | rows | meaning |
|---|---:|---|
| `bug-for-bug` | **149** | the port reproduces the reference, including where the reference is wrong — plus port defects found and **fixed to match**, and rows **superseded** by a later leg that ported what they deferred |
| `fixed-in-rust` | **108** | the port deliberately differs and that difference stands: a correction, a narrowing, an engine limit (`bug-for-bug impossible`), or a branch not ported |
| `harness` | **67** | a finding about the **proof apparatus or the port's internal structure** — a differ law, an evidence/corpus limit, a wall-clock field that makes a row unpinnable, a duplication or crate-graph note. No behavioural divergence asserted |
| `RULED:<item>` | **21** | decided by the 16+1b sitting; all 17 items are present (1, 1b, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16) |
| `n/a` | **9** | the renumbering table's old-id rows — the disposition lives on the new id |
| `filed-post-soak` | **5** | deferred with a named owner and gate; the reference covers the surface through the soak (DIV-016, DIV-029, DIV-138, DIV-199, DIV-213) |
| `fixed-python-first` | **1** | DIV-021 — the reference changed first, goldens re-pinned, the port matched |
| **`UNDECIDED-maintainer`** | **0** | retired 2026-08-04 |

**The gate is a grep now, which is the whole point of the exercise:**

```sh
grep -cE '^\| ?\*{0,2}DIV-[0-9]{3}' rust/TASKS-RS.md                            # 360
grep -E  '^\| ?\*{0,2}DIV-[0-9]{3}' rust/TASKS-RS.md | grep -vc 'disposition:'  # 0
grep -E  '^\| ?\*{0,2}DIV-[0-9]{3}' rust/TASKS-RS.md | grep -c 'UNDECIDED'      # 0
```

Two honesty notes the sweep owes a reader. First, **§6b permits two end states
and the ledger needed six** — `harness`, `filed-post-soak`, `RULED:` and
`fixed-python-first` name things the campaign was already doing in prose, and
making them tokens is what turned 109 unclassifiable rows into zero. Second,
**thirteen rows were flagged `ARCHITECT-REVIEW` in place** — DIV-018, DIV-098,
DIV-149, DIV-150, DIV-151, DIV-152, DIV-256, DIV-262, DIV-263, DIV-266, DIV-306,
DIV-404, DIV-441 — because they carried the directive's default and wanted one
word each rather than a guess. **All thirteen are DISCHARGED (2026-08-04); zero
flags remain** (`grep -n 'ARCHITECT-REVIEW' rust/TASKS-RS.md` returns only the
paragraph that records the discharge). Eleven confirmed the sweep's reading;
**DIV-149 and DIV-150 were corrected `harness` → `fixed-in-rust`**, because both
assert a divergence from Python and `harness` is the token for rows that assert
none. **DIV-306 and DIV-441 stay `fixed-in-rust`**: they were maintainer-facing
but never carried the retired transient, and in both the port measurably does
not reproduce the reference — a `bug-for-bug` label there would change no code
and describe a tree that does not exist. The maintainer question each one
records (a one-line revert to reference behaviour) is unchanged and still
theirs.

**The sweep also found two new discrepancies**, filed as **DIV-480** (the id
`DIV-119` names two different findings — a duplication note in `TASKS-RS.md` and
the unrowed pydantic `json_invalid` 422 shape in `parity/DIV-c-optimize.md`;
DIV-340's shape recurring one id short of the renumbering that was meant to end
it) and **DIV-481** (split ticks recurred: the telephone leg re-listed
`RS-5-039` / `RS-5-041` as `[x]` while their canonical lines stay `[ ]`, four
legs after `TICK-RECONCILIATION.md` §1 deleted twelve of exactly that). Both are
written up in `rust/TASKS-RS.md` § *Disposition sweep*.

### 2.2 The five pre-filed §6b divergences — RULED 2026-08-04

All five are **implemented bug-for-bug and proven so**; what remained was whether
Python should be *fixed*, and that call has been made:
**bug-for-bug STANDS for all five through the soak.** Python-side fixes for
**DIV-002** (classifier fall-through) and **DIV-005** (tz sign, which flips with
the frontend) are **FILED post-soak, not executed now**. DIV-001, DIV-003 and
DIV-004 stand as ported with no follow-up filed. The evidence is unchanged and
is reproduced here because it is what the ruling was made against:

| id | what | evidence at HEAD |
|---|---|---|
| DIV-001 | Frozen mart costs (`daily_mart.cost_usd` freezes the rate card at normalisation) | structurally out of the mart gate's way — the rebuild reads `usage_events.cost_usd`; `price_book` (105 rows: 32 live / 20 manifest / 53 rate_card) and `SUM(usage_events.cost_usd)` fingerprinted before and after on both sides, unchanged |
| DIV-002 | Classifier fall-through dims (`stats/classifier.py:174` → `"assistant"`) | reproduced exactly: **12,878** messages reach the fall-through, **7,343** with `role='user'`; **57 of 244** events-backed `project_mart` rows carry `total_commands = 0` (§6b predicted "57 of 243") |
| DIV-003 | `<synthetic>` folding into `by_model["N/A"]` | mart-gated summary path only |
| DIV-004 | Sub-second `until`-edge asymmetry | no occurrence in the real store; fixtures must not mint one |
| DIV-005 | Sign-inverted tz offsets from the React callers | **exercised and inherited faithfully** — `D-stats-tz` (−480) and `D-stats-tz-inverted` (+480, what the React callers actually send) are both byte-identical, so the port reproduces the wrong bucketing exactly until the frontend fix lands and both flip together |

### 2.3 The 16+1b rulings — ANSWERED 2026-08-04, and what each answer costs

`rust/MAINTAINER-RULINGS-REQUEST.md` consolidated 16 decisions (17 with item 1b).
**All are ruled** (`rust/DIRECTIVE-RULINGS-AND-FLIP.md`). This table is now a
work list, not a question list: each row is the ruling and the shape of the work
it authorises. Nothing here is scheduled by this document.

| # | DIV | the ruling | state at HEAD |
|---:|---|---|---|
| 1 + 1b | DIV-133, DIV-361 | **port both 307s** — the app-level trailing slash and the `/static` mount root, ruled together as one decision | one `lib.rs::app()` fallback plus the mount root; the three `!` rows exist so it cannot be half-closed. Id collision already resolved (DIV-340): DIV-133 is the app-level row, `/api/sync/status` is DIV-358 |
| 2 | DIV-168, DIV-169 | **match Python, pin the rows** | reproduced on three endpoints that are green today; invisible because no case row sends a `%2F` or an empty non-terminal segment. The rows are the deliverable |
| 3 | DIV-050 | **port CORS** | one tower layer. **Landing in the working tree as this document is written** — uncommitted, counted nowhere in §1 |
| 4 | DIV-107 | **match pydantic** | `qs::opt_int` rejects `?at=3036.0` (200 there, 422 here); the float leg is a two-line fix in the *shared* helper, the big-int leg needs `arbitrary_precision` or a recorded exception |
| 5 | DIV-055 | **port the stats memo** | the one place the reference wins: warm 5–12 ms vs 43–46 ms. Payloads are byte-identical either way (gate 6), so this is purely latency vs memory |
| 6 | DIV-091 | **as recommended — Python-first date-key fix, then port** | the `_SPEND_CACHE` key omits `date.today()`, so crossing local midnight serves yesterday's answer until process restart. Same procedure as the resume-ordering ruling (DIV-021) |
| 7 | DIV-240 | **contract-clean is the ruled bar for `--help`** | settles §1.1: 93/100 clean, 7 divergent are now work against a standard. Byte parity would have meant a second help engine whose only consumer is a differ |
| 8 | DIV-079 | **as recommended — guard + 422, Python-first** | a live `ZeroDivisionError` 500 in the reference's `/api/search` and `/api/qa`. Python-first, then port, then rows |
| 9 | DIV-067 | **as recommended — post-soak, with DIV-016** | `/api/commands` prices every provider as anthropic; ported bug-for-bug. The fix changes numbers the Commands tab already shows, so it rides with the price-book seam |
| 10 | DIV-040 | **schedule the backfill with the DIV-016 fix, post-flip** | $617.41 (1.45 %) of `command_mart` cost that is not in `usage_events`; both implementations agree and the *store* is wrong |
| 11 | DIV-044 | **accept for the port; file the product migration item** | schema property, both sides. The soak event that would cross it is a month boundary with a log rotation (§6.1) |
| 12 | DIV-042, DIV-194 | **noted (now closed anyway)** | CLOSED by batch TEAMS 2026-08-01; see §1.6g |
| 13 | DIV-200 (hooks) | **ratify read-only; fix Python to match its own docstring** | the port already went read-only; this makes the reference agree with itself. Id collision resolved (DIV-340): batch E's pricing side-effect row is **DIV-350** |
| 14 | DIV-010, DIV-027, DIV-453 | **reconfirmed** | the 4 `>u64` clamp cases stay accepted-by-name in gate 4; the harness prints a *re-examine* warning if one starts passing |
| 15 | DIV-013 | **reconfirmed** | depth ceiling 1024 (orjson's exact ceiling) with loud counted skips |
| 16 | DIV-195 | **as recommended — re-measure scheduled, not hotfixed. DONE.** | `PYTHONHASHSEED=0` is exported and the matrix was run twice in a row for the first time: **735 for 735, same verdict on the same row, verdict-diff EMPTY**. See R1 |

**Additional maintainer-facing rows that are *not* in the 16** and would
otherwise be decided by silence. All now carry a disposition, so none of them is
silent in the ledger any more — but a disposition is a record, not a decision,
and these are the ones where the record says *the maintainer may want to look*:

* **DIV-016** — the primed-vs-unprimed price-book seam. Ruled bug-for-bug for the
  port; the real fix is **filed post-soak for both implementations** (desk ruling
  7), and rulings 9 and 10 both ride on it.
* **The thirteen `ARCHITECT-REVIEW` rows** listed in §2.1 — dispositioned
  2026-08-04, so what is open is the *behaviour*, not the classification. Five
  are behavioural
  and worth a line each: **DIV-151** (four `{"detail":"<field>"}` 422s the port
  knows are the wrong shape, pinned by a test, no case row — risk R8),
  **DIV-263** (the reference aborts before mutating on any backup IO error; the
  port states the constraint and does not enforce it), **DIV-306** (`stax start`
  deliberately does **not** reproduce the reference's success-claim on a failed
  bind — an explicit non-bug-for-bug product choice), **DIV-404** (`/static/x.js`
  answers `text/javascript` and `/assets/x.js` answers `application/javascript`,
  on both implementations' own terms — named, not fixed, because the row that
  would pin it comes out red), and **DIV-441** (`ingest webhook serve` publishes
  an OpenAPI document and a Swagger UI on the receiver port; the port does not,
  which is arguably safer and is exactly why it was not closed either way).
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

### 2.4 Where the port is no longer a drop-in

Worth naming because "drop-in" is the gate's own language and these are the
places it is deliberately false. **The full set is now greppable** — the 108
rows dispositioned `fixed-in-rust` in §2.1 (`grep 'disposition:\*\* fixed-in-rust'
rust/TASKS-RS.md`) — and most of them are unreachable narrowings with a measured
zero incidence. The ones a maintainer would actually notice:

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

**This section is carried unchanged from the `a40db22` draft, deliberately.**
Every row is a quotation from `rust/PERF.md` with its command, nothing in it was
re-run for this refresh, and re-stating a measurement without re-taking it is how
a number loses its provenance. Read it as *last measured*, not as *at HEAD*. The
one place a later landing would move a row is §3.5's `/api/stats` line, which
ruling 5 authorises changing.

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

**The flip is executing.** Per `rust/DIRECTIVE-RULINGS-AND-FLIP.md` (2026-08-04)
the maintainer is repointing entry points machine-side, on both machines:
**`stax` = the Rust binary, `stackunderflow` = Python, both on `PATH`, through
the soak.** Entry points only — no version numbers, no tags, no release framing,
and none of that is this document's to propose. What follows is what that
decision runs into and what it does not.

### 4.1 The entry-point collision, and why it is now a one-line decision

`pyproject.toml:66-68` declares **two** console scripts, both pointing at the
Python CLI:

```toml
[project.scripts]
stackunderflow = "stackunderflow.cli:cli"
stax = "stackunderflow.cli:cli"
```

The Rust binary is also `stax` (`crates/stax-cli/Cargo.toml:8-10`). Whichever of
the two is installed last wins the name. The draft laid this out as three options
because it could not assume any of them; **the directive picks the middle one** —
Python keeps `stackunderflow`, Rust takes `stax`, both stay on `PATH` — and the
consequence the draft attached to it has largely dissolved:

| option | consequence, restated at this HEAD |
|---|---|
| Python keeps both names | the Rust binary needs a third name; every proof in this report was taken against a binary called `stax`, and the harness normalises that name on `Usage:` lines |
| **Python keeps `stackunderflow`, Rust takes `stax`** — *the executing flip* | matches the built artifacts. The draft's objection was that **70 of 105 CLI nodes had no Rust implementation**, making `stax <verb>` a partial surface; at this HEAD it is **5 of 105**, all five named and blocker-backed (§1.1), and Python covers all five under its own name for the whole soak |
| Python retires both | still requires §4.3's blockers closed — and §4.3 is closed (below). What remains for retirement is **DIV-349**, not packaging |

### 4.2 What runs where, at HEAD

The port is **three binaries**, not one:

| binary | crate | covers |
|---|---|---|
| `stax` | `stax-cli` | **100 of 105** CLI nodes + `store` + `anchor` + `msg` |
| `stax-server` | `stax-server` | **100 of 101** route+method pairs + the SPA + `/static` |
| `stax-hooks` | `stax-hooks` | all 9 hook ids, `hooks run <id> [--capture-content]` |

`stax hooks run` **is** in `stax-cli` at this HEAD (RS-8-054..058 landed in
tranche 2), so the hook contract is answerable from either binary; the separate
`stax-hooks` binary remains the one with the reference's exact argv and stdin
contract and the one the 2–5 ms budget was measured on.

**A hard constraint on renaming, recorded in the wave-6 findings and unchanged
by the flip:** `templates.canonical_command` writes the literal `stackunderflow
hooks run <id>` into a user's `settings.json`, and `parse_hook_command` is what
decides whether an existing entry is *ours* — a false positive **deletes another
tool's hook**. The port keeps the literal `stackunderflow`. Because the executing
flip leaves `stackunderflow` installed and pointing at Python, **this constraint
is satisfied by the flip as ordered** and no migration is needed. It would become
live the moment `stackunderflow` stopped existing, and that migration is still
not written.

### 4.3 Packaging — CLOSED: the binaries stand alone

**This section inverted since the draft, and it is the change that matters most
to a flip.** The draft's finding was that the Rust binaries read files living
*inside* `stackunderflow/`, so "a machine running only the Rust binaries still
needs the `stackunderflow` package directory on disk". A re-derivation then found
the list was **short by two** (DIV-400): `models.toml` is also a *runtime* read
in a different consumer (`stax-reports/src/pricing.rs:37`, on every priced
request) and `stackunderflow/infra/model_candidates.json` is a runtime read in
two more. A machine shipped only the two artifacts §4.3 originally listed would
have served the dashboard and then `500`-ed on every priced endpoint.

**All four are compiled in now** — disk-first with the binary as fallback for
three of them, so the parity harness still diffs two readers of one tree, and
embedded-by-default for `/static` so the endpoint matrix gates the embedded path:

| artifact | binding at HEAD |
|---|---|
| `stackunderflow/data/models.toml` | build-time `include_str!` **and** runtime disk-first with an embedded fallback |
| `stackunderflow/adapters/capabilities.json` | runtime disk-first, embedded fallback (`STACKUNDERFLOW_CAPABILITIES` still overrides — DIV-405: a *missing* override file now resolves to the embedded table) |
| `stackunderflow/infra/model_candidates.json` | runtime disk-first, embedded fallback |
| `stackunderflow/static/react/` | **embedded by default** (67 files, 4,786,123 bytes) |

**The proof is operational, not a unit test.** `rust/STANDALONE-PROOF.md`: both
checkouts bind-mounted over with an empty directory inside an unprivileged mount
namespace (`unshare -rm`), `cd /`, every relevant environment variable unset, the
binaries copied out first — because `cd /` alone would only defeat the *cwd* leg
of the resolvers, and `stax-server`'s `--package-dir` default is a compile-time
absolute path that a different cwd does nothing to. From inside that namespace,
with `capabilities`, `models.toml` and the React bundle all reported
`UNREACHABLE`: `stax store` exit 0 on a 3.9 GB store copy; `stax memory
decisions` exit 0 over the real 384,697-message corpus; `stax docs show
support-matrix` exit 0 — **a command that used to error off a checkout**;
`stax-hooks` answering its usage contract; and `stax-server` on `:8102` serving
`/`, `/settings`, `/static/react/…`, `/assets/…`, `/favicon.ico` and four API
endpoints, with the **md5s verified equal to the checked-in files'** outside the
namespace. The endpoint matrix was re-run before and after the embedding:
**763 rows, 692 identical / 0 divergent / 71 known-open, RC=0, verdict-diff
EMPTY**. What it cost and the behaviour-for-behaviour diff against the old
`ServeDir` mount are in `rust/PACKAGING-STANDALONE.md`; three residues are
recorded rather than hidden — **DIV-401** (the static root is *nominal* now; the
containment guard is lexical path math), **DIV-402** (`last-modified` is gone
from `/static`, because embedded bytes have no mtime — closing it means a
build-time timestamp, i.e. a reproducible-builds decision, not a parity one) and
**DIV-403** (the SPA 500 body is starlette's text verbatim now).

**Consequence, stated plainly and reversing the draft:** at this HEAD a machine
running only the Rust binaries **does not need the `stackunderflow` package
directory on disk**. Packaging is no longer a blocker to anything. It is also
not an argument *for* retiring Python — the executing flip keeps both on `PATH`
deliberately.

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

1. **The port migrates now — this caveat is retired.** The draft said
   `schema.apply()` was unported across 29 open migration items; at this HEAD all
   30 are landed and `rust/schema-differ.sh` proves **37 states, 37 identical**,
   `sqlite_master` text included. Rolling forward and rolling back are both onto
   a schema both implementations can produce and consume. The residue is
   **DIV-302**: `v008` reads the wall clock into the partition names, so two
   stores *created* either side of a UTC month boundary differ legitimately —
   a fresh-install property, not a rollback one.
2. **The port still does not create data files it was not handed** — no sidecar
   creation (DIV-077, narrowed exactly once at DIV-415 for `memory embed`), no
   price-book priming. The `status` half of this (DIV-239 / DIV-291) is closed
   for the reports family by **DIV-374**: `_open_store` creates *and* migrates
   exactly as `cli.py:1830` does, proved off-matrix on two empty homes that each
   bootstrapped a 528,384-byte v030 store with 37 tables. A rollback onto a home
   the port created may still find *sidecars* Python expects missing.
3. **Capture hooks write on every fire in both implementations** (DIV-201), so
   the two can be run side by side without either losing capture rows — the
   `UNIQUE (ts, hook_id, session_id)` key with microsecond `ts` is what makes a
   re-fire idempotent.

---

## 5. Risk register — what is *not* proven

Re-derived at this HEAD. Four of the draft's seventeen rows are **closed** and
say so with their evidence; the rest are re-stated with what moved.

| # | risk | status | evidence |
|---|---|---|---|
| R1 | **`PYTHONHASHSEED` and consecutive-run stability.** CPython randomises `str` hashing per process, so any reference payload built by iterating a `set` had a per-boot key order; `compare` measured **three different `diff.tokens` orders in three runs** with every other byte identical. The draft's sharpest sentence was that the matrix *"has never been run twice in a row to find out how many rows that is"*. | **CLOSED — DIV-195, ruled item 16 (re-measure, not hotfix).** `endpoint-parity.sh` now exports `PYTHONHASHSEED=0` plus `LC_ALL`/`LANG`/`TZ`/`PYTHONIOENCODING`, matching `parity-cli.sh`'s determinism block. | The matrix was run **twice consecutively and the verdicts compared: 657 identical · 0 divergent · 76 known-open · 2 flip-candidate of 735 on both runs, and the verdict diff between them is EMPTY** — not equal totals, the same verdict on the same row, **735 for 735**. The CLI matrix was likewise run twice with the same tally (1,104 cases). Consecutive-run stability now **exists** as a measured property, where the draft could only name its absence. What the seed buys prospectively: starlette's `", ".join(route.methods)` on a 405 is set-ordered and single-token today only because every FastAPI route declares one method. |
| R2 | **S3 transport is not ported and cannot be differed here** — `S3ObjectStore` is a `boto3` client and `boto3` is not installed on the parity host. | **DEFERRED with the reason** (DIV-213, `filed-post-soak`). What *is* ported and unit-pinned: `_full`, the `list` prefix strip, `parse_bucket_url`, `scheme_of`, `requires_boto3`, `store_from_url`'s dispatch. | `stax sync push` to an `s3://` destination prints `_SYNC_INSTALL_HINT` and exits 1 — **byte-identical to what Python does on this host**, because `_sync_missing_deps` also finds no `boto3`. That is *parity by agreement on the facts*, not parity of the transport. Closing it needs an S3 client **and** a host with `boto3`. |
| R3 | **The 4 `>u64` clamp cases** — `V-dec-limit-huge` / `V-dec-limit-bad` × 2 states. | **ACCEPTED and RECONFIRMED** (rulings item 14, 2026-08-04). | Named every run with `ACPT` lines and a tally section; a case that starts *passing* prints a re-examine warning (`rust/parity-cli.sh:336/341/350`). The same ruled class now also covers DIV-027 and DIV-453 in the ledger. |
| R4 | **Streams are probed, not differed.** `/api/live/stream` has **no case row, ever** — a `Verdict::Error` (socket timeout) is not downgraded by `!` and ranks *above* a divergence at exit 2, so one SSE row would take every other verdict with it. | **MOUNTED, still PROBE-ONLY.** The draft's blocker (DIV-165, *"the handler cannot mount at HEAD"*) is **closed** — DIV-320, one manifest line, a crate `axum-core` already depended on. DIV-136's rule is unchanged and deliberate: still no row. | `rust/parity/SSE-PROBE-d.md`. Cadence re-measured from the payload clock over 45 s: reference **6.0394 s** mean, port **6.0736 s** — **+34.2 ms per tick, +0.57 %** (DIV-321). Two structural notes came with the mount: the port can sit **one frame ahead** of the socket (DIV-322, capacity-1 `mpsc`), and hyper flushes the response head at 0.194 ms where uvicorn's h11 coalesces it with the first frame at 2.835 ms — identical byte *sequence*, different TCP segmentation (DIV-324). |
| R5 | **The port cannot create a store.** | **CLOSED.** All 30 migration items landed; `stax` bootstraps and migrates. | `rust/schema-differ.sh` — **37 states, 37 identical / 0 divergent**, `sqlite_master` compared verbatim. DIV-216 closed with it; DIV-374 proves the CLI path off-matrix (two empty homes → 528,384-byte v030 stores, 37 tables). Residue: DIV-302's month-boundary schema. |
| R6 | **No regression gate exists for perf.** Zero criterion benches in the workspace; spec §6 designated them as the replacement for the Python suite's 4 load-sensitive perf-budget tests. | **OPEN — DIV-349, and it is now the RULED Python-retirement gate.** Build the criterion gates **during** the soak; the 4 Python perf tests are not retired until they exist. **The flip is not gated on this; retirement is.** | Verified at HEAD: no `criterion` in any manifest, no `benches/` directory. `PERF.md` is a scoreboard, not a check. |
| R7 | **`422` body bytes are pinned to a fastapi/pydantic release, not to a contract.** | **RECORDED** (DIV-053), and materially narrowed since the draft. | Four shapes were measured byte-identical, then a fifth (the two-entry `missing` list, DIV-198), and then **DIV-367 closed the whole `dict`-bodied handler class byte-for-byte**: ten dict-bodied handlers enumerated from the Python tree by `ast`, one shared extractor, and three probe findings that no transcription would have produced (a `null` body is `missing`, not `dict_type`; `dict[str,str]` reports every bad key in body order; the malformed-body leg is FastAPI's, not pydantic's, so all three `BaseModel` handlers shared it and all three were wrong). What stays unpinned is the wording across a dependency bump — cross-reference the known CI drift note: CI installs unpinned latest fastapi. |
| R8 | **Four `{"detail":"<field>"}` 422s survive** in `routes/commands.rs` ×2, `routes/cost.rs`, `routes/budgets.rs`. **None has a case row**, so nothing has ever measured them. | **NAMED, NOT FIXED** (DIV-151, `fixed-in-rust` — the flag was discharged 2026-08-04 and the shape still stands). Now `json::validation_422_field_only` with a test pinning the wrong-but-current bytes. | The batch's own generalised lesson: *a validation path with no case row is an unported branch wearing a green tick.* |
| R9 | **`math.erf` is unreachable under `#![forbid(unsafe_code)]`**; the fdlibm transcription differs from glibc 2.31 on **2.520 % of 220,042 points, always exactly 1 ULP**. | **OFF the case-row path** (`var<=0` short-circuits every p-value on this store) — DIV-176. One manifest/`lib.rs` line. | Not reachable today; reachable the moment a store produces a positive variance on that path. |
| R10 | **No TLS crate in the workspace at all**, so `_fetch_from_litellm` (HTTPS) cannot be ported. Outbound HTTPS *works* on this host (`200 1670646`) — the port is pinned to the reference's fetch-failure leg **by a dependency gap, not by the network**. | **RECORDED** (DIV-199, `filed-post-soak`). This is also the named blocker parking `ingest github`, the fifth unported CLI node. | `PRICING-REFRESH-DIFFER.md`: deterministic set 6/6 identical, fetching set 4/4 divergent, as predicted. |
| R11 | **Two ported endpoints run an LLM / network call on the reference and would become writers if the service is up.** If :11434 is ever open, Python runs first and the port would serve **Python's freshly-written grade back** — an accidental near-match hiding a fabricated row. | **PROCEDURAL GUARD ONLY** (DIV-170, DIV-352) — a banner in the case file. A mechanical guard is an `endpoint-parity.sh` change. | The determinism of `M-ollama` is *a property of the machine, not the code*. Soak event §6.1. |
| R12 | **The differ is a sequencer over one shared home**, so two endpoint classes fall outside it: a body that never ends (R4) and a request whose second execution observably differs from its first (DIV-146). | **STRUCTURAL**, and materially reduced. | Both are written into the case-file header rules. What shrank the exposure is **case-local homes** (`@home[:SEED]`): each side gets its own fresh copy of a seed at the same path and the two trees are diffed, so a writer is proven on three axes — stdout, exit code, and the bytes left on disk. Six writer procedures run green under it. This still bounds what "692 identical" can mean. |
| R13 | **A fixture produced by the code under test can be green by vacuum.** | **FIXED** and generalised into a law. | `build_sync_state.py` seeded `merged.db` by running `runner.push`, so every later case was a no-op both sides agreed on — the differ reported green **on empty buckets**. Same class as the hooks corpus finding (the maintainer's `tools_json` is names-only, so **the active-recall feature cannot fire on his own data**) and as DIV-447, where two rejection rows named destinations the parser *accepts*. Three instances, one law: *agreement is not the property.* |
| R14 | **The live store's marts are stale against a from-scratch rebuild** — both implementations agree; the *store* is wrong. `command_mart` carries **959 events that no longer exist and $617.41 (1.45 %) of cost that is not in `usage_events`**; `tool_mart` 32 events / $19.25; `message_tool_mart.byte_count` 30,541 low. | **NOT a port divergence** (DIV-040) — a property of the Python mart layer, reproduced exactly. **RULED (item 10): schedule the backfill with the DIV-016 fix, post-flip.** | The ruling converts this from an open risk into scheduled work; the number itself is unchanged. |
| R15 | **A watermark hazard is structural on the watcher path.** `count_added == 0 → processed_offset = file_size` means a cycle that reads a partially-flushed line advances the watermark **past** it and the record is lost permanently. | **OPEN, maintainer's desk** (wave-4 finding 1). Not in the 16 and not ruled. | DIV-043's event filter removed the pressure that made it reachable; the branch is unchanged **on both sides**. Seven consecutive appends land clean; the hazard is structural, not incidental. |
| R16 | **CPython exception text interpolated into handler messages** — DIV-214's delimiter families, and DIV-137 / DIV-197 / DIV-357 / DIV-451 as the same shape. | **BUG-FOR-BUG IMPOSSIBLE**, marked loudly rather than faked. | A corpus row that crosses one **fails** rather than quietly agreeing on the wrong string (`crates/stax-sync/src/pyerr.rs:81`'s `<unported: …>` marker); none does today, which is exactly why the marker exists. The `Expecting value` family *is* translated, including CPython's token-start back-up, and the scanner was fuzzed against `json.loads` over 645K documents with one mismatch class found and fixed (DIV-261). |
| R17 | **Ledger integrity.** The draft: 9 double-allocated ids, 109 of 224 rows with no disposition, ticks lagging the landings ~3×. | **LARGELY CLOSED, with two new instances.** Ids: renumbered (DIV-340). Dispositions: **all 360 rows carry one, zero `UNDECIDED`** (§2.1) — DIV-342 closed, and RS-10-003's divergence-closure half with it. Ticks: 146/551 → **321/542**, and the reconciliation that moved them is auditable per item (`TICK-RECONCILIATION.md`). | **The two new instances are why this row is not marked CLOSED:** **DIV-480** (one id, two findings — DIV-340's shape, recurring) and **DIV-481** (split ticks re-created four legs after twelve of them were deleted and the rule *"ids are unique — never re-list"* was written down). Both are bookkeeping, not absent work — but the draft's point stands: from the file alone, a bookkeeping lag and a claimed-but-absent-work lag are indistinguishable, which is why the mechanical gate in §2.1 matters more than either count. |

## 6. What a soak would have to cover

RS-10-002 asks for *"both stacks side-by-side on the live store; every
response/mart/envelope diff either closed or filed."* The campaign's own findings
say a soak measured in *days* proves less than a soak measured in **events
crossed**. What follows is the coverage list, not a schedule; the decision to run
it, and for how long, is the maintainer's.

**What the rulings changed here, and it is the whole shape of this section.**
The draft's soak list was a list of *undecided things a soak might settle*. It
is not that any more. Of the eleven events below, **six now have a ruled
disposition on their divergence** — the soak no longer decides them, it exercises
work that has already been authorised, or it confirms a decision already made.
Two events remain genuinely soak-only: nothing in the tree can cross them and no
ruling covers them. That distinction is now the first column.

### 6.1 Events the soak must cross

| event to cross | covered by a ruling? | what it settles |
|---|---|---|
| **A local-midnight boundary with no intervening ingest** | **RULED — item 6** (Python-first date-key fix, then port) | DIV-091: `_SPEND_CACHE` is keyed `(store_path, period_start, period_end)` and validated on `store.db`'s mtime, but `date.today()` is **not in the key**, so Python serves yesterday's `daily_costs` — one element short, moving the projection denominator, the weighted-7d tail, `days_to_limit` and `alert`. The port is *more correct* there. The soak now **verifies a fix**, not a question. Unreachable by the differ at all; needs a running pair over midnight |
| **A month boundary with a log rotation** | **RULED — item 11** (accept for the port; file the product migration item) | DIV-044: `UNIQUE (session_fk, seq)` is per-partition, so a reparse whose month moved lands a **second** row. The same-month absorption is pinned by a test; the cross-month case is documented, not exercised. The soak measures the accepted behaviour's real incidence |
| **A rate-card edit** | **RULED — §6b five: bug-for-bug stands** | DIV-001: every rate-card edit reopens the frozen-mart-cost divergence (±0.001 % on `all`, **−65 %** on a dirty project's `week`). Nothing crosses it unless prices move |
| **A `%2F` or trailing-slash request from a real client** | **RULED — items 1, 1b, 2** (port both 307s; match Python on `%2F`) | DIV-133 / DIV-361 / DIV-168 / DIV-169, reproduced on three endpoints that are **green today** and invisible because no case row sends one. The rulings say the rows get written; crossing it in a soak is then a regression check, not a discovery |
| **A cross-origin caller** | **RULED — item 3** (port CORS; landing now) | DIV-050. The differ is same-origin by construction, so a soak with a real browser origin is the only thing that exercises the layer being added |
| **An Ollama instance actually running** | partly — **R11's procedural guard**, not ruled | DIV-170 / DIV-352 / DIV-135: the quality endpoint **becomes a writer**, and because Python runs first the port could serve Python's freshly-written grade back — an accidental near-match hiding a fabricated row. :11434 is closed on the parity host. This is the one soak event that can produce a *false green*, and the guard against it is a banner in a case file |
| **`status` / `plan` on a window with data in it** | **no longer a soak event — CLOSED** | §1.6f: tranche 3's run-clock fixture (`build_clock_state.py`) crosses it every gate run, and the seam it found is filed as DIV-281 |
| **A second consecutive full matrix run** | **no longer a soak event — DONE** | R1 / DIV-195: run, and the verdict diff is **empty across 735 rows**. This was the draft's headline unknown and it is now a measured property |
| **A non-USD configured currency** | **soak-only** — no ruling, no row, no fixture | DIV-052: the port answers an error rather than inventing a rate. Both harness states are USD, so nothing in the tree can cross this |
| **A model alias being set** | **soak-only** — no ruling, no row, no fixture | DIV-147: `crate::pricing::engine` does not consult `settings.model_aliases` but `infra.costs.compute_cost` does on **every** call. There is no divergence today *only because the alias map is empty on every state the campaign tests*. One user action changes prices on one side and not the other — the highest-value soak event on this list |
| **A hook fire on a store whose `tools_json` carries arguments** | **soak-only in substance** (the corpus limit is recorded, not ruled) | The maintainer's own data is names-only, so four of nine hooks only ever exercise their silent branch and the active-recall feature **cannot fire on his own data**. The synthetic corpus proves the code; only real data proves the feature |

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
   DIV-350). One `POST /api/search/reindex` rebuilt a parity home's index from
   53 KB to **520 MB in 192 s** and starved 47 cases (DIV-078).
3. **Writers get case-local homes**, the mechanism tranche 1 built: each side
   gets its own fresh copy of a seed **at the same path** (because `backup list`
   prints its own directory), and the two trees are diffed — so a writer is
   proven on three axes: stdout, exit code, and the bytes left on disk.

And one the soak inherits from the flip: **both entry points stay live**
(`stax` = Rust, `stackunderflow` = Python), which is what makes a side-by-side
soak possible at all and what makes rollback a `PATH` decision rather than an
install.

### 6.3 What the soak cannot close

No soak closes: **DIV-349** — the criterion perf gates, which are *built during*
the soak by ruling and which gate **Python's retirement** (not the flip);
DIV-240 (ruled: contract-clean is the bar, so byte parity is closed by decision,
not by measurement); DIV-213 (needs an S3 client *and* a `boto3` host); DIV-199
(needs a TLS crate, and it is what parks `ingest github`); DIV-205 (recall's
CPython child is deliberate); DIV-138 (`POST /api/meta-agent/chat`, deferred
whole); the wall-clock known-open rows, which are unpinnable by construction and
not by neglect; and the five behavioural rows of the
former `ARCHITECT-REVIEW` thirteen (§2.1) — dispositioned 2026-08-04, but each
still a decision the maintainer can reverse rather than a measurement anyone is
missing.

**The §6b five close by ruling and are closed** — bug-for-bug stands through the
soak, with DIV-002's and DIV-005's Python-side fixes filed for after it. They are
listed here only so nobody expects a soak to move them.

## Appendix A — reconciliation: where the artifacts disagree with each other

Ten discrepancies were found while cross-checking `TASKS-RS.md`'s ticks and
tallies against the proof artifacts. New ids are allocated from **DIV-340** per
the campaign's numbering handoff.

**Count: 10 (DIV-340 … DIV-349).**

**State at this HEAD, and where their dispositions live.** These ten are the only
DIV ids whose home is this document rather than the ledger, so the §2.1 sweep
does not cover them and they are dispositioned here instead:
**DIV-340 RESOLVED** (renumbered at fleet-quiet), **DIV-341 CLOSED** (both
endpoints ported and byte-identical), **DIV-342 CLOSED** (every ledger row
carries a disposition — §2.1), **DIV-343 CLOSED** (321 of 542 ticked, every tick
artifact-backed via `TICK-RECONCILIATION.md`), **DIV-344 partly closed** (17
duplicate ids → 8; twelve split-tick duplicates deleted — but see DIV-481),
**DIV-345 OPEN** (the numeric disagreements between `PERF.md` and the landing
notes are unchanged; nothing re-ran those corpora), **DIV-346 CLOSED** (the
inventory generator now verifies every node against the shipped binary and
reports zero disagreements), **DIV-347 CLOSED** (DIV-025's row now carries
`fixed-in-rust` and names the executed desk ruling instead of reading `OPEN`),
**DIV-348 narrowed** (a passing `!` row is no longer scored as a win in the
flip-candidate column, 0 at HEAD), **DIV-349 OPEN and RULED** — the criterion
gates are built *during* the soak and they gate **Python's retirement**, not the
flip.

**Two new discrepancies were found by the 2026-08-04 disposition sweep** and are
written up in `rust/TASKS-RS.md` § *Disposition sweep* rather than duplicated
here: **DIV-480** (the id `DIV-119` names two different findings — a duplication
note in the ledger and the unrowed pydantic `json_invalid` 422 shape in
`parity/DIV-c-optimize.md`; DIV-340's shape, one id short of the renumbering
that was meant to end it) and **DIV-481** (split ticks recurred — the telephone
leg re-listed `RS-5-039` / `RS-5-041` as `[x]` while their canonical lines stay
`[ ]`, four legs after twelve such duplicates were deleted and the rule *"ids are
unique — never re-list"* was written down).

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
| `rust/parity-cli.sh` + `parity/cases.txt` | gate 4 — **552 rows × 2 store states = 1,104 cases**; 1,100 pass + 4 accepted, RC=0, run twice |
| `rust/endpoint-parity.sh` + `parity/endpoint-cases.txt` | gate 6 — **763 rows**; 692 identical / 0 divergent / 71 known-open, RC=0 |
| `rust/ingest-parity.sh`, `rust/ingest-tail-proof.sh` | gate 5 + the live tail |
| `rust/hooks-parity.sh` + `parity/hook-cases.txt` | 80 hook invocations, five surfaces |
| `rust/sync-parity.sh` + `parity/sync-cases.txt` | 192 sync rows incl. cross-impl `age` interop |
| `rust/help-tree.sh` → `parity/HELP-TREE.md` | the `--help` contract — **105 nodes / 100 in the binary / 93 contract-clean**, binary-verified per node |
| `rust/parity/CLI-INVENTORY.md` | the generated 105-node / 276-param CLI map (regenerated with a binary guard — DIV-346) |
| `rust/schema-differ.sh` | the migration runner — 37 states, 37 identical |
| `rust/STANDALONE-PROOF.md`, `rust/PACKAGING-STANDALONE.md` | the three binaries with both checkouts bind-mounted away; what was embedded and what it cost |
| `rust/import-differ.sh`, `rust/telephone-differ.sh`, `rust/project-set-differ.sh`, `rust/etl-backfill-cli-differ.sh`, `rust/memory-embed-differ.sh` | the later legs' isolated writer procedures |
| `rust/TICK-RECONCILIATION.md` | every proposed tick with a pointer a reader can run |
| `rust/DIRECTIVE-RULINGS-AND-FLIP.md` | the 16+1b rulings and the executing flip |
| `rust/{ETL-BACKFILL,PRICING-REFRESH,SEARCH-REINDEX,QA-REINDEX,TAGS-REINDEX}-DIFFER.md`, `REFRESH-DIFFER.md` | the six isolated writer procedures |
| `rust/parity/SSE-PROBE-d.md` | the stream, probed |
| `rust/parity/DIV-c-*.md` (8), `DIV-e-*.md` (12) | per-member divergence write-ups |
| `rust/PARITY-wave1-*.md` (4) | wave-1 dated runs |
| `rust/PERF.md` | every perf row with its command |
| `rust/TASKS-RS.md` | the item ledger + the divergence ledger |
| `rust/MAINTAINER-RULINGS-REQUEST.md` | the 16+1b decisions — **all answered 2026-08-04** |
| `rust/ARCHITECT-STATE.md` | wave state, findings ledger, fleet roster |
| `rust/BATCH-*-CLAIM.md` | batch fences — **all landed and committed at this HEAD** |
