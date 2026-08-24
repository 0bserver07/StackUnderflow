//! `stax optimize` — `cli.py:3314`–`:3389`.
//!
//! Two service calls (`find_waste` + `find_patterns`, both already ported into
//! `stax_reports::optimize`) and a plain `click.echo` block. No Rich, no table:
//! every line here is an f-string, which is why this is the cheapest of the
//! tranche-3 remainder.
//!
//! # The CLI payload is NOT the route payload
//!
//! `GET /api/optimize` returns seven keys (`scope`, `waste`, `patterns`,
//! `total_waste_usd`, `anomalies`, `warnings`, `cache`); the verb returns
//! **three** — `scope`, `waste`, `patterns` — and computes no total, runs no
//! anomaly pass and emits no mart warning. Reproduced as the three keys it is;
//! reusing `routes/optimize.rs`'s builder would have been the obvious
//! refactor and would have printed four fields the reference never prints.
//!
//! # `--project` is spelled `include`, and it feeds BOTH calls differently
//!
//! `find_waste(include=…, exclude=…)` filters the project list on both ends;
//! `find_patterns(project_filter=…)` takes only the include half — `--exclude`
//! has no effect at all on the structural detectors. That asymmetry is the
//! reference's, and it is preserved.
//!
//! # DIV-371 — `QAService()` creates `qa_pairs.db`; this port does not
//!
//! `find_waste` reaches the Q&A pair store through `_qa_service_factory()`,
//! whose `__init__` runs `mkdir(parents=True)` + `_ensure_schema()`. So on a
//! home with no `qa_pairs.db`, `stax optimize` **materialises one**
//! (a ~32 KB database with three empty tables) before answering "no waste". The
//! port opens it read-only through [`stax_reports::optimize::find_waste`]'s
//! `Option<&Path>` and treats a missing file as "every project totals 0", which
//! is the same answer with no file written. Both shared parity states already
//! carry `qa_pairs.db`, so the matrix rows run over the identical read path;
//! the divergence is on a fresh `@home` only, and it is filed rather than
//! papered over — see the ledger entry.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Map, Value};

use clap::Args;
use stax_reports::optimize::{Finding, FsRoots, find_patterns, find_waste, lookback_iso};
use stax_reports::render::{self, py_thousands};
use stax_reports::scope::{Instant, Scope, parse_period};

use crate::click::Output;
use crate::reports::{IngestFlags, click_exception, guard_refresh, open_store};
use crate::status::{engine_for_cli, package_dir};

/// `stax optimize`.
#[derive(Debug, Args)]
pub struct OptimizeArgs {
    /// The window. Free text, not a `Choice` — `parse_period` validates it and
    /// raises a `ClickException` (exit 1), where a `Choice` would be exit 2.
    #[arg(
        short = 'p',
        long = "period",
        value_name = "PERIOD",
        default_value = "30days"
    )]
    pub period: String,
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
    /// Include only this project slug (repeatable).
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub include: Vec<String>,
    /// Exclude this project slug (repeatable).
    #[arg(long = "exclude", value_name = "EXCLUDE", allow_hyphen_values = true)]
    pub exclude: Vec<String>,
    /// `--ingest` / `--auto-ingest`.
    #[command(flatten)]
    pub ingest: IngestFlags,
}

/// `QA_DB_PATH` — `app_dir() / "qa_pairs.db"`, the derivation
/// `services/qa_service.py:23` uses and `routes/optimize.rs` mirrors.
#[must_use]
pub fn qa_db_path() -> PathBuf {
    stax_core::settings::app_dir().join("qa_pairs.db")
}

