//! `stax import` — `cli.py:823`–`:900` (RS-8-101), the last portable node.
//!
//! Forty lines of Click over a 1,700-line contract: resolve a manifest, open
//! the store, run the import, print six aligned lines or one JSON object. The
//! weight is all underneath — [`stax_adapters::custom_jsonl`] (the format half
//! and the guarded runner), [`stax_adapters::custom_import`] (the store
//! mapping), [`stax_etl::history_import`] (the orchestration).
//!
//! # The resolution order IS the contract (the RS-8-101 item says so)
//!
//! ```text
//! ./.stackunderflow/history-plugins/<NAME>/stackunderflow-history-plugin.json
//! $STACKUNDERFLOW_HOME/history-plugins/<NAME>/stackunderflow-history-plugin.json
//! ```
//!
//! …*after* the two direct forms: `--history-source` naming an existing file,
//! or a directory holding the canonical filename. `resolve_manifest_path`
//! (wave 2) owns the walk; this module owns the two roots it is handed, in
//! order — project-local first, so a repo can ship its own plugin without
//! touching the user's state dir.
//!
//! # Both error funnels are `ClickException`, not `UsageError`
//!
//! `raise click.ClickException(str(exc))` twice — once around the resolution,
//! once around the import — so every failure is `Error: <message>` on stderr at
//! exit **1**, with an empty stdout and no usage block. That is why the
//! validation messages in `custom_jsonl` are byte contracts: they are the
//! entire output on every rejection leg.
//!
//! # `--history-source` is `required=True`, and that is clap's error, not ours
//!
//! A missing required option is the parser's message on both sides, which is a
//! recorded divergence class already (D-2 in `PARITY-wave1-resume.md`); the
//! matrix rows here start after the parse.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use stax_core::queries::pyjson::{self, Value};

use crate::click::Output;
use crate::reports::click_exception;
use crate::status::{engine_for_cli, package_dir};

/// Where a named `--history-source` resolves (`_HISTORY_PLUGIN_DIRNAME`).
pub const HISTORY_PLUGIN_DIRNAME: &str = "history-plugins";

/// `stax import`.
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// A named history source (resolved under
    /// ./.stackunderflow/history-plugins/ or
    /// ~/.stackunderflow/history-plugins/) or a path to a
    /// stackunderflow-history-plugin.json manifest (file or its directory).
    //
    // The line breaks fall at word boundaries on purpose: clap joins a doc
    // comment's lines with a SPACE, so wrapping mid-path would print
    // `./.stackunderflow/ history-plugins/` — a path the reference does not
    // have, inside the one field `help-tree.sh` compares as an option's text.
    #[arg(long = "history-source", value_name = "NAME|PATH", required = true)]
    pub history_source: String,
    /// Output format.
    #[arg(long = "format", value_name = "FMT", default_value = "text",
          value_parser = ["text", "json"])]
    pub format: String,
}

/// The two search roots, in `cli.py`'s order.
///
/// `Path.cwd()` first and the state dir second, so a repo-local plugin shadows
/// a user-global one of the same name. Injected rather than read here so the
/// order is testable without a process.
#[must_use]
pub fn search_roots(cwd: &Path, state_dir: &Path) -> Vec<PathBuf> {
    vec![
        cwd.join(".stackunderflow").join(HISTORY_PLUGIN_DIRNAME),
        state_dir.join(HISTORY_PLUGIN_DIRNAME),
    ]
}

