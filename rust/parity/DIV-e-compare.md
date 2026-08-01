# Batch E / member `compare` — findings for `GET /api/sessions/compare`

Item **RS-5-105**, the last unported handler in `stackunderflow/routes/sessions.py`
(516 lines; the other two endpoints landed in batch B). Closes **DIV-070**.

Divergence ids are **not self-assigned** — the integrator numbers these from 153
at fold-in, per the batch-E claim. They are FINDING 1…8 below.

Files touched (the member's fence, nothing else):

| file | what |
|---|---|
| `crates/stax-etl/src/stats/aggregator.rs` | `+134` (41 src — `summarise_session_costs` — and 93 test) |
| `crates/stax-etl/src/stats/mod.rs` | `+18/-5`: the scope paragraph that declared it unported |
| `crates/stax-server/src/services/session_compare.rs` | new, **519** lines (was a 1-line placeholder) |
| `crates/stax-server/src/routes/sessions.rs` | `+519/-14`: one endpoint, its 422 helper, 12 tests; 977 → 1474 lines |
| `parity/endpoint-cases-e-compare.txt` | new, 143 lines — 3 known-open rows + 19 green rows |
| `parity/DIV-e-compare.md` | this file |

---

## 0. The charter grant — what the "deliberately unported" comment said, and says now

`crates/stax-etl/src/stats/mod.rs`, before:

> Scope, as of wave 5: the whole `stats/` package except `enricher.scan_sessions`
> and `aggregator.recompute_tz_stats` / `summarise_session_costs` (each named in
> the module that would host it, with the reason).

The stated reason, traced through `TASKS-RS.md` DIV-070 and the header of
`routes/sessions.rs`, is **scope, not a hazard**: nothing in the mart path or in
`dataset::get_project_stats` calls the function, its only consumer is this
endpoint, and closing it "means a new public function in **another crate**, which
the batch fence forbids". There is no correctness, safety or performance caveat
anywhere in the tree. The grant's precondition holds, so it was exercised.

After (abbreviated — the full paragraph is in the file):

> Scope, as of wave 5 batch E: the whole `stats/` package except
> `enricher.scan_sessions` and `aggregator.recompute_tz_stats` …
>
> `aggregator.summarise_session_costs` was on that exclusion list until batch E
> and is not any more … The stated reason was scope, not a hazard: its only
> consumer is `GET /api/sessions/compare`, which was itself unported (DIV-070)
> … Batch E's `compare` member closes RS-5-105, so the function is ported here —
> where `stax-server` can call it — rather than transliterated a second time
> behind the crate boundary, which is precisely the drift `stats/` exists to
> prevent.

The endpoint-side comments that repeated the exclusion (`routes/sessions.rs`
lines 28–37, and the `register` doc comment that explained why the route was
deliberately *unmounted*) are rewritten in the same pass, so no file in the tree
still claims the function is missing.

**The port.** Python source: `stackunderflow/stats/aggregator.py:102-117`
(16 lines including the docstring), driving `_SessionCostCollector`
(`:277-356`), which `stax_etl::stats::aggregator::SessionCostCollector` already
carried because `summarise` needs it. The Rust function is therefore the same
three steps Python takes — resolve `provider` from `ds.records[0]`, feed every
record, call `result(ds.interactions)` — and nothing else. `_safe(fn, [])` and
the route's `or []` are exception guards over a total port; both land on the
same empty array, which the tests pin.

Five tests, in `aggregator.rs`'s existing test module:

* `the_session_cost_shortcut_is_the_section_summarise_would_have_built` —
  equality with `summarise(ds, …)["session_costs"]` on the module fixture. This
  is the whole contract, and it would fail on any drift in row order, float
  accumulation order or the int/float split.
* `the_shortcut_seeds_a_float_zero_cost_like_the_full_sweep` — LAW 3: `0.0`, not
  `0`, for an unpriced session, plus the four-key token bag.
* `an_empty_dataset_is_an_empty_array_not_a_null`.
* `the_rows_carry_every_key_the_compare_endpoint_reads` — the eleven keys in
  order; the endpoint indexes six of them by name.
* `the_shortcut_resolves_the_provider_from_the_first_record` — once, from
  `records[0]`, not per record.

---

## FINDING 1 — the 200 body is not byte-reproducible, in Python either

**`routes/sessions.py:405-408`.**

```python
keys = set(sa.get("tokens", {})) | set(sb.get("tokens", {}))
diff = {
    "cost":   sb["cost"] - sa["cost"],
    "tokens": {k: sb["tokens"].get(k, 0) - sa["tokens"].get(k, 0) for k in keys},
    …
```

The comprehension iterates a **`set` of `str`**. CPython randomises string
hashing per process (PEP 456) unless `PYTHONHASHSEED` is pinned, and
`endpoint-parity.sh` does not pin it — `parity-cli.sh` does, at line 113, which
is exactly why the CLI gate never saw this. The reference server therefore emits
a different `diff.tokens` **key order** on every boot.

**Measured, not reasoned.** Three runs of the real handler (reference tree
`../StackUnderflow`, its `.venv`, `.parity-state/fresh/store.db` opened
read-only) over the `!J-compare` pair:

```
run 1  "tokens":{"cache_read":…,"cache_creation":…,"input":…,"output":…}
run 2  "tokens":{"input":…,"cache_read":…,"cache_creation":…,"output":…}
run 3  "tokens":{"input":…,"cache_creation":…,"cache_read":…,"output":…}
```

Every other byte of the payload was identical across all three, including
`"cost":557.33358795` and `"duration_s":95371.622` — so the arithmetic is
pinnable and only this one object's key order is not.

This is **DIV-085's class of defect arriving through a different door**: a
payload that cannot agree with itself between two runs of the *same* server.
Consequence: `!J-compare` stays known-open, and the two other 200 rows
(`!J-compare-same`, `!J-compare-logpath`) are opened with it. No engineering
around it was attempted; there is nothing to match.

**What the port emits.** `services/session_compare.rs::token_diff` walks `a`'s
token keys in their own insertion order, then `b`'s keys that `a` did not have.
On the live shape (`enricher._usage_from` always writes the same four keys and
`_parse_entry` appends `reasoning` after them when > 0) that is
`input, output, cache_creation, cache_read[, reasoning]` — which is also the
order the `a.tokens` and `b.tokens` objects in the *same response* already use,
so the response at least reads consistently. It is documented at the function as
a chosen order, not a matched one.

**If the maintainer wants the row green**, the one-line fix is on the harness,
not the port: `export PYTHONHASHSEED=0` in `endpoint-parity.sh`'s python
subshell (the same line `parity-cli.sh` already carries). That is a harness
change and out of this member's fence, so it is filed rather than done. It would
also need the Rust order to be re-derived against seed 0, which is a real
measurement, not a guess. The *product* fix — make the comprehension iterate a
dict rather than a set — is a Python-side contract change and belongs to the
maintainer.

---

## FINDING 2 — `_json.loads` has no `try`, so a poison `raw_json` is a 500

**`routes/sessions.py:193`** (`payload = _json.loads(r["raw_json"])`), inside the
handler's `try` / `except Exception as e: raise HTTPException(500, f"Failed to
load stats: {e}")`.

A blob that will not parse, or a NULL column (`TypeError`), takes the whole
request down with CPython's `JSONDecodeError` text in the detail. The port skips
the row instead, which is the choice `routes/data.rs` and
`stats::dataset::build_enriched_dataset` already made and documented as
**DIV-064** ("a store with one is a store Python cannot serve at all"). Recorded
here so the third occurrence is not mistaken for a new decision. Not reachable
on the harness store; no case row (a row would need a deliberately corrupted
blob, which is a home mutation).

---

## FINDING 3 — the timestamp overwrite assumes the payload is a dict

**`routes/sessions.py:197-198`.**

```python
if r["timestamp"]:
    payload["timestamp"] = r["timestamp"]
```

`payload` is whatever `json.loads` returned. A `raw_json` holding a JSON array
or scalar makes this `TypeError` → 500. The port takes `as_object_mut()` and
skips the assignment, keeping the record — the same shape `routes/data.rs` has
at its equivalent line. The guard on `r["timestamp"]` is a **truthiness** test,
so an empty-string timestamp leaves the payload's own value alone; that half is
ported exactly.

---

## FINDING 4 — `resolve_legacy_log_dir` is computed and then read by nobody

**`routes/sessions.py:362-366`.** `log_dir` is passed to
`enricher.build(tagged, log_dir)`, which uses it only in step 5
(`scan_sessions`). `stax_etl::stats::enricher` does not port step 5 — nothing
reads `EnrichedDataset.sessions`, as its module docs say — so the Rust
`build_detailed` takes no `log_dir` and the call is not made at all.

Unobservable: `resolve_legacy_log_dir` is pure (a stored-path check, then an env
read plus `Path.home()`), returns a string, and raises nothing a request can
see. Filed because "the port skips a call the reference makes" is exactly the
kind of thing that should be written down rather than noticed later.

---

## FINDING 5 — the 500 wrapper text cannot be reproduced, and has no case row

**`routes/sessions.py:390-393`.** `f"Failed to load stats: {e}"` interpolates
the exception's `str`. For the reachable failure modes that is a `sqlite3.Error`
message, and `rusqlite::Error`'s `Display` is not the same string. The port
emits the same prefix and its own suffix.

No case row: every safe way to reach it (a locked store, a dropped table) is a
mutation of the shared home, which law 4 forbids. Same disposition the other
`except Exception` funnels in this file already have.

---

## FINDING 6 — the currency branch is DIV-052, and its shape matters when it lands

**`routes/sessions.py:414-421`.** `rate != 1.0` rewrites `a.cost`, `b.cost` and
`diff.cost`. DIV-052 makes the non-USD leg unreachable (`active_currency_payload`
refuses anything but USD until the Frankfurter chain is ported), so the
conversion is **not ported blind** — the same call the ported
`routes/commands.rs` and `routes/data.rs` make.

One detail worth keeping for whoever does port it: Python writes
`sa = {**sa, "cost": float(sa["cost"]) * rate}`, and because `cost` is already a
key, the rebuilt dict keeps it in **position 5**. The correct port is an
in-place update of the existing key, not an append. Getting that wrong is a
silent key-order divergence in a response that has no other float to compare.

Also note the currency read is **outside** the handler's `try`, so a currency
failure is not a "Failed to load stats" 500. The port keeps it outside.

---

## FINDING 7 — the 400 leg is real, verified, and unreachable in the merged matrix

**`routes/sessions.py:349-351`** — `path = log_path or deps.current_log_path`,
then `400 "No project selected or log_path provided"`. Both operands are
truthiness tests, so `?log_path=` is "no override", not "the empty project".

Verified against the reference: `(400, 'No project selected or log_path
provided')`. It cannot be a case row: `P-by-dir-known` selects a project at line
68 of `endpoint-cases.txt` and nothing in the file ever deselects, so
`deps.current_log_path` is truthy for every `J-*` row. The row is offered in
`endpoint-cases-e-compare.txt`'s header for placement in the **pre-selection**
block, and the leg is pinned meanwhile by
`compare_tests::no_project_and_no_log_path_is_the_four_hundred`.

*(Related, and stated because it looks like a bug and is not: the comment above
`J-files-noproject` in the shared matrix says "the no-project state, before
anything is selected", but those rows run after line 68's selection. That is a
stale comment in another member's territory, not a finding of this member's.)*

---

## FINDING 8 — the SECOND session 404 is a distinct branch, and it is reachable

**`routes/sessions.py:398-403`.** After the id check at `:379` has proved both
sessions exist, the handler looks them up again in the `session_costs` rows and
404s a second time. That is not defensive duplication: a session row with **zero
`messages` rows** clears the first check and produces no collector entry.

The harness store has **2 103** such sessions. `2c0af9c7-b01c-45ea-9404-8de61fcb363c`
in project `-home-tmos` is one, and the reference answers
`404 'Session(s) not found: 2c0af9c7-…'` for it — so `J-compare-no-costs` is a
green row on the only path to that branch, and the branch is not an unported leg
wearing a green tick (law 5).

---

## Behaviours pinned that a naive port gets wrong (not divergences)

* **The 404 names an id once per POSITION, not once per value.**
  `[sid for sid in (a, b) if sid not in found_sids]` walks the tuple, so
  `?a=zzz&b=zzz` really does answer `Session(s) not found: zzz, zzz`. Measured.
  Row `J-compare-dup-miss`; unit test in `services/session_compare.rs`.
* **`a == b` is a 200, not a 404.** `session_id IN (?, ?)` binds the same value
  twice and SQLite returns one row; both sides of the diff then point at it and
  every field is zero — with `cost` / `duration_s` still rendering `0.0` and
  `commands` / `errors` rendering `0`. Row `!J-compare-same`; unit test.
* **`?a=` is a valid `str`.** Empty strings pass pydantic and reach the handler,
  where they name no session: `Session(s) not found: ` with a trailing space.
  Rows `J-compare-empty-a` / `J-compare-empty-ab`.
* **A repeated `?a=` keeps the LAST value** (starlette's `QueryParams.get`).
  Row `J-compare-a-repeat`.
* **The project 404 spells the slug** — `f"Project '{slug}' not found in store"`
  — where the neighbouring `/api/jsonl-content` says only "Project not found in
  store". Two handlers, one file, two messages. Row `J-compare-no-proj`.
* **The message fetch has NO `ORDER BY`.** `by_model` is insertion-ordered and
  the session cost is a `+=` chain over it, so adding an `ORDER BY` would move
  the last bits of `cost`. The statement is transliterated, not improved.
* **LAW 2** — the pricing engine comes from `crate::pricing::engine(&conn, …)`,
  never `default_engine()`.
* **LAW 7** — this handler has no `table_exists` guard to get wrong; it reads
  `messages` (the partitioned VIEW) unconditionally, exactly as Python does.
* **LAW 8** — measured, and FastAPI does **not** answer the field-only
  `{"detail":"<field>"}` shape here. It answers the pydantic `missing` list,
  including **two entries in declaration order** when both parameters are absent
  (rows `J-compare-no-a` / `J-compare-no-b` / `J-compare-no-ab`, measured on
  fastapi 0.141.1 / pydantic 2.13.4). The port builds the two-entry body by
  concatenating `json::missing_query_param`'s output rather than re-spelling the
  entry, so there is still exactly one place that knows the shape. This is a
  fourth 422 shape measured byte-identical, further narrowing DIV-053's
  "approximate" caveat.
* **`RawEntry.origin`** — Python's `RawEntry` has a fourth field, set to the
  session id. The Rust struct has never carried it because nothing downstream of
  `classifier.tag` reads it; nothing to port.

---

## Case rows

`parity/endpoint-cases-e-compare.txt`. Three known-open 200 rows (FINDING 1) and
**nineteen** green rows over 422 / 404 / 405 / 404-Not-Found, plus the
`P-by-dir-known` selection row the integrator strips.

| row | proves |
|---|---|
| `!J-compare` | the claim's row: the happy path, float-zero cost, empty `models_used`, multi-model cost fold, ~95 000 s duration |
| `!J-compare-same` | `a == b` is a 200 with an all-zero diff, not a 404 |
| `!J-compare-logpath` | `log_path=` overrides the selected project and lands on the same body |
| `J-compare-no-a` / `-no-b` | the one-entry `missing` 422, per parameter |
| `J-compare-no-ab` | **two** entries, in declaration order (`a`, then `b`) |
| `J-compare-case-key` | query keys are case-sensitive; an unknown `A` is ignored, not rejected |
| `J-compare-unknown-a` / `-unknown-b` / `-unknown2` | the id 404, one and both sides, `', '`-joined in `(a, b)` order |
| `J-compare-dup-miss` | the same unknown id twice — `zzz, zzz`, not `zzz` |
| `J-compare-empty-a` / `-empty-ab` | `?a=` passes validation and 404s with an empty spelling |
| `J-compare-a-repeat` | a repeated parameter keeps the LAST value |
| `J-compare-wrong-prj` | the session lookup is scoped by `project_id IN (…)` |
| `J-compare-no-costs` | the SECOND 404 (FINDING 8) — the only row that reaches it |
| `J-compare-no-proj` | the project 404, with the slug interpolated |
| `J-compare-post` / `-put` / `-delete` | starlette's `{"detail":"Method Not Allowed"}` |
| `J-compare-parent` / `-subpath` | FastAPI's `{"detail":"Not Found"}`, not starlette's plain text |

Deliberately **not** added: a trailing-slash row (`/api/sessions/compare/` is
DIV-133, the architect's `lib.rs` item — the claim says do not extend it), and a
`?nonsense=1` row (it would be a fourth permanently-open 200 proving only that
unknown query keys are ignored, which `PL-plan-junk-query` already proves for a
green endpoint).

## Verification

* `cargo fmt -p stax-etl -p stax-server -- --check` — clean.
* `cargo clippy -p stax-etl --all-targets -- -D warnings` — clean.
* `cargo clippy -p stax-server -p stax-etl --all-targets -- -D warnings` — **no
  diagnostic in any of this member's four files** (verified by grepping the full
  output for them). Two `-D warnings` errors remain in the crate, both in
  another member's territory and both present before this work:
  `routes/search.rs:522` (`contains()` vs `iter().any()`) and
  `routes/tags.rs:1465` (`is_compiled` never used). The shared crate was also
  intermittently *uncompilable* during this member's window while
  `services/{live,patterns,benchmark_stats}.rs` were being written concurrently;
  the runs above are from after those settled.
* `cargo test -p stax-etl` — **330 passed, 0 failed** (lib), plus 5 integration
  binaries green.
* `cargo test -p stax-server` — **916 passed, 0 failed**. 29 of them are this
  member's: 5 in `stats::aggregator`, 6 in `services::session_compare`, 1 in
  `routes::sessions::tests` and 11 in `routes::sessions::compare_tests`, plus
  the 6 pre-existing `routes::sessions::tests` that still pass.
* `rust/parity-cli.sh` — run because this member touched `stax-etl`. **286
  cases, 282 pass, 0 FAIL, 4 accepted, 0 skipped**, exit 0 — the expected
  282 + 4 maintainer-accepted DIV-010 clamp cases, "byte-identical on every
  case".
* `endpoint-parity.sh` was **not** run (the integrator runs the matrix
  centrally).
