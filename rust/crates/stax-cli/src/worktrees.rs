//! `stax worktrees list | attribute` — `cli.py:4966`–`:5082`.
//!
//! `list` renders exactly what `GET /api/worktrees` returns, because the
//! reference imports the route's own `assemble_worktrees_payload` into the CLI
//! — "so they never disagree", in `cli.py`'s own words. This port keeps that
//! property by calling [`stax_reports::worktrees::assemble_worktrees_payload`],
//! which is where the assembler now lives (see DIV-375 for the duplicate the
//! server route still carries).
//!
//! # `attribute` is the group's WRITER, and it is one line
//!
//! `attribute_fragments(conn)` + `conn.commit()`, then a count. It touches only
//! the additive attribution column on `projects`, never git, and it is
//! idempotent — a second run answers `0`. It needs a read-WRITE connection,
//! which [`crate::reports::open_store`] now is (DIV-374).
//!
//! # The text table is `str.ljust`, not Rich
//!
//! Fixed-width, two spaces between columns, and **every** cell is padded —
//! including the last one, so each line carries trailing spaces the reference
//! also emits. `click.secho(..., bold=True)` writes no escape codes off a
//! terminal, so the header is plain text. `len()` is characters, and so is
//! `ljust`'s width.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::Value;
use stax_reports::scope::Instant;
use stax_reports::worktrees::{
    SystemHost, assemble_worktrees_payload, attribute_fragments, usd_currency_payload,
};

use crate::click::Output;
use crate::compare::sort_keys;
use crate::reports::open_store;

/// `stax worktrees`.
#[derive(Debug, Args)]
pub struct WorktreesArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub verb: WorktreesVerb,
}

/// `worktrees`' two leaves.
#[derive(Debug, Subcommand)]
pub enum WorktreesVerb {
    /// List known worktrees with a verdict: ACTIVE, MERGED_SAFE_TO_PRUNE, or HAS_UNIQUE_WORK.
    ///
    /// Reads the live store and renders the same payload ``GET /api/worktrees``
    /// returns. Works without a running server. Read-only: git is only queried,
    /// never mutated — prune commands ship in the json payload as a preview for
    /// you to run yourself.
    List(ListArgs),
    /// Attribute worktree session fragments to their parent projects.
    ///
    /// Rolls phantom sibling "projects" (worktree session logs) up into the
    /// project that owns them. Writes ONLY the additive attribution column in
    /// the store — never git state. Idempotent: once every fragment is linked,
    /// re-running reports 0 rows updated.
    Attribute,
}

/// `worktrees list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Project log path or repo root to scan; omit to scan every known root.
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub project: Option<String>,
    /// Output format (text or json).
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// Run `worktrees`.
///
/// # Errors
/// A store that cannot be opened or migrated, a non-USD configured currency
/// (DIV-052), or a SQLite failure inside `attribute_fragments`.
pub fn run_worktrees(args: &WorktreesArgs) -> Result<Output> {
    match &args.verb {
        WorktreesVerb::List(args) => run_list(args),
        WorktreesVerb::Attribute => run_attribute(),
    }
}

fn run_list(args: &ListArgs) -> Result<Output> {
    let config = crate::settings::load();
    let configured = crate::settings::get("currency", &config, &crate::settings::ProcessEnv)
        .as_ref()
        .and_then(stax_core::queries::pyjson::Value::as_str)
        .unwrap_or("USD")
        .to_owned();
    let currency = usd_currency_payload(&configured).map_err(|message| anyhow::anyhow!(message))?;

    let conn = open_store()?;
    // `datetime.now(UTC).isoformat()` is read INSIDE the assembler, i.e. after
    // the git fan-out — a scan of thirty worktrees can take seconds and the
    // stamp is the scan's end, not its start. Passed as a THUNK so that stays
    // true: an eagerly-evaluated `Instant::now_utc()` here would be read before
    // the assembler had opened its first `git`, which is what this comment used
    // to claim it was not (DIV-378).
    let payload = assemble_worktrees_payload(
        &conn,
        args.project.as_deref(),
        &SystemHost,
        currency,
        || Instant::now_utc().isoformat(),
    );
    drop(conn);

    if args.format == "json" {
        // `json.dumps(payload, indent=2, sort_keys=True)`.
        return Ok(Output::ok(format!(
            "{}\n",
            stax_reports::render::render_json(&sort_keys(&payload))
        )));
    }
    Ok(Output::ok(render_worktrees_text(&payload)))
}

fn run_attribute() -> Result<Output> {
    let conn = open_store()?;
    let updated = attribute_fragments(&conn);
    drop(conn);
    Ok(Output::ok(format!(
        "Attributed {updated} worktree session fragment(s) to parent projects.\n"
    )))
}

