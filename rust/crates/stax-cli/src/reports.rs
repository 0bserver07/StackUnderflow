//! `stax report | today | month` — `cli.py:3133`–`:3229`.
//!
//! Three verbs, one body. Each parses a period, opens the store, calls
//! [`stax_reports::aggregate::build_report`] and hands the result to
//! `_emit_report`, which is `render_json` or `render_text` — the Rich table.
//! `today` and `month` are `report` with the period nailed down and the
//! `--provider` option removed; they are not aliases (Click builds three
//! commands) and their `--help` differs, so they are three clap structs.
//!
//! # The shared decorator
//!
//! `_ingest_options` attaches `--ingest` and `--auto-ingest/--no-auto-ingest` to
//! every data command. [`IngestFlags`] is that decorator, flattened into each
//! command; the refresh *decision* is [`crate::status::ensure_fresh_decision`]'s
//! and the refresh *pass* is still unported (DIV-238) — the port fails loudly
//! rather than silently answering from a stale store, because a skipped refresh
//! is a changed answer.
//!
//! # `--provider` is a stub on BOTH sides
//!
//! `report`'s `--provider` is a `click.Choice` built from the live adapter
//! registry, and the body's entire use of it is `_ = provider  # stub: wired in
//! Plan C`. So the flag validates and then does nothing. That is ported exactly:
//! the choice list is [`stax_adapters::registry::registered_names`] sorted (the
//! same `sorted(a.name for a in registered())` the decorator runs), and the
//! value is dropped. A port that quietly implemented the filter would answer
//! differently from the reference on every `--provider claude` run.

use anyhow::{Context, Result};
use clap::Args;
use rusqlite::Connection;
use stax_reports::aggregate::{Report, build_report};
use stax_reports::render;
use stax_reports::scope::{Instant, Scope, parse_period};

use crate::click::Output;
use crate::status::{engine_for_cli, ensure_fresh_decision, package_dir};

/// `_ingest_options` — the two flags every data command carries.
#[derive(Debug, Args, Clone)]
pub struct IngestFlags {
    /// Force a fresh ingest+backfill pass before running the command. Useful
    /// when 'stackunderflow start' is not active.
    #[arg(long = "ingest", action = clap::ArgAction::SetTrue)]
    pub do_ingest: bool,
    /// Refresh the store automatically when its newest event is older than the
    /// staleness threshold. Default on. Disable with --no-auto-ingest.
    #[arg(long = "auto-ingest", action = clap::ArgAction::SetTrue,
          overrides_with = "no_auto_ingest")]
    pub auto_ingest: bool,
    /// The `--no-auto-ingest` half of Click's `--auto-ingest/--no-auto-ingest`.
    #[arg(long = "no-auto-ingest", action = clap::ArgAction::SetTrue,
          overrides_with = "auto_ingest")]
    pub no_auto_ingest: bool,
}

impl IngestFlags {
    /// The effective `auto_ingest`, honouring Click's last-wins semantics.
    #[must_use]
    pub const fn auto(&self) -> bool {
        !self.no_auto_ingest
    }
}

/// `stax report`.
#[derive(Debug, Args)]
pub struct ReportArgs {
    /// Period: today, 7days, 30days, month, all
    #[arg(
        short = 'p',
        long = "period",
        value_name = "PERIOD",
        default_value = "7days"
    )]
    pub period: String,
    /// Output format
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
    // `allow_hyphen_values`: EVERY project slug starts with `-` (the slug is the
    // absolute path with each non-alphanumeric character replaced), so
    // `--project -Users-…` is the normal call and clap's default would read it
    // as an unknown flag. Click has no such rule for an option's VALUE.
    /// Include only these project dir names (repeatable)
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub include: Vec<String>,
    /// Exclude these project dir names (repeatable)
    #[arg(long = "exclude", value_name = "EXCLUDE", allow_hyphen_values = true)]
    pub exclude: Vec<String>,
    /// Provider filter (stub — wired in Plan C)
    #[arg(long = "provider", value_name = "PROVIDER", default_value = "all",
          value_parser = provider_choices())]
    pub provider: String,
    /// `--ingest` / `--auto-ingest`.
    #[command(flatten)]
    pub ingest: IngestFlags,
}

