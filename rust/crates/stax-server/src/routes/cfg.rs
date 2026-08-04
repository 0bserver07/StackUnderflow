//! `routes/cfg.py` — 6 endpoints, wave 5 (batch D).
//!
//! | Item | Method | FastAPI path | axum path | State |
//! |---|---|---|---|---|
//! | `RS-5-050` | `GET   ` | `/api/cfg              ` | `/api/cfg`              | ported |
//! | `RS-5-051` | `GET   ` | `/api/cfg/currencies   ` | `/api/cfg/currencies`   | ported |
//! | `RS-5-052` | `POST  ` | `/api/cfg/currency     ` | `/api/cfg/currency`     | ported |
//! | `RS-5-053` | `GET   ` | `/api/cfg/model-aliases` | `/api/cfg/model-aliases`| ported |
//! | `RS-5-054` | `POST  ` | `/api/cfg/model-aliases` | `/api/cfg/model-aliases`| ported |
//! | `RS-5-055` | `DELETE` | `/api/cfg/model-aliases` | `/api/cfg/model-aliases`| ported |
//!
//! # `Settings.get_all()` is a declaration order, not a dictionary
//!
//! `get_all` is `{k: self.get(k) for k in self._keys()}` and `_keys()` is
//! `[k for k, v in cls.__dict__.items() if isinstance(v, _Opt)]` — **class body
//! order**, which since 3.7 is insertion order. So `GET /api/cfg` emits its 22
//! settings in the order they are written in `settings.py`, `port` first and
//! `proactive_dismiss_suppress_after` last, and that order is part of the byte
//! contract. [`SETTINGS`] is that class body, transcribed, and a test pins it
//! against the source file rather than against a copy of itself.
//!
//! Each value resolves env → file → default on every read, and three details of
//! that chain are load-bearing:
//!
//! * **A present-but-uncastable env var falls back to the *default*, not the
//!   file.** `_Opt.__get__` returns `self._cast(raw)` the moment `os.getenv`
//!   answers, and `_cast` swallows the `ValueError` into `self.default`. The
//!   file leg is never reached. `state.rs` already records this for its three
//!   settings; here it applies to all fourteen with an env name.
//! * **`env=None` means the setting is file-only.** Nine of the 22 have no env
//!   var at all (the dict/list/None-typed ones), and reading `MODEL_ALIASES`
//!   from the environment would be an invention.
//! * **A persisted value of the wrong shape falls back to a *copy* of the
//!   default** — but only for `dict` and `list` defaults. A string setting
//!   persisted as an integer comes back as that integer, untouched.
//!
//! # The writers, and what the case matrix does and does not exercise
//!
//! All three writers go through `settings._save`, i.e. `json.dumps(data,
//! indent=2)` over `$STACKUNDERFLOW_HOME/config.json` — the **CLI** writer
//! (`ensure_ascii=True`), never the HTTP one. Batch A's `budgets.rs` established
//! that split; this module obeys it for the same reason: the bytes go to disk
//! and the next reader is a `json.load` on the Python side.
//!
//! `POST /api/cfg/currency`'s **success** leg is deliberately not a case row.
//! It persists a `currency` key that nothing in the API can remove, so it would
//! leave the shared harness home permanently altered for every batch after it —
//! and persisting anything but `USD` walks straight into DIV-052 (the port
//! refuses to invent an FX rate and answers 500 where Python answers a payload
//! with a warning). Its two 400 legs are deterministic, side-effect-free, and
//! are rows. The alias writer *is* exercised, and it self-cleans: `POST` sets
//! one alias, `DELETE` removes it, and the file is left holding
//! `"model_aliases": {}` — which is the *declared default*, so `GET /api/cfg`
//! reads identically before and after and `compute_cost`'s alias map is empty
//! again. Both servers write those same bytes in the same order, which is what
//! makes the rows green rather than merely tidy. Stated exactly because "the
//! file is restored byte for byte" would have been the convenient claim and it
//! is not true: one key remains, holding the default.
//!
//! `clear_currency_memo()` and `invalidate_dashboard_cache()` still have no
//! port: neither memo exists here, and neither changes a response body, so the
//! omission is a timing difference rather than a divergence.
//!
//! **`_invalidate_stats_cache()` is a different story now.** DIV-055's memo was
//! ruled and ported, so both alias writers below drop it — and this is the one
//! invalidation in the set that is *not* merely defensive. A model alias changes
//! how rows are AGGREGATED without touching a single session row, so the
//! sessions signature does not move and the memo would happily keep serving
//! pre-alias grouping until the next ingest. Python found that bug the hard way
//! (its invalidator had no production caller at all until this was wired), which
//! is exactly why porting the memo meant porting its four drop sites in the same
//! pass rather than "later".

