//! `stax plan {show,set,reset}` and `plan thresholds {show,set,reset}` —
//! `cli.py:568`–`:783`.
//!
//! Seven inventory nodes: two groups and five leaves. Four of them write
//! `$STACKUNDERFLOW_HOME/config.json`, so every one of their case rows runs on a
//! case-local `@home` and the harness diffs the resulting trees — a `plan set`
//! that prints the right line while persisting the wrong float is exactly the
//! divergence a stdout-only diff misses.
//!
//! # The two spend reads `plan show` performs, and why there are two
//!
//! ```python
//! usage = plans_mod.compute_usage(plan, 0.0)          # for the WINDOW only
//! used  = _resolve_period_spend(usage["period_start"], usage["period_end"])
//! usage = plans_mod.compute_usage(plan, used)         # again, with the money
//! ```
//!
//! `compute_usage` is called twice because the billing window is a function of
//! the plan and today, and the spend query needs the window before it can run.
//! The first call's `used` is a throwaway `0.0`. Reproduced exactly — collapsing
//! it into one call would need `period_window` to be public *and* would drop the
//! second call's recomputation of `status`/`pct` from the real figure.
//!
//! Then a SECOND, differently-shaped read: `_resolve_period_daily_costs` pulls
//! the per-day series so the burn projector sees the *shape* of the spend and
//! not just its total. Both helpers now live in [`stax_reports::plans`] — see
//! that module's tranche-3 note for why they moved out of the route.
//!
//! # `plan show` prices from the MANIFEST, and the route does not
//!
//! `cli.py` never calls `use_price_book_store`, so a CLI process prices from
//! `data/models.toml` while a server process prices from the `price_book` table
//! (RS-3-082's seam). [`crate::status::engine_for_cli`] is the manifest engine;
//! using `stax_reports::pricing::engine` here would silently price a
//! pre-backfill store off the wrong rates (law 2 / DIV-056).
//!
//! # `click.secho` is `click.echo` here
//!
//! `plan show` ends with two `secho` calls (`fg="green"`, `bold=True`). Click
//! strips colour when stdout is not a TTY — and the parity harness redirects
//! both sides — so the bytes are the plain text. Recorded rather than assumed:
//! a `--color` flag would make the two disagree, and neither implementation has
//! one.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Map, Value};
use stax_reports::aggregate::build_report;
use stax_reports::plans::{self, Date, Plan, Usage};
use stax_reports::scope::Scope;
use stax_reports::{burn, render};

use crate::click::{Output, PROGRAM, UsageError};
use crate::reports::open_store;
use crate::status::{engine_for_cli, package_dir};

/// `stax plan`.
#[derive(Debug, Args)]
pub struct PlanArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: PlanVerb,
}

/// `plan`'s subcommands.
#[derive(Debug, Subcommand)]
pub enum PlanVerb {
    /// Clear the active plan.
    Reset,
    /// Set the active plan. NAME is one of: claude-pro, claude-max, cursor-pro, cursor-max, custom.
    Set(PlanSetArgs),
    /// Show the active plan, current usage against budget, and burn projection.
    Show(FormatArgs),
    /// Configure burn-projector alert thresholds (default 50% / 75% / 90%).
    Thresholds(ThresholdsArgs),
}

/// The bare `--format text|json` every read verb here carries.
#[derive(Debug, Args)]
pub struct FormatArgs {
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// `plan set NAME`.
#[derive(Debug, Args)]
pub struct PlanSetArgs {
    /// The plan name.
    pub name: String,
    /// Monthly budget in USD (required for 'custom', overrides preset otherwise).
    #[arg(long = "monthly-usd", value_name = "FLOAT")]
    pub monthly_usd: Option<f64>,
    /// Day of month the budget resets (default 1).
    #[arg(long = "reset-day", value_name = "INTEGER", default_value_t = 1,
          value_parser = clap::value_parser!(i64).range(1..=31))]
    pub reset_day: i64,
}

/// `stax plan thresholds`.
#[derive(Debug, Args)]
pub struct ThresholdsArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: ThresholdsVerb,
}

