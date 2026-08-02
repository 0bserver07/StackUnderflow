//! `stax risk file` — `cli.py:4115`–`:4164`, over `services/risk.py` (179 ln).
//!
//! # The blocker that was not there — DIV-385
//!
//! Tranches 3 and 5 both recorded this verb as OPEN with a named blocker:
//! *"needs `services/risk.py::file_risk_summary`, which is NOT the ported
//! `risk::file_risk_overlay` nor `patterns::file_risk`"*. Both halves of that
//! sentence are TRUE and the conclusion is FALSE. The function was ported in
//! **wave 1**, as [`stax_core::queries::file_risk_summary`], because `memory
//! file` reaches the same four counts — and it has been under byte test since,
//! through the ten `B-file-*` / `F-file-*` rows. `stax_reports::risk`'s own
//! module doc says so in its second paragraph; the search that produced the
//! blocker note looked in `stax-reports` (where `services/risk.py`'s *route*
//! adapter lives) and in `patterns`, and stopped.
//!
//! So this file is a RENDERER, like `compare` and `optimize` were, and the
//! generalisation is worth more than the verb: **a "service unported" blocker is
//! a claim about the whole workspace, and it has to be checked against the whole
//! workspace.** `grep -rn file_risk_summary crates/` answers it in one command
//! and was never run — twice, on a note that says "re-verified".
//!
//! # What the verb actually does
//!
//! `_open_store()` (read-write — it creates and migrates, DIV-374), one call,
//! one of two renderers, `conn.close()` in a `finally`. The only branch with any
//! judgement in it is the error funnel: `except ValueError` and **nothing
//! wider**, re-raised as `click.BadParameter(str(exc), param_hint="--since")`.
//! A `sqlite3.DatabaseError` is not caught, so a corrupt store is exit 1 with a
//! traceback and an empty stdout — the same split `memory.rs` pinned in wave 1
//! (`crate::memory::caught`), reached here through [`stax_core::queries::ValueError`].
//!
//! # Two inherited truthiness seams, both rowed
//!
//! * `if summary["since"]:` prints the echo line, and the echo is the **raw**
//!   `--since` string, not the parsed ISO. `--since ''` parses to `None`
//!   (no cutoff) and echoes `''`, which is falsy, so the line is absent — the
//!   twice-proven `--project ''` class (tranche 1 finding 1).
//! * `if summary["recent_session_ids"]:` gates the whole trailing block, so a
//!   file with failures but a `recent_limit` of 0 prints no block at all. The
//!   CLI never passes `recent_limit`, so it is always 5 here; the branch is
//!   crossed by `R-risk-*` rows on files that do and do not have failure modes.

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_core::queries::{self, RiskSummary, ValueError, pyjson};

use crate::click::{Output, PROGRAM, UsageError};
use crate::reports::open_store;

/// `stax risk`.
#[derive(Debug, Args)]
pub struct RiskArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: RiskVerb,
}

/// `risk`'s single leaf.
#[derive(Debug, Subcommand)]
pub enum RiskVerb {
    /// Risk summary for PATH: how many sessions reverted / failed / worked.
    ///
    /// Counts distinct sessions classified by the v0.7.2 outcome heuristic.
    /// ``recent_session_ids`` is the up-to-5 most recent failure-mode
    /// sessions (reverted ∪ failed) for the file.
    File(RiskFileArgs),
}

/// `risk file PATH`.
#[derive(Debug, Args)]
pub struct RiskFileArgs {
    /// `click.Path(file_okay=True, dir_okay=True)` — and `exists` defaults to
    /// `False`, so Click performs **no** filesystem check. A missing path is a
    /// normal run that finds nothing, not an exit-2 parameter error.
    #[arg(value_name = "PATH")]
    pub path: String,
    /// Only sessions whose activity is newer than this. Accepts '7d', '1w',
    /// '1m', '24h', or an ISO date/datetime.
    #[arg(
        long = "since",
        value_name = "TEXT",
        allow_hyphen_values = true,
        help = "Only sessions whose activity is newer than this. \
                Accepts '7d', '1w', '1m', '24h', or an ISO date/datetime."
    )]
    pub since: Option<String>,
    /// Output format.
    #[arg(
        long = "format",
        value_name = "FMT",
        default_value = "text",
        value_parser = ["text", "json"],
        help = "Output format."
    )]
    pub format: String,
}

/// Run `risk`.
///
/// # Errors
/// A store that cannot be opened or migrated, or any SQLite failure inside
/// `file_risk_summary` — Python catches `ValueError` and nothing wider, so
/// those propagate as exit 1 with an empty stdout.
pub fn run_risk(args: &RiskArgs) -> Result<Output> {
    match &args.verb {
        RiskVerb::File(args) => run_risk_file(args),
    }
}

fn run_risk_file(args: &RiskFileArgs) -> Result<Output> {
    let conn = open_store()?;
    // `try: … except ValueError … finally: conn.close()`. The connection is
    // closed on both legs, which is why the summary is computed before any
    // rendering decision is taken.
    let summary = queries::file_risk_summary(&conn, &args.path, args.since.as_deref(), 5);
    drop(conn);
    let summary = match summary {
        Ok(summary) => summary,
        Err(error) => {
            let Some(message) = ValueError::of(&error) else {
                return Err(error);
            };
            return Ok(Output::usage(
                &UsageError::bad_parameter("risk file", "[OPTIONS] PATH", "--since", message),
                PROGRAM,
            ));
        }
    };

    if args.format == "json" {
        // `json.dumps(summary, indent=2)` — no `sort_keys`, so the dict
        // literal's order is the wire order, which `RiskSummary::to_dict`
        // already carries.
        return Ok(Output::ok(format!(
            "{}\n",
            pyjson::dumps_indent2(&summary.to_dict())
        )));
    }
    Ok(Output::ok(render_risk_text(&summary)))
}