use std::path::Path;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde_json::{Map, Value};

use crate::currency::active_currency_payload;
use crate::json::{HttpError, JsonBody};
use crate::qs::Query;
use crate::state::AppState;

/// Mount this module's endpoints onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/cfg", get(get_cfg))
        .route("/api/cfg/currencies", get(get_currencies))
        .route("/api/cfg/currency", post(set_currency))
        // One `.route` call, three methods: axum panics on a second `.route`
        // for a path it already owns, which is the good outcome and not a
        // shape to work around.
        .route(
            "/api/cfg/model-aliases",
            get(get_model_aliases)
                .post(set_model_alias)
                .delete(delete_model_alias),
        )
}

/// How a setting's default types its env-var cast — `_Opt._cast`'s `type(...)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Bool,
    Int,
    Str,
    /// A `dict` default: the persisted value must also be a mapping.
    Dict,
    /// A `list` default: the persisted value must also be a sequence.
    List,
    /// A `None` default. `type(None)` hits `_cast`'s fallthrough, but every
    /// `None`-defaulted option is `env=None`, so the cast is unreachable.
    None_,
}

/// One `_Opt` declaration: name, default, env var, and the cast's shape.
struct Opt {
    key: &'static str,
    env: Option<&'static str>,
    shape: Shape,
}

/// `class Settings`'s body, in order. See the module docs for why order matters.
const SETTINGS: [Opt; 22] = [
    opt("port", Some("PORT"), Shape::Int),
    opt("host", Some("HOST"), Shape::Str),
    opt("auto_browser", Some("AUTO_BROWSER"), Shape::Bool),
    opt(
        "max_date_range_days",
        Some("MAX_DATE_RANGE_DAYS"),
        Shape::Int,
    ),
    opt(
        "messages_initial_load",
        Some("MESSAGES_INITIAL_LOAD"),
        Shape::Int,
    ),
    opt("log_level", Some("LOG_LEVEL"), Shape::Str),
    opt(
        "auto_reindex_on_ingest",
        Some("AUTO_REINDEX_ON_INGEST"),
        Shape::Bool,
    ),
    opt("currency", Some("STACKUNDERFLOW_CURRENCY"), Shape::Str),
    opt("model_aliases", None, Shape::Dict),
    opt("plan_name", None, Shape::None_),
    opt("plan_monthly_usd", None, Shape::None_),
    opt("plan_reset_day", None, Shape::Int),
    opt("budget_monthly_usd", None, Shape::None_),
    opt("budget_daily_usd", None, Shape::None_),
    opt("plan_alert_thresholds", None, Shape::List),
    opt(
        "discovery_budget_tokens",
        Some("STACKUNDERFLOW_DISCOVERY_BUDGET_TOKENS"),
        Shape::Int,
    ),
    opt(
        "discovery_rank_weights",
        Some("STACKUNDERFLOW_DISCOVERY_RANK_WEIGHTS"),
        Shape::Str,
    ),
    opt(
        "proactive_enabled",
        Some("STACKUNDERFLOW_PROACTIVE_ENABLED"),
        Shape::Bool,
    ),
    opt(
        "proactive_types",
        Some("STACKUNDERFLOW_PROACTIVE_TYPES"),
        Shape::Str,
    ),
    opt(
        "proactive_max_per_session",
        Some("STACKUNDERFLOW_PROACTIVE_MAX_PER_SESSION"),
        Shape::Int,
    ),
    opt(
        "proactive_cooldown_hours",
        Some("STACKUNDERFLOW_PROACTIVE_COOLDOWN_HOURS"),
        Shape::Int,
    ),
    opt("proactive_dismiss_suppress_after", None, Shape::Int),
];