/// `plan thresholds`' subcommands.
#[derive(Debug, Subcommand)]
pub enum ThresholdsVerb {
    /// Restore the default thresholds (50% / 75% / 90%).
    Reset,
    /// Set the alert thresholds (positional integers in [1, 200]).
    Set(ThresholdsSetArgs),
    /// Show the active alert thresholds.
    Show(FormatArgs),
}

/// `plan thresholds set VALUES...`.
#[derive(Debug, Args)]
pub struct ThresholdsSetArgs {
    /// The thresholds, as integer percentages.
    #[arg(required = true, value_name = "VALUES")]
    pub values: Vec<i64>,
}

/// Run a `plan` verb.
///
/// # Errors
/// A store failure, a settings write failure, or the unported refresh pass.
pub fn run_plan(args: &PlanArgs) -> Result<Output> {
    match &args.verb {
        PlanVerb::Reset => run_reset(),
        PlanVerb::Set(set) => run_set(set),
        PlanVerb::Show(fmt) => run_show(&fmt.format),
        PlanVerb::Thresholds(thresholds) => match &thresholds.verb {
            ThresholdsVerb::Reset => run_thresholds_reset(),
            ThresholdsVerb::Set(set) => run_thresholds_set(&set.values),
            ThresholdsVerb::Show(fmt) => Ok(run_thresholds_show(&fmt.format)),
        },
    }
}

// ── plan show ────────────────────────────────────────────────────────────────

fn run_show(format: &str) -> Result<Output> {
    let config = crate::settings::load().to_value();
    let config = match crate::memory::to_serde(&config) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let plan = plans::get_active_plan(&config).map_err(|err| anyhow::anyhow!("{err}"))?;
    let Some(plan) = plan else {
        // `{"plan": None, "usage": None}` — TWO keys, and no `projection`.
        if format == "json" {
            let mut body = Map::new();
            body.insert("plan".to_owned(), Value::Null);
            body.insert("usage".to_owned(), Value::Null);
            return Ok(Output::ok(format!(
                "{}\n",
                render::render_json(&Value::Object(body))
            )));
        }
        return Ok(Output::ok(
            "No plan set. Run: stackunderflow plan set claude-pro\n",
        ));
    };

    let today = Date::today_local();
    // Call one: the window. `used` is a throwaway `0.0`.
    let window = plans::compute_usage(&plan, 0.0, today);
    let used = period_spend(&window.period_start, &window.period_end)?;
    // Call two: the same window, now with the money in it.
    let usage = plans::compute_usage(&plan, used, today);

    let daily = period_daily_costs(&usage.period_start, &usage.period_end)?;
    let thresholds = configured_thresholds(&config);
    let projection = burn::build_projection(
        &daily,
        used,
        plan.monthly_usd,
        usage.days_so_far,
        usage.days_in_period,
        Some(&thresholds),
        None,
    );

    if format == "json" {
        return Ok(Output::ok(format!(
            "{}\n",
            render::render_json(&show_payload(&plan, &usage, &projection))
        )));
    }
    Ok(Output::ok(show_text(&plan, &usage, &projection)))
}

/// `_resolve_period_spend` — `build_report` over a hand-built `Scope`.
///
/// The scope is built BY HAND, not through `parse_period`: the label is the
/// literal `"plan-period"` and the bounds are naive midnight stamps.
fn period_spend(period_start: &str, period_end: &str) -> Result<f64> {
    let (since, until) = plans::window_bounds(period_start, period_end)
        .ok_or_else(|| anyhow::anyhow!("period_start is not an ISO date"))?;
    let conn = open_store()?;
    let engine = engine_for_cli(&package_dir())?;
    let scope = Scope::new(Some(since), Some(until), "plan-period");
    let report = build_report(&conn, &scope, None, None, &engine)?;
    Ok(report.total_cost)
}

