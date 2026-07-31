# Rust port — full reimplementation on branch `rust`

**Status:** approved by maintainer 2026-07-31. Everything, not a core extraction.
**Branch:** `rust`, cut from `feat/relocatable-data-dir-and-ssh-sync` HEAD (39
commits — the port targets *current* behavior including the July perf work, not
`main`). Executed as a wave campaign by an agent fleet on tmos-hq, in a **git
worktree** (`../StackUnderflow-rust`) so it never disturbs other work.
**Prime directives:** fast as hell, runnable at the end of every wave, and
parity proven against the Python implementation — never asserted.

---

## 1. What is being ported (measured inventory, 2026-07-31)

| Surface | Size |
|---|---|
| Python total | **221 files / 76,925 lines** |
| CLI | `cli.py` 6,377 lines, **79 commands** |
| HTTP API | **93 endpoints** across 12 route modules (9,122 lines) |
| Services | 38 files / 20,832 lines |
| Adapters | 24 files / 9,792 lines (20 providers) |
| ETL | 38 files / 6,973 lines (normalizers, marts, watcher) |
| Store | 9 files / 3,105 lines + **26 SQL migrations** (schema v030) |
| Infra / stats / reports / hooks / sync | 29+5+11+8+8 files, ~17,770 lines |
| Tests | 336 files (~4,17x green baseline) + 24 fixture packs |

Not ported: the React UI (stays TypeScript — it becomes the parity harness),
docs-site, skills content, packaging metadata.

## 2. Why this is tractable: the spec already exists in executable form

1. **The SQLite store is the compatibility boundary.** Same `store.db`, schema
   v030, both implementations read/write interchangeably for the whole
   transition. Migrations port SQL-identical.
2. **The JSON envelopes are wire contracts** (`stackunderflow.memory/1`,
   `.resume/1`) with golden fixtures and a validator already in CI. The Rust
   fixture runner consumes the *same* `tests/fixtures/` files.
3. **The HTTP API has a living parity oracle:** the unmodified React build must
   work against the Rust server. Shape-parity minimum, byte-parity preferred.
4. **Identity/pricing are already data, not code** (`data/models.toml`,
   `adapters/capabilities.json`, `infra/model_candidates.json` — the July
   registry work). Rust reads the same files; zero table transcription.
5. **The encryption layer is already Rust underneath:** `pyrage` wraps `rage`.
   The sync crate uses `rage` directly — that dependency gets *simpler*.

## 3. Architecture

Cargo workspace at `rust/` **inside the branch**, so the Python reference, the
fixtures, and the port live side by side and the parity harness can run both:

```
rust/
  Cargo.toml            # workspace; release profile: lto=true, codegen-units=1
  crates/
    stax-core       # store open/migrate (rusqlite, bundled sqlite, fts5 on),
                    # schema v030, queries, watermarks, settings/app_dir
    stax-adapters   # 20 providers; SourceAdapter trait; capabilities.json
    stax-etl        # normalizers, pricing (models.toml, effective-dated at_ts),
                    # marts, transactional writer, watcher (notify crate)
    stax-memory     # FTS5+bm25 candidates, hybrid vector via Ollama HTTP,
                    # envelope serializers (the versioned contracts)
    stax-server     # axum; 93-endpoint parity; serves the existing React build
    stax-cli        # clap; the 79 commands, same names/flags/output shapes
    stax-sync       # ObjectStore trait: s3 + ssh transports; rage encryption
    stax-hooks      # the hook surface: <15ms budget end-to-end
    stax-wasm       # wasm32 query engine (read-only core, no watcher)
  parity/           # the differ: runs Python + Rust against the same store,
                    # diffs envelopes, endpoint responses, mart sums
  TASKS-RS.md       # the living fine-grained todo (~800 items, generated wave 0)
  PERF.md           # scoreboard: every wave, Rust vs the measured Python baseline
```

## 4. Waves

Each wave = fan-out of ~10 Opus agents (implementer/verifier pairs), orchestrated
by Fable. **A wave is done when: `cargo test` green, `cargo clippy` clean, the
wave's demo command runs against the real 382K-message store, parity fixtures
pass, and PERF.md gains measured numbers.** Runnable every wave — no exceptions.

