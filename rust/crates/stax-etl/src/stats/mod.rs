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
//! Scope: this is the mart path, not the whole `stats/` package. RS-3-062
//! (`aggregator.py`, 1,518 lines of collectors), RS-3-065 (`formatter.py`) and
//! the unread half of RS-3-063/-064 are still open; each module below states
//! exactly which Python functions it carries and which it does not, so the
//! remainder is additive rather than a rewrite.

pub mod classifier;
pub mod command_analysis;
pub mod enricher;
pub mod pytext;
