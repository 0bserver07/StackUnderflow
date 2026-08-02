//! The ETL brain: normalizers, pricing, marts, the transactional writer, the watcher.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `etl/` package — the
//! per-provider normalizers that fold base tables into `usage_events`, the
//! effective-dated pricing engine reading the same `data/models.toml` rate cards
//! (`at_ts` semantics preserved), the mart builders, the transactional writer with
//! its watermarks, and the filesystem watcher (`notify`). Wave 3's gate is
//! cent-exact mart sums against Python, which means reproducing the deferred-bug
//! behavior catalogued in §6b (frozen mart costs, classifier fall-through,
//! `<synthetic>` folding) exactly, not fixing it silently.

#![forbid(unsafe_code)]

pub mod backfill;
pub mod ingest;
pub mod marts;
pub mod normalize;
pub mod pricing;
pub mod stats;
// ── RS-8-101 / RS-2-006 (the import leg) — appended, never interleaved ──────
/// `import_history_source` — the orchestration half of the history-plugin
/// contract. Here rather than in `stax-adapters` because it drives
/// `ingest::writer::ingest_file`, and adapters stay storage-free.
pub mod history_import;
