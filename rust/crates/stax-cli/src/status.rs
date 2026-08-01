//! `stax status` — `cli.py:3229`–`:3244`, the reserved verb.
//!
//! DIV-025 renamed this port's schema/row-count command to `store` precisely so
//! the name could be given back to the Python verb it belongs to: a compact
//! one-liner carrying today's and this month's spend. Twelve lines of `cli.py`
//! sitting on top of three ported modules —
//! [`stax_reports::scope::parse_period`],
//! [`stax_reports::aggregate::build_report`] and
//! `reports/render.py::render_status_line`.
//!
//! # Why the CLI depends on `stax-reports`
//!
//! Wave 5 batch C deliberately put the `reports/*.py` + `services/*.py` layer in
//! one place, and its module doc said why in as many words: "`/api/compare` and
//! `stackunderflow compare` share `services/compare.py` … Transliterating that
//! logic into the route module would fork it: wave 8 ports the CLI verbs, finds
//! no shared home, and writes a second copy that drifts." This is that wave, and
//! this is the seam being used as designed. The alternative — a second
//! `build_report` in `stax-cli` — is exactly the fork the split was built to
//! prevent.
//!
//! Tranche 1 could only reach that layer through `stax-server`, and recorded the
//! cost as a deviation: the CLI binary linked `axum`/`tokio` transitively and
//! never served a byte. **Tranche 3 closed it** — the layer is now
//! [`stax_reports`], a crate with no HTTP and no runtime, and `stax-server` and
//! `stax-cli` are peers that both consume it. The functions are the same
//! functions; only the address changed.
//!
//! # Pricing: the manifest, never the price book
//!
//! `server.py`'s lifespan calls `use_price_book_store` + `prime_price_book_cache`
//! before serving; **`cli.py` does not**. So a CLI process prices from
//! `data/models.toml` and a server process prices from the `price_book` table
//! (the RS-3-082 seam). [`engine_for_cli`] therefore builds the manifest engine
//! directly rather than reusing `stax_reports::pricing::engine`, which reads the
//! table. On the maintainer's store this is unreachable — `usage_events` is
//! populated, so `build_report` takes the mart path and never prices anything —
//! but "unreachable today" is how a 2% mispricing gets shipped (law 2 /
//! DIV-056), so it is wired correctly rather than left to the default.
//!
//! # What is NOT ported: the ingest pass
//!
//! `_maybe_refresh_store` → `cli_helpers.ingest.ensure_fresh` runs
//! `run_ingest` + `backfill(force=False)` when the store is stale or `--ingest`
//! is given. The *decision* is ported here, exactly ([`ensure_fresh_decision`]);
//! the *pass* is not — `stax_etl` has `run_ingest` but no equivalent of
//! `etl.backfill.backfill`, and half a refresh is worse than none. When the
//! decision says "run", the port fails loudly with the item id rather than
//! silently skipping, because a skipped refresh is a *changed answer*, not a
//! missing feature (DIV-238). Every parity row passes `--no-auto-ingest`, which
//! is the branch that is proven.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use rusqlite::Connection;
use stax_core::queries::pytime;
use stax_core::store::Store;
use stax_etl::pricing::costs::PricingEngine;
use stax_reports::aggregate::{Report, build_report};
use stax_reports::scope::{Instant, parse_period};

use crate::click::Output;

/// `STALENESS_THRESHOLD_HOURS`.
pub const STALENESS_THRESHOLD_HOURS: f64 = 6.0;

/// `stax status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
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

impl StatusArgs {
    /// The effective `auto_ingest`, honouring Click's last-wins semantics.
    ///
    /// Click keeps the last occurrence of a repeated option; clap's mutual
    /// `overrides_with` gives the same answer, resetting whichever flag lost.
    #[must_use]
    pub const fn auto(&self) -> bool {
        !self.no_auto_ingest
    }
}

