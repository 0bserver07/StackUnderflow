//! `stax pricing doctor` — `cli.py:4858`–`:4983`.
//!
//! The group is one leaf. `doctor` renders exactly what `GET
//! /api/pricing/doctor` returns, because the reference imports the route's own
//! `assemble_pricing_health` into the CLI ("so the two surfaces never
//! disagree", `routes/pricing.py`'s own words). This port keeps that property
//! by calling [`stax_reports::pricing_doctor::assemble_pricing_health`], which
//! is where the assembler now lives — the DIV-375 shape, applied before the
//! fork rather than after it.
//!
//! # The engine is the MANIFEST one, and that is a real asymmetry
//!
//! `routes/pricing.py` imports `estimate_cost` / `is_rate_card_model` from
//! `infra.costs` at module scope, and `server.py`'s lifespan flips that module
//! onto the `price_book` table before it serves a byte. `cli.py` never calls
//! `use_price_book_store`, so the *same assembler* prices from the in-code
//! manifest when a human runs it and from the table when a request does. That
//! is RS-3-082's seam (DIV-016), it is the reference's behaviour, and it is why
//! the engine is a parameter of the assembler rather than a constant inside it:
//! this verb passes [`crate::status::engine_for_cli`], the route passes
//! `crate::pricing::engine`.
//!
//! # `--strict` is a `SystemExit(1)` AFTER the render
//!
//! `raise SystemExit(1)` runs *after* `click.echo`, so a strict failure still
//! prints the whole report and only then exits 1. `SystemExit` with an integer
//! code prints nothing itself — no `Error:` line, no usage block — which is a
//! third exit shape alongside `ClickException` (1, with a message) and
//! `UsageError` (2, with a usage block).
//!
//! # `click.secho` writes no escape codes off a terminal
//!
//! Four of the lines here are `secho(..., fg=…)` and two are `bold=True`.
//! Click strips styling when stdout is not a tty, which is what the harness
//! captures, so the port emits the plain text and the colour arguments have no
//! byte-level existence. Recorded rather than assumed: the same reasoning is
//! why `worktrees`' bold header is plain.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::Value;
use stax_core::queries::pyint::PyInt;
use stax_etl::pricing::costs::format_dollars;
use stax_reports::pricing_doctor::{DEFAULT_LIMIT, DEFAULT_STALE_DAYS, assemble_pricing_health};
use stax_reports::render::py_thousands;

use crate::click::Output;
use crate::compare::sort_keys;
use crate::reports::open_store;
use crate::status::{engine_for_cli, package_dir};

/// `stax pricing`.
#[derive(Debug, Args)]
pub struct PricingArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: PricingVerb,
}

/// `pricing`'s one leaf.
#[derive(Debug, Subcommand)]
pub enum PricingVerb {
    /// Report pricing health: unpriced models, stale rates, unknown cost rows.
    ///
    /// Reads the live store (``~/.stackunderflow/store.db``) and renders the
    /// same payload ``GET /api/pricing/doctor`` returns. Works without a
    /// running server. Strictly read-only — no DB writes, no network.
    Doctor(DoctorArgs),
}

/// `pricing doctor`.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Output format (text or json).
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
    /// Flag the rate overlay stale when older than this many days.
    ///
    /// `click.option(type=int)` — so `allow_hyphen_values` plus the campaign's
    /// `PyInt` parser, because `--stale-days -1` is a legal Click invocation
    /// and clap would otherwise read `-1` as a flag (the wave-1 lesson, and the
    /// tranche-5 finding that only a test transfers it).
    #[arg(long = "stale-days", value_name = "INTEGER",
          default_value_t = PyInt::from(DEFAULT_STALE_DAYS),
          allow_hyphen_values = true, value_parser = crate::memory::py_int,
          overrides_with = "stale_days")]
    pub stale_days: PyInt,
    /// Max model entries listed per section (full counts stay in the summary).
    #[arg(long = "limit", value_name = "INTEGER",
          default_value_t = PyInt::from(DEFAULT_LIMIT),
          allow_hyphen_values = true, value_parser = crate::memory::py_int,
          overrides_with = "limit")]
    pub limit: PyInt,
    /// Exit non-zero when a hard defect is found (billable unpriced model or unknown row with nonzero cost) — for CI gating.
    #[arg(long = "strict", default_value_t = false)]
    pub strict: bool,
}

