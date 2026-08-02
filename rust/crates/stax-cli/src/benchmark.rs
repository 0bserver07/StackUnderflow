//! `stax benchmark show | recommend` — `cli.py:2701`–`:2904`.
//!
//! `stax_reports::benchmark::{analyze_benchmark, recommend_from_history}` are
//! the engine and are already ported (wave 5, batch E). This module is the two
//! verbs: the period alias table, the `--project` resolution, the strata packer,
//! the two envelopes and the two text renderers.
//!
//! # `_bench_pack` is NOT `context-replay`'s packer, and the difference is real
//!
//! `context_replay_cmd` sums a per-event estimate incrementally; `_bench_pack`
//! re-estimates the **whole trial list** on every step
//! (`agent_output.estimate_tokens([*kept, r])`). Those two disagree: the list
//! form pays for `[`, `]` and the `,` separators, so the same rows against the
//! same budget can pack differently. Reproduced as written — O(n²) and all —
//! because the cut point is the output.
//!
//! # The scope the two verbs pass is not the same
//!
//! `benchmark show` passes its resolved `Scope`; `benchmark recommend` calls
//! `recommend_from_history` with **no** scope at all, so it reads the whole
//! store regardless of any window. `routes/benchmark.rs` passes a scope on both
//! endpoints — the CLI verb and the HTTP endpoint genuinely differ here, and
//! copying the route would have silently narrowed the recommendation.
//!
//! # `--period` is free text validated in the body
//!
//! `_bench_scope` raises `click.BadParameter(param_hint="--period")`, i.e.
//! Click's usage block at exit **2** — not the `ClickException` exit 1 that
//! `report -p nope` gives. One decorator apart, one exit code apart.

use anyhow::Result;
use clap::{Args, Subcommand};
use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_core::queries::pyint::PyInt;
use stax_etl::stats::pytext::py_str;
use stax_memory::envelope::{MemoryCommand, build_envelope, render_line};
use stax_memory::pyjson as mempyjson;
use stax_reports::benchmark::{Weights, analyze_benchmark, recommend_from_history};
use stax_reports::benchmark_stats::CI_LEVEL;
use stax_reports::scope::{Instant, Scope, parse_period};

use crate::click::{Output, PROGRAM, UsageError};
use crate::context_replay::resolve_context_budget;
use crate::reports::open_store;

/// `_BENCH_PERIOD_ALIASES` — insertion order, which is what the error lists.
const BENCH_PERIOD_ALIASES: [(&str, &str); 6] = [
    ("today", "today"),
    ("week", "7days"),
    ("7days", "7days"),
    ("month", "month"),
    ("30days", "30days"),
    ("all", "all"),
];

/// `stax benchmark`.
#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: BenchmarkVerb,
}

/// `benchmark`'s two leaves.
#[derive(Debug, Subcommand)]
pub enum BenchmarkVerb {
    /// Leaderboard + per-stratum honesty for the current scope.
    Show(ShowArgs),
    /// Outcome-aware model pick for a described task.
    Recommend(RecommendArgs),
}

/// `benchmark show`.
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// today | week | month | all
    #[arg(
        long = "period",
        value_name = "PERIOD",
        default_value = "all",
        allow_hyphen_values = true
    )]
    pub period: String,
    /// Project slug/path to scope to. Default: whole store.
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,
    /// Filter to one intent stratum (build/fix/explore/refactor/test/ops).
    #[arg(long = "intent", value_name = "INTENT", allow_hyphen_values = true)]
    pub intent: Option<String>,
    /// Token budget for --json output (strata are packed to fit).
    #[arg(long = "context-budget", value_name = "CONTEXT_BUDGET",
          value_parser = crate::memory::py_int, allow_hyphen_values = true)]
    pub context_budget: Option<PyInt>,
    /// Shortcut for --format json.
    #[arg(long = "json", action = clap::ArgAction::SetTrue)]
    pub as_json: bool,
    /// Output format. 'json' emits the stable agent-output envelope.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// `benchmark recommend`.
