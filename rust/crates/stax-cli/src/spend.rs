//! `stax context-budget` and `stax yield` — `cli.py:3532`–`:3693`.
//!
//! Two read-only spend verbs whose whole bodies are one service call plus a
//! renderer. Neither writes; neither takes the `_ingest_options` decorator
//! (`context-budget` does not touch the store at all, and `yield` does not
//! declare it either — checked against `cli.py`, not assumed from its
//! neighbours).
//!
//! # `yield` shells out, and that is the contract
//!
//! `compute_yield` runs `git` per distinct session cwd. The port takes
//! [`stax_reports::yield_tracker::SystemGit`], which is `subprocess.run(["git",
//! "-C", cwd, …], timeout=5)` — same binary, same arguments, same five-second
//! budget. A parity row for `yield` therefore depends on the checkouts the store
//! points at still existing, which is why the rows below pin `--period all` on a
//! store whose cwds are all long gone: **both** implementations take the
//! `no_repo` arm, and they take it for the same reason rather than by luck.
//!
//! # `context-budget` reads `$HOME`, not the store
//!
//! `estimate_global_budget()` walks `~/.claude.json`, `~/.claude/skills/` and
//! the memory files. `Path.home()` is the axis, so the case rows run with the
//! cwd token `home` — the harness exports `HOME` into the case-local tree for
//! exactly this class of verb, and without it a row would read the maintainer's
//! real `~/.claude` and answer differently on every machine.

use anyhow::Result;
use clap::Args;
use serde_json::{Map, Value};
use stax_reports::context_budget::{
    ContextBudget, estimate_context_budget, estimate_global_budget,
};
use stax_reports::render;
use stax_reports::scope::Instant;
use stax_reports::yield_tracker::{
    self, SystemGit, YieldEntry, compute_yield, to_dicts, yield_summary,
};

use crate::click::Output;
use crate::reports::open_store;
use crate::status::{engine_for_cli, package_dir};

// ── context-budget ───────────────────────────────────────────────────────────

/// `stax context-budget`.
#[derive(Debug, Args)]
pub struct ContextBudgetArgs {
    /// Project directory (default: cwd)
    #[arg(long = "project", value_name = "DIRECTORY")]
    pub project_dir: Option<std::path::PathBuf>,
    /// Estimate the global budget only (~/.claude); ignore project files.
    #[arg(long = "global", action = clap::ArgAction::SetTrue)]
    pub use_global: bool,
    /// Output format
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// Run `context-budget`.
///
/// # Errors
/// Never in practice — every filesystem read inside the estimator is
/// best-effort, exactly as the reference's `try/except` walls are. The signature
/// is fallible so the dispatch arm matches its siblings.
pub fn run_context_budget(args: &ContextBudgetArgs) -> Result<Output> {
    let home = stax_reports::context_budget::os_home();
    let budget = if args.use_global {
        estimate_global_budget(&home)
    } else {
        // `Path(project_dir).resolve() if project_dir else Path.cwd()`.
        // `resolve()` is `canonicalize`; a path that does not exist still
        // resolves in Python 3.6+, so a failure here falls back to the input
        // rather than erroring — which is what `strict=False` means.
        let target = match &args.project_dir {
            Some(dir) => dir.canonicalize().unwrap_or_else(|_| dir.clone()),
            None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        };
        estimate_context_budget(&target, &home)
    };
    Ok(render_context_budget(&budget, &args.format))
}

/// The two output formats.
#[must_use]
pub fn render_context_budget(budget: &ContextBudget, format: &str) -> Output {
    if format == "json" {
        return Output::ok(format!("{}\n", render::render_json(&budget.to_dict())));
    }

    let mut out = String::new();
    out.push_str("Context budget (per-session estimate)\n");
    out.push_str(&format!("  heuristic: {}\n", budget.heuristic));
    out.push('\n');
    if budget.slices.is_empty() {
        out.push_str("  (no slices found)\n");
    } else {
        // `name_w = max(len(s.name) for s in slices)` then
        // `max(name_w, len("source"))` — a floor of SIX, from a header string
        // this renderer never actually prints. Faithful: the column is six wide
        // on a budget whose slice names are all shorter, and dropping the floor
        // would silently narrow it.
        let mut name_w = budget
            .slices
            .iter()
            .map(|slice| slice.name.chars().count())
            .max()
            .unwrap_or(0);
        name_w = name_w.max("source".len());
        for slice in &budget.slices {
            // `f"{s.tokens:>7,}"` — thousands-separated AND right-aligned in 7.
            let tokens = pad_left(&render::py_thousands(slice.tokens), 7);
            let source = slice.source_path.as_deref().unwrap_or("(fixed)");
            out.push_str(&format!(
                "  {}  {} tok   {}\n",
                pad_right(&slice.name, name_w),
                tokens,
                source
            ));
        }
    }
    out.push('\n');
    out.push_str(&format!(
        "  total: {} tokens\n",
        render::py_thousands(budget.total_tokens)
    ));
    out.push_str(&format!(
        "  cost per session: ${:.4}\n",
        budget.cost_per_session_usd
    ));
    out.push_str(&format!(
        "  estimated monthly cost: ${:.2}\n",
        budget.estimated_monthly_cost_usd
    ));
    if budget.total_tokens > 20_000 {
        // `click.secho(…, fg="yellow")` — colour is stripped off a pipe, so the
        // bytes are the text. The `⚠` is U+26A0 followed by TWO spaces.
        out.push_str(
            "  ⚠  budget exceeds 20k tokens — consider trimming MCP servers, \
skills, or memory files.\n",
        );
    }
    Output::ok(out)
}

/// `f"{text:<width}"` — pad right with spaces, never truncate.
fn pad_right(text: &str, width: usize) -> String {
    let used = text.chars().count();
    let mut out = String::from(text);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

/// `f"{text:>width}"` — pad left with spaces, never truncate.
fn pad_left(text: &str, width: usize) -> String {
    let used = text.chars().count();
    let mut out = String::with_capacity(width.max(used));
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out.push_str(text);
    out
}

// ── yield ────────────────────────────────────────────────────────────────────

/// `stax yield`.
#[derive(Debug, Args)]
pub struct YieldArgs {
    /// Period to analyse.
    #[arg(short = 'p', long = "period", value_name = "PERIOD", default_value = "month",
          value_parser = ["today", "week", "month", "all", "7days", "30days"])]
    pub period: String,
    /// Filter by project slug (repeatable). Slugs start with `-`, hence
    /// `allow_hyphen_values`.
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub include: Vec<String>,
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
    /// `--ingest` / `--auto-ingest`.
    #[command(flatten)]
    pub ingest: crate::reports::IngestFlags,
}

