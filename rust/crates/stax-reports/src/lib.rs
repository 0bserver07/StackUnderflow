//! The report/spend domain layer — `stackunderflow/reports/*.py` plus the
//! report-shaped half of `stackunderflow/services/*.py`.
//!
//! # Why this crate exists
//!
//! Wave 5 batch C put this layer inside `stax-server`, and its module doc said
//! why in as many words: `/api/compare` and `stackunderflow compare` share
//! `services/compare.py`, so transliterating that logic into a route module
//! would fork it — "wave 8 ports the CLI verbs, finds no shared home, and writes
//! a second copy that drifts". That reasoning was right and it still holds. The
//! only thing wrong with it was the *address*: the shared home was inside the
//! HTTP crate, so wave-8 tranche 1 could only reach `build_report` by making
//! `stax-cli` depend on `stax-server`, and the CLI binary began linking `axum`
//! and `tokio` to print `today: $0.00 (0 msg)`. Tranche 1 filed that as a
//! deviation and named the fix. This crate is the fix.
//!
//! The rule it restores is the wave-5 law, unchanged: **one owner per helper.**
//! `stax-reports` owns the layer; `stax-server` and `stax-cli` both *consume*
//! it, neither owns it, and there is still exactly one `build_report` in the
//! workspace.
//!
//! # What is here, and what deliberately is not
//!
//! Here: everything that is a pure function of a [`rusqlite::Connection`], a
//! scope and a pricing engine, returning [`serde_json::Value`] — the shape this
//! layer already had. Twenty-two modules moved unchanged (`git mv`, then the
//! `super::` → `crate::` rewrite the move forces and nothing else), plus
//! [`pyops`] (the Python one-liners both consumers share) and [`pricing`] (the
//! price-book seam a report prices through).
//!
//! Not here: anything that speaks HTTP or owns a runtime. `agent_teams`,
//! `etl_backfill`, `etl_status`, `json_error`, `live`, `messages`,
//! `ollama_proxy`, `playback`, `playback_fs`, `pricing_refresh` and
//! `session_compare` stayed in `stax-server::services` — they are consumed by
//! exactly one route module each and no CLI verb reaches them. `json::JsonBody`
//! stayed too: it is starlette's `JSONResponse`, and a response writer belongs
//! to the crate that writes responses. `stax_server::services` re-exports every
//! module that moved, so every `crate::services::…` path inside the server is
//! still the path it was — the split cost the routes not one edit.
//!
//! # The two assertions the split moved rather than deleted
//!
//! 1. `compare`'s "the service sorts its aliases and the route does not" test
//!    named `stax_server::routes::compare::unknown_period_detail`. Half of it
//!    now lives in that route module. Both halves still run.
//! 2. `grading`'s frozen-body assertions rendered through
//!    `stax_server::json::JsonBody::ok(…).render()`. They now call
//!    [`stax_memory::pyjson::dumps_http`] directly, which is the *only* thing
//!    `render` ever did — same bytes, one indirection fewer.
//!
//! A moved proof is a proof. A dropped one is how a divergence gets shipped.

#![forbid(unsafe_code)]

pub mod aggregate;
pub mod anomaly;
pub mod benchmark;
pub mod benchmark_stats;
pub mod burn;
pub mod compare;
pub mod context_budget;
pub mod context_replay;
pub mod export;
pub mod forks;
pub mod grading;
pub mod mart_queries;
pub mod mode_recommender;
pub mod optimize;
pub mod outcome_attribution;
pub mod patterns;
pub mod plans;
pub mod prescribe;
pub mod pricing;
pub mod pyops;
pub mod risk;
pub mod scope;
pub mod worktrees;
pub mod yield_tracker;

pub mod render;