#[derive(Debug, Args)]
pub struct RecommendArgs {
    /// Task intent: build/fix/explore/refactor/test/ops.
    #[arg(
        long = "intent",
        value_name = "INTENT",
        required = true,
        allow_hyphen_values = true
    )]
    pub intent: String,
    /// Task size band: tiny/small/med/large.
    #[arg(long = "size", value_name = "SIZE", allow_hyphen_values = true)]
    pub size: Option<String>,
    /// Dominant language hint (e.g. python).
    #[arg(long = "language", value_name = "LANGUAGE", allow_hyphen_values = true)]
    pub language: Option<String>,
    /// Project slug/path to scope to.
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,
    /// Token budget for --json output.
    #[arg(long = "context-budget", value_name = "CONTEXT_BUDGET",
          value_parser = crate::memory::py_int, allow_hyphen_values = true)]
    pub context_budget: Option<PyInt>,
    /// Shortcut for --format json.
    #[arg(long = "json", action = clap::ArgAction::SetTrue)]
    pub as_json: bool,
    /// Output format. 'json' emits the stable agent-output envelope.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// Run `benchmark`.
///
/// # Errors
/// A store that cannot be opened or migrated. A bad `--period` is a
/// `BadParameter` rendered into the returned [`Output`] at exit 2.
pub fn run_benchmark(args: &BenchmarkArgs) -> Result<Output> {
    match &args.verb {
        BenchmarkVerb::Show(args) => run_show(args),
        BenchmarkVerb::Recommend(args) => run_recommend(args),
    }
}