const fn opt(key: &'static str, env: Option<&'static str>, shape: Shape) -> Opt {
    Opt { key, env, shape }
}

/// The declared defaults, kept beside [`SETTINGS`] because a `Value` cannot be
/// built in a `const`.
fn default_for(key: &str) -> Value {
    match key {
        "port" => Value::from(8081),
        "host" => Value::from("127.0.0.1"),
        "auto_browser" | "auto_reindex_on_ingest" => Value::Bool(true),
        "max_date_range_days" => Value::from(30),
        "messages_initial_load" => Value::from(500),
        "log_level" => Value::from("INFO"),
        "currency" => Value::from("USD"),
        "model_aliases" => Value::Object(Map::new()),
        "plan_reset_day" => Value::from(1),
        "plan_alert_thresholds" => serde_json::json!([50, 75, 90]),
        "discovery_budget_tokens" => Value::from(2000),
        "discovery_rank_weights" => Value::from("0.5,0.2,0.3"),
        "proactive_enabled" => Value::Bool(false),
        "proactive_types" => Value::from("command-cluster,file-risk"),
        "proactive_max_per_session" | "proactive_dismiss_suppress_after" => Value::from(3),
        "proactive_cooldown_hours" => Value::from(24),
        // `plan_name`, `plan_monthly_usd`, `budget_monthly_usd`,
        // `budget_daily_usd`.
        _ => Value::Null,
    }
}

/// `_COMMON_CURRENCIES` — the UI's dropdown shortlist, in the literal's order.
const COMMON_CURRENCIES: [&str; 24] = [
    "USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "CNY", "INR", "KRW", "MXN", "BRL", "SEK",
    "NOK", "DKK", "PLN", "RUB", "TRY", "ZAR", "AED", "SAR", "SGD", "HKD", "NZD",
];

/// `RATES_SNAPSHOT`'s keys — the codes, not the rates.
///
/// Only the key set reaches `list_supported`, so the rates are deliberately not
/// duplicated here: this module must never become a second price list.
const RATES_SNAPSHOT_CODES: [&str; 35] = [
    "USD", "EUR", "GBP", "CHF", "CAD", "AUD", "NZD", "JPY", "CNY", "INR", "KRW", "SGD", "HKD",
    "TWD", "THB", "MYR", "IDR", "PHP", "MXN", "BRL", "ILS", "AED", "SAR", "TRY", "ZAR", "NOK",
    "SEK", "DKK", "PLN", "RUB", "CZK", "HUF", "RON", "BGN", "ARS",
];

// ── GET /api/cfg ─────────────────────────────────────────────────────────────

