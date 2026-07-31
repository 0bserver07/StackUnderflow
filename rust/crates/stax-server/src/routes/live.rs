//! `routes/live.py` — 2 endpoints, wave 5 (batch D). **DEFERRED — DIV-141/093.**
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-075` | `GET` | `/api/live/stats ` | `/api/live/stats`  | **open** — DIV-141 |
//! | `RS-5-076` | `GET` | `/api/live/stream` | `/api/live/stream` | **open** — DIV-136 |
//!
//! # `/api/live/stats` — 581 lines of service, and a clock in the payload
//!
//! `services/live.py::snapshot` is three things: `rolling_burn` (a local-day and
//! local-month cutoff arithmetic plus a month-end projection over
//! `calendar.monthrange`), `tool_latency_percentiles` (a `LEAD() OVER (PARTITION
//! BY session_fk ORDER BY seq)` window query with the §6b hoisted-floor and
//! list-subquery shapes, plus a nearest-rank percentile), and the two max-id
//! watermarks. The first two are service-layer ports; the third is two lines.
//!
//! It also stamps `burn.ts = datetime.now(UTC).isoformat()`, so **even a perfect
//! port cannot produce a green row here** — every response differs from every
//! other response, on both servers. That does not make it unportable, but it
//! does mean the endpoint's evidence would be a `!` row either way, which moved
//! it behind the endpoints that can go green.
//!
//! # `/api/live/stream` — the finding, not just the deferral (DIV-136)
//!
//! **A byte differ cannot hold a case row for an SSE endpoint, and `!` does not
//! help.** `!` downgrades `Verdict::Divergent` to `Verdict::KnownOpen`; it does
//! nothing to `Verdict::Error`, which is what `parity/src/endpoints.rs` returns
//! when `http::request` cannot complete — and `Tally::exit_code` ranks `Error`
//! **above** a divergence, at exit **2, a harness failure**. An SSE body never
//! ends: the handler loops on a 2 s poll until the client disconnects, so the
//! differ's socket read times out on both sides and the run exits 2 with two
//! harness errors, taking every other row's verdict down with it.
//!
//! So `/api/live/stream` gets no row in `endpoint-cases-d.txt`, and that is a
//! deliberate extension of the DIV-059 rule ("a `!` row must still be SAFE to
//! execute") to a second class: **a `!` row must also be able to TERMINATE.**
//! What was verified instead is recorded in `rust/parity/SSE-PROBE-d.md` — a
//! manual `curl` against the Python side alone, capturing the status line, the
//! header block, and the first frame, so the shape the port would have to match
//! is written down rather than guessed at when the stream is ported.

use axum::Router;

use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
///
/// Returns the router unchanged: the module is DEFERRED, so every path above
/// 404s. A dark surface the ledger names beats a half-lit one nobody can
/// reason about — the ruling `!A-*` / DIV-082 set for `routes/agent_teams.py`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
}
