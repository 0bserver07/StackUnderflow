//! The service layer — `stackunderflow/services/*.py` and `reports/*.py`.
//!
//! # Why this directory exists
//!
//! Wave 5's first two endpoint batches ported the routes that *are* their
//! handlers: `routes/cost.py` and `routes/projects.py` carry their own SQL and
//! their own arithmetic, so `routes/cost.rs` carries it too and the file map is
//! one-to-one. The seven modules batch C inherited are the opposite shape —
//! each is a thin HTTP wrapper (55–405 lines) over a large unported service
//! (262–871 lines) that the CLI calls through the *same* function. `/api/compare`
//! and `stackunderflow compare` share `services/compare.py`; `/api/export` and
//! `stackunderflow export` share `reports/export.py::run_export`.
//!
//! Transliterating that logic into the route module would fork it: wave 8 ports
//! the CLI verbs, finds no shared home, and writes a second copy that drifts.
//! So the split Python already has is the split reproduced here — the route
//! module parses query parameters, validates them, calls one function in this
//! directory, and stamps currency. Everything else lives here.
//!
//! # Ownership
//!
//! Batch C owns this directory and the single `mod services;` line in
//! [`crate`]'s root. Nothing else in `lib.rs` was touched, and `routes/mod.rs`
//! was not touched at all — the 34 slots were already wired by the wave-5 lead.
//!
//! # The module map
//!
//! | Module | Python source | Consumed by |
//! |---|---|---|
//! | [`aggregate`] | `reports/aggregate.py` | `routes/plan.rs`, [`export`] |
//! | [`anomaly`] | `reports/anomaly.py` | `routes/optimize.rs` |
//! | [`burn`] | `services/burn.py` | `routes/plan.rs` |
//! | [`compare`] | `services/compare.py` | `routes/compare.rs` |
//! | [`context_budget`] | `services/context_budget.py` | `routes/context_budget.rs`, [`prescribe`] |
//! | [`context_replay`] | `services/context_replay.py` | `routes/context_replay.rs` |
//! | [`export`] | `reports/export.py` | `routes/export.rs` |
//! | [`json_error`] | `Lib/json/decoder.py` (error path) | `routes/data.rs` |
//! | [`mart_queries`] | `store/mart_queries.py` | `routes/data.rs`, [`optimize`] |
//! | [`messages`] | `api/messages.py` | `routes/data.rs` |
//! | [`mode_recommender`] | `services/mode_recommender.py` | [`prescribe`] |
//! | [`optimize`] | `reports/optimize.py` | `routes/optimize.rs` |
//! | [`outcome_attribution`] | `services/outcome_attribution.py` | `routes/yield_route.rs` |
//! | [`plans`] | `services/plans.py` | `routes/plan.rs` |
//! | [`prescribe`] | `reports/prescribe.py` | `routes/optimize.rs` |
//! | [`scope`] | `reports/scope.py` | [`optimize`], [`prescribe`], [`export`], `routes/plan.rs` |
//! | [`yield_tracker`] | `services/yield_tracker.py` | `routes/yield_route.rs` |

pub mod aggregate;
pub mod anomaly;
pub mod burn;
pub mod compare;
pub mod context_budget;
pub mod context_replay;
pub mod export;
pub mod json_error;
pub mod mart_queries;
pub mod messages;
pub mod mode_recommender;
pub mod optimize;
pub mod outcome_attribution;
pub mod plans;
pub mod prescribe;
pub mod scope;
pub mod yield_tracker;
