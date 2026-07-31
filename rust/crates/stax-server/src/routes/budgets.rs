//! `routes/budgets.py` — 3 endpoints, wave 5 (batch A).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-047` | `GET`    | `/api/budgets` | `/api/budgets` | ported |
//! | `RS-5-048` | `PUT`    | `/api/budgets` | `/api/budgets` | ported |
//! | `RS-5-049` | `DELETE` | `/api/budgets` | `/api/budgets` | ported |
//!
//! # This is the first ported endpoint that WRITES
//!
//! `PUT` and `DELETE` persist through the descriptor settings, which means
//! `json.dumps(data, indent=2)` over `$STACKUNDERFLOW_HOME/config.json` — the
//! *CLI* writer (`ensure_ascii=True`, two-space indent, no trailing newline),
//! not the HTTP one. Getting that wrong would not show up in the PUT's own
//! response; it would show up on the next `GET`, from either server, reading a
//! file the other wrote. [`save_config`] is deliberately the CLI writer.
//!
//! The parity case rows are sequenced to leave the harness home exactly as they
//! found it: `GET` (unset) → `PUT` → `GET` → `DELETE` → `GET`. A `DELETE` that
//! did not restore the file byte-for-byte would be visible as a divergence on
//! the *following* run, which is the worst kind, so the round-trip is asserted
//! in a unit test as well.
//!
//! # The partial-write the validator leaves behind
//!
//! `set_budget` applies the monthly leg and *then* the daily leg, and each leg
//! writes the file before the next is validated. `{"monthly_usd": null,
//! "daily_usd": -5}` therefore clears the monthly ceiling on disk and *then*
//! 422s on the daily one. Bug-for-bug (DIV-068).

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, put};
use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HandlerResult, HttpError, JsonBody, join_failure};
use crate::qs::Query;
use crate::state::AppState;

/// `services/budgets.APPROACHING_PCT`.
const APPROACHING_PCT: f64 = 80.0;

/// `_SETTINGS_KEYS`.
const MONTHLY_KEY: &str = "budget_monthly_usd";
const DAILY_KEY: &str = "budget_daily_usd";

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/budgets", get(get_budget_status))
        .route("/api/budgets", put(put_budget))
        .route("/api/budgets", delete(delete_budget))
}

// ── the three handlers ───────────────────────────────────────────────────────

async fn get_budget_status(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> HandlerResult {
    let tz = timezone_offset(&raw)?;
    blocking_payload(state, tz, None).await
}

/// `PUT /api/budgets` — `BudgetBody` plus the omitted-vs-null distinction.
///
/// pydantic's `model_fields_set` is the whole design: a field the body did not
/// mention keeps whatever is persisted, while an explicit `null` clears it.
/// Reading the raw body and checking key PRESENCE is the only way to reproduce
/// that — a `serde` struct with `Option<f64>` collapses both into `None`.
async fn put_budget(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    body: Bytes,
) -> HandlerResult {
    let tz = timezone_offset(&raw)?;
    let parsed: Value = serde_json::from_slice(&body).map_err(|_| {
        HttpError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid JSON body".to_owned(),
        )
    })?;
    let Value::Object(fields) = parsed else {
        // A non-object body fails pydantic's model validation — 422 (DIV-053
        // already records that the `detail` list is not reproduced field-wise).
        return Err(HttpError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Input should be a valid dictionary or instance of BudgetBody".to_owned(),
        ));
    };
    // `float | None` — a non-numeric, non-null value is a 422 before the handler
    // body ever runs.
    let monthly = read_leg(&fields, "monthly_usd")?;
    let daily = read_leg(&fields, "daily_usd")?;
    blocking_payload(state, tz, Some(Write::Set { monthly, daily })).await
}

async fn delete_budget(State(state): State<AppState>, RawQuery(raw): RawQuery) -> HandlerResult {
    let tz = timezone_offset(&raw)?;
    blocking_payload(state, tz, Some(Write::Clear)).await
}

/// What a request asks the settings file to become, if anything.
enum Write {
    /// `set_budget(monthly_usd=…, daily_usd=…)`, with `Leg::Absent` preserving.
    Set { monthly: Leg, daily: Leg },
    /// `clear_budget()`.
    Clear,
}

/// One leg of the `PUT` body: mentioned-with-a-number, mentioned-as-null, absent.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Leg {
    Value(f64),
    Null,
    Absent,
}

