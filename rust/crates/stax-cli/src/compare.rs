//! `stax compare` — `cli.py:3394`–`:3499` (`compare_cmd` + `_render_compare_table`).
//!
//! The service is already ported (`stax_reports::compare`, wave 5 batch C); what
//! this module is, is the renderer Python keeps in `cli.py` rather than in the
//! service — a **second** Rich table, at `width=160` instead of the console's.
//!
//! # Why `width=160` is in the reference, and why it matters here
//!
//! `Console(force_terminal=False, highlight=False, width=160)`. The comment in
//! `cli.py` says it plainly: without it Rich falls back to 80 columns off a
//! terminal and truncates `Sessions` to `Sessi…`. So this table does **not**
//! read `$COLUMNS` the way `report`/`today`/`month` do (see
//! [`crate::reports::console_width`]) — the width is a literal, and passing the
//! console width here would be a divergence on every pipe.
//!
//! # `generated` is `time.time()` — DIV-085, inherited by the CLI
//!
//! `build_compare_payload` puts a wall-clock float in the payload, so
//! `compare --format json` can never be byte-identical between two processes.
//! The endpoint batch filed that as DIV-085 and left its 200-rows known-open;
//! the CLI inherits it as **DIV-370** and the matrix carries only the `text`
//! rows, which never print the field. The JSON shape is unit-tested instead.
//!
//! # The pricing engine is the manifest's, not the store's price book
//!
//! `routes/compare.rs` injects `crate::pricing::engine` under LAW 2 because
//! `server.py`'s lifespan calls `use_price_book_store` + `prime_price_book_cache`.
//! `cli.py` calls neither — measured, `grep -rn use_price_book_store` hits
//! exactly `server.py` — so the CLI verb prices through
//! [`crate::status::engine_for_cli`], the bare manifest, which is what CPython
//! does here. Using the primed engine would have been "more correct" and would
//! have answered differently from the reference on `claude-opus-5` by a factor
//! of 5/3 (DIV-085's own measurement).

use anyhow::Result;
use clap::Args;
use serde_json::Value;
use stax_reports::compare::{build_compare_payload, now_unix_seconds};
use stax_reports::render::{self, py_thousands};
use stax_reports::scope::Instant;

use crate::click::Output;
use crate::reports::{IngestFlags, guard_refresh, open_store};
use crate::status::{engine_for_cli, package_dir};

/// The `Console(width=160)` literal in `_render_compare_table`.
const COMPARE_CONSOLE_WIDTH: usize = 160;

/// `stax compare`.
#[derive(Debug, Args)]
pub struct CompareArgs {
    /// Window over which to compare (default: month).
    #[arg(short = 'p', long = "period", value_name = "PERIOD", default_value = "month",
          value_parser = ["today", "week", "month", "all"])]
    pub period: String,
    /// Filter by provider id (e.g. claude, codex, cursor).
    #[arg(long = "provider", value_name = "PROVIDER")]
    pub provider: Option<String>,
    // `allow_hyphen_values`: every project slug starts with `-`. Same rule
    // `reports.rs` and `skills.rs` carry, and the same DIV-290 that found it.
    /// Restrict to this project slug (repeatable).
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Vec<String>,
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
    /// `--ingest` / `--auto-ingest`.
    #[command(flatten)]
    pub ingest: IngestFlags,
}

/// Run `compare`.
///
/// # Errors
/// A missing store (DIV-239), a SQLite failure, or the unported refresh pass
/// (DIV-238). The period cannot fail here — Click validates it as a `Choice`
/// before the body runs, and clap's `value_parser` is the same gate.
pub fn run_compare(args: &CompareArgs) -> Result<Output> {
    let conn = open_store()?;
    guard_refresh(&conn, &args.ingest)?;
    let engine = engine_for_cli(package_dir().as_deref())?;

    // `list(project) or None` — an empty tuple is `None`, which `compare_models`
    // reads as "no filter" and which additionally selects the mart fast path.
    // An empty *list* would take the message path and filter everything out.
    let project_filter = (!args.project.is_empty()).then_some(args.project.as_slice());

    let payload = build_compare_payload(
        &conn,
        &engine,
        &args.period,
        project_filter,
        args.provider.as_deref(),
        Instant::now_utc(),
        now_unix_seconds,
    )?;

    Ok(emit(&payload, &args.format))
}