| Wave | Deliverable | Runnable proof |
|---|---|---|
| **0. Bedrock** | workspace, CI gates, store open + v030 schema read, fixture-harness runner, TASKS-RS.md generated (the ~800 items, one per module/command/endpoint/migration, from §1's inventory) | `stax-rs status` opens the real store, prints counts matching Python |
| **1. Memory + resume** | envelopes byte-parity vs golden fixtures; FTS5 bm25 read path | agents on any box use `stax-rs memory ask` — value ships wave 1 |
| **2. Adapters** | all 20 providers enumerate+parse; malformed-fixture defensive corpus ported | `stax-rs scan` counts == Python's on the real store |
| **3. ETL brain** | normalizers, pricing engine, marts | backfill on a store copy: **mart sums identical to the cent**; the price-book equality gate re-proven |
| **4. Ingest** | transactional writer, watermarks, watcher | live tail: write a session file, row lands < 400ms, watermark parity |
| **5. Server** | axum, all 93 endpoints, static React serving | dashboard on :8096 against the same store; `parity/` endpoint differ green; baseline table beaten |
| **6. Sidecars** | search index build, QA, tags, bookmarks, embeddings via Ollama | search parity on candidate *sets* (see §6 risks) |
| **7. Sync + backup** | ssh + s3 ObjectStores, rage encryption, backup + `--to` replication | cross-impl round-trip: Python-pushed shards pull cleanly in Rust and vice versa |
| **8. CLI long tail + hooks** | remaining commands of the 79; hook surface | hook end-to-end < 15ms (Python floor: 159ms); `--help` tree diff vs Python |
| **9. WASM + perf gate** | wasm32 query engine + demo harness; criterion benches wired as regression gates | store.db dropped in a browser page answers queries; PERF.md complete |
| **10. Soak + decommission report** | both stacks side-by-side on the live store; every diff closed or documented | a maintainer-decision doc: what Rust replaces, what stays, what diverged |

Waves 1–2 can overlap 3 (different crates); 5 needs 3; 9 needs 6. The
orchestrator schedules; the table is dependency order, not strict serial.

## 5. Rules of the campaign

- **Parity is the definition of done.** A wave item closes on passing fixtures
  or a recorded, justified divergence in TASKS-RS.md — never silently.
- **Measure, never assert.** Every perf claim in PERF.md carries the command
  that produced it, vs the Python baseline (stats warm 72ms, include_stats 44ms,
  search 6–14ms, CLI floor 159ms, ingest <400ms).
- **The store is sacred.** All development against *copies* of the dataset
  (`cp` of store.db); the live dataset is read-only to this campaign.
- **Versions are maintainer-only** — the rule applies on this branch too. No
  crate gets a meaningful version; everything is `0.0.0-dev` until the
  maintainer says otherwise. CHANGELOG untouched by this campaign.
- **No pushes.** The branch lives on tmos-hq until the maintainer reviews.
- **TASKS-RS.md is the memory.** Wave state, item state, divergences, and
  maintainer questions live there — the fleet's sessions are ephemeral, the
  file is not.
- Rust toolchain pinned via `rust-toolchain.toml`; rustup user-local (no sudo).

## 6. Honest risks, pinned now

- **bm25 ranking parity:** SQLite's bm25 is deterministic but tie-breaking and
  float rounding can reorder near-equal candidates. Fixtures assert candidate
  *sets* + top-1, not full order, where flakiness appears. Divergences recorded.
- **JSON field order:** envelopes must serialize key-order-identical to Python
  (`serde_json` preserve_order feature) or the byte-parity goal downgrades to
  shape-parity. Decide in wave 0, record.
- **79 commands include macOS-specific paths** (launchd plists in `backup auto`)
  — port behind `#[cfg(target_os)]`, verify on both.
- **rusqlite→wasm:** wasm32 needs sqlite compiled to wasm (official build /
  wa-sqlite bridge). If it fights back, wave 9 falls back to a WASM-native
  read layer over exported pages. Spike early inside wave 9, fail loudly.
- **The 4 load-sensitive perf-budget tests** in the Python suite are flaky by
  nature; the criterion gates replace them with fixed-work benches.
- **Scope honesty:** 77K lines will not compress to a weekend. The wave
  structure exists so value ships from wave 1 (the static memory binary) even
  if the campaign pauses mid-flight.

## 7. Relationship to existing work

- `docs/specs/agent-remotes.md`: unblocked and *improved* by wave 1 — a static
  `stax-rs` binary makes every remote a zero-setup endpoint (the Ubuntu-20.04 /
  Python-3.8 ordeal of 2026-07-29 never happens again).
- The brand campaign (`docs/specs/brand-and-site.md`): wave 9's WASM engine is
  the "drop your store.db on stackunderflow.run, nothing leaves the browser"
  demo — the strongest possible form of the privacy pitch.
- sync-hub / #100: untouched; the sync crate implements the same shard format.
