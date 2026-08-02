//! `routes/pricing.py` — 1 endpoint, wave 5 (batch A).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-094` | `GET` | `/api/pricing/doctor` | `/api/pricing/doctor` | ported |
//!
//! `assemble_pricing_health` is the whole module: three read-only sweeps over
//! `usage_events`, each guarded by a `sqlite_master` probe so a fresh install
//! answers `ok` instead of 500ing, plus a freshness probe of the on-disk
//! LiteLLM overlay that deliberately does **not** fetch.
//!
//! # The assembler moved; this file is the HTTP shell
//!
//! `cli.py`'s `pricing doctor` verb imports **this route module's**
//! `assemble_pricing_health`, exactly as its `worktrees list` imports the
//! worktrees route's assembler. `stax-cli` may not link `stax-server`
//! (DIV-279), so the one implementation lives in
//! [`stax_reports::pricing_doctor`] and both surfaces call it — DIV-375's close
//! applied before the fork rather than after it. What stays here is what is
//! genuinely HTTP: two query parameters, their FastAPI-ordered validation, and
//! the primed engine the *server* prices with.
//!
//! * **The estimate prices through the *primed* book.** `server.py`'s lifespan
//!   flips `infra.costs` onto the `price_book` table before it serves a byte, so
//!   the `estimated_delta_usd` figures here are book-priced, not manifest-priced.
//!   [`crate::pricing::engine`] pins the Rust half to the same source — this is
//!   RS-3-082's seam, and reading the manifest instead would quietly change
//!   every dollar in the payload. The CLI verb passes the *manifest* engine for
//!   the mirror-image reason: `cli.py` never calls `use_price_book_store`.

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::get;
use stax_reports::pricing_doctor::{DEFAULT_LIMIT, DEFAULT_STALE_DAYS, assemble_pricing_health};

use crate::json::{HandlerResult, HttpError, JsonBody, join_failure, validation_422};
use crate::qs::Query;
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/pricing/doctor", get(get_pricing_doctor))
}

async fn get_pricing_doctor(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // FastAPI validates BOTH parameters before the handler body runs, and
    // reports the FIRST failure in declaration order (`stale_days`, then
    // `limit`) — so a request with two bad values names `stale_days`.
    let stale_days = match query.int_or("stale_days", DEFAULT_STALE_DAYS) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };
    let limit = match query.int_or("limit", DEFAULT_LIMIT) {
        Ok(value) => value,
        Err(err) => return Ok(validation_422(&err)),
    };

    tokio::task::spawn_blocking(move || {
        let conn = state
            .connect()
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let engine = crate::pricing::engine(&conn, state.package_dir())
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let app_dir = state
            .store_path()
            .parent()
            .map(std::path::Path::to_path_buf);
        assemble_pricing_health(&conn, &engine, app_dir.as_deref(), stale_days, limit)
            .map(JsonBody::ok)
            .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
    })
    .await
    .map_err(|err| join_failure(&err))?
}
