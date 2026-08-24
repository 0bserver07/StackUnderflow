//! `stax etl status | backfill` — `cli.py:4622`–`:4856`.
//!
//! Both leaves are the CLI half of an endpoint that batch E already ported.
//! `status` renders exactly what `GET /api/etl/status` returns (`cli.py`'s own
//! docstring says so) and `backfill` runs exactly what `POST /api/etl/backfill`
//! schedules — so both call the shared implementations rather than a second
//! copy: [`stax_reports::etl_status::assemble_status`] and
//! [`stax_etl::backfill::backfill`]. Those two modules used to live inside
//! `stax-server`, which `stax-cli` may not link (DIV-279); moving them is what
//! made this file possible and is recorded at both new addresses.
//!
//! # The two job slots are `None` here, and that is not a stub
//!
//! `assemble_status` reads `backfill_jobs.get_current_job()` /
//! `get_last_job()`, a `threading.Lock` and two **module-level** slots. A CLI
//! process has never scheduled a backfill, so both are `None` on the reference
//! too — every `stax etl status` invocation, by construction, not by
//! coincidence. The port passes `None, None` explicitly instead of reaching
//! for a global that would then be shared with an in-process server.
//!
//! # `etl backfill` prices UNPRIMED, and the endpoint prices primed
//!
//! `use_price_book_store` is called by `server.py`'s lifespan and by nothing
//! else, so the CLI's `NormalizeContext` comes from the manifest
//! (`NormalizeContext::unprimed`) where `routes/etl.rs` builds one from
//! `crate::pricing::engine`. Same orchestrator, two rate sources, and the
//! asymmetry is the reference's — DIV-016 / RS-3-082.
//!
//! # There is no progress bar, because there is no `tqdm`
//!
//! `_build_backfill_progress_callback` returns `None` when `tqdm` is not
//! importable, and the reference venv this campaign diffs against does not
//! have it (measured: `import tqdm` raises `ModuleNotFoundError`). So the
//! reference emits no bar on any harness run and neither does this port. On a
//! machine that *does* have `tqdm` the reference would write a live-updating
//! bar to **stderr** — DIV-410, recorded rather than reproduced, because a
//! self-overwriting terminal animation is not a byte-diffable artefact and
//! adding a dependency to emit one would be the port inventing output.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::Value;
use stax_core::queries::pytime;
use stax_reports::etl_status::assemble_status;
use stax_reports::render::py_thousands;

use crate::click::Output;
use crate::compare::sort_keys;
use crate::reports::open_store;
use crate::status::{engine_for_cli, package_dir};

/// `_MART_RENDER_ORDER` — the five names `_render_etl_status_text` walks.
///
/// The literal tuple in `cli.py`, not `KNOWN_MART_NAMES` and not
/// `marts::all()`'s eight. A mart missing from the payload is `continue`d, so
/// the list is a filter as well as an order.
const MART_RENDER_ORDER: [&str; 5] = ["daily", "session", "project", "provider_day", "model_day"];

/// `stax etl`.
#[derive(Debug, Args)]
pub struct EtlArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: EtlVerb,
}

/// `etl`'s two leaves.
#[derive(Debug, Subcommand)]
pub enum EtlVerb {
    /// Convert all existing messages into usage_events, then refresh marts.
    ///
    /// Default mode is incremental: messages already converted on a prior
    /// run are skipped via the ``uniq_events_msg`` UNIQUE index.
    ///
    /// ``--force`` first wipes ``usage_events`` + ``mart_watermark``,
    /// rebuilds every mart from scratch, and then runs the normalize
    /// pass fresh — useful after a normalizer change or a model rate
    /// update.
    Backfill(BackfillArgs),
    /// Show ETL pipeline health: watcher, marts, events, lag.
    ///
    /// Reads the live store and renders a one-screen snapshot — the same
    /// payload ``GET /api/etl/status`` returns. Works without a running
    /// server (the CLI opens its own connection to ``~/.stackunderflow/store.db``).
    Status(StatusArgs),
}

/// `etl backfill`.
#[derive(Debug, Args)]
pub struct BackfillArgs {
    /// Drop events + marts + watermarks and rebuild from scratch.
    #[arg(long = "force", default_value_t = false)]
    pub force: bool,
}

