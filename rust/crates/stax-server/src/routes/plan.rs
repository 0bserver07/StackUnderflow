//! `routes/plan.py` — 1 endpoint, wave 5 (batch C).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-090` | `GET` | `/api/plan` | `/api/plan` | **ported** |
//!
//! The HTTP mirror of `stax plan show`: the active plan, how much of
//! it has been spent this billing period, and a forecast for where the period
//! ends up. No query parameters at all — the response is a pure function of the
//! settings file, the store, and the clock.
//!
//! ```text
//! {
//!   "plan":       {"name", "monthly_usd", "reset_day"}                 | null,
//!   "usage":      {"used", "budget", "remaining", "pct", "projected",
//!                  "status", "period_start", "period_end",
//!                  "days_so_far", "days_in_period"}                    | null,
//!   "projection": {"projected_month_end_usd", "projection_method",
//!                  "daily_burn_usd", "days_to_limit", "thresholds",
//!                  "crossed_threshold", "alert"}                       | null,
//!   "currency":   {"code", "symbol", "rate_from_usd", "warning"}
//! }
//! ```
//!
//! With no plan set, the first three are `null` so the frontend can render an
//! "add a plan" CTA without parsing fields. `currency` is always present.
//!
//! # The shape of the handler
//!
//! Python's `plan.py` is not a thin route module and this is not a thin port of
//! it: the two spend helpers (`_spend_in_window`, `_spend_daily_window`) live in
//! the route module on both sides, because that is where the reference puts
//! them and moving them would fork the file map. The genuinely shared logic —
//! the plan itself, the billing window, the burn projector, the cross-project
//! rollup — is in [`crate::services::plans`], [`crate::services::burn`] and
//! [`crate::services::aggregate`], where the CLI wave will find it.
//!
//! # Three things a reader would not predict
//!
//! * **The two spend halves read two different clocks.** `compute_usage`
//!   anchors the billing window on `datetime.now(UTC).date()`;
//!   `_spend_daily_window` bounds its per-day series with `date.today()`, the
//!   *local* date. Those disagree for seven hours a day in `America/Los_Angeles`
//!   (UTC−7), so `days_so_far` and `len(daily_costs)` can differ by one. The
//!   port reproduces both reads exactly — see DIV-092 and
//!   [`crate::services::plans::Date`].
//! * **`compute_usage` is called twice, and the clock is read twice with it.**
//!   The first call passes a throwaway `used=0.0` purely to resolve the period
//!   window (which depends only on the plan and the date). Two `datetime.now`
//!   calls straddling midnight would resolve two different windows; that is the
//!   reference's behaviour and it is kept.
//! * **The dollar fields are pre-converted before send.** Every amount inside
//!   `usage` and the two inside `projection` are multiplied by
//!   `currency["rate_from_usd"]` so a single `formatCost(amount, currency)` on
//!   the frontend is correct. `plan.monthly_usd` is **not** — it stays the
//!   canonical USD number so pinned tests keep working.
//!
//! # The spend memo is not ported
//!
//! `_SPEND_CACHE` memoises `(used, daily_costs)` and validates against
//! `store.db`'s `st_mtime_ns`. It is a latency device — the precedent for
//! skipping one is DIV-055, `/api/stats`'s equivalent.
//!
//! It was not purely a latency device, and that was a finding rather than an
//! assumption: the key was `(store_path, period_start, period_end)`, while
//! `_spend_daily_window` reads `date.today()`, so a hit served across a local
//! midnight with no intervening ingest returned yesterday's series — one
//! element short, which moves `linear_projection`'s denominator and the
//! weighted tail. **Recorded as DIV-091 and now FIXED in the reference**
//! (ruling 6, Python-first): the local date joined the key, so the memo can no
//! longer answer for a day that has ended. Nothing here changed — the port
//! recomputes both halves per request, which was already the fixed behaviour,
//! and DIV-091 collapses into DIV-055's plain latency trade. Everything else
//! about the memo (and `invalidate_plan_cache`, which has no caller anywhere
//! in the tree) can only ever return the same bytes.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::services::aggregate::build_report;
use crate::services::burn;
use crate::services::plans::{self, Date, Plan, Usage};
use crate::services::scope::Scope;
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
///
/// Called once, from [`super::register_all`], at this module's `include_router`
/// position.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/plan", get(get_plan_status))
}

