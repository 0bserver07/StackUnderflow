//! Port of `stackunderflow/stats/` — the subset `project_mart`'s second pass runs.
//!
//! `etl/marts/project.py` does not re-derive its message dimensions; it calls
//! the pipeline's own `classifier` / `enricher` / `aggregator` functions so the
//! materialised counts are identical to `get_project_stats` (the Python suite's
//! equivalence tests pin that). Porting the marts therefore means porting those
//! functions, and porting them **bug-for-bug** — `classifier._determine_kind`'s
//! fall-through to `"assistant"` is DIV-002 and 5,656 turns on the live store
//! depend on it staying wrong (`docs/specs/rust-port.md` §6b divergence 2).
//!
//! Scope, as of wave 5 batch E: the whole `stats/` package except
//! `enricher.scan_sessions` and `aggregator.recompute_tz_stats` (each named in
//! the module that would host it, with the reason).
//!
//! `aggregator.summarise_session_costs` was on that exclusion list until batch
//! E and is not any more — it is [`aggregator::summarise_session_costs`]. The
//! stated reason was scope, not a hazard: its only consumer is
//! `GET /api/sessions/compare`, which was itself unported (DIV-070), so nothing
//! in the mart path or in [`dataset::get_project_stats`] reached it. Batch E's
//! `compare` member closes RS-5-105, so the function is ported here — where
//! `stax-server` can call it — rather than transliterated a second time behind
//! the crate boundary, which is precisely the drift `stats/` exists to prevent.
//!
//! [`aggregator`] (RS-3-062), [`formatter`] (RS-3-065) and
//! [`dataset`] (`store/queries.build_enriched_dataset` + `get_project_stats`)
//! landed on top of the wave-3 mart subset rather than beside it — [`enricher`]
//! grew the fourteen fields the aggregator needs behind a
//! [`enricher::Detail`] flag so the mart path's memory profile is unchanged.
//!
//! [`dataset::get_project_stats`] is the public entry the server calls.

pub mod aggregator;
pub mod classifier;
pub mod command_analysis;
pub mod dataset;
pub mod enricher;
pub mod formatter;
pub mod pydatetime;
pub mod pytext;
pub mod sha256;