/// `etl status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Output format (text or json).
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// Run `etl`.
///
/// # Errors
/// A store that cannot be opened or migrated, a manifest that will not load,
/// or any SQLite error out of the assembler or the orchestrator. Python lets
/// the same errors propagate: `etl backfill` has no `except`, only a `finally`
/// that closes the connection.
pub fn run_etl(args: &EtlArgs) -> Result<Output> {
    match &args.verb {
        EtlVerb::Backfill(args) => run_backfill(args),
        EtlVerb::Status(args) => run_status(args),
    }
}

fn run_status(args: &StatusArgs) -> Result<Output> {
    let conn = open_store()?;
    let app_dir = stax_core::settings::app_dir();
    let disable_watcher = std::env::var("STACKUNDERFLOW_DISABLE_WATCHER").ok();
    let payload = assemble_status(&conn, &app_dir, disable_watcher.as_deref(), None, None)?;
    drop(conn);

    if args.format == "json" {
        // `json.dumps(payload, indent=2, sort_keys=True)`.
        return Ok(Output::ok(format!(
            "{}\n",
            stax_reports::render::render_json(&sort_keys(&payload))
        )));
    }
    Ok(Output::ok(render_etl_status_text(&payload)))
}

fn run_backfill(args: &BackfillArgs) -> Result<Output> {
    let conn = open_store()?;
    let engine = engine_for_cli(package_dir().as_deref())?;
    let ctx = stax_etl::normalize::NormalizeContext::new(engine);
    // `refresh_all_marts` takes ONE stamp here where Python's `set_watermark`
    // re-reads `datetime.now(UTC)` per mart — the second finding in
    // `parity/DIV-e-etl.md`, inherited unchanged from the endpoint that shares
    // this orchestrator. It is invisible to any stdout diff (the stamp is
    // never printed) and visible in `mart_watermark.last_refresh_ts`.
    let now = pytime::isoformat_utc(pytime::now_micros());
    let report = stax_etl::backfill::backfill(&conn, &ctx, args.force, &now);
    // Python's `finally: conn.close()` runs before the report is rendered and
    // *before* an exception propagates, so the close is unconditional here too.
    drop(conn);
    let report = report?;

    let mut out = String::new();
    out.push_str("\nBackfill complete.\n");
    out.push_str(&format!(
        "  events inserted:            {}\n",
        py_thousands(i64::try_from(report.events_inserted).unwrap_or(i64::MAX))
    ));
    out.push_str(&format!(
        "  events skipped (duplicate): {}\n",
        py_thousands(i64::try_from(report.events_skipped_duplicate).unwrap_or(i64::MAX))
    ));
    if report.marts_refreshed.is_empty() {
        out.push_str("  marts refreshed:            (none registered)\n");
    } else {
        out.push_str("  marts refreshed:\n");
        // `sorted(report.marts_refreshed.items())` — by NAME, ascending, not
        // the registry order `refresh_all_marts` returns.
        let mut rows = report.marts_refreshed.clone();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, count) in rows {
            // `f"    {name:<14s}  {count:>8,} events"` — ljust 14, then a
            // comma-grouped count RIGHT-justified in 8.
            out.push_str(&format!(
                "    {:<14}  {:>8} events\n",
                name,
                py_thousands(count)
            ));
        }
    }
    // `f"{report.duration_seconds:.3f}s"` — a wall clock, and the reason this
    // verb cannot be a byte-diff row (see `rust/ETL-BACKFILL-DIFFER.md`).
    out.push_str(&format!(
        "  duration:                   {:.3}s\n",
        report.duration_seconds
    ));
    Ok(Output::ok(out))
}

