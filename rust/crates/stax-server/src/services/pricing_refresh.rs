//! `services/pricing_service.py::PricingService` — the read half.
//!
//! Two endpoints consume it: `GET /api/pricing` ([`PricingService::get_pricing`])
//! and `POST /api/pricing/refresh` ([`PricingService::force_refresh`]).
//!
//! # The one thing this port cannot do, stated first
//!
//! `_fetch_from_litellm` is
//! `urllib.request.urlopen("https://raw.githubusercontent.com/…", timeout=10)`.
//! **HTTPS.** `stax-server` has no TLS crate, the workspace lock has none either
//! (`rustls`, `native-tls`, `openssl`, `reqwest`, `ureq` are all absent — `hyper`
//! is in the tree for the *server* side only), and the batch-E fence forbids
//! `Cargo.toml` edits. A raw `TcpStream` reaches port 443 and then has nothing to
//! say to it.
//!
//! So [`fetch_from_litellm`] is the *failure* leg of the reference, permanently:
//! it returns `None`, which is exactly what `_fetch_from_litellm` returns when
//! `urlopen` raises. That is not a stub standing in for unwritten code — it is
//! the one branch of the reference this crate is able to reach, and it is
//! reached honestly.
//!
//! The consequence is measured, not guessed. Outbound HTTPS **works** on this
//! host (`curl -o /dev/null -w '%{http_code} %{size_download}'` against the
//! LiteLLM URL answers `200 1670646`), so the reference takes the *success* leg
//! and the port takes the failure leg on every branch that fetches. The branches
//! that do not fetch agree byte-for-byte. `rust/PRICING-REFRESH-DIFFER.md` is
//! the procedure that establishes which is which, and `parity/DIV-e-misc.md`
//! carries the finding.
//!
//! # Why no writer is ported
//!
//! `_save_to_cache` (JSON overlay + `price_book` live-row append +
//! `refresh_price_book_cache`) is only ever called with a *successful* fetch's
//! payload. With the fetch permanently failing there is no reachable caller, and
//! an untestable writer aimed at the maintainer's store is the last thing this
//! campaign needs. Named here so the omission is a decision, not an oversight.
//!
//! # Why neither endpoint gets a row in `parity/endpoint-cases.txt`
//!
//! `REFRESH-DIFFER.md` point 2, and `routes/misc.rs`'s header: a `!` row still
//! ISSUES the request. Python then fetches LiteLLM and rewrites
//! `$STACKUNDERFLOW_HOME/cache/pricing.json`, which is the *input* to all five
//! `PR-doctor*` rows — five clean cases became five divergences from a case-file
//! edit with no code change. An endpoint whose side effect is another endpoint's
//! input cannot share a home with it.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use stax_etl::stats::pydatetime::{PyDateTime, civil_from_epoch, parse_ts};

/// `cache_duration = timedelta(hours=24)`, in seconds.
pub const CACHE_DURATION_SECS: f64 = 24.0 * 3600.0;
/// `STALE_THRESHOLD = timedelta(days=7)`, in seconds.
pub const STALE_THRESHOLD_SECS: f64 = 7.0 * 86_400.0;

/// The exceptions `get_pricing` lets escape, which `routes/misc.py` renders as
/// `{"error": f"Failed to get pricing: {str(e)}"}` with a 500.
///
/// Every string here was **measured** against the reference (batch-E law 6), not
/// transcribed from a reading of CPython. The probe seeded a cache file, called
/// `PricingService.get_pricing()` and printed `str(e)`; see
/// `rust/PRICING-REFRESH-DIFFER.md` §A.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingRaise(String);

impl PricingRaise {
    /// `str(e)` — the text the route interpolates.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PricingRaise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `PricingService` — the cache paths and the branch tree over them.
#[derive(Debug, Clone)]
pub struct PricingService {
    cache_dir: PathBuf,
    cache_file: PathBuf,
}

impl PricingService {
    /// `__init__` — derive the paths and `mkdir(parents=True, exist_ok=True)`.
    ///
    /// The mkdir is a real side effect and the reference performs it **once at
    /// startup** (`server.py::_lifespan` constructs the service), whereas the
    /// port has no service layer and constructs one per request. The observable
    /// end state is identical — an empty `cache/` directory exists — and the one
    /// endpoint that could tell them apart, `GET /api/pricing/doctor`, probes the
    /// cache *file*, not the directory (`read_cache_status`). Recorded in
    /// `parity/DIV-e-misc.md` rather than assumed harmless.
    #[must_use]
    pub fn new(app_dir: &Path) -> Self {
        let cache_dir = app_dir.join("cache");
        // `exist_ok=True`; a failure here is not raised by `__init__` either —
        // `mkdir` would raise, but the lifespan wraps the construction in a
        // `try/except` that only logs, so the process continues without one.
        let _ = std::fs::create_dir_all(&cache_dir);
        let cache_file = cache_dir.join("pricing.json");
        Self {
            cache_dir,
            cache_file,
        }
    }