/// Run `status`.
///
/// # Errors
/// When the store cannot be opened, a query fails, or the refresh decision
/// says a pass must run (DIV-238).
pub fn run_status(args: &StatusArgs) -> Result<Output> {
    let path = stax_core::settings::store_path();
    if !path.exists() {
        // Python's `_open_store` calls `db.connect`, which CREATES the store
        // and applies the schema; this port does not write data files it was
        // not handed (DIV-239). The output would have been the all-zero
        // report; fabricating it would hide the state divergence.
        bail!(
            "no store at {} — the port does not create one (Python's `_open_store` would). \
             Run `stackunderflow init` or point $STACKUNDERFLOW_HOME at an existing store.",
            path.display()
        );
    }
    let store = Store::open_read_only(&path)?;
    let conn = store.conn();
    if ensure_fresh_decision(conn, args.do_ingest, args.auto(), pytime::now_micros())? {
        bail!(
            "`status` wants an ingest+backfill pass (the store is stale, or --ingest was given) \
             and that pass is not ported yet — RS-8-104 / RS-8-089. Re-run with \
             --no-auto-ingest to read the store as it stands."
        );
    }
    let engine = engine_for_cli(&package_dir())?;
    let now = Instant::now_utc();
    let today = report_for(conn, "today", now, &engine)?;
    let month = report_for(conn, "month", now, &engine)?;
    Ok(render(&today, &month, &args.format))
}

fn report_for(
    conn: &Connection,
    spec: &str,
    now: Instant,
    engine: &PricingEngine,
) -> Result<Report> {
    let scope = parse_period(spec, now).map_err(|message| anyhow::anyhow!(message))?;
    build_report(conn, &scope, None, None, engine).with_context(|| format!("build_report({spec})"))
}

/// The two output formats.
#[must_use]
pub fn render(today: &Report, month: &Report, format: &str) -> Output {
    if format == "json" {
        let mut body = serde_json::Map::new();
        body.insert("today".to_owned(), today.to_value());
        body.insert("month".to_owned(), month.to_value());
        return Output::ok(format!(
            "{}\n",
            stax_memory::pyjson::dumps_pretty(&serde_json::Value::Object(body))
        ));
    }
    Output::ok(format!("{}\n", status_line(today, month)))
}

/// `reports/render.py::render_status_line`.
#[must_use]
pub fn status_line(today: &Report, month: &Report) -> String {
    format!(
        "today: ${:.2} ({} msg) | month: ${:.2} ({} msg)",
        today.total_cost, today.total_messages, month.total_cost, month.total_messages,
    )
}

// ── the refresh decision (`cli_helpers/ingest.py`) ───────────────────────────

/// `ensure_fresh(conn, force=…, auto=…)` — **the decision only**.
///
/// Returns `true` when Python would have run the pass. The two early returns
/// are in the reference's order, which matters: `--ingest` skips the staleness
/// probe entirely, so a `--ingest` run does not even open `usage_events`.
///
/// # Errors
/// A SQLite failure on the `MAX(ts)` probe — Python would raise there too.
pub fn ensure_fresh_decision(
    conn: &Connection,
    force: bool,
    auto: bool,
    now_micros: i64,
) -> Result<bool> {
    if !force && !auto {
        return Ok(false);
    }
    if force {
        return Ok(true);
    }
    Ok(is_stale(conn, now_micros)?)
}

/// `is_stale(conn)` — the newest `usage_events.ts` older than the threshold.
///
/// Three "no" answers are reproduced exactly: an empty store (a fresh install
/// must not walk every adapter root on its first CLI call), a `NULL`/empty
/// stamp, and an unparseable one.
///
/// # Errors
/// Any SQLite error. `usage_events` missing is one — the reference raises there
/// as well, uncaught.
pub fn is_stale(conn: &Connection, now_micros: i64) -> rusqlite::Result<bool> {
    let max_ts: Option<String> =
        conn.query_row("SELECT MAX(ts) AS max_ts FROM usage_events", [], |row| {
            row.get(0)
        })?;
    // `if not max_ts_raw` — Python truthiness, so `""` is as absent as `None`.
    let Some(raw) = max_ts.filter(|text| !text.is_empty()) else {
        return Ok(false);
    };
    let Some(stamp) = pytime::parse_iso(&raw) else {
        return Ok(false);
    };
    #[allow(
        clippy::cast_precision_loss,
        reason = "microsecond epochs are exact in f64 out to year 2255"
    )]
    let now = now_micros as f64 / 1_000_000.0;
    Ok(now - stamp > STALENESS_THRESHOLD_HOURS * 3600.0)
}