/// Run `import`.
///
/// # Errors
/// A store that cannot be opened or migrated, or any SQLite error out of the
/// writer — the same set Python lets propagate as a traceback, since its two
/// `except` clauses catch `HistorySourceError` alone.
pub fn run_import(args: &ImportArgs) -> Result<Output> {
    let state_dir = stax_core::settings::app_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let roots = search_roots(&cwd, &state_dir);

    // `Path(name).expanduser()` — `~` against `$HOME`, resolved per call.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let manifest_path = match stax_adapters::custom_import::resolve_manifest_path(
        &args.history_source,
        &roots,
        home.as_deref(),
    ) {
        Ok(path) => path,
        // The FIRST funnel: this one runs before the store is opened, so a
        // bad `--history-source` never creates a store.db.
        Err(message) => return Ok(click_exception(&message)),
    };

    let conn = crate::reports::open_store()?;
    let engine = engine_for_cli(&package_dir())?;
    let ctx = stax_etl::normalize::NormalizeContext::new(engine);
    // `now: Callable[[], float] = time.time` — the ref's `file_mtime` and the
    // cursor sidecar's `updated_at`. A wall clock, so it is injected and never
    // frozen.
    let now = now_seconds();
    let result = stax_etl::history_import::import_history_source(
        &manifest_path,
        &conn,
        &state_dir,
        now,
        &ctx,
        &stax_etl::ingest::SystemClock,
        &stax_etl::history_import::spawn_runner,
    );
    // `finally: conn.close()` — before the render, and before an error
    // propagates (DIV-259: an open handle at exit is an artifact).
    drop(conn);

    let result = match result {
        Ok(result) => result,
        // The SECOND funnel catches `HistorySourceError` only; a store error
        // is a traceback on the reference and an `Err` here.
        Err(error) => {
            let history = error.downcast::<stax_adapters::custom_jsonl::HistorySourceError>()?;
            return Ok(click_exception(&history.to_string()));
        }
    };

    if args.format == "json" {
        return Ok(Output::ok(format!(
            "{}\n",
            pyjson::dumps_indent2(&result_payload(&result))
        )));
    }
    Ok(Output::ok(render_text(&result)))
}

/// `time.time()` — seconds since the epoch, as a float.
fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |delta| delta.as_secs_f64())
}

/// `asdict(result)` — the dataclass field order, which is what `json.dumps`
/// writes and what a `--format json` consumer parses.
#[must_use]
pub fn result_payload(result: &stax_adapters::custom_import::ImportResult) -> Value {
    Value::Object(vec![
        ("source_id".into(), Value::Str(result.source_id.clone())),
        ("provider".into(), Value::Str(result.provider.clone())),
        (
            "projects".into(),
            Value::Array(
                result
                    .projects
                    .iter()
                    .map(|slug| Value::Str(slug.clone()))
                    .collect(),
            ),
        ),
        (
            "sessions_seen".into(),
            Value::Int(i64::try_from(result.sessions_seen).unwrap_or(i64::MAX)),
        ),
        (
            "messages_ingested".into(),
            Value::Int(result.messages_ingested),
        ),
        (
            "file_touches_seen".into(),
            Value::Int(i64::try_from(result.file_touches_seen).unwrap_or(i64::MAX)),
        ),
        (
            "records_validated".into(),
            Value::Int(i64::try_from(result.records_validated).unwrap_or(i64::MAX)),
        ),
        (
            "cursor_before".into(),
            opt_value(result.cursor_before.as_ref()),
        ),
        (
            "cursor_after".into(),
            opt_value(result.cursor_after.as_ref()),
        ),
        (
            "cursor_advanced".into(),
            Value::Bool(result.cursor_advanced),
        ),
    ])
}

/// `None` is JSON `null`, not the empty string.
fn opt_value(value: Option<&String>) -> Value {
    value.map_or(Value::Null, |text| Value::Str(text.clone()))
}

/// The six (or seven) `click.echo` lines, with their exact column alignment.
///
/// The labels are padded in the SOURCE — `"  projects:          "` is a literal
/// with its own spaces, not a computed width — so the alignment is inherited
/// character for character rather than re-derived.
#[must_use]
pub fn render_text(result: &stax_adapters::custom_import::ImportResult) -> String {
    let mut out = format!(
        "Imported history source {} (provider: {})\n",
        py_repr(&result.source_id),
        result.provider,
    );
    // `', '.join(result.projects) or '(none)'` — truthiness, so an EMPTY join
    // (no projects at all) prints the placeholder. DIV-234's class.
    let joined = result.projects.join(", ");
    let projects = if joined.is_empty() { "(none)" } else { &joined };
    out.push_str(&format!("  projects:          {projects}\n"));
    out.push_str(&format!("  sessions:          {}\n", result.sessions_seen));
    out.push_str(&format!(
        "  messages ingested: {}\n",
        result.messages_ingested
    ));
    out.push_str(&format!(
        "  file touches:      {}\n",
        result.file_touches_seen
    ));
    out.push_str(&format!(
        "  records validated: {}\n",
        result.records_validated
    ));
    if result.cursor_advanced {
        out.push_str(&format!(
            "  cursor advanced:   {} -> {}\n",
            opt_repr(result.cursor_before.as_ref()),
            opt_repr(result.cursor_after.as_ref()),
        ));
    } else {
        out.push_str(&format!(
            "  cursor:            unchanged ({})\n",
            opt_repr(result.cursor_after.as_ref()),
        ));
    }
    out
}