/// `stax today` and `stax month` — the same body with the period fixed.
///
/// One struct for two commands: their parameter lists are identical
/// (`--format`, `--project`, `--exclude`, plus `_ingest_options`) and only the
/// period and the docstring differ. Click builds two commands; clap gets two
/// variants pointing at one `Args`, which is the same surface.
#[derive(Debug, Args)]
pub struct PeriodArgs {
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
    /// Filter by project slug (repeatable).
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub include: Vec<String>,
    /// Exclude this project slug (repeatable).
    #[arg(long = "exclude", value_name = "EXCLUDE", allow_hyphen_values = true)]
    pub exclude: Vec<String>,
    /// `--ingest` / `--auto-ingest`.
    #[command(flatten)]
    pub ingest: IngestFlags,
}

/// `["all", *sorted(a.name for a in registered())]` — the live registry.
///
/// Sourced from [`stax_adapters::registry::PYTHON_WALK_ORDER`], the `'static`
/// spelling of the same twenty names `registered()` yields. clap's
/// `PossibleValue` needs `'static`, and `registered_names()` hands back owned
/// `String`s; leaking them would work and would also make the choice list
/// invisible to a reader. A test below asserts the two sources agree, so this
/// cannot silently go stale when an adapter lands.
fn provider_choices() -> clap::builder::PossibleValuesParser {
    let mut names: Vec<&'static str> = stax_adapters::registry::PYTHON_WALK_ORDER.to_vec();
    names.sort_unstable();
    let mut values = vec!["all"];
    values.extend(names);
    clap::builder::PossibleValuesParser::new(values)
}

/// Run `report`.
///
/// # Errors
/// An unknown period (Click's `ClickException`, exit 1), a missing store, a
/// SQLite failure, or the unported refresh pass.
pub fn run_report(args: &ReportArgs) -> Result<Output> {
    // `_ = provider  # stub: wired in Plan C`. Named so the drop is deliberate.
    let _stub = &args.provider;
    let scope = match parse_period(&args.period, Instant::now_utc()) {
        Ok(scope) => scope,
        Err(message) => return Ok(click_exception(&message)),
    };
    emit(
        &args.ingest,
        &scope,
        &args.include,
        &args.exclude,
        &args.format,
    )
}

/// Run `today`.
///
/// # Errors
/// As [`run_report`], minus the period parse — `parse_period("today")` cannot
/// fail, so the reference has no `try` here either.
pub fn run_today(args: &PeriodArgs) -> Result<Output> {
    run_fixed_period(args, "today")
}

/// Run `month`.
///
/// # Errors
/// As [`run_today`].
pub fn run_month(args: &PeriodArgs) -> Result<Output> {
    run_fixed_period(args, "month")
}

fn run_fixed_period(args: &PeriodArgs, spec: &str) -> Result<Output> {
    let scope = parse_period(spec, Instant::now_utc())
        .map_err(|message| anyhow::anyhow!("{message}"))
        .with_context(|| format!("parse_period({spec})"))?;
    emit(
        &args.ingest,
        &scope,
        &args.include,
        &args.exclude,
        &args.format,
    )
}

fn emit(
    ingest: &IngestFlags,
    scope: &Scope,
    include: &[String],
    exclude: &[String],
    format: &str,
) -> Result<Output> {
    let report = build(ingest, scope, include, exclude)?;
    Ok(emit_report(&report, format))
}

/// `_emit_report(report, fmt)`.
///
/// `render_json` goes through `click.echo`, which appends the newline;
/// `render_text` writes through Rich, which already ends every line.
#[must_use]
pub fn emit_report(report: &Report, format: &str) -> Output {
    if format == "json" {
        return Output::ok(format!("{}\n", render::render_json(&report.to_value())));
    }
    Output::ok(render::render_text(report, console_width()))
}

/// `Console.width` — `$COLUMNS`, else 80.
///
/// Rich reads `COLUMNS` through `os.environ` when the file is not a terminal.
/// The parity harness pins it to 80 on both sides; a human's terminal sets it
/// per-shell. Read here rather than in `stax-reports` so the layout engine stays
/// a pure function (the injection law).
#[must_use]
pub fn console_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|width| *width > 0)
        .unwrap_or(render::DEFAULT_CONSOLE_WIDTH)
}

/// Open the store, honour the refresh decision, build the report.
///
/// # Errors
/// A missing store (DIV-239), a SQLite failure, or the unported refresh pass
/// (DIV-238).
pub fn build(
    ingest: &IngestFlags,
    scope: &Scope,
    include: &[String],
    exclude: &[String],
) -> Result<Report> {
    let conn = open_store()?;
    guard_refresh(&conn, ingest)?;
    let engine = engine_for_cli(&package_dir())?;
    // `list(include) or None` — an EMPTY tuple becomes `None`, which is not the
    // same as an empty list: `build_report` treats `[]` as "keep nothing" and
    // `None` as "keep everything". Python's truthiness is the filter here.
    let include = (!include.is_empty()).then_some(include);
    let exclude = (!exclude.is_empty()).then_some(exclude);
    build_report(&conn, scope, include, exclude, &engine).context("build_report")
}