fn read_leg(fields: &Map<String, Value>, key: &str) -> Result<Leg, HttpError> {
    match fields.get(key) {
        None => Ok(Leg::Absent),
        Some(Value::Null) => Ok(Leg::Null),
        Some(Value::Number(n)) => n.as_f64().map(Leg::Value).ok_or_else(|| {
            HttpError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Input should be a valid number: {key}"),
            )
        }),
        // pydantic's lax mode accepts a numeric STRING for a `float` field.
        Some(Value::String(s)) => s.trim().parse::<f64>().map(Leg::Value).map_err(|_| {
            HttpError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "Input should be a valid number, unable to parse string as a number: {key}"
                ),
            )
        }),
        Some(_) => Err(HttpError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Input should be a valid number: {key}"),
        )),
    }
}

/// `timezone_offset: int = 0` — a query param on all three verbs.
fn timezone_offset(raw: &Option<String>) -> Result<i64, HttpError> {
    Query::parse(raw.as_deref().unwrap_or_default())
        .int_or("timezone_offset", 0)
        .map_err(|err| HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, err.field))
}

async fn blocking_payload(state: AppState, tz_offset: i64, write: Option<Write>) -> HandlerResult {
    tokio::task::spawn_blocking(move || build_payload(&state, tz_offset, write.as_ref()))
        .await
        .map_err(|err| join_failure(&err))?
}

// ── the payload ──────────────────────────────────────────────────────────────

/// `_build_payload`, with the optional write applied first.
///
/// Order matters: `put_budget` / `delete_budget` mutate settings and *then* call
/// `_build_payload`, so the response always reflects the post-write state.
fn build_payload(state: &AppState, tz_offset: i64, write: Option<&Write>) -> HandlerResult {
    let app_dir = state.store_path().parent().map_or_else(
        || std::path::PathBuf::from("."),
        std::path::Path::to_path_buf,
    );

    match write {
        Some(Write::Clear) => {
            let mut data = load_config(&app_dir);
            data.remove(MONTHLY_KEY);
            data.remove(DAILY_KEY);
            save_config(&app_dir, &data)?;
        }
        Some(Write::Set { monthly, daily }) => {
            let current = read_budget(&app_dir);
            // "present-in-body → use the body value (incl. explicit null =
            // clear); absent → preserve whatever is already persisted."
            let monthly = resolve_leg(*monthly, current.0);
            let daily = resolve_leg(*daily, current.1);
            // DIV-068: each leg writes before the next is validated.
            apply_leg(&app_dir, MONTHLY_KEY, monthly)?;
            apply_leg(&app_dir, DAILY_KEY, daily)?;
        }
        None => {}
    }

    // `_build_payload` resolves currency FIRST, then the budget, then spend.
    let currency = active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let (monthly_usd, daily_usd) = read_budget(&app_dir);

    let conn = state
        .connect()
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let spend = spend_scalars(&conn, tz_offset)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    drop(conn);

    let mut status = compute_status(monthly_usd, daily_usd, &spend);
    // `_convert_status` is a no-op at rate 1.0 and DIV-052 makes anything else
    // unreachable, so the conversion is recorded rather than ported.
    let mut models = spend.models;
    models.sort();
    status.insert(
        "models".to_owned(),
        Value::Array(models.into_iter().map(Value::from).collect()),
    );

    let mut budget = Map::new();
    budget.insert(
        "monthly_usd".to_owned(),
        monthly_usd.map_or(Value::Null, Value::from),
    );
    budget.insert(
        "daily_usd".to_owned(),
        daily_usd.map_or(Value::Null, Value::from),
    );

    let mut payload = Map::new();
    payload.insert("budget".to_owned(), Value::Object(budget));
    // `status if budget.is_set else None` — the `models` key rides along inside
    // `status`, so an unset budget loses it too.
    payload.insert(
        "status".to_owned(),
        if monthly_usd.is_some() || daily_usd.is_some() {
            Value::Object(status)
        } else {
            Value::Null
        },
    );
    payload.insert("currency".to_owned(), currency);
    Ok(JsonBody::ok(Value::Object(payload)))
}

fn resolve_leg(leg: Leg, current: Option<f64>) -> Option<f64> {
    match leg {
        Leg::Value(value) => Some(value),
        Leg::Null => None,
        Leg::Absent => current,
    }
}