/// Run `optimize`.
///
/// # Errors
/// A missing store (DIV-239), a SQLite failure, or the unported refresh pass
/// (DIV-238). An unknown period is NOT an error — it is a `ClickException`
/// rendered into the returned [`Output`] at exit 1.
pub fn run_optimize(args: &OptimizeArgs) -> Result<Output> {
    let scope = match parse_period(&args.period, Instant::now_utc()) {
        Ok(scope) => scope,
        Err(message) => return Ok(click_exception(&message)),
    };

    let conn = open_store()?;
    guard_refresh(&conn, &args.ingest)?;
    let engine = engine_for_cli(package_dir().as_deref())?;

    // `list(include) or None` twice over — an empty repeatable option is `None`
    // ("keep everything"), never `[]` ("keep nothing"). `find_waste` tests
    // `is not None`, so the distinction is load-bearing on both arguments.
    let include = (!args.include.is_empty()).then_some(args.include.as_slice());
    let exclude = (!args.exclude.is_empty()).then_some(args.exclude.as_slice());

    let qa_db = qa_db_path();
    let waste = find_waste(&conn, Some(qa_db.as_path()), &scope, include, exclude)?;
    let patterns = find_patterns(
        &conn,
        &engine,
        &FsRoots::from_env(),
        Some(&scope),
        // `project_filter=list(include) or None` — `exclude` is NOT passed.
        include,
        &lookback_iso(30),
    )?;

    Ok(emit(&scope, &waste, &patterns, &args.format))
}