async fn get_cfg(State(state): State<AppState>) -> Result<JsonBody, HttpError> {
    let app_dir = app_dir(&state);
    let mut payload = Map::new();
    payload.insert("settings".to_owned(), Value::Object(get_all(&app_dir)));
    payload.insert("currency".to_owned(), currency_payload(&state)?);
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// `Settings().get_all()`.
fn get_all(app_dir: &Path) -> Map<String, Value> {
    let file = load_config(app_dir);
    let mut out = Map::new();
    for setting in &SETTINGS {
        out.insert(setting.key.to_owned(), resolve(setting, &file));
    }
    out
}

/// `_Opt.__get__` — env → file → default.
fn resolve(setting: &Opt, file: &Map<String, Value>) -> Value {
    let default = default_for(setting.key);
    // `if self.env is not None:` then `os.getenv` — one leg, and reaching it at
    // all skips the file entirely, even when the value will not cast.
    if let Some(Ok(raw)) = setting.env.map(std::env::var) {
        // Present-but-uncastable falls back to the DEFAULT, never the file.
        return cast(&raw, setting.shape, default);
    }
    if let Some(saved) = file.get(setting.key) {
        // The two defensive shape guards, and only these two.
        return match setting.shape {
            Shape::Dict if !saved.is_object() => default,
            Shape::List if !saved.is_array() => default,
            _ => saved.clone(),
        };
    }
    default
}

/// `_Opt._cast`.
fn cast(raw: &str, shape: Shape, default: Value) -> Value {
    match shape {
        // `raw.lower() in ("1", "true", "yes", "on")` — note `on` is in and
        // `t`/`y` are NOT, so this vocabulary is narrower than the query
        // string's (`qs::Query::bool_or`). Two different parsers, on purpose.
        Shape::Bool => Value::Bool(matches!(
            raw.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )),
        Shape::Int => raw.parse::<i64>().map_or(default, Value::from),
        // No `float`-defaulted setting exists today; `_cast`'s float leg is
        // unreachable and is not invented here.
        Shape::Str | Shape::Dict | Shape::List | Shape::None_ => Value::from(raw),
    }
}

// ── GET /api/cfg/currencies ──────────────────────────────────────────────────

async fn get_currencies(State(state): State<AppState>) -> Result<JsonBody, HttpError> {
    let app_dir = app_dir(&state);
    let mut payload = Map::new();
    payload.insert(
        "common".to_owned(),
        Value::Array(COMMON_CURRENCIES.iter().map(|c| Value::from(*c)).collect()),
    );
    payload.insert(
        "supported".to_owned(),
        Value::Array(
            list_supported(&app_dir)
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    );
    payload.insert("current".to_owned(), currency_payload(&state)?);
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// `currency.list_supported()` — `["USD"] + sorted(seen - {"USD"})`.
///
/// USD is pinned to the front rather than sorted into place, which puts it
/// before `AED`; a plain `sorted()` over the whole set would not.
fn list_supported(app_dir: &Path) -> Vec<String> {
    let mut seen: std::collections::BTreeSet<String> = RATES_SNAPSHOT_CODES
        .iter()
        .filter(|code| is_iso_code(code))
        .map(|code| (*code).to_owned())
        .collect();
    // The disk cache `$STACKUNDERFLOW_HOME/cache/exchange-rate.json`, whose
    // `rates` keys join the picker. Unreadable or absent is not an error.
    if let Some(rates) = std::fs::read_to_string(app_dir.join("cache").join("exchange-rate.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("rates").cloned())
        .and_then(|rates| rates.as_object().cloned())
    {
        seen.extend(rates.keys().filter(|k| is_iso_code(k)).cloned());
    }
    seen.remove("USD");
    let mut out = vec!["USD".to_owned()];
    out.extend(seen);
    out
}

/// `_ISO_CODE_RE = ^[A-Z]{3}$`.
fn is_iso_code(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())
}

// ── POST /api/cfg/currency ───────────────────────────────────────────────────

async fn set_currency(State(state): State<AppState>, body: Bytes) -> Result<JsonBody, HttpError> {
    let data = match parse_object_body(&body) {
        Ok(map) => map,
        Err(rejection) => return Ok(rejection),
    };
    // `data.get("code") or data.get("currency")` — `or`, so an empty-string
    // `code` falls through to `currency` before the isinstance check.
    let code = truthy(data.get("code")).or_else(|| truthy(data.get("currency")));
    let Some(Value::String(code)) = code else {
        return Err(HttpError::bad_request("Body must include a 'code' string."));
    };
    if code.trim().is_empty() {
        return Err(HttpError::bad_request("Body must include a 'code' string."));
    }
    let validated = validate_currency(code.trim())?;

    let app_dir = app_dir(&state);
    let mut file = load_config(&app_dir);
    file.insert("currency".to_owned(), Value::from(validated));
    save_config(&app_dir, &file)?;

    // `clear_currency_memo()` — no memo exists in the port; see the module docs.
    let mut payload = Map::new();
    payload.insert(
        "currency".to_owned(),
        currency_payload_for(&app_dir, &state)?,
    );
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// `settings._validate_currency` — one message for both rejections.
fn validate_currency(value: &str) -> Result<String, HttpError> {
    let code = value.trim().to_ascii_uppercase();
    if is_iso_code(&code) {
        Ok(code)
    } else {
        Err(HttpError::bad_request(
            "currency must be a 3-letter ISO 4217 code (e.g. USD, EUR, GBP)",
        ))
    }
}

// ── GET / POST / DELETE /api/cfg/model-aliases ───────────────────────────────

async fn get_model_aliases(State(state): State<AppState>) -> JsonBody {
    let aliases = read_aliases(&app_dir(&state));
    let mut payload = Map::new();
    payload.insert("aliases".to_owned(), Value::Object(aliases));
    JsonBody::ok(Value::Object(payload))
}

async fn set_model_alias(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<JsonBody, HttpError> {
    let data = match parse_object_body(&body) {
        Ok(map) => map,
        Err(rejection) => return Ok(rejection),
    };
    let src = require_non_empty_string(data.get("from"), "'from' must be a non-empty string.")?;
    let dst = require_non_empty_string(data.get("to"), "'to' must be a non-empty string.")?;

    let app_dir = app_dir(&state);
    let mut aliases = read_aliases(&app_dir);
    aliases.insert(src, Value::from(dst));
    persist_aliases(&app_dir, &aliases)?;
    // `_invalidate_stats_cache()` — DIV-055's memo, FULL clear. An alias is
    // global, not per-slug, and Python's own comment says why it cannot be left
    // to the self-invalidation: "the stats memo aggregates per-model too, and
    // its sessions signature does NOT move on a config edit". This is the site
    // that had no production caller at all before Python wired it up.
    state.stats_memo().invalidate(None);

    let mut payload = Map::new();
    payload.insert("aliases".to_owned(), Value::Object(aliases));
    Ok(JsonBody::ok(Value::Object(payload)))
}

async fn delete_model_alias(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<JsonBody, HttpError> {
    let query = Query::parse(raw.as_deref().unwrap_or_default());
    // `Query("", alias="from")` — the default is the empty string, so an absent
    // parameter and `?from=` take the same 400.
    let src = query.str_or("from", "").to_owned();
    if src.is_empty() {
        return Err(HttpError::bad_request(
            "'from' query parameter is required.",
        ));
    }
    let app_dir = app_dir(&state);
    let mut aliases = read_aliases(&app_dir);
    if !aliases.contains_key(&src) {
        // `f"No alias for {src!r}."` — Python's `repr` of a `str`, which is
        // single-quoted unless the value itself contains a single quote.
        return Err(HttpError::not_found(format!(
            "No alias for {}.",
            py_repr(&src)
        )));
    }
    aliases.shift_remove(&src);
    persist_aliases(&app_dir, &aliases)?;
    // "same blast radius as the set path above" — DIV-055.
    state.stats_memo().invalidate(None);

    let mut payload = Map::new();
    payload.insert("aliases".to_owned(), Value::Object(aliases));
    Ok(JsonBody::ok(Value::Object(payload)))
}

/// `dict(s.get("model_aliases") or {})`.
fn read_aliases(app_dir: &Path) -> Map<String, Value> {
    let file = load_config(app_dir);
    match resolve(&opt("model_aliases", None, Shape::Dict), &file) {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// `s.persist("model_aliases", aliases)` — no validator on this key.
fn persist_aliases(app_dir: &Path, aliases: &Map<String, Value>) -> Result<(), HttpError> {
    let mut file = load_config(app_dir);
    file.insert("model_aliases".to_owned(), Value::Object(aliases.clone()));
    save_config(app_dir, &file)
}

// ── shared plumbing ──────────────────────────────────────────────────────────

/// `$STACKUNDERFLOW_HOME` — `settings._APP_DIR`, derived from the store path the
/// same way `routes/budgets.rs` derives it.
fn app_dir(state: &AppState) -> std::path::PathBuf {
    state
        .store_path()
        .parent()
        .map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf)
}

fn currency_payload(state: &AppState) -> Result<Value, HttpError> {
    active_currency_payload(&state.config().currency)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

/// The payload after a write, which must read the code back off disk rather
/// than from the startup-resolved config.
fn currency_payload_for(app_dir: &Path, state: &AppState) -> Result<Value, HttpError> {
    let file = load_config(app_dir);
    let code = file
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or(&state.config().currency)
        .to_owned();
    active_currency_payload(&code)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

/// The bare `data: dict` body parameter — DIV-367's container-only member.
///
/// A bare `dict` is `dict[Any, Any]`: pydantic checks that the body IS a
/// mapping and nothing about its values, which is why `{"code": 3}`
/// (`K-cur-not-string`) reaches the handler and takes its `400`. The rejection
/// is [`crate::json::dict_body`]'s, shared with the other nine handlers; the
/// two spellings this module used to carry (`"Input should be a valid
/// dictionary"` and `"Invalid JSON body"`, both as a single-string `detail`)
/// had the status right and the body wrong, and neither had ever been probed.
///
/// # Errors
/// The rendered `422` — a `JsonBody` and not an [`HttpError`], because a
/// validation `detail` is a LIST and `HttpError` models the single-string form.
fn parse_object_body(body: &Bytes) -> Result<Map<String, Value>, JsonBody> {
    crate::json::dict_body(body)
}

/// Python truthiness for the `or` chain: `None`, `""`, `{}`, `[]`, `0`, `false`
/// are all falsy.
fn truthy(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    let is_truthy = match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    };
    is_truthy.then(|| value.clone())
}

/// `if not isinstance(x, str) or not x.strip(): raise HTTPException(400, msg)`,
/// then `x.strip()`.
fn require_non_empty_string(value: Option<&Value>, message: &str) -> Result<String, HttpError> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_owned()),
        _ => Err(HttpError::bad_request(message)),
    }
}

/// Python's `repr()` of a `str`, for the DELETE 404 message.
///
/// Single quotes unless the string contains one and no double quote — CPython's
/// `unicode_repr` rule, reproduced because the message is compared byte for byte.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `settings._load()` — a missing or corrupt file is `{}`, never an error.
///
/// FLAGGED FOR DEDUP: identical to `routes/budgets.rs::load_config`.
fn load_config(app_dir: &Path) -> Map<String, Value> {
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
/// FLAGGED FOR DEDUP: identical to `routes/budgets.rs::save_config`.
fn save_config(app_dir: &Path, data: &Map<String, Value>) -> Result<(), HttpError> {
    std::fs::create_dir_all(app_dir)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let rendered = stax_memory::pyjson::dumps_pretty(&Value::Object(data.clone()));
    std::fs::write(app_dir.join("config.json"), rendered)
        .map_err(|err| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The class body of `settings.Settings`, read from the source of truth.
    fn declared_keys() -> Vec<String> {
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../stackunderflow/settings.py");
        let text = std::fs::read_to_string(source).expect("settings.py is readable");
        let body = text
            .split_once("class Settings:")
            .expect("the class exists")
            .1;
        let body = body
            .split_once("# ── public helpers")
            .expect("the declarations end")
            .0;
        body.lines()
            .filter_map(|line| {
                let (name, rest) = line.split_once('=')?;
                let name = name.trim();
                if name.is_empty() || name.starts_with('#') || name.contains(' ') {
                    return None;
                }
                rest.trim_start()
                    .starts_with("_Opt(")
                    .then(|| name.to_owned())
            })
            .collect()
    }

    #[test]
    fn the_settings_table_is_the_class_body_in_order() {
        // Not a copy of itself: this reads `settings.py` and compares. A new
        // setting added upstream fails here instead of silently vanishing from
        // `GET /api/cfg`.
        let declared = declared_keys();
        let ported: Vec<String> = SETTINGS
            .iter()
            .map(|setting| setting.key.to_owned())
            .collect();
        assert_eq!(ported, declared);
        assert_eq!(ported.len(), 22);
    }

    #[test]
    fn an_uncastable_env_value_falls_back_to_the_default_not_the_file() {
        let mut file = Map::new();
        file.insert("plan_reset_day".to_owned(), Value::from(9));
        // No env leg for this one, so the file wins.
        assert_eq!(
            resolve(&opt("plan_reset_day", None, Shape::Int), &file),
            Value::from(9)
        );
        // The cast itself: an unparseable int is the default, not the file.
        assert_eq!(
            cast("not-a-number", Shape::Int, Value::from(30)),
            Value::from(30)
        );
    }

    #[test]
    fn the_env_bool_vocabulary_is_narrower_than_the_query_strings() {
        // `_cast` accepts `on` but not `t` / `y`; `qs::Query::bool_or` is the
        // opposite. Two parsers, both live, and neither is the other's bug.
        for raw in ["1", "true", "TRUE", "yes", "on"] {
            assert_eq!(
                cast(raw, Shape::Bool, Value::Bool(false)),
                Value::Bool(true)
            );
        }
        for raw in ["t", "y", "0", "false", "off", "nonsense", ""] {
            assert_eq!(
                cast(raw, Shape::Bool, Value::Bool(true)),
                Value::Bool(false),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_wrong_shaped_persisted_value_falls_back_only_for_dicts_and_lists() {
        let mut file = Map::new();
        file.insert("model_aliases".to_owned(), Value::from("oops"));
        file.insert("plan_alert_thresholds".to_owned(), Value::from(7));
        file.insert("log_level".to_owned(), Value::from(3));
        assert_eq!(
            resolve(&opt("model_aliases", None, Shape::Dict), &file),
            Value::Object(Map::new())
        );
        assert_eq!(
            resolve(&opt("plan_alert_thresholds", None, Shape::List), &file),
            serde_json::json!([50, 75, 90])
        );
        // A string setting persisted as an int comes back as that int — there
        // is no guard for scalars, and inventing one would be a divergence.
        assert_eq!(
            resolve(&opt("log_level", Some("LOG_LEVEL"), Shape::Str), &file),
            Value::from(3)
        );
    }

    #[test]
    fn supported_currencies_pin_usd_in_front_of_the_sort() {
        let dir = std::path::PathBuf::from("/nonexistent-app-dir");
        let codes = list_supported(&dir);
        assert_eq!(codes[0], "USD");
        assert_eq!(codes[1], "AED", "the rest is sorted, USD is not in it");
        assert_eq!(codes.len(), RATES_SNAPSHOT_CODES.len());
        assert!(codes.iter().filter(|c| *c == "USD").count() == 1);
    }

    #[test]
    fn the_code_falls_through_from_code_to_currency_on_a_falsy_value() {
        let mut body = Map::new();
        body.insert("code".to_owned(), Value::from(""));
        body.insert("currency".to_owned(), Value::from("eur"));
        let code = truthy(body.get("code")).or_else(|| truthy(body.get("currency")));
        assert_eq!(code, Some(Value::from("eur")));
    }

    #[test]
    fn the_currency_validator_uppercases_and_rejects_by_shape() {
        assert_eq!(validate_currency("eur").expect("valid"), "EUR");
        assert_eq!(validate_currency("  gbp ").expect("valid"), "GBP");
        for bad in ["EURO", "e", "12", "€€€"] {
            let err = validate_currency(bad).expect_err("rejected");
            assert_eq!(
                err.body().render(),
                r#"{"detail":"currency must be a 3-letter ISO 4217 code (e.g. USD, EUR, GBP)"}"#
            );
        }
    }

    #[test]
    fn the_delete_404_carries_pythons_repr_of_the_key() {
        assert_eq!(py_repr("openrouter/x"), "'openrouter/x'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\"b"), "'a\"b'");
    }

    #[test]
    fn the_config_writer_is_the_cli_writer_not_the_http_one() {
        // Two spaces, `ensure_ascii=True`, no trailing newline.
        let mut data = Map::new();
        data.insert("currency".to_owned(), Value::from("café"));
        let rendered = stax_memory::pyjson::dumps_pretty(&Value::Object(data));
        assert_eq!(rendered, "{\n  \"currency\": \"caf\\u00e9\"\n}");
    }

    #[test]
    fn an_alias_round_trip_restores_the_file_key_order() {
        let dir = std::env::temp_dir().join(format!("stax-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp");
        let path = dir.join("config.json");
        let before = "{\n  \"version\": \"0.1.0\",\n  \"auto_browser\": false\n}";
        std::fs::write(&path, before).expect("seed");

        let mut aliases = read_aliases(&dir);
        aliases.insert("proxy/x".to_owned(), Value::from("claude-opus-4-8"));
        persist_aliases(&dir, &aliases).expect("write");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("proxy/x")
        );

        // The DELETE leg drops the alias but re-persists the (now empty) map,
        // so the original keys keep their order and one key is appended.
        let mut aliases = read_aliases(&dir);
        aliases.shift_remove("proxy/x");
        persist_aliases(&dir, &aliases).expect("write");
        let after = std::fs::read_to_string(&path).expect("read");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            after,
            "{\n  \"version\": \"0.1.0\",\n  \"auto_browser\": false,\n  \"model_aliases\": {}\n}"
        );
        // Not byte-identical to `before`: the key is left behind as an empty
        // map, exactly as Python leaves it. The harness row after the DELETE
        // therefore reads `{}` on BOTH sides, which is what makes it green.
        assert_ne!(after, before);
    }
}