    /// `self.cache_dir`.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// `self.pricing_cache_file`.
    #[must_use]
    pub fn cache_file(&self) -> &Path {
        &self.cache_file
    }

    /// `get_pricing()` — the four-way branch over cache presence and freshness.
    ///
    /// `default_pricing` is evaluated **lazily** and only on the `source:
    /// "default"` leg, because building it means walking every canonical id
    /// through the pricing engine and three of the four legs never look at it.
    ///
    /// # Errors
    /// The three shapes the reference raises rather than returns — see
    /// [`PricingRaise`].
    pub fn get_pricing<F>(&self, default_pricing: F) -> Result<Value, PricingRaise>
    where
        F: FnOnce() -> Value,
    {
        let cache = self.load_cache();
        // `if cache_data:` — Python truthiness, so `{}`, `[]`, `""`, `0`,
        // `false` and `null` all fall through to the no-cache branch.
        let Some(cache) = cache.filter(truthy) else {
            // `fresh_data = self._fetch_from_litellm()` — permanently `None`
            // here, so this is the `else` of `if fresh_data:`.
            if let Some(fresh) = fetch_from_litellm() {
                return Ok(payload(
                    fresh,
                    "litellm",
                    Value::from(now_isoformat()),
                    false,
                ));
            }
            return Ok(payload(
                default_pricing(),
                "default",
                Value::from(now_isoformat()),
                true,
            ));
        };

        // `cache_data.get("timestamp")` on a non-dict is an `AttributeError`,
        // and the route's bare `except Exception` turns it into the 500.
        let Some(obj) = cache.as_object() else {
            return Err(PricingRaise(format!(
                "'{}' object has no attribute 'get'",
                python_type_name(&cache)
            )));
        };
        let timestamp = obj.get("timestamp").cloned().unwrap_or(Value::Null);

        let valid = is_cache_valid(&timestamp)?;
        // `cache_data["pricing"]` — a SUBSCRIPT, not a `.get`, on BOTH the fresh
        // and the stale leg. A cache file without the key is a `KeyError`, whose
        // `str()` is the repr of the key: `'pricing'`, quotes included.
        let pricing = obj
            .get("pricing")
            .cloned()
            .ok_or_else(|| PricingRaise("'pricing'".to_owned()))?;

        if valid {
            let stale = is_beyond_stale_threshold(&timestamp)?;
            return Ok(payload(pricing, "cache", timestamp, stale));
        }
        // Stale: `fresh_data = self._fetch_from_litellm()`, permanently `None`,
        // so the reference's "failed to fetch — surface staleness" leg.
        if let Some(fresh) = fetch_from_litellm() {
            return Ok(payload(
                fresh,
                "litellm",
                Value::from(now_isoformat()),
                false,
            ));
        }
        Ok(payload(pricing, "cache", timestamp, true))
    }

    /// `force_refresh()` — `True` when the fetch succeeded and was cached.
    ///
    /// Always `false` here: see the module header. `routes/misc.py` turns `False`
    /// into a **500** with `{"status": "error", "message": "Failed to fetch
    /// pricing from LiteLLM"}`, which was confirmed against the reference by
    /// failing its fetch (a dead `https_proxy`, an environment condition rather
    /// than a patch): `force_refresh() = False`.
    #[must_use]
    pub fn force_refresh(&self) -> bool {
        fetch_from_litellm().is_some()
    }