/// `_render_etl_status_text(payload)`.
///
/// `click.secho(..., fg=…, bold=…)` writes no escape codes off a terminal, so
/// the four styled lines here are plain text — the same reasoning `worktrees`'
/// bold header carries.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn render_etl_status_text(payload: &Value) -> String {
    let mut out = String::new();

    let health = str_or(payload.get("health"), "unknown");
    let watcher = block(payload, "watcher");
    // `watcher.get("seconds_since_refresh")` — `is not None`, so a `0` prints
    // "last refresh 0s ago" rather than falling through to the else.
    let refresh_phrase = match watcher.get("seconds_since_refresh") {
        Some(Value::Null) | None => "no refresh observed".to_owned(),
        Some(value) => format!("last refresh {}s ago", py_scalar(value)),
    };
    out.push_str(&format!("ETL pipeline — {health} ({refresh_phrase})\n"));
    out.push('\n');

    // ── events ───────────────────────────────────────────────────────────────
    let events = block(payload, "events");
    let max_id = int_of(&events, "max_id");
    out.push_str(&format!(
        "  Events:        {} total ({} max id)\n",
        py_thousands(int_of(&events, "total")),
        py_thousands(max_id)
    ));
    for (key, label) in [
        ("by_provider", "by provider"),
        ("by_cost_source", "by cost source"),
    ] {
        // `if by_provider:` — an empty dict prints no line at all.
        let Some(map) = events.get(key).and_then(Value::as_object) else {
            continue;
        };
        if map.is_empty() {
            continue;
        }
        // `sorted(d.items(), key=lambda kv: (-kv[1], kv[0]))` — heaviest first,
        // ties broken by the key ascending.
        let mut pairs: Vec<(&String, i64)> = map
            .iter()
            .map(|(name, count)| (name, count.as_i64().unwrap_or(0)))
            .collect();
        pairs.sort_by(|left, right| (-left.1, left.0).cmp(&(-right.1, right.0)));
        let rendered: Vec<String> = pairs
            .into_iter()
            .map(|(name, count)| format!("{name}={}", py_thousands(count)))
            .collect();
        out.push_str(&format!(
            "                 {label}: {}\n",
            rendered.join(" ")
        ));
    }
    out.push('\n');

    // ── marts ────────────────────────────────────────────────────────────────
    let marts = block(payload, "marts");
    out.push_str("  Marts:\n");
    if marts.as_object().is_none_or(serde_json::Map::is_empty) {
        out.push_str("                 (no marts registered)\n");
    } else {
        for name in MART_RENDER_ORDER {
            // `row = marts.get(name); if not row: continue` — Python
            // truthiness, so an EMPTY dict is skipped as well as an absent key.
            let Some(row) = marts.get(name).filter(|row| !is_falsy(row)) else {
                continue;
            };
            let watermark = int_of(row, "watermark");
            let rows = int_of(row, "row_count");
            // `max(0, max_event_id - wm) if max_event_id else 0` — an empty
            // store reports every mart fresh, however far behind it is.
            let lag = if max_id == 0 {
                0
            } else {
                (max_id - watermark).max(0)
            };
            let tag = if lag == 0 {
                "fresh".to_owned()
            } else {
                format!("{} behind", py_thousands(lag))
            };
            let label = format!("{name}={} rows", py_thousands(rows));
            out.push_str(&format!(
                "                 {label:<24}  (watermark {}, {tag})\n",
                py_thousands(watermark)
            ));
        }
    }

    // ── coverage ─────────────────────────────────────────────────────────────
    let coverage = block(payload, "coverage");
    let projects = int_of(&coverage, "projects");
    if projects != 0 {
        let with_mart = int_of(&coverage, "projects_with_mart");
        let without = int_of(&coverage, "projects_without_mart");
        out.push_str(&format!(
            "                 project coverage: {}/{}\n",
            py_thousands(with_mart),
            py_thousands(projects)
        ));
        if without != 0 {
            let empty: Vec<Value> = Vec::new();
            let sample = coverage
                .get("projects_without_mart_sample")
                .and_then(Value::as_array)
                .unwrap_or(&empty);
            // `", ".join(str(i) for i in sample)` — `str()`, so an int prints
            // bare and the ellipsis marks a truncated list.
            let mut sample_str = sample
                .iter()
                .map(py_scalar)
                .collect::<Vec<String>>()
                .join(", ");
            if i64::try_from(sample.len()).unwrap_or(i64::MAX) < without {
                sample_str.push_str(", …");
            }
            out.push_str(&format!(
                "                 {} project(s) have NO mart row{}\n",
                py_thousands(without),
                if sample_str.is_empty() {
                    String::new()
                } else {
                    format!(" (ids: {sample_str})")
                }
            ));
        }
    }
    out.push('\n');

    // ── watcher ──────────────────────────────────────────────────────────────
    // `enabled` false wins over everything; then the string `"unknown"`, which
    // is what every reachable path actually produces (there is no live handle
    // in a CLI process, nor in the port's server); then truthiness.
    let enabled = truthy(watcher.get("enabled"));
    let running = watcher.get("running").cloned().unwrap_or(Value::Null);
    let state = if enabled {
        // `running == "unknown"` — a STRING comparison, so the integer 0 and
        // the boolean False both fall through to the truthiness branch below.
        if running.as_str() == Some("unknown") {
            "state unknown (no live handle — server not running?)"
        } else if truthy(Some(&running)) {
            "running"
        } else {
            "stopped"
        }
    } else {
        "disabled (STACKUNDERFLOW_DISABLE_WATCHER=1)"
    };
    out.push_str(&format!("  Watcher:       {state}\n"));
    if let Some(value) = watcher
        .get("events_in_last_cycle")
        .filter(|value| **value != Value::Null)
    {
        out.push_str(&format!(
            "                 last cycle: {} events processed\n",
            py_thousands(value.as_i64().unwrap_or(0))
        ));
    }
    if let Some(value) = watcher
        .get("lock_held_by")
        .filter(|value| **value != Value::Null)
    {
        out.push_str(&format!(
            "                 lock held by PID {}\n",
            py_scalar(value)
        ));
    }

    // ── the footer, printed only when there IS lag ────────────────────────────
    let lag = int_of(payload, "lag_seconds");
    if lag != 0 {
        out.push('\n');
        out.push_str(&format!(
            "  Lag (events behind marts): {}\n",
            py_thousands(lag)
        ));
    }
    out
}