/// The text renderer — every literal and every column of padding is `cli.py`'s.
///
/// The three count lines are hand-aligned in the reference with literal runs of
/// spaces (not a computed width), so they are transcribed as literals here too:
/// a `{:<33}` that happened to agree today would silently disagree the moment
/// somebody edited one label.
#[must_use]
pub fn render_risk_text(summary: &RiskSummary) -> String {
    let mut out = format!("File risk for {}\n", summary.path);
    // `if summary["since"]:` — truthiness, so `--since ''` prints nothing.
    if let Some(since) = summary.since.as_deref().filter(|raw| !raw.is_empty()) {
        out.push_str(&format!("  since: {since}\n"));
    }
    out.push('\n');
    out.push_str(&format!(
        "  total sessions touching the file: {}\n",
        summary.total_sessions
    ));
    out.push_str(&format!(
        "  reverted:                         {}\n",
        summary.reverted
    ));
    out.push_str(&format!(
        "  failed:                           {}\n",
        summary.failed
    ));
    out.push_str(&format!(
        "  worked:                           {}\n",
        summary.worked
    ));
    if !summary.recent_session_ids.is_empty() {
        out.push('\n');
        out.push_str("  recent failure-mode sessions:\n");
        for session_id in &summary.recent_session_ids {
            out.push_str(&format!("    - {session_id}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> RiskSummary {
        RiskSummary {
            path: "/x/cost.py".into(),
            since: None,
            total_sessions: 7,
            reverted: 2,
            failed: 1,
            worked: 3,
            recent_session_ids: Vec::new(),
        }
    }

    #[test]
    fn the_clean_shape_is_the_header_a_blank_line_and_four_counts() {
        assert_eq!(
            render_risk_text(&summary()),
            concat!(
                "File risk for /x/cost.py\n",
                "\n",
                "  total sessions touching the file: 7\n",
                "  reverted:                         2\n",
                "  failed:                           1\n",
                "  worked:                           3\n",
            )
        );
    }

    #[test]
    fn the_since_echo_is_the_raw_string_and_is_gated_on_truthiness() {
        let mut with_since = summary();
        with_since.since = Some("30d".into());
        assert!(render_risk_text(&with_since).contains("\n  since: 30d\n"));

        // `--since ''` parses to "no cutoff" and echoes `''` — falsy, so the
        // line is absent even though the key is present in the JSON.
        let mut empty = summary();
        empty.since = Some(String::new());
        assert_eq!(
            render_risk_text(&empty),
            render_risk_text(&summary()),
            "an empty `--since` renders exactly as an absent one"
        );
    }

    #[test]
    fn the_recent_block_is_gated_on_a_non_empty_list() {
        let mut with_recent = summary();
        with_recent.recent_session_ids = vec!["s-1".into(), "s-2".into()];
        let text = render_risk_text(&with_recent);
        assert!(text.ends_with(concat!(
            "  worked:                           3\n",
            "\n",
            "  recent failure-mode sessions:\n",
            "    - s-1\n",
            "    - s-2\n",
        )));
        assert!(
            !render_risk_text(&summary()).contains("recent failure-mode"),
            "an empty list prints no header at all"
        );
    }

    #[test]
    fn the_json_form_is_indent_two_in_the_dicts_own_key_order() {
        let mut full = summary();
        full.since = Some("7d".into());
        full.recent_session_ids = vec!["s-1".into()];
        assert_eq!(
            pyjson::dumps_indent2(&full.to_dict()),
            concat!(
                "{\n",
                "  \"path\": \"/x/cost.py\",\n",
                "  \"since\": \"7d\",\n",
                "  \"total_sessions\": 7,\n",
                "  \"reverted\": 2,\n",
                "  \"failed\": 1,\n",
                "  \"worked\": 3,\n",
                "  \"recent_session_ids\": [\n",
                "    \"s-1\"\n",
                "  ]\n",
                "}"
            ),
            "no sort_keys — `path` leads and `recent_session_ids` trails"
        );
    }

    #[test]
    fn a_malformed_since_is_clicks_bad_parameter_on_the_since_hint() {
        let error = UsageError::bad_parameter(
            "risk file",
            "[OPTIONS] PATH",
            "--since",
            "Invalid since value 'notadate': expected '7d'/'1w'/'1m'/'24h' or an ISO date/datetime.",
        );
        assert_eq!(
            error.render("stackunderflow"),
            concat!(
                "Usage: stackunderflow risk file [OPTIONS] PATH\n",
                "Try 'stackunderflow risk file --help' for help.\n",
                "\n",
                "Error: Invalid value for --since: Invalid since value 'notadate': ",
                "expected '7d'/'1w'/'1m'/'24h' or an ISO date/datetime.\n",
            )
        );
        assert_eq!(Output::usage(&error, PROGRAM).code, 2);
    }
}