/// `_apply_leg` — `None` removes the key, a non-positive amount is a 422.
fn apply_leg(app_dir: &std::path::Path, key: &str, value: Option<f64>) -> Result<(), HttpError> {
    let mut data = load_config(app_dir);
    match value {
        None => {
            data.remove(key);
            save_config(app_dir, &data)
        }
        Some(amount) => {
            if amount <= 0.0 {
                // `raise HTTPException(status_code=422, detail=str(exc))` — the
                // ValueError's own message, not a pydantic error list.
                return Err(HttpError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("{key} must be a positive number"),
                ));
            }
            data.insert(key.to_owned(), Value::from(amount));
            save_config(app_dir, &data)
        }
    }
}

// ── settings I/O ─────────────────────────────────────────────────────────────

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

/// `settings._save()` — `json.dumps(data, indent=2)`, the **CLI** writer.
///
/// `ensure_ascii=True` and a two-space indent, with no trailing newline. This is
/// the one place in the crate where the HTTP writer would be wrong: the bytes go
/// to disk, not to a socket, and the next reader is a `json.load` on the Python
/// side.
fn save_config(app_dir: &std::path::Path, data: &Map<String, Value>) -> Result<(), HttpError> {
    // `parents=True` — a custom `--data-dir` may sit several levels deep.
    std::fs::create_dir_all(app_dir)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let rendered = stax_memory::pyjson::dumps_pretty(&Value::Object(data.clone()));
    std::fs::write(app_dir.join("config.json"), rendered)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

/// `get_budget()` — both legs through `_coerce_positive`.
///
/// A non-numeric or non-positive persisted value reads as UNSET rather than
/// raising, so a hand-edited `config.json` cannot wedge the route.
fn read_budget(app_dir: &std::path::Path) -> (Option<f64>, Option<f64>) {
    let data = load_config(app_dir);
    (
        coerce_positive(data.get(MONTHLY_KEY)),
        coerce_positive(data.get(DAILY_KEY)),
    )
}

fn coerce_positive(raw: Option<&Value>) -> Option<f64> {
    let amount = match raw? {
        Value::Number(n) => n.as_f64()?,
        // `float(raw)` accepts a numeric string.
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        // `float(True)` is 1.0 in Python — a `bool` is an `int`.
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => return None,
    };
    (amount > 0.0).then_some(amount)
}

// ── spend ────────────────────────────────────────────────────────────────────

struct Spend {
    month: f64,
    today: f64,
    models: Vec<String>,
    days_so_far: i64,
    days_in_month: i64,
}

/// `_spend_scalars` — month-to-date and today, across the WHOLE store.
///
/// A budget is a cap on everything the user does, so this is deliberately not
/// project-scoped. `tz_offset` is minutes east of UTC (`aggregator._local_day`'s
/// convention); the local month/day boundaries are expressed back as UTC
/// instants so they compare directly against the stored ISO-8601 `ts`.
fn spend_scalars(conn: &Connection, tz_offset: i64) -> rusqlite::Result<Spend> {
    let now_utc_us = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros(),
    )
    .unwrap_or(0);
    let now_local_us = now_utc_us + tz_offset * 60 * 1_000_000;
    let (year, month, day) = civil_from_micros(now_local_us);

    // `now_local.replace(hour=0, minute=0, second=0, microsecond=0)`.
    let local_today_us = micros_from_civil(year, month, day);
    // `.replace(day=1)`.
    let local_month_us = micros_from_civil(year, month, 1);
    // `(local_* - timedelta(minutes=tz_offset)).isoformat()`.
    let today_cutoff = isoformat_utc(local_today_us - tz_offset * 60 * 1_000_000);
    let month_cutoff = isoformat_utc(local_month_us - tz_offset * 60 * 1_000_000);

    let days_so_far = day;
    let days_in_month = days_in_month(year, month);

    if !table_exists(conn, "usage_events")? {
        return Ok(Spend {
            month: 0.0,
            today: 0.0,
            models: Vec::new(),
            days_so_far,
            days_in_month,
        });
    }

    // The `WHERE ts >= month_cutoff` prefilter makes the month CASE redundant
    // and the today CASE cheap. Ported shape-for-shape (§6b).
    let (month_cost, today_cost): (f64, f64) = conn.query_row(
        "SELECT \
           COALESCE(SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END), 0.0) AS month_cost, \
           COALESCE(SUM(CASE WHEN ts >= ? THEN cost_usd ELSE 0 END), 0.0) AS today_cost \
         FROM usage_events WHERE ts >= ?",
        rusqlite::params![&month_cutoff, &today_cutoff, &month_cutoff],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let mut stmt =
        conn.prepare("SELECT DISTINCT model FROM usage_events WHERE ts >= ? AND model <> ''")?;
    let models: Vec<String> = stmt
        .query_map([&month_cutoff], |row| row.get::<_, Option<String>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        // `[r[0] for r in model_rows if r[0]]` — a NULL or empty model is
        // dropped after the query, not by it.
        .flatten()
        .filter(|model| !model.is_empty())
        .collect();

    Ok(Spend {
        month: month_cost,
        today: today_cost,
        models,
        days_so_far,
        days_in_month,
    })
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?")?;
    let mut rows = stmt.query([name])?;
    Ok(rows.next()?.is_some())
}

// ── status math (`services/budgets.compute_status`) ──────────────────────────

fn compute_status(
    monthly_usd: Option<f64>,
    daily_usd: Option<f64>,
    spend: &Spend,
) -> Map<String, Value> {
    let mut monthly_block = Value::Null;
    let mut projected = Value::Null;
    let mut projection_overruns = Value::Null;
    if let Some(limit) = monthly_usd {
        monthly_block = leg_status(spend.month, limit);
        let days_left = (spend.days_in_month - spend.days_so_far).max(0);
        let daily_burn = if spend.days_so_far > 0 {
            spend.month / spend.days_so_far as f64
        } else {
            0.0
        };
        // `project_month_end` returns 0.0 for a non-positive burn or no days
        // left, and the caller ADDS the current spend to the delta.
        let delta = if daily_burn <= 0.0 || days_left <= 0 {
            0.0
        } else {
            daily_burn * days_left as f64
        };
        let total = spend.month + delta;
        projected = Value::from(total);
        projection_overruns = Value::Bool(total > limit);
    }
    let daily_block = daily_usd.map_or(Value::Null, |limit| leg_status(spend.today, limit));

    let mut status = Map::new();
    status.insert("monthly".to_owned(), monthly_block);
    status.insert("daily".to_owned(), daily_block);
    status.insert("projected_month_end".to_owned(), projected);
    status.insert("projection_overruns".to_owned(), projection_overruns);
    status
}

/// `_leg_status` — five keys, in the literal's order.
fn leg_status(used: f64, limit: f64) -> Value {
    let pct = if limit > 0.0 {
        100.0 * used / limit
    } else {
        0.0
    };
    let mut leg = Map::new();
    leg.insert("budget".to_owned(), Value::from(limit));
    leg.insert("used".to_owned(), Value::from(used));
    leg.insert("remaining".to_owned(), Value::from(limit - used));
    leg.insert("pct".to_owned(), Value::from(pct));
    leg.insert("status".to_owned(), Value::from(band(used, limit)));
    Value::Object(leg)
}

/// `_band` — note it recomputes `pct` rather than reusing `_leg_status`'s, so a
/// `limit <= 0` short-circuits to `"under"` before the division.
fn band(used: f64, limit: f64) -> &'static str {
    if limit <= 0.0 {
        return "under";
    }
    let pct = 100.0 * used / limit;
    if pct > 100.0 {
        "over"
    } else if pct >= APPROACHING_PCT {
        "approaching"
    } else {
        "under"
    }
}

// ── civil calendar (`datetime` + `calendar.monthrange`) ──────────────────────
//
// FLAGGED FOR THE ARCHITECT'S DEDUP LIST: `stax_etl::stats::pydatetime` owns a
// `civil_from_epoch` and a day-of-month formatter, both private. These four
// functions are Howard Hinnant's `days_from_civil` / `civil_from_days` pair plus
// two formatters; they belong next to that module, and are file-local here
// because batch A may not edit `stax-etl`.

/// `(year, month, day)` for a wall-clock microsecond count.
fn civil_from_micros(micros: i64) -> (i64, i64, i64) {
    let days = micros.div_euclid(86_400_000_000);
    civil_from_days(days)
}

/// Microseconds at midnight of `(year, month, day)`.
fn micros_from_civil(year: i64, month: i64, day: i64) -> i64 {
    days_from_civil(year, month, day) * 86_400_000_000
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `calendar.monthrange(year, month)[1]`.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 30,
    }
}