/// `{value!r}` for an `str | None` — `None` without quotes, a string with them.
fn opt_repr(value: Option<&String>) -> String {
    value.map_or_else(|| "None".to_owned(), |text| py_repr(text))
}

/// `repr(str)` — single quotes unless the string itself carries one.
fn py_repr(text: &str) -> String {
    stax_adapters::pyval::py_repr(&serde_json::Value::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use stax_adapters::custom_import::ImportResult;

    fn result() -> ImportResult {
        ImportResult {
            source_id: "amp".into(),
            provider: "custom".into(),
            projects: vec!["amp".into(), "amp--app".into()],
            sessions_seen: 2,
            messages_ingested: 7,
            file_touches_seen: 1,
            records_validated: 8,
            cursor_before: Some("page-1".into()),
            cursor_after: Some("page-2".into()),
            cursor_advanced: true,
        }
    }

    #[test]
    fn the_text_block_is_clicks_seven_lines() {
        assert_eq!(
            render_text(&result()),
            concat!(
                "Imported history source 'amp' (provider: custom)\n",
                "  projects:          amp, amp--app\n",
                "  sessions:          2\n",
                "  messages ingested: 7\n",
                "  file touches:      1\n",
                "  records validated: 8\n",
                "  cursor advanced:   'page-1' -> 'page-2'\n",
            )
        );
    }

    #[test]
    fn an_unmoved_cursor_takes_the_other_branch_and_none_has_no_quotes() {
        let mut result = result();
        result.cursor_advanced = false;
        result.cursor_before = None;
        result.cursor_after = None;
        result.projects = Vec::new();
        let text = render_text(&result);
        assert!(text.contains("  projects:          (none)\n"), "{text}");
        assert!(
            text.contains("  cursor:            unchanged (None)\n"),
            "{text}"
        );
        assert!(!text.contains("cursor advanced"), "{text}");
    }

    #[test]
    fn the_json_payload_is_the_dataclasss_field_order() {
        let payload = result_payload(&result());
        let rendered = pyjson::dumps_indent2(&payload);
        assert_eq!(
            rendered,
            concat!(
                "{\n",
                "  \"source_id\": \"amp\",\n",
                "  \"provider\": \"custom\",\n",
                "  \"projects\": [\n",
                "    \"amp\",\n",
                "    \"amp--app\"\n",
                "  ],\n",
                "  \"sessions_seen\": 2,\n",
                "  \"messages_ingested\": 7,\n",
                "  \"file_touches_seen\": 1,\n",
                "  \"records_validated\": 8,\n",
                "  \"cursor_before\": \"page-1\",\n",
                "  \"cursor_after\": \"page-2\",\n",
                "  \"cursor_advanced\": true\n",
                "}"
            )
        );
        // `None` is `null`, and an empty projects list is `[]` on one line.
        let mut bare = result();
        bare.cursor_before = None;
        bare.projects = Vec::new();
        let rendered = pyjson::dumps_indent2(&result_payload(&bare));
        assert!(rendered.contains("\"cursor_before\": null"), "{rendered}");
        assert!(rendered.contains("\"projects\": []"), "{rendered}");
    }

    #[test]
    fn the_search_roots_are_project_local_then_global() {
        let roots = search_roots(Path::new("/repo"), Path::new("/home/u/.stackunderflow"));
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/repo/.stackunderflow/history-plugins"),
                PathBuf::from("/home/u/.stackunderflow/history-plugins"),
            ]
        );
    }

    #[test]
    fn a_missing_source_is_a_click_exception_before_the_store_is_touched() {
        // The message is `resolve_manifest_path`'s, and the exit code is 1 —
        // `ClickException`, not `BadParameter`'s 2.
        let out = click_exception("no history-source manifest for 'nope'. …");
        assert_eq!(out.code, 1);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.starts_with("Error: "));
    }
}