// ── pricing + the package directory ──────────────────────────────────────────

/// The CLI's pricing engine: `data/models.toml`, and nothing else.
///
/// # Errors
/// When the manifest is missing or unparseable — the same hard failure the
/// Python import would raise, rather than a silently free price list.
pub fn engine_for_cli(package_dir: &Path) -> Result<PricingEngine> {
    let path = package_dir.join("data").join("models.toml");
    PricingEngine::from_manifest_path(&path)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .with_context(|| format!("loading {}", path.display()))
}

/// `deps.BASE_DIR` — the installed package directory.
///
/// Found the way `resume` finds `capabilities.json` (D-3): walk up from the
/// working directory, then from the executable, looking for a checkout that
/// carries `stackunderflow/data/models.toml`. `include_str!` stays banned —
/// a build-time copy would let the two implementations disagree about the
/// rates while the harness swore they agreed. `$STACKUNDERFLOW_PACKAGE_DIR`
/// wins when set, which is how a packaged install will point at its own copy.
#[must_use]
pub fn package_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe = std::env::current_exe().ok();
    resolve_package_dir(
        std::env::var_os("STACKUNDERFLOW_PACKAGE_DIR").as_deref(),
        &cwd,
        exe.as_deref(),
    )
}

/// The pure core of [`package_dir`], with the environment injected.
#[must_use]
pub fn resolve_package_dir(
    raw: Option<&std::ffi::OsStr>,
    cwd: &Path,
    exe: Option<&Path>,
) -> PathBuf {
    if let Some(value) = raw.filter(|value| !value.is_empty()) {
        return PathBuf::from(value);
    }
    let from_exe = exe.and_then(Path::parent);
    for start in [Some(cwd), from_exe].into_iter().flatten() {
        for ancestor in start.ancestors() {
            let candidate = ancestor.join("stackunderflow");
            if candidate.join("data").join("models.toml").is_file() {
                return candidate;
            }
        }
    }
    cwd.join("stackunderflow")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use stax_reports::aggregate::ProjectRow;

    use super::*;

    fn report(label: &str, cost: f64, messages: i64) -> Report {
        Report {
            scope_label: label.to_owned(),
            total_cost: cost,
            total_messages: messages,
            total_sessions: 0,
            by_project: Vec::new(),
        }
    }

    #[test]
    fn the_status_line_is_the_reference_f_string() {
        assert_eq!(
            status_line(
                &report("today", 1.5, 12),
                &report("July 2026", 234.5678, 9_876)
            ),
            "today: $1.50 (12 msg) | month: $234.57 (9876 msg)"
        );
    }

    #[test]
    fn a_zero_report_still_prints_two_decimals() {
        assert_eq!(
            status_line(&report("today", 0.0, 0), &report("July 2026", 0.0, 0)),
            "today: $0.00 (0 msg) | month: $0.00 (0 msg)"
        );
    }

    #[test]
    fn message_counts_are_not_thousands_separated() {
        // `{today['total_messages']}` — no `:,`, unlike `render_text`'s totals.
        assert!(
            status_line(&report("t", 0.0, 1_000_000), &report("m", 0.0, 0))
                .contains("(1000000 msg)")
        );
    }

    #[test]
    fn the_json_body_is_two_keys_in_insertion_order() {
        let mut today = report("today", 0.5, 2);
        today.by_project.push(ProjectRow {
            name: "-x".into(),
            cost: 0.5,
            messages: 2,
            sessions: 1,
        });
        let rendered = render(&today, &report("July 2026", 0.0, 0), "json").stdout;
        assert!(rendered.starts_with("{\n  \"today\": {\n"), "{rendered}");
        assert!(rendered.contains("\n  \"month\": {\n"), "{rendered}");
        assert!(rendered.ends_with("}\n"), "click.echo adds the newline");
        // The report's own key order is the dict-literal order, not sorted.
        let today_block = rendered.split("\"month\"").next().unwrap();
        let scope_at = today_block.find("scope_label").unwrap();
        let by_project_at = today_block.find("by_project").unwrap();
        assert!(scope_at < by_project_at);
    }

    #[test]
    fn the_decision_short_circuits_before_touching_the_store() {
        let conn = Connection::open_in_memory().unwrap();
        // No `usage_events` table at all: `--no-auto-ingest` must still answer
        // "no", because the reference returns before the probe.
        assert!(!ensure_fresh_decision(&conn, false, false, 0).unwrap());
        // …and `--ingest` must answer "yes" without probing either.
        assert!(ensure_fresh_decision(&conn, true, false, 0).unwrap());
        assert!(ensure_fresh_decision(&conn, true, true, 0).unwrap());
        // Only the auto path needs the table.
        assert!(ensure_fresh_decision(&conn, false, true, 0).is_err());
    }

    #[test]
    fn staleness_is_measured_against_the_newest_event() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE usage_events (ts TEXT)", [])
            .unwrap();
        // Empty store → not stale, deliberately.
        assert!(!is_stale(&conn, 0).unwrap());

        conn.execute(
            "INSERT INTO usage_events (ts) VALUES ('2026-07-31T00:00:00+00:00')",
            [],
        )
        .unwrap();
        let base = 1_785_456_000_i64; // 2026-07-31T00:00:00Z, seconds.
        assert!(
            !is_stale(&conn, (base + 5 * 3600) * 1_000_000).unwrap(),
            "5 h < 6 h"
        );
        assert!(
            !is_stale(&conn, (base + 6 * 3600) * 1_000_000).unwrap(),
            "the test is >, not >="
        );
        assert!(is_stale(&conn, (base + 7 * 3600) * 1_000_000).unwrap());
    }

    #[test]
    fn an_unparseable_or_empty_stamp_is_not_stale() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE usage_events (ts TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO usage_events (ts) VALUES ('')", [])
            .unwrap();
        assert!(!is_stale(&conn, i64::MAX / 2).unwrap());
        conn.execute("DELETE FROM usage_events", []).unwrap();
        conn.execute("INSERT INTO usage_events (ts) VALUES ('not a date')", [])
            .unwrap();
        assert!(!is_stale(&conn, i64::MAX / 2).unwrap());
    }

    #[test]
    fn the_package_dir_env_wins_and_the_walk_up_finds_the_checkout() {
        assert_eq!(
            resolve_package_dir(Some(OsStr::new("/pkg")), Path::new("/tmp"), None),
            PathBuf::from("/pkg")
        );
        assert_eq!(
            resolve_package_dir(Some(OsStr::new("")), Path::new("/tmp"), None),
            PathBuf::from("/tmp/stackunderflow"),
            "an empty value counts as unset and the last resort names something concrete"
        );
    }

    #[test]
    fn the_repo_checkout_carries_the_manifest_this_binary_prices_with() {
        // Not a tautology: it proves the walk-up actually resolves from the
        // test binary's own location, which is where a `cargo test` run of the
        // real command would look.
        let dir = package_dir();
        assert!(
            dir.join("data").join("models.toml").is_file(),
            "package dir resolved to {dir:?}"
        );
        engine_for_cli(&dir).expect("the manifest parses");
    }

    #[test]
    fn last_wins_on_the_auto_ingest_pair() {
        use clap::Parser as _;

        #[derive(clap::Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: StatusArgs,
        }

        let on = Wrapper::try_parse_from(["x", "--no-auto-ingest", "--auto-ingest"]).unwrap();
        assert!(on.args.auto(), "the last flag wins, as Click's parser does");
        let off = Wrapper::try_parse_from(["x", "--auto-ingest", "--no-auto-ingest"]).unwrap();
        assert!(!off.args.auto());
        let default = Wrapper::try_parse_from(["x"]).unwrap();
        assert!(default.args.auto(), "the pair defaults to on");
    }
}