/// `_resolve_period_daily_costs` — the per-day series the projector needs.
fn period_daily_costs(period_start: &str, period_end: &str) -> Result<Vec<f64>> {
    let Some((since, until)) = plans::window_bounds(period_start, period_end) else {
        return Ok(Vec::new());
    };
    let conn = open_store()?;
    Ok(plans::spend_daily_window(
        &conn,
        period_start,
        period_end,
        &since,
        &until,
    )?)
}

/// `Settings().get("plan_alert_thresholds") or list(burn.DEFAULT_THRESHOLDS)`.
///
/// Truthiness, not a null check: a persisted empty list falls back to the
/// defaults exactly as `None` does.
fn configured_thresholds(config: &Map<String, Value>) -> Vec<i64> {
    let raw = config.get("plan_alert_thresholds");
    let list = raw
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty());
    list.map_or_else(
        || burn::DEFAULT_THRESHOLDS.to_vec(),
        |items| items.iter().filter_map(Value::as_i64).collect(),
    )
}

/// The `--format json` body: three keys, in the dict-literal order.
#[must_use]
pub fn show_payload(plan: &Plan, usage: &Usage, projection: &burn::Projection) -> Value {
    let mut plan_block = Map::new();
    plan_block.insert("name".to_owned(), Value::String(plan.name.clone()));
    plan_block.insert("monthly_usd".to_owned(), Value::from(plan.monthly_usd));
    plan_block.insert("reset_day".to_owned(), Value::from(plan.reset_day));

    let mut usage_block = Map::new();
    usage_block.insert("used".to_owned(), Value::from(usage.used));
    usage_block.insert("budget".to_owned(), Value::from(usage.budget));
    usage_block.insert("remaining".to_owned(), Value::from(usage.remaining));
    usage_block.insert("pct".to_owned(), Value::from(usage.pct));
    usage_block.insert(
        "projected_month_end".to_owned(),
        Value::from(usage.projected_month_end),
    );
    usage_block.insert("status".to_owned(), Value::String(usage.status.to_owned()));
    usage_block.insert(
        "period_start".to_owned(),
        Value::String(usage.period_start.clone()),
    );
    usage_block.insert(
        "period_end".to_owned(),
        Value::String(usage.period_end.clone()),
    );
    usage_block.insert("days_so_far".to_owned(), Value::from(usage.days_so_far));
    usage_block.insert(
        "days_in_period".to_owned(),
        Value::from(usage.days_in_period),
    );

    let mut projection_block = Map::new();
    projection_block.insert(
        "projected_month_end_usd".to_owned(),
        Value::from(projection.projected_month_end_usd),
    );
    projection_block.insert(
        "projection_method".to_owned(),
        Value::String(projection.projection_method.as_str().to_owned()),
    );
    projection_block.insert(
        "daily_burn_usd".to_owned(),
        Value::from(projection.daily_burn_usd),
    );
    projection_block.insert(
        "days_to_limit".to_owned(),
        projection.days_to_limit.map_or(Value::Null, Value::from),
    );
    projection_block.insert(
        "thresholds".to_owned(),
        Value::Array(
            projection
                .thresholds
                .iter()
                .copied()
                .map(Value::from)
                .collect(),
        ),
    );
    projection_block.insert(
        "crossed_threshold".to_owned(),
        projection
            .crossed_threshold
            .map_or(Value::Null, Value::from),
    );
    projection_block.insert(
        "alert".to_owned(),
        projection.alert.clone().map_or(Value::Null, Value::String),
    );

    let mut body = Map::new();
    body.insert("plan".to_owned(), Value::Object(plan_block));
    body.insert("usage".to_owned(), Value::Object(usage_block));
    body.insert("projection".to_owned(), Value::Object(projection_block));
    Value::Object(body)
}