/// `payload.get(key) or {}` — an absent, null or empty block is `{}`.
fn block(payload: &Value, key: &str) -> Value {
    match payload.get(key) {
        Some(value) if !is_falsy(value) => value.clone(),
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// `not x` for the shapes a JSON value can take.
fn is_falsy(value: &Value) -> bool {
    !truthy(Some(value))
}

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

/// `int(row.get(key, 0))` — a null or a missing key is 0.
fn int_of(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// `str(x)` for a scalar the payload can carry.
fn py_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_owned(),
        Value::Bool(flag) => (if *flag { "True" } else { "False" }).to_owned(),
        other => other.to_string(),
    }
}

fn str_or(value: Option<&Value>, default: &str) -> String {
    match value {
        None => default.to_owned(),
        Some(other) => py_scalar(other),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn empty_payload() -> Value {
        json!({
            "watcher": {"enabled": true, "running": "unknown", "last_refresh_ts": null,
                        "seconds_since_refresh": null, "events_in_last_cycle": null,
                        "lock_held_by": null},
            "marts": {
                "daily": {"watermark": 0, "row_count": 0, "last_refresh_ts": null},
                "session": {"watermark": 0, "row_count": 0, "last_refresh_ts": null},
                "project": {"watermark": 0, "row_count": 0, "last_refresh_ts": null},
                "provider_day": {"watermark": 0, "row_count": 0, "last_refresh_ts": null},
                "model_day": {"watermark": 0, "row_count": 0, "last_refresh_ts": null},
            },
            "events": {"total": 0, "max_id": 0, "by_provider": {}, "by_cost_source": {}},
            "coverage": {"projects": 0, "projects_with_mart": 0, "projects_without_mart": 0,
                         "projects_without_mart_sample": []},
            "lag_seconds": 0,
            "health": "live",
            "current_job": null,
            "last_job": null,
        })
    }

    #[test]
    fn an_empty_store_renders_the_skeleton_with_no_breakdown_lines() {
        assert_eq!(
            render_etl_status_text(&empty_payload()),
            concat!(
                "ETL pipeline — live (no refresh observed)\n",
                "\n",
                "  Events:        0 total (0 max id)\n",
                "\n",
                "  Marts:\n",
                "                 daily=0 rows              (watermark 0, fresh)\n",
                "                 session=0 rows            (watermark 0, fresh)\n",
                "                 project=0 rows            (watermark 0, fresh)\n",
                "                 provider_day=0 rows       (watermark 0, fresh)\n",
                "                 model_day=0 rows          (watermark 0, fresh)\n",
                "\n",
                "  Watcher:       state unknown (no live handle — server not running?)\n",
            )
        );
    }

    #[test]
    fn the_breakdowns_sort_by_descending_count_then_by_key() {
        let mut payload = empty_payload();
        payload["events"] = json!({
            "total": 3300, "max_id": 4000,
            "by_provider": {"codex": 300, "claude": 3000, "cursor": 300},
            "by_cost_source": {"unknown": 1, "rate_card": 3299},
        });
        let text = render_etl_status_text(&payload);
        assert!(text.contains("  Events:        3,300 total (4,000 max id)\n"));
        assert!(
            text.contains("                 by provider: claude=3,000 codex=300 cursor=300\n"),
            "ties keep the key ascending:\n{text}"
        );
        assert!(
            text.contains("                 by cost source: rate_card=3,299 unknown=1\n"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_store_reports_every_mart_fresh_however_far_behind() {
        // `max(0, max_event_id - wm) if max_event_id else 0` — the guard is on
        // `max_event_id`, so a store with no events cannot show lag even with
        // a nonsense watermark.
        let mut payload = empty_payload();
        payload["marts"]["daily"]["watermark"] = json!(999);
        assert!(render_etl_status_text(&payload).contains("(watermark 999, fresh)"));

        payload["events"]["max_id"] = json!(1000);
        let text = render_etl_status_text(&payload);
        assert!(text.contains("(watermark 999, 1 behind)"), "{text}");
        assert!(
            text.contains("                 session=0 rows            (watermark 0, 1,000 behind)"),
            "{text}"
        );
    }

    #[test]
    fn a_mart_missing_from_the_payload_is_skipped_not_zero_filled() {
        let mut payload = empty_payload();
        payload["marts"].as_object_mut().unwrap().remove("project");
        // An EMPTY dict is falsy in Python, so `if not row: continue` drops it
        // exactly as an absent key does.
        payload["marts"]["session"] = json!({});
        let text = render_etl_status_text(&payload);
        assert!(!text.contains("project="), "{text}");
        assert!(!text.contains("session="), "{text}");
        assert!(text.contains("daily=0 rows"));
        assert!(text.contains("model_day=0 rows"));
    }

    #[test]
    fn coverage_prints_only_when_projects_are_known_and_marks_a_truncated_sample() {
        let mut payload = empty_payload();
        assert!(!render_etl_status_text(&payload).contains("project coverage"));

        payload["coverage"] = json!({
            "projects": 25, "projects_with_mart": 1, "projects_without_mart": 24,
            "projects_without_mart_sample": [2, 3, 4],
        });
        let text = render_etl_status_text(&payload);
        assert!(text.contains("                 project coverage: 1/25\n"));
        assert!(
            text.contains("                 24 project(s) have NO mart row (ids: 2, 3, 4, …)\n"),
            "3 of 24 sampled ⇒ the ellipsis:\n{text}"
        );

        payload["coverage"]["projects_without_mart"] = json!(3);
        assert!(
            render_etl_status_text(&payload)
                .contains("3 project(s) have NO mart row (ids: 2, 3, 4)\n"),
            "a complete sample gets no ellipsis"
        );
    }

    #[test]
    fn the_watcher_line_has_four_states_and_enabled_wins() {
        let mut payload = empty_payload();
        payload["watcher"]["enabled"] = json!(false);
        assert!(
            render_etl_status_text(&payload)
                .contains("  Watcher:       disabled (STACKUNDERFLOW_DISABLE_WATCHER=1)\n"),
            "disabled beats a `running` of any value"
        );

        payload["watcher"]["enabled"] = json!(true);
        payload["watcher"]["running"] = json!(true);
        assert!(render_etl_status_text(&payload).contains("  Watcher:       running\n"));
        payload["watcher"]["running"] = json!(false);
        assert!(render_etl_status_text(&payload).contains("  Watcher:       stopped\n"));
    }

    #[test]
    fn a_zero_seconds_since_refresh_still_prints_the_phrase() {
        // `if last_refresh is not None` — `0` is not None, and Python's `or`
        // is nowhere near this branch.
        let mut payload = empty_payload();
        payload["watcher"]["seconds_since_refresh"] = json!(0);
        assert!(
            render_etl_status_text(&payload)
                .starts_with("ETL pipeline — live (last refresh 0s ago)\n")
        );
    }

    #[test]
    fn the_lag_footer_and_the_two_optional_watcher_lines() {
        let mut payload = empty_payload();
        payload["lag_seconds"] = json!(12345);
        payload["watcher"]["events_in_last_cycle"] = json!(9000);
        payload["watcher"]["lock_held_by"] = json!(4242);
        let text = render_etl_status_text(&payload);
        assert!(text.contains("                 last cycle: 9,000 events processed\n"));
        assert!(text.contains("                 lock held by PID 4242\n"));
        assert!(
            text.ends_with("\n  Lag (events behind marts): 12,345\n"),
            "{text}"
        );
    }
}