/// The two output branches, split out so both are testable without a store.
#[must_use]
pub fn emit(payload: &Value, format: &str) -> Output {
    if format == "json" {
        // `json.dumps(payload, indent=2, sort_keys=True)` — the ONLY sorted
        // writer in the reports family, and it sorts at every level, so the
        // eleven `ModelStats` keys come out alphabetically too.
        return Output::ok(format!("{}\n", render::render_json(&sort_keys(payload))));
    }
    Output::ok(render_compare_table(payload))
}

/// `sort_keys=True`, applied recursively.
///
/// CPython sorts with `list.sort()` over `str`, which is code-point order;
/// `str::cmp` is UTF-8 byte order, and the two coincide for every string. Built
/// as a new tree rather than mutated in place because `serde_json::Map` under
/// `preserve_order` is an `IndexMap` whose sort surface differs by version.
#[must_use]
pub fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                pairs
                    .into_iter()
                    .map(|(key, val)| (key.clone(), sort_keys(val)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// `_render_compare_table(payload)` — the exact bytes `console.print` writes.
#[must_use]
pub fn render_compare_table(payload: &Value) -> String {
    // `payload.get("period", "")` — a missing key is the empty string, not a
    // crash. Kept literal: the payload is always this module's own, but the
    // reference's default is the reference's.
    let period = payload
        .get("period")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let empty: Vec<Value> = Vec::new();
    let rows = payload
        .get("models")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    if rows.is_empty() {
        // TWO `console.print` calls and NO table — the early return is the
        // whole branch.
        let mut out =
            render::rich::print_text(&format!("Compare — {period}"), COMPARE_CONSOLE_WIDTH);
        out.push_str(&render::rich::print_text(
            "No model activity in this window.",
            COMPARE_CONSOLE_WIDTH,
        ));
        return out;
    }

    use render::rich::Justify::{Left, Right};
    let mut table = render::rich::Table::new(&[
        ("Model", Left),
        ("Sessions", Right),
        ("Calls", Right),
        ("1-shot%", Right),
        ("Retry", Right),
        ("Cache%", Right),
        ("$/call", Right),
        ("$/session", Right),
        ("Total$", Right),
    ])
    .with_title(format!("Compare — {period}"));

    for row in rows {
        table.add_row(&[
            row.get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            py_thousands(int_of(row, "sessions")),
            py_thousands(int_of(row, "calls")),
            format!("{:.1}%", float_of(row, "one_shot_pct") * 100.0),
            format!("{:.2}", float_of(row, "retry_rate")),
            format!("{:.1}%", float_of(row, "cache_hit_rate") * 100.0),
            format!("${:.4}", float_of(row, "cost_per_call")),
            format!("${:.2}", float_of(row, "cost_per_session")),
            format!("${:.2}", float_of(row, "total_cost")),
        ]);
    }
    table.render(COMPARE_CONSOLE_WIDTH)
}

fn int_of(row: &Value, key: &str) -> i64 {
    row.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn float_of(row: &Value, key: &str) -> f64 {
    row.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use serde_json::json;

    use super::*;

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        args: CompareArgs,
    }

    #[test]
    fn the_defaults_are_the_decorators() {
        let parsed = Wrap::try_parse_from(["x"]).expect("bare parse");
        assert_eq!(parsed.args.period, "month");
        assert_eq!(parsed.args.format, "text");
        assert!(parsed.args.provider.is_none());
        assert!(parsed.args.project.is_empty());
        assert!(parsed.args.ingest.auto());
    }

    #[test]
    fn the_period_is_a_choice_and_an_unknown_one_never_reaches_the_body() {
        assert!(Wrap::try_parse_from(["x", "-p", "week"]).is_ok());
        assert!(Wrap::try_parse_from(["x", "-p", "7days"]).is_err());
    }

    #[test]
    fn a_real_slug_parses_as_a_value_not_a_flag() {
        let parsed = Wrap::try_parse_from([
            "x",
            "--project",
            "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
        ])
        .expect("a leading-hyphen slug is a value");
        assert_eq!(parsed.args.project.len(), 1);
    }

    #[test]
    fn the_empty_window_prints_two_lines_and_no_table() {
        let payload = json!({"period": "month", "models": [], "generated": 0.0});
        let text = render_compare_table(&payload);
        assert_eq!(
            text, "Compare — month\nNo model activity in this window.\n",
            "the early return emits no box-drawing at all"
        );
    }

    #[test]
    fn the_table_carries_the_title_and_the_nine_columns() {
        let payload = json!({
            "period": "all",
            "models": [{
                "model": "claude-opus-5",
                "provider": "anthropic",
                "sessions": 1234,
                "calls": 5678,
                "one_shot_pct": 0.125,
                "retry_rate": 3.5,
                "cache_hit_rate": 0.9,
                "cost_per_call": 0.001_25,
                "cost_per_session": 12.5,
                "total_cost": 15_432.5,
                "total_tokens": 9,
            }],
            "generated": 0.0,
        });
        let text = render_compare_table(&payload);
        assert!(text.contains("Compare — all"), "{text}");
        for header in [
            "Model",
            "Sessions",
            "Calls",
            "1-shot%",
            "Retry",
            "Cache%",
            "$/call",
            "$/session",
            "Total$",
        ] {
            assert!(text.contains(header), "missing {header} in\n{text}");
        }
        // The nine cell formats, each one an f-string in the reference.
        assert!(text.contains("1,234"), "sessions are `:,`");
        assert!(text.contains("5,678"), "calls are `:,`");
        assert!(text.contains("12.5%"), "one_shot_pct is `*100:.1f`");
        assert!(text.contains("3.50"), "retry_rate is `:.2f`");
        assert!(text.contains("90.0%"), "cache_hit_rate is `*100:.1f`");
        assert!(text.contains("$0.0013"), "cost_per_call is `:.4f`");
        assert!(text.contains("$12.50"), "cost_per_session is `:.2f`");
        assert!(text.contains("$15432.50"), "total_cost is `:.2f`, NOT `:,`");
    }

    #[test]
    fn the_table_is_one_hundred_and_sixty_wide_not_the_console() {
        // The regression the reference's own comment names: at 80 columns the
        // `Sessions` header truncates. Nine columns of content have to fit.
        let mut models = Vec::new();
        for index in 0..3 {
            models.push(json!({
                "model": format!("some-fairly-long-model-name-{index}"),
                "provider": "anthropic",
                "sessions": 10, "calls": 20,
                "one_shot_pct": 0.5, "retry_rate": 1.0, "cache_hit_rate": 0.25,
                "cost_per_call": 0.5, "cost_per_session": 1.0, "total_cost": 2.0,
                "total_tokens": 3,
            }));
        }
        let payload = json!({"period": "month", "models": models, "generated": 0.0});
        let text = render_compare_table(&payload);
        assert!(text.contains("Sessions"), "no truncation to `Sessi…`");
        assert!(!text.contains('…'), "nothing is ellipsised at 160:\n{text}");
    }

    #[test]
    fn the_json_branch_sorts_every_level() {
        let payload = json!({
            "period": "month",
            "models": [{"model": "m", "sessions": 1, "calls": 2}],
            "generated": 0.0,
        });
        let out = emit(&payload, "json").stdout;
        // Top level: generated, models, period.
        assert!(
            out.starts_with("{\n  \"generated\": 0.0,\n  \"models\": ["),
            "{out}"
        );
        // Nested: calls, model, sessions.
        let models_at = out.find("\"models\"").expect("models key");
        let nested = &out[models_at..];
        assert!(
            nested.find("\"calls\"").unwrap() < nested.find("\"model\"").unwrap(),
            "sort_keys=True is recursive:\n{out}"
        );
        assert!(out.ends_with("}\n"), "click.echo adds the newline");
    }

    #[test]
    fn sort_keys_leaves_arrays_in_order() {
        let value = json!({"b": [3, 1, 2], "a": 1});
        let sorted = sort_keys(&value);
        assert_eq!(sorted["b"], json!([3, 1, 2]), "only KEYS are sorted");
        let keys: Vec<&String> = sorted.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn a_missing_period_key_renders_the_empty_string_not_a_panic() {
        // …and the trailing space Rich would have printed is gone: every line
        // `console.print` emits is right-stripped (`render::rich::print_text`),
        // so `f"Compare — {''}"` lands as `Compare —` with no trailing blank.
        let payload = json!({"models": []});
        assert_eq!(
            render_compare_table(&payload),
            "Compare —\nNo model activity in this window.\n"
        );
    }
}