/// Run `yield`.
///
/// # Errors
/// A missing store, a SQLite failure, the unported refresh pass, or a period
/// `parse_period` rejects (unreachable through the CLI — the `click.Choice`
/// allow-list is a strict subset of what `normalize_period` accepts).
pub fn run_yield(args: &YieldArgs) -> Result<Output> {
    let conn = open_store()?;
    crate::reports::guard_refresh(&conn, &args.ingest)?;
    let engine = engine_for_cli(&package_dir())?;
    let project_filter = (!args.include.is_empty()).then_some(args.include.as_slice());
    let cap = yield_tracker::max_sessions_per_project(&|key| std::env::var(key).ok());
    let entries = compute_yield(
        &conn,
        &args.period,
        project_filter,
        cap,
        Instant::now_utc(),
        &SystemGit,
        &engine,
    )
    .map_err(|err| anyhow::anyhow!("{err:?}"))?;
    Ok(render_yield(&entries, &args.period, &args.format))
}

/// The two output formats.
///
/// `summary` is computed on the **unsorted** entries and the body renders the
/// cost-sorted copy. Both orders exist in the Python and the distinction is
/// load-bearing for the float sums: Neumaier over a different order is a
/// different last bit.
#[must_use]
pub fn render_yield(entries: &[YieldEntry], period: &str, format: &str) -> Output {
    let summary = yield_summary(entries);
    // `sorted(entries, key=lambda e: e.cost_usd, reverse=True)` — Python's sort
    // is STABLE, and `reverse=True` reverses the comparison, not the list, so
    // ties keep their original relative order.
    let mut sorted: Vec<&YieldEntry> = entries.iter().collect();
    sorted.sort_by(|left, right| {
        right
            .cost_usd
            .partial_cmp(&left.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if format == "json" {
        let owned: Vec<YieldEntry> = sorted.into_iter().cloned().collect();
        let mut body = Map::new();
        body.insert("period".to_owned(), Value::String(period.to_owned()));
        body.insert("summary".to_owned(), summary);
        body.insert("entries".to_owned(), Value::Array(to_dicts(&owned)));
        return Output::ok(format!("{}\n", render::render_json(&Value::Object(body))));
    }

    if sorted.is_empty() {
        return Output::ok(format!("No sessions found for period '{period}'.\n"));
    }

    let count = |key: &str| summary.get(key).and_then(Value::as_i64).unwrap_or(0);
    let money = |key: &str| summary.get(key).and_then(Value::as_f64).unwrap_or(0.0);

    let mut out = String::new();
    out.push_str(&format!("Yield analysis — period: {period}\n"));
    for (label, count_key, cost_key) in [
        ("productive:", "productive", "productive_cost"),
        ("reverted:  ", "reverted", "reverted_cost"),
        ("abandoned: ", "abandoned", "abandoned_cost"),
        ("no_repo:   ", "no_repo", "no_repo_cost"),
        ("total:     ", "total", "total_cost"),
    ] {
        // `f"  {label} {n:>4d}  (${cost:.2f})"` — the labels are padded in the
        // SOURCE f-strings, not by a formatter, so their trailing spaces are
        // literal and are reproduced literally.
        out.push_str(&format!(
            "  {label} {:>4}  (${:.2})\n",
            count(count_key),
            money(cost_key)
        ));
    }
    out.push('\n');
    out.push_str("Top sessions by cost:\n");
    out.push_str(&format!(
        "  {:<11}  {:>8}  {:<28}  SESSION\n",
        "CLASS", "COST", "PROJECT"
    ));
    for entry in sorted.iter().take(20) {
        // `{(e.project_slug or '')[:28]:<28}` — sliced FIRST, then padded, so a
        // 40-character slug becomes exactly 28 and a 3-character one becomes 28.
        let project: String = entry.project_slug.chars().take(28).collect();
        let session: String = entry.session_id.chars().take(36).collect();
        out.push_str(&format!(
            "  {:<11}  ${:>7.2}  {}  {}\n",
            entry.classification.as_str(),
            entry.cost_usd,
            pad_right(&project, 28),
            session
        ));
    }
    out.push('\n');
    out.push_str(
        "  note: yield is correlated by time, not by content — a commit \
within 24h is credited to the session even if unrelated.\n",
    );
    Output::ok(out)
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use stax_reports::context_budget::ContextSlice;
    use stax_reports::yield_tracker::Classification;

    use super::*;

    #[derive(clap::Parser)]
    struct WrapBudget {
        #[command(flatten)]
        args: ContextBudgetArgs,
    }

    #[derive(clap::Parser)]
    struct WrapYield {
        #[command(flatten)]
        args: YieldArgs,
    }

    fn budget(slices: Vec<ContextSlice>) -> ContextBudget {
        let total: i64 = slices.iter().map(|slice| slice.tokens).sum();
        #[allow(clippy::cast_precision_loss, reason = "token counts are small")]
        let cost = (total as f64 / 1e6) * 3.0;
        ContextBudget::new(total, slices, cost, cost * 100.0)
    }

    #[test]
    fn an_empty_budget_prints_the_no_slices_line() {
        let out = render_context_budget(&budget(Vec::new()), "text").stdout;
        assert_eq!(
            out,
            concat!(
                "Context budget (per-session estimate)\n",
                "  heuristic: len(text) // 4; per-MCP-server 200 + 50/tool\n",
                "\n",
                "  (no slices found)\n",
                "\n",
                "  total: 0 tokens\n",
                "  cost per session: $0.0000\n",
                "  estimated monthly cost: $0.00\n",
            )
        );
    }

    #[test]
    fn the_name_column_has_a_floor_of_six_from_a_header_that_is_never_printed() {
        // `max(name_w, len("source"))`. Every slice name here is 3 characters,
        // so a port without the floor would emit a 3-wide column.
        let out =
            render_context_budget(&budget(vec![ContextSlice::new("abc", 12, None)]), "text").stdout;
        assert!(out.contains("  abc          12 tok   (fixed)\n"), "{out}");
    }

    #[test]
    fn the_token_column_is_thousands_separated_and_right_aligned_in_seven() {
        let out = render_context_budget(
            &budget(vec![ContextSlice::new(
                "memory:CLAUDE.md",
                1_234_567,
                Some("/x/CLAUDE.md".to_owned()),
            )]),
            "text",
        )
        .stdout;
        assert!(out.contains("1,234,567 tok   /x/CLAUDE.md"), "{out}");
        // …and a short one is padded to seven.
        let short = render_context_budget(
            &budget(vec![ContextSlice::new("system_prompt", 42, None)]),
            "text",
        )
        .stdout;
        assert!(
            short.contains("system_prompt       42 tok   (fixed)"),
            "{short}"
        );
    }

    #[test]
    fn the_twenty_thousand_token_warning_is_strictly_greater_than() {
        let at = render_context_budget(&budget(vec![ContextSlice::new("s", 20_000, None)]), "text")
            .stdout;
        assert!(!at.contains('⚠'), "20_000 exactly does NOT warn");
        let over =
            render_context_budget(&budget(vec![ContextSlice::new("s", 20_001, None)]), "text")
                .stdout;
        assert!(over.contains("  ⚠  budget exceeds 20k tokens"), "{over}");
    }

    #[test]
    fn the_json_body_is_the_dataclass_dict_plus_clicks_newline() {
        let out = render_context_budget(&budget(Vec::new()), "json").stdout;
        assert!(out.starts_with("{\n  \"total_tokens\": 0,"), "{out}");
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn the_global_flag_and_the_project_option_both_parse() {
        let parsed = WrapBudget::try_parse_from(["x", "--global"]).expect("global");
        assert!(parsed.args.use_global);
        assert!(parsed.args.project_dir.is_none());
        let parsed = WrapBudget::try_parse_from(["x", "--project", "/tmp/p"]).expect("project");
        assert_eq!(
            parsed.args.project_dir.as_deref(),
            Some(std::path::Path::new("/tmp/p"))
        );
        assert_eq!(parsed.args.format, "text");
    }

    #[test]
    fn context_budget_declares_no_ingest_flags() {
        // `cli.py`'s `context_budget_cmd` is NOT decorated with
        // `_ingest_options`, unlike every neighbour in the file. Accepting the
        // flag would be a wider surface than the reference.
        assert!(WrapBudget::try_parse_from(["x", "--no-auto-ingest"]).is_err());
    }

    #[test]
    fn the_yield_period_allow_list_is_the_click_choice() {
        for period in ["today", "week", "month", "all", "7days", "30days"] {
            assert!(
                WrapYield::try_parse_from(["x", "-p", period]).is_ok(),
                "{period}"
            );
        }
        assert!(WrapYield::try_parse_from(["x", "-p", "yesterday"]).is_err());
        assert_eq!(
            WrapYield::try_parse_from(["x"]).expect("bare").args.period,
            "month"
        );
    }

    fn entry(id: &str, slug: &str, cost: f64, class: Classification) -> YieldEntry {
        YieldEntry {
            session_id: id.to_owned(),
            project_slug: slug.to_owned(),
            cwd: String::new(),
            started_at: "2026-07-01T00:00:00Z".to_owned(),
            cost_usd: cost,
            classification: class,
            follow_commit_sha: None,
            follow_commit_msg: None,
            follow_commit_age_hours: None,
        }
    }

    #[test]
    fn an_empty_yield_prints_one_line_naming_the_period() {
        assert_eq!(
            render_yield(&[], "month", "text").stdout,
            "No sessions found for period 'month'.\n"
        );
        // …and the JSON form is still the full three-key envelope.
        let json = render_yield(&[], "month", "json").stdout;
        assert!(json.contains("\"entries\": []"), "{json}");
        assert!(json.contains("\"total\": 0"), "{json}");
    }

    #[test]
    fn the_summary_labels_carry_their_literal_trailing_spaces() {
        let out = render_yield(
            &[entry("s1", "-p", 1.5, Classification::Productive)],
            "month",
            "text",
        )
        .stdout;
        assert!(out.contains("  productive:    1  ($1.50)\n"), "{out}");
        assert!(out.contains("  reverted:      0  ($0.00)\n"), "{out}");
        assert!(out.contains("  no_repo:       0  ($0.00)\n"), "{out}");
        assert!(out.contains("  total:         1  ($1.50)\n"), "{out}");
    }

    #[test]
    fn the_rows_are_cost_descending_and_the_project_column_is_sliced_then_padded() {
        let long = "-".repeat(40);
        let out = render_yield(
            &[
                entry("cheap", "-a", 1.0, Classification::Abandoned),
                entry("dear", &long, 9.0, Classification::Productive),
            ],
            "all",
            "text",
        )
        .stdout;
        let dear = out.find("dear").expect("dear row");
        let cheap = out.find("cheap").expect("cheap row");
        assert!(dear < cheap, "cost descending");
        // 40 characters in, exactly 28 out — sliced, not padded to 40.
        assert!(out.contains(&"-".repeat(28)), "{out}");
        assert!(!out.contains(&"-".repeat(29)), "the slice caps at 28");
    }

    #[test]
    fn only_the_top_twenty_rows_are_listed_but_the_summary_counts_them_all() {
        let entries: Vec<YieldEntry> = (0..25)
            .map(|index| {
                entry(
                    &format!("s{index:02}"),
                    "-p",
                    f64::from(index),
                    Classification::Productive,
                )
            })
            .collect();
        let out = render_yield(&entries, "all", "text").stdout;
        assert!(out.contains("  total:        25  ($300.00)\n"), "{out}");
        assert_eq!(
            out.matches("productive  ").count(),
            20,
            "`sorted_entries[:20]` — the table is capped, the summary is not"
        );
        assert!(out.contains("s24"), "the dearest is listed");
        assert!(!out.contains("s04"), "the cheapest five are not");
    }

    #[test]
    fn the_note_is_the_last_line_and_the_dash_is_an_em_dash() {
        let out = render_yield(
            &[entry("s", "-p", 0.0, Classification::NoRepo)],
            "all",
            "text",
        )
        .stdout;
        assert!(
            out.ends_with(
                "  note: yield is correlated by time, not by content — a commit \
within 24h is credited to the session even if unrelated.\n"
            ),
            "{out}"
        );
    }
}