/// `_short_worktree_path(path, max_len=44)`.
///
/// `~`-abbreviate against `$HOME`, then keep the **tail**: `"…" + path[-43:]`.
/// Both `len` and the slice are in characters.
#[must_use]
pub fn short_worktree_path(path: &str, home: Option<&str>, max_len: usize) -> String {
    let mut path = path.to_owned();
    // `if home and path.startswith(home + os.sep)` — the separator is required,
    // so a sibling directory whose name merely starts with `$HOME` is untouched.
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        let prefix = format!("{home}/");
        if path.starts_with(&prefix) {
            // `"~" + path[len(home):]` keeps the separator, giving `~/rest`.
            let rest: String = path.chars().skip(home.chars().count()).collect();
            path = format!("~{rest}");
        }
    }
    let length = path.chars().count();
    if length > max_len {
        let tail: String = path.chars().skip(length - (max_len - 1)).collect();
        return format!("…{tail}");
    }
    path
}

/// `_render_worktrees_text(payload)`.
#[must_use]
pub fn render_worktrees_text(payload: &Value) -> String {
    let empty: Vec<Value> = Vec::new();
    let worktrees = payload
        .get("worktrees")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let summary = payload.get("summary").cloned().unwrap_or(Value::Null);
    // `(payload.get("currency") or {}).get("symbol", "$")`.
    let symbol = payload
        .get("currency")
        .and_then(|node| node.get("symbol"))
        .and_then(Value::as_str)
        .unwrap_or("$")
        .to_owned();

    if worktrees.is_empty() {
        return format!(
            "No worktrees found (scope: {}).\n",
            payload
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("store")
        );
    }

    let home = std::env::home_dir().map(|path| path.to_string_lossy().into_owned());
    const HEADERS: [&str; 7] = [
        "PATH", "BRANCH", "VERDICT", "DIRTY", "UNIQUE", "SESSIONS", "COST",
    ];

    let rows: Vec<[String; 7]> = worktrees
        .iter()
        .map(|worktree| {
            [
                short_worktree_path(str_of(worktree, "path"), home.as_deref(), 44),
                str_of(worktree, "branch").to_owned(),
                str_of(worktree, "verdict").to_owned(),
                int_of(worktree, "dirty_count").to_string(),
                int_of(worktree, "unique_commits").to_string(),
                int_of(worktree, "sessions").to_string(),
                format!("{symbol}{:.2}", float_of(worktree, "cost_usd")),
            ]
        })
        .collect();

    // `max(len(header), *(len(r[i]) for r in rows))` — characters, not bytes.
    let widths: Vec<usize> = (0..HEADERS.len())
        .map(|index| {
            std::iter::once(HEADERS[index].chars().count())
                .chain(rows.iter().map(|row| row[index].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    out.push_str(&join_ljust(&HEADERS.map(str::to_owned), &widths));
    for row in &rows {
        out.push_str(&join_ljust(row, &widths));
    }

    out.push('\n');
    out.push_str(&format!(
        "{} worktree(s) — {} active, {} safe to prune, {} with unique work — \
         attributed cost {symbol}{:.2}\n",
        int_of(&summary, "total"),
        int_of(&summary, "active"),
        int_of(&summary, "safe_to_prune"),
        int_of(&summary, "has_unique_work"),
        float_of(&summary, "attributed_cost_usd"),
    ));
    out.push_str("Prune commands are a preview (see --format json); nothing is deleted for you.\n");
    out
}

/// `"  ".join(cell.ljust(widths[i]) …)` plus the newline `click.echo` adds.
///
/// The LAST cell is padded too, so most lines end in trailing spaces. That is
/// the reference's output, and a "tidier" `trim_end` here would diverge on
/// every row whose final column is not the widest.
fn join_ljust(cells: &[String; 7], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        let used = cell.chars().count();
        let width = widths.get(index).copied().unwrap_or(0);
        line.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    }
    line.push('\n');
    line
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    // `str(wt.get("path", ""))` — a JSON null renders as Python's `None`
    // through `str()`, which is what `branch` does on a detached HEAD.
    match value.get(key) {
        Some(Value::String(text)) => text,
        Some(Value::Null) => "None",
        _ => "",
    }
}

fn int_of(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn float_of(value: &Value, key: &str) -> f64 {
    // `float(wt.get("cost_usd") or 0.0)` — null AND 0.0 both give 0.0.
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn payload(worktrees: Value, summary: Value, scope: &str) -> Value {
        json!({
            "scope": scope,
            "worktrees": worktrees,
            "summary": summary,
            "scanned_at": "2026-08-01T00:00:00+00:00",
            "currency": {"code": "USD", "symbol": "$", "rate_from_usd": 1.0, "warning": null},
        })
    }

    #[test]
    fn an_empty_scan_is_one_line_naming_the_scope() {
        assert_eq!(
            render_worktrees_text(&payload(json!([]), json!({}), "store")),
            "No worktrees found (scope: store).\n"
        );
        assert_eq!(
            render_worktrees_text(&payload(json!([]), json!({}), "/repo")),
            "No worktrees found (scope: /repo).\n"
        );
    }

    #[test]
    fn the_table_pads_every_cell_including_the_last() {
        let worktrees = json!([
            {"path": "/tmp/wt-a", "branch": "feat/x", "verdict": "ACTIVE",
             "dirty_count": 2, "unique_commits": 0, "sessions": 3, "cost_usd": 1.5},
            {"path": "/tmp/wt-bbbbbb", "branch": "main", "verdict": "MERGED_SAFE_TO_PRUNE",
             "dirty_count": 0, "unique_commits": 11, "sessions": 100, "cost_usd": 1234.567},
        ]);
        let summary = json!({
            "total": 2, "safe_to_prune": 1, "has_unique_work": 0, "active": 1,
            "attributed_cost_usd": 1236.067,
        });
        let text = render_worktrees_text(&payload(worktrees, summary, "store"));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "PATH            BRANCH  VERDICT               DIRTY  UNIQUE  SESSIONS  COST    ",
            "the header's last column is ljust-padded to the widest COST cell"
        );
        assert_eq!(
            lines[1],
            "/tmp/wt-a       feat/x  ACTIVE                2      0       3         $1.50   "
        );
        assert_eq!(
            lines[2],
            "/tmp/wt-bbbbbb  main    MERGED_SAFE_TO_PRUNE  0      11      100       $1234.57"
        );
        assert_eq!(lines[3], "");
        assert_eq!(
            lines[4],
            "2 worktree(s) — 1 active, 1 safe to prune, 0 with unique work — \
             attributed cost $1236.07"
        );
        assert_eq!(
            lines[5],
            "Prune commands are a preview (see --format json); nothing is deleted for you."
        );
    }

    #[test]
    fn the_home_abbreviation_needs_the_separator() {
        assert_eq!(
            short_worktree_path("/home/me/dev/wt", Some("/home/me"), 44),
            "~/dev/wt"
        );
        assert_eq!(
            short_worktree_path("/home/meowmix/dev", Some("/home/me"), 44),
            "/home/meowmix/dev",
            "`home + os.sep` — a prefix without the separator is NOT the home"
        );
        assert_eq!(
            short_worktree_path("/home/me", Some("/home/me"), 44),
            "/home/me",
            "the home itself does not start with `home + sep`"
        );
        assert_eq!(
            short_worktree_path("/a/b", None, 44),
            "/a/b",
            "`if home and …` — an unknown home skips the branch"
        );
    }

    #[test]
    fn a_long_path_keeps_the_tail_behind_one_ellipsis() {
        let long = format!("/x/{}", "y".repeat(60));
        let short = short_worktree_path(&long, None, 44);
        assert_eq!(short.chars().count(), 44, "`…` + the last 43 characters");
        assert!(short.starts_with('…'));
        assert!(long.ends_with(&short[short.char_indices().nth(1).unwrap().0..]));
        // Exactly at the limit nothing happens — `>` is strict.
        let exact = "z".repeat(44);
        assert_eq!(short_worktree_path(&exact, None, 44), exact);
    }

    #[test]
    fn the_ellipsis_slice_counts_characters_not_bytes() {
        let long = "é".repeat(60);
        assert_eq!(short_worktree_path(&long, None, 44).chars().count(), 44);
    }

    #[test]
    fn a_null_branch_renders_pythons_none() {
        let worktrees = json!([{
            "path": "/w", "branch": null, "verdict": "ACTIVE",
            "dirty_count": 0, "unique_commits": 0, "sessions": 0, "cost_usd": null,
        }]);
        let text = render_worktrees_text(&payload(worktrees, json!({}), "store"));
        assert!(text.contains("None"), "`str(None)` is `None`:\n{text}");
        assert!(text.contains("$0.00"), "`or 0.0` on a null cost");
    }

    #[test]
    fn the_currency_symbol_comes_from_the_payload_and_defaults_to_a_dollar() {
        let mut value = payload(
            json!([{"path": "/w", "branch": "b", "verdict": "ACTIVE",
                    "dirty_count": 0, "unique_commits": 0, "sessions": 0, "cost_usd": 2.0}]),
            json!({"total": 1, "active": 1, "safe_to_prune": 0, "has_unique_work": 0,
                   "attributed_cost_usd": 2.0}),
            "store",
        );
        value.as_object_mut().unwrap().remove("currency");
        assert!(render_worktrees_text(&value).contains("$2.00"));
    }

    #[test]
    fn the_usd_payload_is_the_four_keys_with_a_float_rate() {
        let payload = usd_currency_payload("USD").expect("USD");
        assert_eq!(
            stax_reports::render::render_json(&payload),
            "{\n  \"code\": \"USD\",\n  \"symbol\": \"$\",\n  \"rate_from_usd\": 1.0,\n  \"warning\": null\n}",
            "`rate_from_usd` is the FLOAT 1.0"
        );
        // A non-ISO code silently becomes USD before resolution.
        assert!(usd_currency_payload("us").is_ok());
        assert!(usd_currency_payload("").is_ok());
        assert!(usd_currency_payload("usd").is_ok(), "uppercased first");
        assert!(usd_currency_payload("EUR").is_err(), "DIV-052");
    }
}