    /// `_load_cache()` — `None` when the file is absent, unreadable, or not JSON.
    fn load_cache(&self) -> Option<Value> {
        if !self.cache_file.exists() {
            return None;
        }
        let text = std::fs::read_to_string(&self.cache_file).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// `_fetch_from_litellm()` — the branch this crate can reach, which is failure.
///
/// See the module header: the URL is HTTPS and the workspace has no TLS. The
/// reference returns `None` from exactly this function when `urlopen` raises,
/// and every caller already handles it, so nothing downstream is stubbed.
#[must_use]
pub fn fetch_from_litellm() -> Option<Value> {
    None
}

/// `{"pricing": …, "source": …, "timestamp": …, "is_stale": …}`.
///
/// Key order is the dict literal's, and the route re-builds it in the same order
/// (`pricing`, `source`, `timestamp`, `is_stale`) — so this order IS the byte
/// contract, twice over.
fn payload(pricing: Value, source: &str, timestamp: Value, is_stale: bool) -> Value {
    let mut obj = Map::new();
    obj.insert("pricing".to_owned(), pricing);
    obj.insert("source".to_owned(), Value::from(source));
    obj.insert("timestamp".to_owned(), timestamp);
    obj.insert("is_stale".to_owned(), Value::Bool(is_stale));
    Value::Object(obj)
}

/// `_is_cache_valid` — `age < timedelta(hours=24)`.
///
/// The `except (ValueError, AttributeError)` around it catches a malformed
/// string and a non-string value. It does **not** catch `TypeError`, which is
/// what `datetime.now(UTC) - naive` raises — so a naive cached timestamp
/// escapes the whole service and 500s. Measured, not read:
/// `{"error": "Failed to get pricing: can't subtract offset-naive and
/// offset-aware datetimes"}`.
///
/// # Errors
/// The naive-timestamp `TypeError`.
fn is_cache_valid(timestamp: &Value) -> Result<bool, PricingRaise> {
    // `if not timestamp_str: return False`.
    let Some(age) = timestamp_age_seconds(timestamp)? else {
        return Ok(false);
    };
    Ok(age < CACHE_DURATION_SECS)
}

/// `_is_beyond_stale_threshold` — `(now - cache_time) >= timedelta(days=7)`.
///
/// Same `TypeError` escape as [`is_cache_valid`], for the same reason: the
/// `try` wraps only the `fromisoformat` call, and the subtraction is outside it.
///
/// # Errors
/// The naive-timestamp `TypeError`.
fn is_beyond_stale_threshold(timestamp: &Value) -> Result<bool, PricingRaise> {
    // `if not timestamp_str: return True` and `except …: return True` — both
    // collapse to "cannot prove freshness, so warn".
    let Some(age) = timestamp_age_seconds(timestamp)? else {
        return Ok(true);
    };
    Ok(age >= STALE_THRESHOLD_SECS)
}

/// `(datetime.now(UTC) - fromisoformat(ts.replace("Z", "+00:00"))).total_seconds()`.
///
/// `Ok(None)` is the "falsy or unparseable" case both callers fold into their own
/// default; `Err` is the naive-vs-aware `TypeError`.
fn timestamp_age_seconds(timestamp: &Value) -> Result<Option<f64>, PricingRaise> {
    if !truthy(timestamp) {
        return Ok(None);
    }
    // `timestamp_str.replace(…)` — an `AttributeError` on anything that is not a
    // string, which both callers catch.
    let Value::String(raw) = timestamp else {
        return Ok(None);
    };
    // `parse_ts` performs the `Z` → `+00:00` replacement itself; it is the
    // deduped owner of `datetime.fromisoformat` for the whole workspace
    // (batch-E law 9), and `routes/pricing.rs` reads the same field through it.
    let Some(cache_time) = parse_ts(raw) else {
        return Ok(None);
    };
    let now = now_utc();
    now.sub_total_seconds(cache_time).map_or_else(
        || {
            Err(PricingRaise(
                "can't subtract offset-naive and offset-aware datetimes".to_owned(),
            ))
        },
        |age| Ok(Some(age)),
    )
}

/// `datetime.now(UTC)` as microseconds, in the workspace's datetime type.
fn now_utc() -> PyDateTime {
    let wall_us = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX);
    PyDateTime {
        wall_us,
        offset_s: Some(0),
    }
}

/// `datetime.now(UTC).isoformat()`.
///
/// CPython omits the fractional part entirely when the microsecond field is 0
/// (`isoformat` uses `%H:%M:%S` then appends `.%f` only if `microsecond`), so
/// `…T00:00:00+00:00` and `…T00:00:00.000001+00:00` are both reachable spellings
/// and a fixed `%.6f` would be wrong once per million calls.
#[must_use]
pub fn now_isoformat() -> String {
    let micros = now_utc().wall_us;
    let (year, month, day, hour, minute, second) = civil_from_epoch(micros.div_euclid(1_000_000));
    let sub = micros.rem_euclid(1_000_000);
    let stamp = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");
    if sub == 0 {
        format!("{stamp}+00:00")
    } else {
        format!("{stamp}.{sub:06}+00:00")
    }
}

/// `RATE_CARD` as `/api/pricing`'s `default` payload would serialise it.
///
/// `RATE_CARD = {mid: get_model_pricing(mid) for mid in _CANONICAL_IDS}` — a
/// dict comprehension in manifest order whose values are the four per-token
/// rates or `None`. [`stax_etl::pricing::costs::PricingEngine::rate_card`] is
/// that same comprehension, and the engine handed in **must** be
/// [`crate::pricing::engine`] and never `default_engine` (batch-E law 2,
/// DIV-056 — a silent 2 % cost error).
///
/// Measured shape, from the reference under a failed fetch: 53 entries, first
/// key `claude-fable-5`, first value
/// `{"input_cost_per_token": 1e-05, "output_cost_per_token": 5e-05,
///   "cache_creation_cost_per_token": 1.25e-05, "cache_read_cost_per_token": 1e-06}`,
/// no `null` values.
#[must_use]
pub fn rate_card_payload(engine: &stax_etl::pricing::costs::PricingEngine) -> Value {
    let mut obj = Map::new();
    for (model, pricing) in engine.rate_card() {
        let value = pricing.map_or(Value::Null, |rates| {
            let mut entry = Map::new();
            entry.insert(
                "input_cost_per_token".to_owned(),
                Value::from(rates.input_cost_per_token),
            );
            entry.insert(
                "output_cost_per_token".to_owned(),
                Value::from(rates.output_cost_per_token),
            );
            entry.insert(
                "cache_creation_cost_per_token".to_owned(),
                Value::from(rates.cache_creation_cost_per_token),
            );
            entry.insert(
                "cache_read_cost_per_token".to_owned(),
                Value::from(rates.cache_read_cost_per_token),
            );
            Value::Object(entry)
        });
        obj.insert(model, value);
    }
    Value::Object(obj)
}

/// The name CPython puts in `'X' object has no attribute 'get'`.
fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        // `json.load` produces an `int` for an integral literal and a `float`
        // otherwise; `serde_json` keeps the same split.
        Value::Number(n) => {
            if n.is_f64() {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python truthiness for the shapes a JSON value can take.
///
/// A file-local copy: `routes/pricing.rs` has the same three lines, private, and
/// that file belongs to another batch's fence. Named as a duplicate rather than
/// silently re-derived — lifting it to `crate::pyops` is a one-line architect
/// change, recorded in `parity/DIV-e-misc.md`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(dir: &Path) -> PricingService {
        PricingService::new(dir)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stax-pricing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn write_cache(svc: &PricingService, body: &str) {
        std::fs::write(svc.cache_file(), body).expect("seed");
    }

    fn iso_ago(seconds: i64) -> String {
        let micros = now_utc().wall_us - seconds * 1_000_000;
        let (y, mo, d, h, mi, s) = civil_from_epoch(micros.div_euclid(1_000_000));
        let sub = micros.rem_euclid(1_000_000);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{sub:06}+00:00")
    }

    #[test]
    fn the_constructor_creates_the_cache_directory() {
        let dir = scratch("mkdir");
        let svc = service(&dir);
        assert!(svc.cache_dir().is_dir());
        assert!(!svc.cache_file().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_cache_is_served_verbatim_and_is_not_stale() {
        let dir = scratch("fresh");
        let svc = service(&dir);
        let ts = iso_ago(60);
        write_cache(
            &svc,
            &format!(r#"{{"timestamp": "{ts}", "pricing": {{"m": 1}}}}"#),
        );
        let out = svc
            .get_pricing(|| Value::Null)
            .expect("no raise on a well-formed cache");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&out),
            format!(
                r#"{{"pricing":{{"m":1}},"source":"cache","timestamp":"{ts}","is_stale":false}}"#
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_cache_is_served_with_the_staleness_surfaced() {
        // 30 days old: `_is_cache_valid` is false, the fetch fails, so the
        // reference re-serves the cached payload with `is_stale: true`.
        let dir = scratch("stale");
        let svc = service(&dir);
        let ts = iso_ago(30 * 86_400);
        write_cache(
            &svc,
            &format!(r#"{{"timestamp": "{ts}", "pricing": {{"m": 1}}}}"#),
        );
        let out = svc.get_pricing(|| Value::Null).expect("no raise");
        assert_eq!(out["source"], Value::from("cache"));
        assert_eq!(out["is_stale"], Value::Bool(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_timestamp_in_the_future_is_valid_and_not_stale() {
        // `age` is negative, so it is both `< 24 h` and `< 7 d`. The reference
        // agrees — probed with a stamp 400 days ahead.
        let dir = scratch("future");
        let svc = service(&dir);
        let ts = iso_ago(-400 * 86_400);
        write_cache(
            &svc,
            &format!(r#"{{"timestamp": "{ts}", "pricing": {{"m": 1}}}}"#),
        );
        let out = svc.get_pricing(|| Value::Null).expect("no raise");
        assert_eq!(out["source"], Value::from("cache"));
        assert_eq!(out["is_stale"], Value::Bool(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_without_the_pricing_key_raises_the_keyerror_repr() {
        let dir = scratch("keyerror");
        let svc = service(&dir);
        write_cache(&svc, &format!(r#"{{"timestamp": "{}"}}"#, iso_ago(60)));
        assert_eq!(
            svc.get_pricing(|| Value::Null).unwrap_err().message(),
            "'pricing'"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_object_cache_raises_the_attributeerror_with_the_python_type_name() {
        for (body, name) in [
            ("[1, 2]", "list"),
            (r#""hello""#, "str"),
            ("5", "int"),
            ("true", "bool"),
        ] {
            let dir = scratch(&format!("attr-{name}"));
            let svc = service(&dir);
            write_cache(&svc, body);
            assert_eq!(
                svc.get_pricing(|| Value::Null).unwrap_err().message(),
                format!("'{name}' object has no attribute 'get'"),
                "for cache body {body}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_naive_timestamp_escapes_as_the_typeerror_the_service_does_not_catch() {
        let dir = scratch("naive");
        let svc = service(&dir);
        // No offset: `datetime.now(UTC) - naive` raises, and the `except` clause
        // lists only `(ValueError, AttributeError)`.
        write_cache(
            &svc,
            r#"{"timestamp": "2026-07-31T12:00:00", "pricing": {"m": 1}}"#,
        );
        assert_eq!(
            svc.get_pricing(|| Value::Null).unwrap_err().message(),
            "can't subtract offset-naive and offset-aware datetimes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparseable_or_empty_timestamp_falls_to_the_stale_leg() {
        for ts in ["not-a-date", ""] {
            let dir = scratch("badts");
            let svc = service(&dir);
            write_cache(
                &svc,
                &format!(r#"{{"timestamp": "{ts}", "pricing": {{"m": 1}}}}"#),
            );
            let out = svc.get_pricing(|| Value::Null).expect("no raise");
            assert_eq!(out["source"], Value::from("cache"), "ts={ts}");
            assert_eq!(out["is_stale"], Value::Bool(true), "ts={ts}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn an_empty_object_cache_is_falsy_and_takes_the_no_cache_branch() {
        // `if cache_data:` — `{}` is falsy, so the service never looks at it.
        let dir = scratch("empty");
        let svc = service(&dir);
        write_cache(&svc, "{}");
        let out = svc
            .get_pricing(|| Value::from("THE-RATE-CARD"))
            .expect("no raise");
        assert_eq!(out["source"], Value::from("default"));
        assert_eq!(out["pricing"], Value::from("THE-RATE-CARD"));
        assert_eq!(out["is_stale"], Value::Bool(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_cache_is_the_default_branch_and_the_key_order_is_the_literals() {
        let dir = scratch("absent");
        let svc = service(&dir);
        let out = svc.get_pricing(|| Value::from(1)).expect("no raise");
        let rendered = stax_memory::pyjson::dumps_http(&out);
        assert!(rendered.starts_with(r#"{"pricing":1,"source":"default","timestamp":""#));
        assert!(rendered.ends_with(r#"","is_stale":true}"#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn force_refresh_is_false_because_the_fetch_cannot_succeed() {
        let dir = scratch("refresh");
        assert!(!service(&dir).force_refresh());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_isoformat_stamp_matches_cpythons_two_spellings() {
        let stamp = now_isoformat();
        assert!(stamp.ends_with("+00:00"), "{stamp}");
        let head = stamp.trim_end_matches("+00:00");
        // Either `YYYY-MM-DDTHH:MM:SS` (19) or that plus `.uuuuuu` (26).
        assert!(head.len() == 19 || head.len() == 26, "{stamp}");
        assert!(parse_ts(&stamp).is_some(), "round-trips: {stamp}");
    }

    #[test]
    fn the_rate_card_payload_is_the_manifest_order_with_four_keys_per_entry() {
        let package = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow");
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory");
        // `crate::pricing::engine`, NEVER `default_engine` — batch-E law 2.
        let engine = crate::pricing::engine(&conn, &package).expect("engine");
        let card = rate_card_payload(&engine);
        let obj = card.as_object().expect("object");
        assert!(!obj.is_empty());
        let (first_key, first_value) = obj.iter().next().expect("one entry");
        assert_eq!(first_key, "claude-fable-5", "manifest order, not sorted");
        let entry = first_value.as_object().expect("entry object");
        assert_eq!(
            entry.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "input_cost_per_token",
                "output_cost_per_token",
                "cache_creation_cost_per_token",
                "cache_read_cost_per_token"
            ]
        );
    }
}