fn run_show(args: &ShowArgs) -> Result<Output> {
    let json_mode = args.as_json || args.format == "json";
    let budget = resolve_context_budget(args.context_budget.as_ref());

    // `_bench_scope` runs BEFORE `_open_store`, so a bad period never touches
    // the disk — and on a machine with no store it still exits 2, not 1.
    let scope = match bench_scope(&args.period) {
        Ok(scope) => scope,
        Err(usage) => return Ok(Output::usage(&usage, PROGRAM)),
    };

    let conn = open_store()?;
    let project_ids = bench_project_ids(&conn, args.project.as_deref());
    let report = analyze_benchmark(
        &conn,
        Some(&scope),
        project_ids.as_deref(),
        args.intent.as_deref(),
        Weights::default(),
        CI_LEVEL,
    );
    drop(conn);

    if json_mode {
        let strata = report
            .get("strata")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (rows, truncated) = bench_pack(&strata, budget);

        let mut query = Map::new();
        query.insert("period".to_owned(), Value::String(args.period.clone()));
        query.insert("project".to_owned(), opt_str(args.project.as_deref()));
        query.insert("intent".to_owned(), opt_str(args.intent.as_deref()));

        let mut extra = Map::new();
        for key in [
            "verdict",
            "coverage",
            "weights",
            "rubric_version",
            "ci_level",
            "warning",
        ] {
            // `report.get(key)` — an ABSENT key is `None`, which renders `null`.
            extra.insert(
                key.to_owned(),
                report.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        let envelope = build_envelope(
            MemoryCommand::from("benchmark"),
            query,
            rows,
            budget,
            truncated,
            extra,
        );
        return Ok(Output::ok(render_line(&envelope)));
    }

    Ok(Output::ok(render_show_text(
        &report,
        &args.period,
        &scope.label,
    )))
}

fn run_recommend(args: &RecommendArgs) -> Result<Output> {
    let json_mode = args.as_json || args.format == "json";
    let budget = resolve_context_budget(args.context_budget.as_ref());

    let conn = open_store()?;
    let project_ids = bench_project_ids(&conn, args.project.as_deref());
    // NO scope — see the module docs. The CLI verb reads the whole store.
    let rec = recommend_from_history(
        &conn,
        &args.intent,
        args.size.as_deref(),
        args.language.as_deref(),
        None,
        project_ids.as_deref(),
        Weights::default(),
        CI_LEVEL,
    );
    drop(conn);

    if json_mode {
        let mut query = Map::new();
        query.insert("intent".to_owned(), Value::String(args.intent.clone()));
        query.insert("size".to_owned(), opt_str(args.size.as_deref()));
        query.insert("language".to_owned(), opt_str(args.language.as_deref()));
        query.insert("project".to_owned(), opt_str(args.project.as_deref()));
        // `truncated=False` is hardcoded and there is no `extra` — the single
        // result row is never packed, whatever `--context-budget` says.
        let envelope = build_envelope(
            MemoryCommand::from("benchmark-recommend"),
            query,
            vec![rec],
            budget,
            false,
            Map::new(),
        );
        return Ok(Output::ok(render_line(&envelope)));
    }

    Ok(Output::ok(render_recommend_text(&rec, args)))
}

// ── the helpers `cli.py` keeps beside the verbs ──────────────────────────────

/// `_bench_scope(period)`.
///
/// # Errors
/// A period outside the alias table, as Click's `BadParameter`.
pub fn bench_scope(period: &str) -> Result<Scope, UsageError> {
    let Some((_, spec)) = BENCH_PERIOD_ALIASES
        .iter()
        .find(|(alias, _)| *alias == period)
    else {
        return Err(UsageError::bad_parameter(
            "benchmark show",
            "[OPTIONS]",
            "--period",
            format!(
                "Invalid period {}. Valid: {}",
                stax_core::queries::paths::py_repr(period),
                BENCH_PERIOD_ALIASES
                    .iter()
                    .map(|(alias, _)| *alias)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    };
    // Every value in the table is one `parse_period` knows, so this cannot fail.
    Ok(parse_period(spec, Instant::now_utc()).unwrap_or_else(|_| unreachable!("alias table")))
}

/// `_bench_project_ids(conn, project)` — `None` for "every project".
#[must_use]
pub fn bench_project_ids(conn: &Connection, project: Option<&str>) -> Option<Vec<i64>> {
    // `if not project` — Python truthiness, so `--project ''` is "everything".
    let project = project.filter(|value| !value.is_empty())?;
    // `Path(project).name` — the last component, so a full log path resolves to
    // its directory name (which IS the slug). `Path("a/b/").name` is `"b"`.
    let slug = std::path::Path::new(project).file_name().map_or_else(
        || project.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let Ok(mut stmt) = conn.prepare("SELECT id FROM projects WHERE slug = ?") else {
        // `except Exception: return []` — a bad store scopes to NOTHING, which
        // is not the same as `None` ("everything").
        return Some(Vec::new());
    };
    let rows = stmt.query_map([&slug], |row| row.get::<_, i64>(0));
    match rows {
        Ok(rows) => Some(rows.filter_map(Result::ok).collect()),
        Err(_) => Some(Vec::new()),
    }
}

/// `_bench_pack(rows, budget)` — greedy, re-estimating the whole trial list.
#[must_use]
pub fn bench_pack(rows: &[Value], budget: i64) -> (Vec<Value>, bool) {
    // `if not budget or budget <= 0` — a zero or negative budget disables it.
    if budget <= 0 {
        return (rows.to_vec(), false);
    }
    let mut kept: Vec<Value> = Vec::new();
    for row in rows {
        let mut trial = kept.clone();
        trial.push(row.clone());
        // `estimate_tokens(trial) > budget and kept` — the LIST is measured, so
        // the brackets and separators are paid for. Not an incremental sum.
        if i64::try_from(mempyjson::estimate_tokens(&trial)).unwrap_or(i64::MAX) > budget
            && !kept.is_empty()
        {
            return (kept, true);
        }
        kept = trial;
    }
    (kept, false)
}

// ── the two text renderers ───────────────────────────────────────────────────

/// `_emit_benchmark_text(report, period=…, scope=…)`.
#[must_use]
pub fn render_show_text(report: &Value, period: &str, scope: &str) -> String {
    let verdict = report.get("verdict").cloned().unwrap_or(Value::Null);
    let coverage = report.get("coverage").cloned().unwrap_or(Value::Null);
    let get = |value: &Value, key: &str| value.get(key).cloned().unwrap_or(Value::Null);

    let mut out = format!("Benchmark — {scope} (period: {period})\n\n");

    // `if v.get("winning_model"):` — truthiness, so an empty-string model takes
    // the "insufficient evidence" leg.
    let winning = get(&verdict, "winning_model");
    if truthy(&winning) {
        // `cost_per_outcome_usd is not None` — a 0.0 still renders the clause.
        let cpo = verdict.get("cost_per_outcome_usd").and_then(Value::as_f64);
        let cpo_s = cpo.map_or_else(String::new, |value| {
            format!(" at ${value:.4}/successful outcome")
        });
        out.push_str(&format!(
            "Verdict: {}{cpo_s}\n",
            py_str(&get(&verdict, "headline"))
        ));
        let runner = get(&verdict, "runner_up");
        out.push_str(&format!(
            "  confidence: {}   runner-up: {}\n",
            py_str(&get(&verdict, "confidence")),
            if truthy(&runner) {
                py_str(&runner)
            } else {
                "—".to_owned()
            },
        ));
    } else {
        // `v.get("headline", "insufficient evidence")` — the DEFAULT fires only
        // when the key is ABSENT; a present `None` prints as `None`.
        let headline = verdict
            .get("headline")
            .map_or_else(|| "insufficient evidence".to_owned(), py_str);
        out.push_str(&format!("Verdict: {headline}\n"));
        for caveat in verdict
            .get("caveats")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(1)
        {
            out.push_str(&format!("  {}\n", py_str(caveat)));
        }
    }

    out.push('\n');
    out.push_str(&format!(
        "Coverage: {}/{} sessions scored · grade coverage {:.0}%\n",
        py_str(&coverage.get("sessions_scored").cloned().unwrap_or(0.into())),
        py_str(&coverage.get("sessions_total").cloned().unwrap_or(0.into())),
        coverage
            .get("grade_coverage")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            * 100.0,
    ));

    let strata = report
        .get("strata")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if strata.is_empty() {
        return out;
    }
    out.push('\n');
    out.push_str("Per-stratum (intent × size):\n");
    for stratum in &strata {
        let mut head = format!(
            "  {} × {}: {}",
            py_str(&get(stratum, "intent")),
            py_str(&get(stratum, "size_band")),
            py_str(&get(stratum, "cell_verdict")),
        );
        let winner = get(stratum, "winner");
        if truthy(&winner) {
            head.push_str(&format!(" — {} leads", py_str(&winner)));
        }
        out.push_str(&head);
        out.push('\n');
        for model in stratum
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let sr = model
                .get("success_rate")
                .and_then(|node| node.get("point"))
                .and_then(Value::as_f64);
            let sr_s = sr.map_or_else(
                || "n/a".to_owned(),
                |value| format!("{:.0}%", value * 100.0),
            );
            let cpo = model
                .get("cost_per_outcome")
                .and_then(|node| node.get("point"))
                .and_then(Value::as_f64);
            let cpo_s = cpo.map_or_else(|| "—".to_owned(), |value| format!("${value:.4}/outcome"));
            // `"" if m["qualified"] else "  [below sample floor]"` — truthiness
            // on the flag, so a missing key would be falsy here too.
            let floor = if truthy(&get(model, "qualified")) {
                ""
            } else {
                "  [below sample floor]"
            };
            out.push_str(&format!(
                "      {}: n={}, success {sr_s}, {cpo_s}, composite {:.2}{floor}\n",
                py_str(&get(model, "model")),
                py_str(&get(model, "n")),
                model
                    .get("composite")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            ));
        }
    }
    out
}

/// The `benchmark recommend` text block — four `click.echo`s at most.
#[must_use]
pub fn render_recommend_text(rec: &Value, args: &RecommendArgs) -> String {
    let mut out = format!(
        "Task: intent={} size={} language={}\n",
        args.intent,
        // `size or 'any'` — Python truthiness, so `--size ''` is also `any`.
        args.size
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("any"),
        args.language
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("any"),
    );
    let model = rec.get("recommended_model").cloned().unwrap_or(Value::Null);
    if truthy(&model) {
        out.push_str(&format!(
            "  → {}  (confidence: {}, basis: {})\n",
            py_str(&model),
            py_str(&rec.get("confidence").cloned().unwrap_or(Value::Null)),
            py_str(&rec.get("basis").cloned().unwrap_or(Value::Null)),
        ));
    } else {
        out.push_str("  → insufficient evidence\n");
    }
    let rationale = rec.get("rationale").cloned().unwrap_or(Value::Null);
    if truthy(&rationale) {
        out.push_str(&format!("  {}\n", py_str(&rationale)));
    }
    out
}

/// Python truthiness over a JSON node — `stax_etl`'s, re-exported for clarity.
fn truthy(value: &Value) -> bool {
    stax_etl::stats::pytext::py_truthy(value)
}

fn opt_str(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| Value::String(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use serde_json::json;

    use super::*;

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(subcommand)]
        verb: BenchmarkVerb,
    }

    fn show(argv: &[&str]) -> ShowArgs {
        let mut all = vec!["x", "show"];
        all.extend_from_slice(argv);
        match Wrap::try_parse_from(all).expect("parse").verb {
            BenchmarkVerb::Show(args) => args,
            BenchmarkVerb::Recommend(_) => panic!("expected show"),
        }
    }

    fn rec_args(argv: &[&str]) -> RecommendArgs {
        let mut all = vec!["x", "recommend"];
        all.extend_from_slice(argv);
        match Wrap::try_parse_from(all).expect("parse").verb {
            BenchmarkVerb::Recommend(args) => args,
            BenchmarkVerb::Show(_) => panic!("expected recommend"),
        }
    }

    #[test]
    fn the_defaults_are_the_decorators() {
        let args = show(&[]);
        assert_eq!(args.period, "all");
        assert_eq!(args.format, "text");
        assert!(args.project.is_none() && args.intent.is_none());
    }

    #[test]
    fn recommend_requires_an_intent() {
        assert!(Wrap::try_parse_from(["x", "recommend"]).is_err());
        assert!(Wrap::try_parse_from(["x", "recommend", "--intent", "fix"]).is_ok());
    }

    #[test]
    fn every_alias_resolves_and_an_unknown_one_is_a_bad_parameter() {
        for alias in ["today", "week", "7days", "month", "30days", "all"] {
            assert!(bench_scope(alias).is_ok(), "{alias}");
        }
        let err = bench_scope("yesterday").expect_err("unknown");
        let out = Output::usage(&err, "stackunderflow");
        assert_eq!(
            out.code, 2,
            "BadParameter is exit 2, not ClickException's 1"
        );
        assert_eq!(
            out.stderr,
            concat!(
                "Usage: stackunderflow benchmark show [OPTIONS]\n",
                "Try 'stackunderflow benchmark show --help' for help.\n",
                "\n",
                "Error: Invalid value for --period: Invalid period 'yesterday'. ",
                "Valid: today, week, 7days, month, 30days, all\n",
            ),
            "the alias list is in DICT order, not sorted"
        );
    }

    #[test]
    fn the_packer_measures_the_whole_list_not_the_row() {
        let rows: Vec<Value> = (0..5).map(|i| json!({"intent": "fix", "i": i})).collect();
        let (all, truncated) = bench_pack(&rows, 0);
        assert_eq!(all.len(), 5);
        assert!(!truncated, "a non-positive budget disables packing");

        let (kept, truncated) = bench_pack(&rows, 1);
        assert_eq!(kept.len(), 1, "the first row always survives");
        assert!(truncated);

        // The cut is decided by the estimate of the TRIAL LIST, not by a
        // running sum of per-row estimates. Pinned against the measure itself
        // rather than a magic number: at exactly `estimate_tokens([r0, r1])`
        // two rows fit, and one token below it only one does.
        let two: Vec<Value> = rows[..2].to_vec();
        let at = i64::try_from(mempyjson::estimate_tokens(&two)).expect("fits");
        assert_eq!(bench_pack(&rows, at).0.len(), 2, "`>` is strict");
        assert_eq!(bench_pack(&rows, at - 1).0.len(), 1);
        // …and the list measure is genuinely NOT the sum of the row measures:
        // the brackets and the separator are paid once, each row's `+1` once
        // per row. They coincide for some shapes, which is exactly why the
        // packer has to be transcribed rather than "equivalently" rewritten.
        let per_row: u64 = two.iter().map(mempyjson::estimate_tokens).sum();
        let whole = mempyjson::estimate_tokens(&two);
        assert!(
            whole <= per_row,
            "whole={whole} sum={per_row} — the list form is never the larger one \
             for these shapes, and only the list form is the reference's"
        );
    }

    #[test]
    fn the_show_text_is_the_reference_f_strings() {
        let report = json!({
            "verdict": {
                "winning_model": "claude-opus-5",
                "headline": "claude-opus-5 wins on composite",
                "cost_per_outcome_usd": 0.123_456,
                "confidence": "medium",
                "runner_up": Value::Null,
            },
            "coverage": {"sessions_scored": 12, "sessions_total": 40, "grade_coverage": 0.305},
            "strata": [{
                "intent": "fix", "size_band": "small", "cell_verdict": "clear",
                "winner": "claude-opus-5",
                "models": [
                    {"model": "claude-opus-5", "n": 9,
                     "success_rate": {"point": 0.777}, "cost_per_outcome": {"point": 1.5},
                     "composite": 0.815, "qualified": true},
                    {"model": "cheap", "n": 2,
                     "success_rate": {"point": Value::Null}, "cost_per_outcome": {"point": Value::Null},
                     "composite": 0.1, "qualified": false},
                ],
            }],
        });
        assert_eq!(
            render_show_text(&report, "month", "this month"),
            concat!(
                "Benchmark — this month (period: month)\n",
                "\n",
                "Verdict: claude-opus-5 wins on composite at $0.1235/successful outcome\n",
                "  confidence: medium   runner-up: —\n",
                "\n",
                "Coverage: 12/40 sessions scored · grade coverage 30%\n",
                "\n",
                "Per-stratum (intent × size):\n",
                "  fix × small: clear — claude-opus-5 leads\n",
                "      claude-opus-5: n=9, success 78%, $1.5000/outcome, composite 0.81\n",
                "      cheap: n=2, success n/a, —, composite 0.10  [below sample floor]\n",
            )
        );
    }

    #[test]
    fn no_winner_takes_the_caveat_leg_and_prints_at_most_one() {
        let report = json!({
            "verdict": {
                "winning_model": Value::Null,
                "headline": "insufficient evidence",
                "caveats": ["only 2 sessions scored", "and a second caveat"],
            },
            "coverage": {"sessions_scored": 2, "sessions_total": 2, "grade_coverage": 1.0},
            "strata": [],
        });
        let text = render_show_text(&report, "all", "all time");
        assert!(text.contains("Verdict: insufficient evidence\n  only 2 sessions scored\n"));
        assert!(!text.contains("and a second caveat"), "`[:1]`");
        assert!(
            !text.contains("Per-stratum"),
            "an empty strata list emits nothing, not a header"
        );
        assert!(text.ends_with("grade coverage 100%\n"));
    }

    #[test]
    fn an_absent_headline_key_takes_the_default_but_a_null_one_prints_none() {
        let absent = json!({"verdict": {}, "coverage": {}, "strata": []});
        assert!(
            render_show_text(&absent, "all", "all").contains("Verdict: insufficient evidence\n")
        );
        let null = json!({"verdict": {"headline": Value::Null}, "coverage": {}, "strata": []});
        assert!(
            render_show_text(&null, "all", "all").contains("Verdict: None\n"),
            "`dict.get(k, default)` does not fire on a present None"
        );
    }

    #[test]
    fn the_recommend_text_covers_both_legs() {
        let args = rec_args(&["--intent", "fix"]);
        let hit = json!({
            "recommended_model": "claude-opus-5",
            "confidence": "high", "basis": "12 sessions",
            "rationale": "wins on composite in fix × small",
        });
        assert_eq!(
            render_recommend_text(&hit, &args),
            concat!(
                "Task: intent=fix size=any language=any\n",
                "  → claude-opus-5  (confidence: high, basis: 12 sessions)\n",
                "  wins on composite in fix × small\n",
            )
        );
        let miss = json!({"recommended_model": Value::Null, "rationale": ""});
        assert_eq!(
            render_recommend_text(&miss, &args),
            "Task: intent=fix size=any language=any\n  → insufficient evidence\n",
            "an empty rationale is falsy and prints no line"
        );
    }

    #[test]
    fn size_and_language_echo_verbatim_when_given() {
        let args = rec_args(&["--intent", "fix", "--size", "large", "--language", "rust"]);
        let text = render_recommend_text(&json!({}), &args);
        assert!(text.starts_with("Task: intent=fix size=large language=rust\n"));
    }
}