/// `datetime.isoformat()` for a UTC-aware value whose microseconds are zero.
///
/// CPython omits the microsecond field when it is 0 and always writes the
/// `+00:00` offset for an aware value, so the cutoffs are exactly
/// `YYYY-MM-DDTHH:MM:SS+00:00`. A non-zero microsecond would need `.%06d`, which
/// these two callers can never produce (both truncate to a whole day and then
/// shift by whole minutes).
fn isoformat_utc(micros: i64) -> String {
    let (year, month, day) = civil_from_micros(micros);
    let secs_of_day = micros.div_euclid(1_000_000).rem_euclid(86_400);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_civil_pair_round_trips_across_a_leap_boundary() {
        for (y, m, d) in [
            (2026, 7, 31),
            (2024, 2, 29),
            (2000, 2, 29),
            (1900, 3, 1),
            (1970, 1, 1),
        ] {
            let us = micros_from_civil(y, m, d);
            assert_eq!(civil_from_micros(us), (y, m, d), "{y}-{m}-{d}");
        }
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
    }

    #[test]
    fn the_cutoff_is_the_shape_the_ts_column_stores() {
        // 2026-07-31T00:00 local at UTC+8 is 2026-07-30T16:00Z.
        let local_midnight = micros_from_civil(2026, 7, 31);
        assert_eq!(
            isoformat_utc(local_midnight - 480 * 60 * 1_000_000),
            "2026-07-30T16:00:00+00:00"
        );
        assert_eq!(isoformat_utc(local_midnight), "2026-07-31T00:00:00+00:00");
    }

    #[test]
    fn an_unset_budget_nulls_the_whole_status_including_models() {
        let spend = Spend {
            month: 12.0,
            today: 3.0,
            models: vec!["opus".to_owned()],
            days_so_far: 10,
            days_in_month: 31,
        };
        let status = compute_status(None, None, &spend);
        assert_eq!(
            stax_memory::pyjson::dumps_http(&Value::Object(status)),
            r#"{"monthly":null,"daily":null,"projected_month_end":null,"projection_overruns":null}"#
        );
    }

    #[test]
    fn the_projection_is_spend_plus_a_linear_extrapolation() {
        let spend = Spend {
            month: 100.0,
            today: 5.0,
            models: Vec::new(),
            days_so_far: 10,
            days_in_month: 30,
        };
        let status = compute_status(Some(150.0), None, &spend);
        // burn 10/day × 20 days left = 200, plus the 100 already spent.
        assert_eq!(
            status.get("projected_month_end").and_then(Value::as_f64),
            Some(300.0)
        );
        assert_eq!(
            status.get("projection_overruns").and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn the_bands_are_pct_gt_100_then_gte_80() {
        assert_eq!(band(101.0, 100.0), "over");
        assert_eq!(band(100.0, 100.0), "approaching");
        assert_eq!(band(80.0, 100.0), "approaching");
        assert_eq!(band(79.999, 100.0), "under");
        // `limit <= 0` short-circuits BEFORE the division.
        assert_eq!(band(5.0, 0.0), "under");
    }

    #[test]
    fn a_nonpositive_or_junk_persisted_value_reads_as_unset() {
        assert_eq!(coerce_positive(Some(&Value::from(0.0))), None);
        assert_eq!(coerce_positive(Some(&Value::from(-1.0))), None);
        assert_eq!(coerce_positive(Some(&Value::from("nope"))), None);
        assert_eq!(coerce_positive(Some(&Value::Null)), None);
        assert_eq!(coerce_positive(Some(&Value::from("12.5"))), Some(12.5));
        assert_eq!(coerce_positive(None), None);
    }

    #[test]
    fn a_set_then_clear_round_trip_restores_the_file_byte_for_byte() {
        let dir = std::env::temp_dir().join(format!("stax-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp");
        let path = dir.join("config.json");
        let original = "{\n  \"version\": \"0.1.0\",\n  \"auto_browser\": false\n}";
        std::fs::write(&path, original).expect("seed");

        apply_leg(&dir, MONTHLY_KEY, Some(150.0)).expect("set");
        assert_eq!(read_budget(&dir).0, Some(150.0));
        let mut data = load_config(&dir);
        data.remove(MONTHLY_KEY);
        data.remove(DAILY_KEY);
        save_config(&dir, &data).expect("clear");

        // The harness home must survive the case sequence unchanged; a writer
        // that reordered keys or added a newline would only show up on the NEXT
        // run, as a divergence nobody could attribute.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absent_leg_preserves_and_an_explicit_null_clears() {
        assert_eq!(resolve_leg(Leg::Absent, Some(9.0)), Some(9.0));
        assert_eq!(resolve_leg(Leg::Null, Some(9.0)), None);
        assert_eq!(resolve_leg(Leg::Value(3.0), Some(9.0)), Some(3.0));
    }
}