/// `_open_store()` — `db.connect(deps.store_path)` + `schema.apply(conn)`.
///
/// # DIV-374 — DIV-239 is CLOSED, and this is the whole of it
///
/// Tranche 1 filed DIV-239 (and tranche 4 re-filed it as DIV-291 at four more
/// call sites): `cli.py:1830`'s helper *creates and migrates* a store, and the
/// port refused to, because "reproducing the create would mean porting the
/// migration chain into a read verb". **Wave 7 ported the migration chain**
/// (`stax_core::schema`, 29 migrations to v30, the SQL `include_str!`-ed out of
/// the reference's own `.sql` files), so the reason has dissolved and the
/// parity-correct answer is the one Python gives: create it.
///
/// Two consequences worth naming:
///
/// * **The connection is READ-WRITE**, which is DIV-295's lesson applied here:
///   a read-only SQLite connection to a WAL database cannot remove the `-shm` /
///   `-wal` files it creates on open, so a case-local home diff sees two files
///   the reference never leaves behind. `export`'s first `@home` run reported
///   exactly that. [`stax_etl::ingest::guard::open_read_write`] is `db.connect`
///   pragma-for-pragma (wave 4 pinned that) and additionally refuses the live
///   dataset by path.
/// * **The bootstrap case cannot be a matrix row.** Two implementations
///   creating a store write different `SQLITE_VERSION_NUMBER`s at page-1
///   offset 96 (DIV-257), and `diff -r` compares bytes — harness finding 3 for
///   tranche 2. So the create path is proven by probe and by unit test, and the
///   gated rows all run on homes whose store already exists.
///
/// # Errors
/// When the file cannot be created or opened, or when a migration fails.
pub fn open_store() -> Result<Connection> {
    let path = stax_core::settings::store_path();
    let conn = stax_etl::ingest::guard::open_read_write(&path)?;
    stax_core::schema::apply(&conn).context("schema.apply")?;
    Ok(conn)
}

/// `_maybe_refresh_store` — the decision, and a loud failure where the pass
/// would run.
///
/// # Errors
/// When the decision says a pass must run (DIV-238), or when the staleness
/// probe fails.
pub fn guard_refresh(conn: &Connection, ingest: &IngestFlags) -> Result<()> {
    let now = stax_core::queries::pytime::now_micros();
    anyhow::ensure!(
        !ensure_fresh_decision(conn, ingest.do_ingest, ingest.auto(), now)?,
        "this verb wants an ingest+backfill pass (the store is stale, or --ingest was given) \
         and that pass is not ported yet — RS-8-104 / RS-8-089. Re-run with \
         --no-auto-ingest to read the store as it stands."
    );
    Ok(())
}