/// The render half, split out so every branch is testable without a store.
#[must_use]
pub fn emit(scope: &Scope, waste: &[Value], patterns: &[Finding], format: &str) -> Output {
    if format == "json" {
        let mut payload = Map::new();
        payload.insert("scope".to_owned(), Value::from(scope.label.clone()));
        payload.insert("waste".to_owned(), Value::Array(waste.to_vec()));
        payload.insert(
            "patterns".to_owned(),
            Value::Array(patterns.iter().map(Finding::to_dict).collect()),
        );
        return Output::ok(format!(
            "{}\n",
            render::render_json(&Value::Object(payload))
        ));
    }

    // `if not waste and not patterns` — Python truthiness on two lists.
    if waste.is_empty() && patterns.is_empty() {
        return Output::ok(format!(
            "No waste or structural patterns found in {}.\n",
            scope.label
        ));
    }

    let mut out = format!("Waste report — {}\n\n", scope.label);

    if !waste.is_empty() {
        out.push_str("Q&A loops:\n");
        for row in waste {
            out.push_str(&format!(
                "  {}: {} looped pair(s)\n",
                row.get("project")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                // `{row['looped_pairs']}` is `str(int)` — NO thousands
                // separator here, unlike the token line below.
                row.get("looped_pairs").and_then(Value::as_i64).unwrap_or(0),
            ));
            if let Some(samples) = row.get("sample_questions").and_then(Value::as_array) {
                for question in samples {
                    out.push_str(&format!(
                        "    - {}\n",
                        question.as_str().unwrap_or_default()
                    ));
                }
            }
        }
        out.push('\n');
    }

    if !patterns.is_empty() {
        out.push_str("Structural patterns:\n");
        for finding in patterns {
            // `f.severity.upper()` — ASCII upper on `"high"`/`"medium"`/`"low"`.
            out.push_str(&format!(
                "  [{}] {}: {}\n",
                finding.severity.to_uppercase(),
                finding.pattern_id,
                finding.title,
            ));
            out.push_str(&format!("      {}\n", finding.description));
            // `is not None`, not truthiness: a detector reporting **0** wasted
            // tokens still prints the line.
            if let Some(tokens) = finding.estimated_waste_tokens {
                out.push_str(&format!("      ~{} wasted tokens\n", py_thousands(tokens)));
            }
            out.push_str(&format!("      fix: {}\n", finding.suggested_fix));
        }
    }

    Output::ok(out)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use serde_json::json;

    use super::*;

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        args: OptimizeArgs,
    }

    fn scope_of(label: &str) -> Scope {
        let mut scope = parse_period("30days", Instant::from_parts(2026, 8, 1, 12, 0, 0, 0))
            .expect("30days parses");
        scope.label = label.to_owned();
        scope
    }

    fn finding(tokens: Option<i64>) -> Finding {
        Finding {
            pattern_id: "bloated_claude_md",
            severity: "high",
            title: "CLAUDE.md is 12,000 tokens".to_owned(),
            description: "Every request pays for it.".to_owned(),
            affected_count: 1,
            suggested_fix: "Trim it.",
            estimated_waste_tokens: tokens,
            estimated_waste_usd: None,
            details: json!({}),
        }
    }

    #[test]
    fn the_defaults_are_the_decorators() {
        let parsed = Wrap::try_parse_from(["x"]).expect("bare parse");
        assert_eq!(parsed.args.period, "30days");
        assert_eq!(parsed.args.format, "text");
        assert!(parsed.args.include.is_empty());
        assert!(parsed.args.exclude.is_empty());
        assert!(parsed.args.ingest.auto());
    }

    #[test]
    fn the_period_is_free_text_so_a_bad_one_reaches_the_body() {
        // `-p` has no `click.Choice`, so `optimize -p nope` is a ClickException
        // at exit 1 — NOT clap's exit-2 parse error. One decorator apart from
        // `compare`, and a whole exit code apart on the wire.
        let parsed = Wrap::try_parse_from(["x", "-p", "nope"]).expect("parses");
        assert_eq!(parsed.args.period, "nope");
        let out = click_exception(
            &parse_period("nope", Instant::from_parts(2026, 8, 1, 0, 0, 0, 0))
                .expect_err("unknown"),
        );
        assert_eq!(out.code, 1);
        assert_eq!(
            out.stderr,
            "Error: Unknown period 'nope'. Valid: today, 7days, 30days, month, all\n"
        );
    }

    #[test]
    fn the_empty_report_is_one_line() {
        assert_eq!(
            emit(&scope_of("last 30 days"), &[], &[], "text").stdout,
            "No waste or structural patterns found in last 30 days.\n"
        );
    }

    #[test]
    fn the_json_branch_has_exactly_three_keys_in_literal_order() {
        let out = emit(&scope_of("last 30 days"), &[], &[], "json").stdout;
        assert_eq!(
            out, "{\n  \"scope\": \"last 30 days\",\n  \"waste\": [],\n  \"patterns\": []\n}\n",
            "the verb's payload is three keys — the ROUTE's is seven"
        );
    }

    #[test]
    fn the_waste_block_indents_the_samples_by_four() {
        let waste = vec![json!({
            "project": "-home-me-proj",
            "looped_pairs": 4200,
            "sample_questions": ["why is it slow", "still slow"],
        })];
        let out = emit(&scope_of("last 30 days"), &waste, &[], "text").stdout;
        assert_eq!(
            out,
            concat!(
                "Waste report — last 30 days\n",
                "\n",
                "Q&A loops:\n",
                "  -home-me-proj: 4200 looped pair(s)\n",
                "    - why is it slow\n",
                "    - still slow\n",
                "\n",
            ),
            "`looped_pairs` is bare `str(int)` — no `:,` — and the block ends with a blank line"
        );
    }

    #[test]
    fn the_pattern_block_prints_the_token_line_only_when_it_is_not_none() {
        let with = emit(&scope_of("all"), &[], &[finding(Some(12_345))], "text").stdout;
        assert_eq!(
            with,
            concat!(
                "Waste report — all\n",
                "\n",
                "Structural patterns:\n",
                "  [HIGH] bloated_claude_md: CLAUDE.md is 12,000 tokens\n",
                "      Every request pays for it.\n",
                "      ~12,345 wasted tokens\n",
                "      fix: Trim it.\n",
            )
        );
        let without = emit(&scope_of("all"), &[], &[finding(None)], "text").stdout;
        assert!(!without.contains("wasted tokens"));
        // …but ZERO tokens still prints, because the test is `is not None`.
        let zero = emit(&scope_of("all"), &[], &[finding(Some(0))], "text").stdout;
        assert!(zero.contains("      ~0 wasted tokens\n"), "{zero}");
    }

    #[test]
    fn both_blocks_together_keep_the_separating_blank_line() {
        let waste = vec![json!({
            "project": "p", "looped_pairs": 1, "sample_questions": [],
        })];
        let out = emit(&scope_of("all"), &waste, &[finding(None)], "text").stdout;
        assert!(
            out.contains("  p: 1 looped pair(s)\n\nStructural patterns:\n"),
            "{out}"
        );
    }

    #[test]
    fn the_qa_db_sits_beside_the_store() {
        let path = qa_db_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("qa_pairs.db")
        );
        assert_eq!(
            path.parent(),
            Some(stax_core::settings::app_dir()).as_deref()
        );
    }
}