/// `GET /api/plan`.
///
/// Python declares this `async def` and then does blocking SQLite inside it,
/// which pins the event loop for the ~0.6s the rollup takes. The port runs the
/// body on `spawn_blocking` instead: same bytes, and the difference is not
/// observable in a response.
async fn get_plan_status(State(state): State<AppState>) -> HandlerResult {
    tokio::task::spawn_blocking(move || build_payload(&state))
        .await
        .map_err(|err| join_failure(&err))?
}

/// The whole handler body, off the event loop.
fn build_payload(state: &AppState) -> HandlerResult {
    // `currency = active_currency_payload()` runs FIRST, before the plan lookup,
    // so a currency failure is reported even when there is no plan at all.
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let app_dir = app_dir(state);
    let config = load_config(&app_dir);
    let plan = plans::get_active_plan(&config)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // `if plan is None:` — all three blocks null, currency still stamped.
    let Some(plan) = plan else {
        let mut payload = Map::new();
        payload.insert("plan".to_owned(), Value::Null);
        payload.insert("usage".to_owned(), Value::Null);
        payload.insert("projection".to_owned(), Value::Null);
        payload.insert("currency".to_owned(), currency);
        return Ok(JsonBody::ok(Value::Object(payload)));
    };

    // First call resolves the period window; the `used=0` argument is a
    // throwaway. The window depends only on the plan + today, not on spend.
    let window = plans::compute_usage(&plan, 0.0, Date::today_utc());
    let (used, daily) = spend_for_window(state, &window.period_start, &window.period_end)?;
    // …and the clock is read again here, exactly as Python's second
    // `datetime.now(UTC)` default does.
    let usage = plans::compute_usage(&plan, used, Date::today_utc());

    let thresholds = alert_thresholds(&config)?;
    let projection_usd = burn::build_projection(
        &daily,
        usage.used,
        plan.monthly_usd,
        usage.days_so_far,
        usage.days_in_period,
        Some(&thresholds),
        None,
    );

    // `rate = float(currency.get("rate_from_usd") or 1.0)` — TRUTHINESS, so a
    // rate of exactly 0.0 becomes 1.0 rather than zeroing every amount.
    // `crate::currency` only ever produces 1.0 today (DIV-052), which makes
    // every multiplication below an identity; it is written out anyway because
    // it is one operation and the day the rate chain lands it must already be
    // in the right places.
    let rate = currency
        .get("rate_from_usd")
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or(1.0);

    let mut payload = Map::new();
    payload.insert("plan".to_owned(), plan_block(&plan));
    payload.insert("usage".to_owned(), usage_block(&usage, rate));
    payload.insert(
        "projection".to_owned(),
        projection_block(&projection_usd, rate),
    );
    payload.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// `{"name": …, "monthly_usd": …, "reset_day": …}`.
///
/// `monthly_usd` is the canonical USD amount and is deliberately **not**
/// rate-converted; the active-currency mirror is `usage.budget`.
fn plan_block(plan: &Plan) -> Value {
    let mut block = Map::new();
    block.insert("name".to_owned(), Value::String(plan.name.clone()));
    block.insert("monthly_usd".to_owned(), Value::from(plan.monthly_usd));
    block.insert("reset_day".to_owned(), Value::from(plan.reset_day));
    Value::Object(block)
}

/// The `usage` block, with the four dollar fields pre-converted.
///
/// `_USAGE_COST_FIELDS = ("used", "budget", "remaining", "projected")`. `pct`,
/// the two `days_*` counts and the two `period_*` strings are dimensionless and
/// pass through. Note the KEY rename: the service calls it
/// `projected_month_end`, the wire calls it `projected`.
fn usage_block(usage: &Usage, rate: f64) -> Value {
    let mut block = Map::new();
    block.insert("used".to_owned(), Value::from(usage.used * rate));
    block.insert("budget".to_owned(), Value::from(usage.budget * rate));
    block.insert("remaining".to_owned(), Value::from(usage.remaining * rate));
    block.insert("pct".to_owned(), Value::from(usage.pct));
    block.insert(
        "projected".to_owned(),
        Value::from(usage.projected_month_end * rate),
    );
    block.insert("status".to_owned(), Value::from(usage.status));
    block.insert(
        "period_start".to_owned(),
        Value::String(usage.period_start.clone()),
    );
    block.insert(
        "period_end".to_owned(),
        Value::String(usage.period_end.clone()),
    );
    block.insert("days_so_far".to_owned(), Value::from(usage.days_so_far));
    block.insert(
        "days_in_period".to_owned(),
        Value::from(usage.days_in_period),
    );
    Value::Object(block)
}

/// The `projection` block. Only the two `*_usd` fields are converted.
fn projection_block(projection: &burn::Projection, rate: f64) -> Value {
    let mut block = Map::new();
    block.insert(
        "projected_month_end_usd".to_owned(),
        Value::from(projection.projected_month_end_usd * rate),
    );
    block.insert(
        "projection_method".to_owned(),
        Value::from(projection.projection_method.as_str()),
    );
    block.insert(
        "daily_burn_usd".to_owned(),
        Value::from(projection.daily_burn_usd * rate),
    );
    block.insert(
        "days_to_limit".to_owned(),
        projection.days_to_limit.map_or(Value::Null, Value::from),
    );
    block.insert(
        "thresholds".to_owned(),
        Value::Array(
            projection
                .thresholds
                .iter()
                .copied()
                .map(Value::from)
                .collect(),
        ),
    );
    block.insert(
        "crossed_threshold".to_owned(),
        projection
            .crossed_threshold
            .map_or(Value::Null, Value::from),
    );
    block.insert(
        "alert".to_owned(),
        projection
            .alert
            .as_ref()
            .map_or(Value::Null, |text| Value::String(text.clone())),
    );
    Value::Object(block)
}

// ── settings ─────────────────────────────────────────────────────────────────

/// `$STACKUNDERFLOW_HOME` — the directory `store.db` sits in.
fn app_dir(state: &AppState) -> std::path::PathBuf {
    state.store_path().parent().map_or_else(
        || std::path::PathBuf::from("."),
        std::path::Path::to_path_buf,
    )
}

/// `settings._load()` — a missing or corrupt file is `{}`, never an error.
fn load_config(app_dir: &std::path::Path) -> Map<String, Value> {
    std::fs::read_to_string(app_dir.join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// `Settings().get("plan_alert_thresholds") or list(burn.DEFAULT_THRESHOLDS)`.
///
/// Two fallbacks stacked, and they are not the same one:
///
/// 1. `_Opt.__get__`'s defensive type check — the declared default is a *list*,
///    so a persisted value that is not a list returns `list(self.default)`,
///    i.e. `[50, 75, 90]`. A number, a string, an object: all become the
///    default rather than raising.
/// 2. then `or` — Python truthiness, so an EMPTY list falls through to
///    `burn.DEFAULT_THRESHOLDS`. `[0]` does not; it is a perfectly good
///    one-rung ladder at 0%.
///
/// # Errors
/// An element `int()` would reject, which Python raises on inside
/// `build_projection`'s set comprehension.
fn alert_thresholds(config: &Map<String, Value>) -> Result<Vec<i64>, HttpError> {
    let persisted = match config.get("plan_alert_thresholds") {
        // Fallback 1: a non-list persisted value never reaches the caller.
        Some(Value::Array(items)) => items.as_slice(),
        _ => return Ok(burn::DEFAULT_THRESHOLDS.to_vec()),
    };
    // Fallback 2: `or` on an empty list.
    if persisted.is_empty() {
        return Ok(burn::DEFAULT_THRESHOLDS.to_vec());
    }
    persisted
        .iter()
        .map(|value| {
            threshold_int(value).ok_or_else(|| {
                HttpError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "plan_alert_thresholds contains a value int() cannot read".to_owned(),
                )
            })
        })
        .collect()
}

/// `int(t)` over one threshold element.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a percentage outside i64 is already nonsense; `as i64` saturates"
)]
fn threshold_int(value: &Value) -> Option<i64> {
    match value {
        // `int(50.7)` is 50 — truncation toward zero, not rounding.
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|raw| raw.trunc() as i64)),
        // `int("50")` works; `int("50.7")` raises.
        Value::String(text) => text.trim().parse::<i64>().ok(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

// ── the spend rollup ─────────────────────────────────────────────────────────

/// `_spend_for_window` minus the memo — see the module docs.
fn spend_for_window(
    state: &AppState,
    period_start: &str,
    period_end: &str,
) -> Result<(f64, Vec<f64>), HttpError> {
    let (since, until) = window_bounds(period_start, period_end)?;
    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // Law 2: the engine is built from THIS connection's `price_book`, never
    // `default_engine()`. The legacy `messages` fallback inside `build_report`
    // is the only consumer, but a store where the mart gate flips is exactly
    // where an un-injected pricer would silently mis-bill by 2%.
    let engine = crate::pricing::engine(&conn, state.package_dir())
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // `_spend_in_window` — one `Scope` built BY HAND, not through
    // `parse_period`: the label is the literal "plan-period" and the bounds are
    // naive midnight stamps, not the offset-aware ones `parse_period` renders.
    let scope = Scope::new(Some(since.clone()), Some(until.clone()), "plan-period");
    let report = build_report(&conn, &scope, None, None, &engine)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    // `float(report["total_cost"])` — already a float; the cast is Python's.
    let used = report.total_cost;

    let daily = spend_daily_window(&conn, period_start, period_end, &since, &until)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    Ok((used, daily))
}

/// The `(since, until)` pair both spend halves share.
///
/// ```python
/// since = datetime.combine(start_d, datetime.min.time()).isoformat()
/// until = datetime.combine(end_d + timedelta(days=1), datetime.min.time()).isoformat()
/// ```
///
/// `datetime.min.time()` is midnight and the result is **naive** — no `+00:00`
/// suffix — so the strings are ten characters plus `T00:00:00`. Those bounds are
/// compared as strings against a `ts` column that holds `+00:00` and `Z` forms
/// alike; `"…T00:00:00" < "…T00:00:00+00:00"` lexicographically, which makes the
/// lower bound slightly permissive and the (half-open) upper bound slightly
/// strict. Inherited, not corrected.
fn window_bounds(period_start: &str, period_end: &str) -> Result<(String, String), HttpError> {
    // WAVE 8 TRANCHE 3: the arithmetic moved to `stax_reports::plans` so the CLI
    // can reach it (`cli.py` imports `routes.plan._spend_daily_window` for the
    // same reason). This function is now the HTTP error shape and nothing else.
    plans::window_bounds(period_start, period_end).ok_or_else(|| {
        HttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "period_start is not an ISO date".to_owned(),
        )
    })
}