/// The `--format text` block — eight or nine `click.echo` lines.
#[must_use]
pub fn show_text(plan: &Plan, usage: &Usage, projection: &burn::Projection) -> String {
    let mut out = String::new();
    out.push_str(&format!("Plan:          {}\n", plan.name));
    out.push_str(&format!(
        "Budget:        {} / month  (resets day {})\n",
        format_money(plan.monthly_usd),
        plan.reset_day
    ));
    out.push_str(&format!(
        "Period:        {} → {}  (day {} of {})\n",
        usage.period_start, usage.period_end, usage.days_so_far, usage.days_in_period
    ));
    out.push_str(&format!(
        "Used:          {}  ({:.1}% of budget)\n",
        format_money(usage.used),
        usage.pct
    ));
    out.push_str(&format!(
        "Remaining:     {}\n",
        format_money(usage.remaining)
    ));
    out.push_str(&format!(
        "Projected:     {}  ({}, {}/day burn)\n",
        format_money(projection.projected_month_end_usd),
        projection.projection_method.as_str(),
        format_money(projection.daily_burn_usd),
    ));
    if let Some(days) = projection.days_to_limit {
        // `day{'s' if … != 1 else ''}` — the plural is on the count, so `-1`
        // days is "days" too.
        let plural = if days == 1 { "" } else { "s" };
        out.push_str(&format!(
            "Days to limit: ~{days} day{plural} at current burn\n"
        ));
    }
    out.push_str(&format!("Status:        {}\n", usage.status));
    if let Some(alert) = &projection.alert {
        out.push_str(&format!("Alert:         {alert}\n"));
    }
    out
}

/// `_format_money(amount)` — `f"${amount:,.2f}"`.
///
/// Thousands-separated, unlike `render_status_line`'s bare `:.2f`. A negative
/// amount (an over-budget `remaining`) renders as `$-12.34`: the `$` is a
/// literal prefix in the f-string, so the minus lands INSIDE it, and that is the
/// reference's shape rather than a bug to fix here.
#[must_use]
pub fn format_money(amount: f64) -> String {
    let rendered = format!("{amount:.2}");
    let (sign, digits) = rendered
        .strip_prefix('-')
        .map_or(("", rendered.as_str()), |rest| ("-", rest));
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "00"));
    let grouped = group_digits(whole);
    format!("${sign}{grouped}.{fraction}")
}