/// `click.ClickException(msg)` — `Error: {msg}` on stderr, exit **1**.
///
/// Not a `UsageError`: `ClickException.show` writes no usage block and exits 1,
/// where `BadParameter` writes the block and exits 2. `report -p nope` is the
/// former and `plan set nope` is the latter; the two are one character apart in
/// `cli.py` and a whole exit code apart on the wire.
#[must_use]
pub fn click_exception(message: &str) -> Output {
    Output {
        stdout: String::new(),
        stderr: format!("Error: {message}\n"),
        code: 1,
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        args: ReportArgs,
    }

    #[derive(clap::Parser)]
    struct WrapPeriod {
        #[command(flatten)]
        args: PeriodArgs,
    }

    #[test]
    fn the_report_defaults_are_the_decorators() {
        let parsed = Wrap::try_parse_from(["x"]).expect("bare parse");
        assert_eq!(parsed.args.period, "7days");
        assert_eq!(parsed.args.format, "text");
        assert_eq!(parsed.args.provider, "all");
        assert!(parsed.args.include.is_empty());
        assert!(parsed.args.ingest.auto());
    }

    #[test]
    fn project_and_exclude_are_repeatable() {
        let parsed =
            Wrap::try_parse_from(["x", "--project", "-a", "--project", "-b", "--exclude", "-c"])
                .expect("parse");
        assert_eq!(parsed.args.include, vec!["-a".to_owned(), "-b".to_owned()]);
        // The real shape, and the reason `allow_hyphen_values` is on: every slug
        // in the store begins with `-`.
        let real = Wrap::try_parse_from([
            "x",
            "--project",
            "-Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow",
        ])
        .expect("a real slug parses");
        assert_eq!(real.args.include.len(), 1);
        assert_eq!(parsed.args.exclude, vec!["-c".to_owned()]);
    }

    #[test]
    fn the_provider_choice_is_the_live_registry_sorted_with_all_first() {
        // Not a hand-list: if an adapter lands, both sides gain the same value.
        // The `'static` table and the runtime registry are pinned to each other
        // here, so `provider_choices`' `PYTHON_WALK_ORDER` source cannot drift.
        let mut expected = stax_adapters::registry::registered_names();
        expected.sort_unstable();
        let mut table = stax_adapters::registry::PYTHON_WALK_ORDER.to_vec();
        table.sort_unstable();
        assert_eq!(
            expected,
            table
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            "the static choice table IS the live registry"
        );
        for name in &expected {
            assert!(
                Wrap::try_parse_from(["x", "--provider", name]).is_ok(),
                "{name} should be accepted"
            );
        }
        assert!(Wrap::try_parse_from(["x", "--provider", "all"]).is_ok());
        assert!(Wrap::try_parse_from(["x", "--provider", "nope"]).is_err());
    }

    #[test]
    fn today_and_month_have_no_provider_option() {
        assert!(
            WrapPeriod::try_parse_from(["x", "--provider", "claude"]).is_err(),
            "`today` / `month` never declared one; accepting it would be a wider surface"
        );
    }

    #[test]
    fn an_unknown_period_is_a_click_exception_not_a_usage_error() {
        let out = click_exception("Unknown period 'nope'. Valid: today, 7days, 30days, month, all");
        assert!(out.stdout.is_empty());
        assert_eq!(
            out.stderr,
            "Error: Unknown period 'nope'. Valid: today, 7days, 30days, month, all\n"
        );
        assert_eq!(out.code, 1, "ClickException exits 1; BadParameter exits 2");
    }

    #[test]
    fn the_period_error_text_is_the_scope_modules_own() {
        let err = parse_period("nope", Instant::from_parts(2026, 7, 31, 0, 0, 0, 0))
            .expect_err("unknown period");
        assert_eq!(
            err,
            "Unknown period 'nope'. Valid: today, 7days, 30days, month, all"
        );
    }

    #[test]
    fn the_json_emitter_is_dumps_indent_two_plus_clicks_newline() {
        let report = Report {
            scope_label: "today".to_owned(),
            total_cost: 0.0,
            total_messages: 0,
            total_sessions: 0,
            by_project: Vec::new(),
        };
        let out = emit_report(&report, "json").stdout;
        assert!(out.starts_with("{\n  \"scope_label\": \"today\","), "{out}");
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn the_text_emitter_takes_the_no_activity_branch_on_an_empty_report() {
        let report = Report {
            scope_label: "today".to_owned(),
            total_cost: 0.0,
            total_messages: 0,
            total_sessions: 0,
            by_project: Vec::new(),
        };
        assert_eq!(
            emit_report(&report, "text").stdout,
            "StackUnderflow — today\nNo activity in this period.\nTotal: $0.00  0 messages  0 sessions\n"
        );
    }

    #[test]
    fn the_console_width_comes_from_columns_and_falls_back_to_eighty() {
        // Read through the process environment, so this asserts the fallback
        // shape rather than mutating it (`set_var` is unsafe — finding 5).
        let width = console_width();
        assert!(width > 0);
        assert_eq!(
            render::DEFAULT_CONSOLE_WIDTH,
            80,
            "Rich's own no-terminal default"
        );
    }

    #[test]
    fn last_wins_on_the_auto_ingest_pair() {
        let on = Wrap::try_parse_from(["x", "--no-auto-ingest", "--auto-ingest"]).unwrap();
        assert!(on.args.ingest.auto());
        let off = Wrap::try_parse_from(["x", "--auto-ingest", "--no-auto-ingest"]).unwrap();
        assert!(!off.args.ingest.auto());
    }

    #[test]
    fn an_empty_repeatable_option_is_none_not_an_empty_filter() {
        // `list(include) or None` — the difference between "keep everything"
        // and "keep nothing", and it is one Python truthiness check.
        let empty: Vec<String> = Vec::new();
        assert!((!empty.is_empty()).then_some(&empty).is_none());
        let one = vec![String::new()];
        assert!(
            (!one.is_empty()).then_some(&one).is_some(),
            "a single EMPTY STRING is still a filter — the `--project ''` class"
        );
    }
}