/// `_spend_daily_window` — per-day USD across every project, oldest-first.
///
/// Days with no recorded spend are `0.0`, not elided: a quiet weekend should
/// drag the weighted average down rather than vanish from the series. The query
/// hits `usage_events` only — there is deliberately **no** `messages` fallback
/// here, so on a pre-backfill store the list is all zeroes and the projector
/// degrades to "no data → linear projection of 0".
///
/// The walk runs `start_d → min(end_d, date.today())`, and `date.today()` is
/// the LOCAL date (DIV-092). The last element is therefore today's spend, which
/// is the orientation `weighted_projection` assumes.
fn spend_daily_window(
    conn: &Connection,
    period_start: &str,
    period_end: &str,
    since: &str,
    until: &str,
) -> rusqlite::Result<Vec<f64>> {
    // WAVE 8 TRANCHE 3: moved to `stax_reports::plans`, where `stax-cli` can
    // reach it. One owner per helper — the doc comment above is the contract and
    // it now lives with the implementation.
    plans::spend_daily_window(conn, period_start, period_end, since, until)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: Value) -> Map<String, Value> {
        match json {
            Value::Object(map) => map,
            _ => panic!("object"),
        }
    }

    /// A throwaway `$STACKUNDERFLOW_HOME` under the system temp dir.
    ///
    /// `line!()` keeps two tests in this file from colliding when the harness
    /// runs them on different threads of the same process.
    fn temp_home(marker: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stax-plan-{}-{marker}",
            u64::from(std::process::id())
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// The real `stackunderflow/` package dir — `data/models.toml` hangs off it.
    fn package_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets")
    }

    fn state_at(home: &std::path::Path) -> AppState {
        AppState::new(
            home.join("store.db"),
            package_dir(),
            crate::state::Config::default(),
        )
    }

    #[test]
    fn the_no_plan_payload_is_three_nulls_and_a_currency_block() {
        // The exact bytes the "add a plan" CTA parses — straight out of the
        // handler, not reassembled. Key order is the dict literal's, and
        // `rate_from_usd` renders `1.0` and not `1`. This is also the ONLY
        // branch the parity differ reaches on the harness home, whose
        // `config.json` carries no plan (see DIV-c-plan.md).
        let home = temp_home(line!());
        let body = build_payload(&state_at(&home)).expect("no plan is not an error");
        assert_eq!(
            body.render(),
            r#"{"plan":null,"usage":null,"projection":null,"currency":{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}}"#
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_configured_plan_over_a_quiet_window_renders_every_block_in_order() {
        // A whole trip through the handler: settings → billing window → spend
        // rollup → burn projector → currency. The store's only events are in
        // 1999, so `used` is deterministically 0.0 and everything downstream of
        // it is too; only the four clock-dependent fields are masked before the
        // byte comparison, and their KEYS still have to be in the right places.
        let home = temp_home(line!());
        std::fs::write(
            home.join("config.json"),
            r#"{"plan_name": "claude-max", "plan_monthly_usd": 200, "plan_reset_day": 1}"#,
        )
        .expect("config");

        let state = state_at(&home);
        let conn = state.connect().expect("store");
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
             CREATE TABLE usage_events (
                 id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                 session_id TEXT, ts TEXT, cost_usd REAL);
             INSERT INTO projects (id, slug) VALUES (1, 'alpha');
             INSERT INTO usage_events (project_id, session_id, ts, cost_usd)
             VALUES (1, 's', '1999-01-01T00:00:00+00:00', 12.0);",
        )
        .expect("fixture");
        drop(conn);

        let mut payload: Value =
            serde_json::from_str(&build_payload(&state).expect("plan set").render())
                .expect("valid json");
        // `period_start` / `period_end` / `days_so_far` / `days_in_period` move
        // with the calendar. Masking them in place keeps the key ORDER under
        // test while the values stop being a clock read.
        for (block, key) in [
            ("usage", "period_start"),
            ("usage", "period_end"),
            ("usage", "days_so_far"),
            ("usage", "days_in_period"),
        ] {
            let slot = payload
                .get_mut(block)
                .and_then(|b| b.get_mut(key))
                .expect("field is present");
            assert!(!slot.is_null(), "{block}.{key} must not be null");
            *slot = Value::String("…".to_owned());
        }
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            concat!(
                r#"{"plan":{"name":"claude-max","monthly_usd":200.0,"reset_day":1},"#,
                r#""usage":{"used":0.0,"budget":200.0,"remaining":200.0,"pct":0.0,"#,
                r#""projected":0.0,"status":"ok","period_start":"…","period_end":"…","#,
                r#""days_so_far":"…","days_in_period":"…"},"#,
                r#""projection":{"projected_month_end_usd":0.0,"projection_method":"linear","#,
                r#""daily_burn_usd":0.0,"days_to_limit":null,"thresholds":[50,75,90],"#,
                r#""crossed_threshold":null,"alert":null},"#,
                r#""currency":{"code":"USD","symbol":"$","rate_from_usd":1.0,"warning":null}}"#,
            )
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_custom_threshold_ladder_reaches_the_projection_block() {
        // Proves the settings → `build_projection` wire, which is the one thing
        // the no-plan differ row can never exercise. A 0% rung crosses at zero
        // spend, so the alert line fires on an untouched store.
        let home = temp_home(line!());
        std::fs::write(
            home.join("config.json"),
            r#"{"plan_name": "custom", "plan_monthly_usd": 50.5,
                "plan_alert_thresholds": [0, 90, 0]}"#,
        )
        .expect("config");

        let state = state_at(&home);
        let conn = state.connect().expect("store");
        // `usage_events` exists but is EMPTY, so `_has_usage_events` is false and
        // `build_report` takes the legacy `messages` path — the branch the
        // differ can never reach on a backfilled harness store.
        conn.execute_batch(
            "CREATE TABLE projects (id INTEGER PRIMARY KEY, slug TEXT NOT NULL);
             CREATE TABLE sessions (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL);
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY, session_fk INTEGER NOT NULL,
                 timestamp TEXT, model TEXT, speed TEXT,
                 input_tokens INTEGER, output_tokens INTEGER);
             CREATE TABLE usage_events (
                 id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                 session_id TEXT, ts TEXT, cost_usd REAL);",
        )
        .expect("fixture");
        drop(conn);

        let payload: Value =
            serde_json::from_str(&build_payload(&state).expect("plan set").render())
                .expect("valid json");
        let projection = payload.get("projection").expect("projection block");
        // Deduplicated and sorted, exactly as `sorted({int(t) …})` leaves it.
        assert_eq!(
            stax_memory::pyjson::dumps_http(projection.get("thresholds").expect("thresholds")),
            "[0,90]"
        );
        assert_eq!(
            projection.get("crossed_threshold").and_then(Value::as_i64),
            Some(0)
        );
        assert_eq!(
            projection.get("alert").and_then(Value::as_str),
            Some("Crossed 0% of plan budget")
        );
        // …and `monthly_usd` survives as the float it was persisted as.
        assert_eq!(
            stax_memory::pyjson::dumps_http(payload.get("plan").expect("plan block")),
            r#"{"name":"custom","monthly_usd":50.5,"reset_day":1}"#
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn the_plan_block_keeps_monthly_usd_in_dollars_and_renders_it_as_a_float() {
        let plan = Plan {
            name: "claude-max".to_owned(),
            monthly_usd: 200.0,
            reset_day: 1,
        };
        assert_eq!(
            stax_memory::pyjson::dumps_http(&plan_block(&plan)),
            r#"{"name":"claude-max","monthly_usd":200.0,"reset_day":1}"#
        );
    }

    #[test]
    fn the_usage_block_renames_projected_month_end_and_converts_only_dollars() {
        let usage = Usage {
            used: 10.0,
            budget: 100.0,
            remaining: 90.0,
            pct: 10.0,
            projected_month_end: 40.0,
            status: "ok",
            period_start: "2026-07-01".to_owned(),
            period_end: "2026-07-31".to_owned(),
            days_so_far: 5,
            days_in_period: 31,
        };
        // A rate of 2.0 scales `used` / `budget` / `remaining` / `projected` and
        // leaves `pct` and the day counts alone — the whole point of
        // `_USAGE_COST_FIELDS`.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&usage_block(&usage, 2.0)),
            r#"{"used":20.0,"budget":200.0,"remaining":180.0,"pct":10.0,"projected":80.0,"status":"ok","period_start":"2026-07-01","period_end":"2026-07-31","days_so_far":5,"days_in_period":31}"#
        );
    }

    #[test]
    fn the_projection_block_nulls_the_optionals_and_converts_two_fields() {
        let projection = burn::Projection {
            projected_month_end_usd: 12.5,
            projection_method: burn::ProjectionMethod::Weighted7d,
            daily_burn_usd: 2.5,
            days_to_limit: None,
            thresholds: vec![50, 75, 90],
            crossed_threshold: None,
            alert: None,
        };
        assert_eq!(
            stax_memory::pyjson::dumps_http(&projection_block(&projection, 1.0)),
            r#"{"projected_month_end_usd":12.5,"projection_method":"weighted-7d","daily_burn_usd":2.5,"days_to_limit":null,"thresholds":[50,75,90],"crossed_threshold":null,"alert":null}"#
        );

        let projection = burn::Projection {
            projected_month_end_usd: 12.5,
            projection_method: burn::ProjectionMethod::Linear,
            daily_burn_usd: 2.5,
            days_to_limit: Some(4),
            thresholds: vec![50],
            crossed_threshold: Some(50),
            alert: Some("Crossed 50% of plan budget".to_owned()),
        };
        // Only the two `*_usd` fields scale; `days_to_limit` and the ladder are
        // dimensionless.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&projection_block(&projection, 2.0)),
            r#"{"projected_month_end_usd":25.0,"projection_method":"linear","daily_burn_usd":5.0,"days_to_limit":4,"thresholds":[50],"crossed_threshold":50,"alert":"Crossed 50% of plan budget"}"#
        );
    }

    #[test]
    fn the_window_bounds_are_naive_midnight_stamps_and_the_upper_one_is_the_day_after() {
        let (since, until) = window_bounds("2026-07-01", "2026-07-31").expect("iso dates");
        // No `+00:00` — `datetime.combine(date, time.min)` is naive.
        assert_eq!(since, "2026-07-01T00:00:00");
        // `end_d + timedelta(days=1)`, so the query is half-open over the last
        // day rather than dropping it.
        assert_eq!(until, "2026-08-01T00:00:00");

        // …and the day-after roll crosses a month AND a year boundary.
        let (_, until) = window_bounds("2026-12-01", "2026-12-31").expect("iso dates");
        assert_eq!(until, "2027-01-01T00:00:00");
    }

    #[test]
    fn a_non_list_threshold_setting_falls_back_to_the_declared_default() {
        // `_Opt.__get__`: the declared default is a list, so a persisted
        // non-list returns `list(self.default)` rather than raising.
        for junk in [
            serde_json::json!(5),
            serde_json::json!("50,75"),
            serde_json::json!({"a": 1}),
            serde_json::json!(null),
        ] {
            let config = cfg(serde_json::json!({"plan_alert_thresholds": junk}));
            assert_eq!(
                alert_thresholds(&config).expect("defaults"),
                vec![50, 75, 90]
            );
        }
        // Absent entirely is the same answer.
        assert_eq!(
            alert_thresholds(&cfg(serde_json::json!({}))).expect("defaults"),
            vec![50, 75, 90]
        );
    }

    #[test]
    fn an_empty_threshold_list_falls_back_but_a_zero_rung_ladder_does_not() {
        // `Settings().get(...) or list(burn.DEFAULT_THRESHOLDS)` — truthiness.
        let empty = cfg(serde_json::json!({"plan_alert_thresholds": []}));
        assert_eq!(
            alert_thresholds(&empty).expect("defaults"),
            vec![50, 75, 90]
        );

        let zero = cfg(serde_json::json!({"plan_alert_thresholds": [0]}));
        assert_eq!(alert_thresholds(&zero).expect("kept"), vec![0]);

        let custom = cfg(serde_json::json!({"plan_alert_thresholds": [25, "60", 80.9]}));
        // `int("60")` is 60 and `int(80.9)` truncates to 80.
        assert_eq!(
            alert_thresholds(&custom).expect("coerces"),
            vec![25, 60, 80]
        );
    }

    #[test]
    fn an_uncoercible_threshold_element_is_a_500_not_a_silent_drop() {
        let config = cfg(serde_json::json!({"plan_alert_thresholds": [50, "ninety"]}));
        let err = alert_thresholds(&config).expect_err("int('ninety') raises");
        assert!(err.body().render().contains("int()"));
    }

    #[test]
    fn the_rate_falls_back_to_one_when_the_currency_block_says_zero() {
        // `float(currency.get("rate_from_usd") or 1.0)` — truthiness, so 0.0
        // becomes 1.0 instead of zeroing every dollar figure on the page.
        let rate = |value: Value| {
            value
                .get("rate_from_usd")
                .and_then(Value::as_f64)
                .filter(|v| *v != 0.0)
                .unwrap_or(1.0)
        };
        assert_eq!(rate(serde_json::json!({"rate_from_usd": 0.0})), 1.0);
        assert_eq!(rate(serde_json::json!({"rate_from_usd": null})), 1.0);
        assert_eq!(rate(serde_json::json!({})), 1.0);
        assert_eq!(rate(serde_json::json!({"rate_from_usd": 0.91})), 0.91);
    }

    #[test]
    fn the_daily_window_fills_absent_days_with_zero_and_stops_at_today() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE usage_events (ts TEXT, cost_usd REAL);
             INSERT INTO usage_events (ts, cost_usd) VALUES
                 ('2020-01-01T10:00:00+00:00', 1.0),
                 ('2020-01-01T11:00:00+00:00', 2.0),
                 ('2020-01-03T09:00:00+00:00', 4.0);",
        )
        .expect("fixture");

        // A window entirely in the past, so `min(end_d, today)` is `end_d` and
        // the length is deterministic.
        let series = spend_daily_window(
            &conn,
            "2020-01-01",
            "2020-01-04",
            "2020-01-01T00:00:00",
            "2020-01-05T00:00:00",
        )
        .expect("query");
        // Day 2 had no spend and is a 0.0, not a gap — the quiet-weekend rule.
        assert_eq!(series, vec![3.0, 0.0, 4.0, 0.0]);
    }

    #[test]
    fn a_window_that_has_not_started_yet_is_an_empty_series() {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch("CREATE TABLE usage_events (ts TEXT, cost_usd REAL);")
            .expect("fixture");
        // `while cursor <= last_day` with `last_day < start_d` never runs, so
        // the projector sees `[]` and degrades to a linear projection of 0.
        let series = spend_daily_window(
            &conn,
            "2099-01-01",
            "2099-01-31",
            "2099-01-01T00:00:00",
            "2099-02-01T00:00:00",
        )
        .expect("query");
        assert!(series.is_empty());
        assert_eq!(burn::linear_projection(&series), 0.0);
    }

    #[test]
    fn a_malformed_period_string_is_a_500_and_not_a_panic() {
        assert!(window_bounds("not-a-date", "2026-07-31").is_err());
        assert!(window_bounds("2026-07-01", "2026-02-30").is_err());
    }
}