/// `{:,}` over an already-rendered digit string.
fn group_digits(digits: &str) -> String {
    let first = digits.len() % 3;
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    if first > 0 {
        out.push_str(&digits[..first]);
    }
    for (index, chunk) in digits.as_bytes()[first..].chunks(3).enumerate() {
        if index > 0 || first > 0 {
            out.push(',');
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    out
}

// ── plan set / reset ─────────────────────────────────────────────────────────

fn run_set(args: &PlanSetArgs) -> Result<Output> {
    let plan = match set_plan(&args.name, args.monthly_usd, args.reset_day) {
        Ok(plan) => plan,
        Err(message) => {
            // `raise click.BadParameter(str(e), param_hint="NAME")`.
            let error = UsageError::bad_parameter("plan set", "[OPTIONS] NAME", "NAME", message);
            return Ok(Output::usage(&error, PROGRAM));
        }
    };
    Ok(Output::ok(format!(
        "  plan = {}  ({}/month, resets day {})\n",
        plan.name,
        format_money(plan.monthly_usd),
        plan.reset_day
    )))
}

/// `plans.set_plan(name, monthly_usd=…, reset_day=…)`.
///
/// The three `persist` calls run in the reference's order, and they are NOT
/// transactional there either: a write that fails on the second key leaves the
/// first one set. Reproduced.
///
/// # Errors
/// The `ValueError` messages, verbatim — `plan set` turns them into a
/// `BadParameter` on `NAME` regardless of which argument was actually wrong,
/// which is the reference's `param_hint` and not a mistake to fix here.
pub fn set_plan(name: &str, monthly_usd: Option<f64>, reset_day: i64) -> Result<Plan, String> {
    let preset = plans::PRESETS.iter().find(|(key, _)| *key == name);
    let Some((_, preset_amount)) = preset else {
        // `', '.join(sorted(PRESETS))` — the dict's KEYS, sorted.
        let mut names: Vec<&str> = plans::PRESETS.iter().map(|(key, _)| *key).collect();
        names.sort_unstable();
        return Err(format!(
            "Unknown plan name '{name}'. Valid: {}",
            names.join(", ")
        ));
    };

    let amount = if name == "custom" {
        monthly_usd.ok_or_else(|| "custom plan requires --monthly-usd".to_owned())?
    } else {
        // `float(monthly_usd) if monthly_usd is not None else float(preset or 0.0)`
        monthly_usd.unwrap_or_else(|| preset_amount.unwrap_or(0.0))
    };
    if amount <= 0.0 {
        return Err("monthly_usd must be a positive number".to_owned());
    }
    if !(1..=31).contains(&reset_day) {
        return Err("reset_day must be between 1 and 31".to_owned());
    }

    persist_or_die(
        "plan_name",
        stax_core::queries::pyjson::Value::Str(name.to_owned()),
    )?;
    persist_or_die(
        "plan_monthly_usd",
        stax_core::queries::pyjson::Value::Float(amount),
    )?;
    persist_or_die(
        "plan_reset_day",
        stax_core::queries::pyjson::Value::Int(reset_day),
    )?;

    Ok(Plan {
        name: name.to_owned(),
        monthly_usd: amount,
        reset_day,
    })
}

fn persist_or_die(key: &str, value: stax_core::queries::pyjson::Value) -> Result<(), String> {
    crate::settings::persist(key, value).map_err(|err| match err {
        crate::settings::PersistError::Value(message) => message,
        crate::settings::PersistError::Io(error) => error.to_string(),
    })
}

fn run_reset() -> Result<Output> {
    // `_SETTINGS_KEYS` — all three, every time, whether set or not. `remove`
    // saves unconditionally, so this CREATES `config.json` on a bare home.
    for key in ["plan_name", "plan_monthly_usd", "plan_reset_day"] {
        crate::settings::remove(key)?;
    }
    Ok(Output::ok("  plan cleared\n"))
}

// ── plan thresholds ──────────────────────────────────────────────────────────

/// `plan thresholds show`.
#[must_use]
pub fn run_thresholds_show(format: &str) -> Output {
    let config = crate::settings::load().to_value();
    let config = match crate::memory::to_serde(&config) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    // `sorted({int(t) for t in raw})` — a SET, so duplicates collapse.
    let mut thresholds = configured_thresholds(&config);
    thresholds.sort_unstable();
    thresholds.dedup();

    if format == "json" {
        let mut body = Map::new();
        body.insert(
            "thresholds".to_owned(),
            Value::Array(thresholds.iter().copied().map(Value::from).collect()),
        );
        return Output::ok(format!("{}\n", render::render_json(&Value::Object(body))));
    }
    Output::ok(format!("  thresholds = {}\n", percent_list(&thresholds)))
}

fn run_thresholds_set(values: &[i64]) -> Result<Output> {
    for value in values {
        if !(1..=200).contains(value) {
            let error = UsageError::bad_parameter(
                "plan thresholds set",
                // `VALUES...`, NOT `[VALUES]...`: Click brackets a variadic
                // argument only when it is OPTIONAL, and this one is
                // `required=True`. Measured against the reference's usage line,
                // which is the one byte this case row exists to pin.
                "[OPTIONS] VALUES...",
                "VALUES",
                format!("threshold {value} must be an integer in [1, 200]"),
            );
            return Ok(Output::usage(&error, PROGRAM));
        }
    }
    let mut deduped = values.to_vec();
    deduped.sort_unstable();
    deduped.dedup();
    crate::settings::persist(
        "plan_alert_thresholds",
        stax_core::queries::pyjson::Value::Array(
            deduped
                .iter()
                .copied()
                .map(stax_core::queries::pyjson::Value::Int)
                .collect(),
        ),
    )
    .map_err(|err| match err {
        crate::settings::PersistError::Value(message) => anyhow::anyhow!("{message}"),
        crate::settings::PersistError::Io(error) => error,
    })?;
    Ok(Output::ok(format!(
        "  thresholds = {}\n",
        percent_list(&deduped)
    )))
}

fn run_thresholds_reset() -> Result<Output> {
    crate::settings::remove("plan_alert_thresholds")?;
    Ok(Output::ok(format!(
        "  thresholds = {}  (default)\n",
        percent_list(&burn::DEFAULT_THRESHOLDS)
    )))
}

/// `', '.join(f'{t}%' for t in values)`.
#[must_use]
pub fn percent_list(values: &[i64]) -> String {
    values
        .iter()
        .map(|value| format!("{value}%"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_is_thousands_separated_and_the_minus_lands_inside_the_dollar() {
        assert_eq!(format_money(20.0), "$20.00");
        assert_eq!(format_money(1234.5), "$1,234.50");
        assert_eq!(format_money(1_234_567.891), "$1,234,567.89");
        assert_eq!(format_money(0.0), "$0.00");
        // `f"${-12.34:,.2f}"` — the `$` is a literal PREFIX, so this is the
        // reference's shape, odd as it looks. `plan show`'s `Remaining:` line
        // reaches it the moment a budget is overspent.
        assert_eq!(format_money(-12.34), "$-12.34");
        assert_eq!(format_money(-1234.5), "$-1,234.50");
    }

    #[test]
    fn the_threshold_list_is_percent_suffixed_and_comma_joined() {
        assert_eq!(percent_list(&[50, 75, 90]), "50%, 75%, 90%");
        assert_eq!(percent_list(&[]), "");
        assert_eq!(percent_list(&[7]), "7%");
    }

    #[test]
    fn an_unknown_plan_name_lists_the_presets_sorted() {
        let err = set_plan("nope", None, 1).expect_err("unknown");
        assert_eq!(
            err,
            "Unknown plan name 'nope'. Valid: claude-max, claude-pro, cursor-max, cursor-pro, custom",
            "`sorted(PRESETS)` is alphabetical, not the dict order"
        );
    }

    #[test]
    fn custom_without_an_amount_is_the_first_error_and_the_order_matters() {
        // The name check runs BEFORE the amount check, so a bad name with no
        // amount reports the name — not "custom plan requires --monthly-usd".
        assert!(
            set_plan("nope", None, 1)
                .unwrap_err()
                .starts_with("Unknown")
        );
    }

    #[test]
    fn a_bad_name_never_reaches_the_settings_writer() {
        // The three `persist` calls are the LAST thing `set_plan` does. This is
        // the proof that a rejected name leaves `config.json` untouched — which
        // is what the `@home` case row then verifies on disk.
        let err = set_plan("", None, 1);
        assert!(err.is_err());
    }

    #[test]
    fn the_bad_parameter_hint_is_name_even_for_an_amount_error() {
        // `raise click.BadParameter(str(e), param_hint="NAME")` — ONE hint for
        // every `ValueError` `set_plan` can raise, including the ones about
        // `--monthly-usd` and `--reset-day`. Faithful, not fixed.
        let error = UsageError::bad_parameter(
            "plan set",
            "[OPTIONS] NAME",
            "NAME",
            "custom plan requires --monthly-usd",
        );
        assert_eq!(
            error.render("stackunderflow"),
            concat!(
                "Usage: stackunderflow plan set [OPTIONS] NAME\n",
                "Try 'stackunderflow plan set --help' for help.\n",
                "\n",
                "Error: Invalid value for NAME: custom plan requires --monthly-usd\n",
            )
        );
    }

    #[test]
    fn the_reset_day_range_is_clicks_intrange_and_clap_rejects_the_same_values() {
        use clap::Parser as _;

        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            args: PlanSetArgs,
        }
        assert!(Wrap::try_parse_from(["x", "claude-pro", "--reset-day", "1"]).is_ok());
        assert!(Wrap::try_parse_from(["x", "claude-pro", "--reset-day", "31"]).is_ok());
        assert!(Wrap::try_parse_from(["x", "claude-pro", "--reset-day", "0"]).is_err());
        assert!(Wrap::try_parse_from(["x", "claude-pro", "--reset-day", "32"]).is_err());
        let default = Wrap::try_parse_from(["x", "claude-pro"]).expect("bare");
        assert_eq!(default.args.reset_day, 1);
    }

    #[test]
    fn thresholds_set_requires_at_least_one_value() {
        use clap::Parser as _;

        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            args: ThresholdsSetArgs,
        }
        assert!(Wrap::try_parse_from(["x"]).is_err(), "`required=True`");
        let parsed = Wrap::try_parse_from(["x", "50", "75"]).expect("two values");
        assert_eq!(parsed.args.values, vec![50, 75]);
    }

    #[test]
    fn a_threshold_out_of_range_is_a_usage_error_on_values() {
        let out = run_thresholds_set(&[50, 500]).expect("no io");
        assert_eq!(out.code, 2);
        assert!(
            out.stderr.contains(
                "Error: Invalid value for VALUES: threshold 500 must be an integer in [1, 200]"
            ),
            "{}",
            out.stderr
        );
        assert!(out.stdout.is_empty(), "nothing is printed before the raise");
    }

    #[test]
    fn the_range_check_reports_the_first_offender_not_the_last() {
        // The loop raises inside `for v in values`, so `500 300` names 500.
        let out = run_thresholds_set(&[500, 300]).expect("no io");
        assert!(out.stderr.contains("threshold 500 "), "{}", out.stderr);
    }

    #[test]
    fn configured_thresholds_falls_back_on_an_empty_list_as_well_as_on_absence() {
        let mut config = Map::new();
        assert_eq!(configured_thresholds(&config), vec![50, 75, 90]);
        config.insert("plan_alert_thresholds".to_owned(), Value::Array(Vec::new()));
        assert_eq!(
            configured_thresholds(&config),
            vec![50, 75, 90],
            "`or list(DEFAULT_THRESHOLDS)` is truthiness, so `[]` falls back"
        );
        config.insert(
            "plan_alert_thresholds".to_owned(),
            Value::Array(vec![Value::from(10), Value::from(20)]),
        );
        assert_eq!(configured_thresholds(&config), vec![10, 20]);
    }

    #[test]
    fn the_no_plan_json_body_has_two_keys_and_no_projection() {
        // The early return is a different SHAPE, not a nulled-out projection.
        let mut body = Map::new();
        body.insert("plan".to_owned(), Value::Null);
        body.insert("usage".to_owned(), Value::Null);
        assert_eq!(
            render::render_json(&Value::Object(body)),
            "{\n  \"plan\": null,\n  \"usage\": null\n}"
        );
    }

    #[test]
    fn the_days_to_limit_line_is_omitted_when_it_is_none() {
        let plan = Plan {
            name: "claude-pro".to_owned(),
            monthly_usd: 20.0,
            reset_day: 1,
        };
        let usage = plans::compute_usage(&plan, 0.0, Date::from_ymd(2026, 7, 15));
        let projection = burn::build_projection(&[], 0.0, 20.0, 15, 31, None, None);
        let text = show_text(&plan, &usage, &projection);
        assert!(!text.contains("Days to limit"), "{text}");
        assert!(text.contains("Status:        ok"), "{text}");
        assert!(!text.contains("Alert:"), "no alert at zero spend");
    }

    #[test]
    fn the_show_payload_key_order_is_the_dict_literal_order() {
        let plan = Plan {
            name: "custom".to_owned(),
            monthly_usd: 100.0,
            reset_day: 5,
        };
        let usage = plans::compute_usage(&plan, 50.0, Date::from_ymd(2026, 7, 15));
        let projection = burn::build_projection(&[1.0, 2.0], 50.0, 100.0, 11, 31, None, None);
        let rendered = render::render_json(&show_payload(&plan, &usage, &projection));
        let plan_at = rendered.find("\"plan\"").expect("plan");
        let usage_at = rendered.find("\"usage\"").expect("usage");
        let projection_at = rendered.find("\"projection\"").expect("projection");
        assert!(plan_at < usage_at && usage_at < projection_at);
        // …and inside `plan`, `name` before `monthly_usd` before `reset_day`.
        let name_at = rendered.find("\"name\"").expect("name");
        let monthly_at = rendered.find("\"monthly_usd\"").expect("monthly_usd");
        assert!(name_at < monthly_at);
    }
}