/// Run `pricing`.
///
/// # Errors
/// A store that cannot be opened or migrated, a manifest that will not load, or
/// a SQLite failure inside the assembler.
pub fn run_pricing(args: &PricingArgs) -> Result<Output> {
    match &args.verb {
        PricingVerb::Doctor(args) => run_doctor(args),
    }
}

fn run_doctor(args: &DoctorArgs) -> Result<Output> {
    let conn = open_store()?;
    let engine = engine_for_cli(package_dir().as_deref())?;
    // `PricingService.read_cache_status()` reads `app_dir()/cache/pricing.json`
    // — `settings.app_dir()`, not the store file's parent. They are the same
    // directory today; naming the right one keeps them the same directory the
    // day `--data-dir` moves the store.
    let app_dir = stax_core::settings::app_dir();
    let payload = assemble_pricing_health(
        &conn,
        &engine,
        Some(app_dir.as_path()),
        args.stale_days.saturating_i64(),
        args.limit.saturating_i64(),
    )?;
    drop(conn);

    let mut out = if args.format == "json" {
        // `json.dumps(payload, indent=2, sort_keys=True)`.
        Output::ok(format!(
            "{}\n",
            stax_reports::render::render_json(&sort_keys(&payload))
        ))
    } else {
        Output::ok(render_pricing_doctor_text(&payload))
    };
    // `if strict and not payload.get("ok", True): raise SystemExit(1)` — after
    // the render, and with no message of its own.
    if args.strict && !payload.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        out.code = 1;
    }
    Ok(out)
}

/// `_render_pricing_doctor_text(payload)`.
///
/// Every number comes out of the payload; the renderer computes nothing beyond
/// formatting, which is what its docstring promises and what makes the JSON leg
/// a complete oracle for the text one.
#[must_use]
pub fn render_pricing_doctor_text(payload: &Value) -> String {
    let summary = payload.get("summary").cloned().unwrap_or(Value::Null);
    let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(true);

    let mut out = String::new();
    out.push_str(if ok {
        "Pricing health — OK\n"
    } else {
        "Pricing health — ISSUES FOUND\n"
    });
    out.push('\n');

    let total_events = int_of(&summary, "total_events");
    let total_cost = float_of(&summary, "total_cost_usd");
    out.push_str(&format!(
        "  Events:        {} ({} total cost)\n",
        py_thousands(total_events),
        format_dollars(total_cost)
    ));

    // ── rate overlay freshness ───────────────────────────────────────────────
    let freshness = payload
        .get("rate_freshness")
        .cloned()
        .unwrap_or(Value::Null);
    let age = freshness.get("age_days").and_then(Value::as_f64);
    // `freshness.get("source", "none")` — the DEFAULT is "none", so a present
    // key holding `null` renders `None` (Python's `str(None)`), not `"none"`.
    let source = py_str(freshness.get("source"), "none");
    // `freshness.get("stale_days_threshold", payload.get("stale_days"))` — the
    // fallback is itself a `.get`, so an absent key on both sides is `None`.
    let threshold = match freshness.get("stale_days_threshold") {
        Some(value) => py_number_str(value),
        None => py_number_str(payload.get("stale_days").unwrap_or(&Value::Null)),
    };
    let age_phrase = match age {
        // `f"{age:.1f}d old"`. Rust's `{:.1}` and CPython's `.1f` both round
        // the EXACT binary value half-to-even, so they agree on every tie
        // (`0.25`→`0.2`, `0.35`→`0.3`, `1.15`→`1.1`) — measured, not assumed,
        // and the same equivalence `worktrees.rs`'s `{:.2}` already rides on.
        Some(age) => format!("{age:.1}d old"),
        None if source == "none" => "no cached overlay".to_owned(),
        None => "age unknown".to_owned(),
    };
    let stale = truthy(freshness.get("stale"));
    out.push_str(&format!(
        "  Rate overlay:  {source} — {age_phrase} (threshold {threshold}d) [{}]\n",
        if stale { "STALE" } else { "fresh" }
    ));
    out.push('\n');

    // ── unpriced models ──────────────────────────────────────────────────────
    let empty: Vec<Value> = Vec::new();
    let unpriced = payload
        .get("unpriced_models")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    // `summary.get("unpriced_model_count", len(unpriced))` — the SUMMARY count,
    // which is the untruncated one; the list below it is `--limit`-capped.
    let n_unpriced = match summary.get("unpriced_model_count") {
        Some(value) => py_number_str(value),
        None => unpriced.len().to_string(),
    };
    let n_billable = int_of(&summary, "billable_unpriced_model_count");
    let exposure = float_of(&summary, "estimated_unpriced_exposure_usd");
    out.push_str(&format!(
        "  Unpriced models (no rate card): {n_unpriced} (est. exposure {})\n",
        format_dollars(exposure)
    ));
    if n_billable != 0 {
        out.push_str(&format!(
            "    ! {n_billable} are BILLABLE (priced rows against an unresolvable model — a defect)\n"
        ));
    }
    for row in unpriced {
        // `if delta:` — `None` and `0.0` are both falsy, so a zero delta reads
        // "no estimate" exactly as a null one does.
        let delta = row.get("estimated_delta_usd").and_then(Value::as_f64);
        let delta_phrase = match delta.filter(|value| *value != 0.0) {
            Some(value) => format!("+{} if priced", format_dollars(value)),
            None => "no estimate".to_owned(),
        };
        let flag = if truthy(row.get("billable")) {
            " [billable]"
        } else {
            ""
        };
        out.push_str(&format!(
            "      {}/{}: {} events, {delta_phrase}{flag}\n",
            py_str(row.get("provider"), ""),
            py_str(row.get("model"), ""),
            py_thousands(int_of(row, "events")),
        ));
    }
    out.push('\n');

    // ── unknown cost_source ──────────────────────────────────────────────────
    let unknown = payload
        .get("unknown_cost_source")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let n_unknown = match summary.get("unknown_cost_source_model_count") {
        Some(value) => py_number_str(value),
        None => unknown.len().to_string(),
    };
    let violations = int_of(&summary, "unknown_nonzero_cost_rows");
    out.push_str(&format!("  Unknown cost_source models: {n_unknown}\n"));
    for row in unknown {
        let delta = row.get("estimated_delta_usd").and_then(Value::as_f64);
        let delta_phrase = match delta.filter(|value| *value != 0.0) {
            Some(value) => format!("{} recoverable", format_dollars(value)),
            None => "no estimate".to_owned(),
        };
        out.push_str(&format!(
            "      {}/{}: {} events, {delta_phrase}\n",
            py_str(row.get("provider"), ""),
            py_str(row.get("model"), ""),
            py_thousands(int_of(row, "events")),
        ));
    }
    if violations != 0 {
        out.push_str(&format!(
            "    ! {} unknown rows carry a NONZERO cost (contract: unknown ⇒ $0.0)\n",
            py_thousands(violations)
        ));
    }
    out
}

/// `str(value)` for the shapes a payload field can take, with Python's `.get`
/// default when the key is absent.
///
/// A present-but-null field is `str(None)` = `None`, which is what a store with
/// a `NULL` provider prints. The default only fires when the key is missing.
fn py_str(value: Option<&Value>, default: &str) -> String {
    match value {
        None => default.to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => "None".to_owned(),
        Some(Value::Bool(flag)) => (if *flag { "True" } else { "False" }).to_owned(),
        Some(other) => other.to_string(),
    }
}

/// `f"{n}"` for a JSON number — an integral float still renders `7.0` in
/// Python, so the int/float distinction survives into the text.
fn py_number_str(value: &Value) -> String {
    match value {
        Value::Number(number) if number.is_f64() => {
            let float = number.as_f64().unwrap_or(0.0);
            if float.fract() == 0.0 && float.is_finite() {
                format!("{float:.1}")
            } else {
                float.to_string()
            }
        }
        Value::Number(number) => number.to_string(),
        Value::Null => "None".to_owned(),
        other => py_str(Some(other), ""),
    }
}

fn int_of(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn float_of(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Python truthiness for the shapes a JSON value can take.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|float| float != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cold_payload() -> Value {
        json!({
            "stale_days": 7,
            "ok": true,
            "summary": {
                "total_events": 0, "total_cost_usd": 0.0,
                "unpriced_model_count": 0, "billable_unpriced_model_count": 0,
                "unknown_cost_source_model_count": 0, "unknown_nonzero_cost_rows": 0,
                "estimated_unpriced_exposure_usd": 0.0, "rate_cache_stale": true,
            },
            "unpriced_models": [],
            "unknown_cost_source": [],
            "rate_freshness": {
                "source": "none", "timestamp": null, "age_days": null,
                "is_stale": true, "model_count": 0,
                "stale_days_threshold": 7, "stale": true,
            },
        })
    }

    #[test]
    fn a_cold_store_renders_the_five_line_skeleton() {
        assert_eq!(
            render_pricing_doctor_text(&cold_payload()),
            "Pricing health — OK\n\
             \n\
             \x20 Events:        0 ($0.0000 total cost)\n\
             \x20 Rate overlay:  none — no cached overlay (threshold 7d) [STALE]\n\
             \n\
             \x20 Unpriced models (no rate card): 0 (est. exposure $0.0000)\n\
             \n\
             \x20 Unknown cost_source models: 0\n"
        );
    }

    #[test]
    fn an_unreadable_overlay_says_age_unknown_only_when_a_source_is_named() {
        // `age_phrase` branches on `src == "none"`, NOT on the presence of the
        // cache file — a cached overlay whose timestamp will not parse is
        // `source: "cache"` with `age_days: null`, and that is the second leg.
        let mut payload = cold_payload();
        payload["rate_freshness"]["source"] = json!("cache");
        assert!(render_pricing_doctor_text(&payload).contains("cache — age unknown"));
    }

    #[test]
    fn a_zero_delta_is_no_estimate_because_python_tests_truthiness() {
        let mut payload = cold_payload();
        payload["ok"] = json!(false);
        payload["summary"]["unpriced_model_count"] = json!(2);
        payload["summary"]["billable_unpriced_model_count"] = json!(1);
        payload["unpriced_models"] = json!([
            {"provider": "claude", "model": "m-a", "events": 1234,
             "estimated_delta_usd": 0.0, "billable": true},
            {"provider": null, "model": "m-b", "events": 7,
             "estimated_delta_usd": null, "billable": false},
        ]);
        let text = render_pricing_doctor_text(&payload);
        assert!(text.starts_with("Pricing health — ISSUES FOUND\n"));
        assert!(text.contains(
            "    ! 1 are BILLABLE (priced rows against an unresolvable model — a defect)\n"
        ));
        assert!(
            text.contains("      claude/m-a: 1,234 events, no estimate [billable]\n"),
            "a 0.0 delta is falsy in Python:\n{text}"
        );
        assert!(
            text.contains("      None/m-b: 7 events, no estimate\n"),
            "`str(None)` is the four-character `None`:\n{text}"
        );
    }

    #[test]
    fn the_unknown_section_prints_recoverable_and_then_the_violation_line() {
        let mut payload = cold_payload();
        payload["summary"]["unknown_cost_source_model_count"] = json!(1);
        payload["summary"]["unknown_nonzero_cost_rows"] = json!(12345);
        payload["unknown_cost_source"] = json!([
            {"provider": "codex", "model": "gpt-x", "events": 3,
             "estimated_delta_usd": 1.5},
        ]);
        let text = render_pricing_doctor_text(&payload);
        assert!(text.contains("  Unknown cost_source models: 1\n"));
        assert!(text.contains("      codex/gpt-x: 3 events, $1.50 recoverable\n"));
        assert!(text.ends_with(
            "    ! 12,345 unknown rows carry a NONZERO cost (contract: unknown ⇒ $0.0)\n"
        ));
    }

    #[test]
    fn the_age_phrase_is_one_decimal_place() {
        let mut payload = cold_payload();
        payload["rate_freshness"]["source"] = json!("cache");
        payload["rate_freshness"]["age_days"] = json!(30.449_999);
        payload["rate_freshness"]["stale"] = json!(false);
        assert!(
            render_pricing_doctor_text(&payload)
                .contains("  Rate overlay:  cache — 30.4d old (threshold 7d) [fresh]\n")
        );
    }

    #[test]
    fn the_threshold_falls_back_to_the_payloads_stale_days() {
        let mut payload = cold_payload();
        payload["rate_freshness"]
            .as_object_mut()
            .unwrap()
            .remove("stale_days_threshold");
        payload["stale_days"] = json!(90);
        assert!(render_pricing_doctor_text(&payload).contains("(threshold 90d)"));
    }

    #[test]
    fn strict_only_bites_when_ok_is_false() {
        // The renderer is not the gate — `--strict` reads `payload["ok"]`, and
        // `ok` is a HARD-defect flag: a stale overlay leaves it true.
        let payload = cold_payload();
        assert!(payload["ok"].as_bool().unwrap());
        assert!(payload["rate_freshness"]["stale"].as_bool().unwrap());
    }
}
