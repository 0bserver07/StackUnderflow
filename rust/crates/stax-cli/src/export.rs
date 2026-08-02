//! `stax export` — `cli.py:3229`–`:3311`.
//!
//! The only WRITER in the tranche-3 family: it renders through the already-ported
//! `stax_reports::export::run_export` and then puts the bytes on disk through
//! `safe_write_text`. Two `ClickException` funnels, one echo line, and that is
//! the whole verb.
//!
//! # Both of the verb's error paths are `ClickException`, not `UsageError`
//!
//! `run_export`'s `ValueError` (an unknown period or format — unreachable
//! through the two `click.Choice`s, kept because the service is also called
//! from HTTP) and `safe_write_text`'s `FileExistsError` are both re-raised as
//! `click.ClickException`, i.e. `Error: <message>` on stderr at exit **1** with
//! no usage block. The refusal a user actually hits — "already exists. Pass
//! --force to overwrite." — is therefore exit 1, not exit 2.
//!
//! # `-f` and `-o` are `required=True`, which is clap's error, not Click's
//!
//! Omitting either is a parser error on both sides at exit 2 with different
//! stderr — the pre-existing class (DIV-235 / DIV-240 / DIV-260), not a new
//! divergence. `click.Path(dir_okay=False)` additionally rejects an EXISTING
//! directory inside Click's own type conversion (`Error: Invalid value for '-o'
//! / '--output': File '<x>' is a directory.`, exit 2); clap has no such check
//! and the port reaches `safe_write_text`, which fails differently. Filed as
//! **DIV-372** rather than half-implemented, because closing it means a
//! Click-shaped parameter-error renderer — which is exactly what DIV-240 put on
//! the maintainer's desk.
//!
//! # The written file is the proof
//!
//! Every `export` parity row uses a case-local `@home` and writes *into* it, so
//! the harness's `diff -r` compares the CSV/JSON bytes the two implementations
//! produced — not merely the one line they printed. A row that wrote to the
//! shared state would have had the second run trip over the first run's file.

use anyhow::Result;
use clap::Args;
use stax_reports::export::{ExportError, WriteError, run_export, safe_write_text};
use stax_reports::scope::Instant;

use crate::click::Output;
use crate::reports::{IngestFlags, click_exception, guard_refresh, open_store};
use crate::status::{engine_for_cli, package_dir};

/// `stax export`.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Output format.
    #[arg(short = 'f', long = "format", value_name = "FMT", required = true,
          value_parser = ["csv", "json"])]
    pub format: String,
    /// Destination file path.
    #[arg(
        short = 'o',
        long = "output",
        value_name = "OUTPUT",
        required = true,
        allow_hyphen_values = true
    )]
    pub output: String,
    /// Window. Omit to roll up today + 7 days + 30 days into one file.
    #[arg(short = 'p', long = "period", value_name = "PERIOD",
          value_parser = ["today", "week", "month", "all"])]
    pub period: Option<String>,
    /// Filter by provider (e.g. claude, codex, cursor).
    #[arg(long = "provider", value_name = "PROVIDER")]
    pub provider: Option<String>,
    /// Include only this project slug (repeatable).
    #[arg(long = "project", value_name = "PROJECT", allow_hyphen_values = true)]
    pub include: Vec<String>,
    /// Exclude this project slug (repeatable).
    #[arg(long = "exclude", value_name = "EXCLUDE", allow_hyphen_values = true)]
    pub exclude: Vec<String>,
    /// Overwrite the output file if it already exists.
    #[arg(long = "force", action = clap::ArgAction::SetTrue)]
    pub force: bool,
    /// `--ingest` / `--auto-ingest`.
    #[command(flatten)]
    pub ingest: IngestFlags,
}

/// Run `export`.
///
/// # Errors
/// A missing store (DIV-239), a SQLite failure inside the builder, the unported
/// refresh pass (DIV-238), or a non-`FileExistsError` IO failure on the write —
/// the last of which is a traceback on the reference and an `Err` here.
pub fn run_export_cmd(args: &ExportArgs) -> Result<Output> {
    let conn = open_store()?;
    guard_refresh(&conn, &args.ingest)?;
    let engine = engine_for_cli(&package_dir())?;

    // `list(include) or None` / `list(exclude) or None`.
    let include = (!args.include.is_empty()).then_some(args.include.as_slice());
    let exclude = (!args.exclude.is_empty()).then_some(args.exclude.as_slice());

    let export = match run_export(
        &conn,
        &engine,
        &args.format,
        args.period.as_deref(),
        args.provider.as_deref(),
        include,
        exclude,
        &Instant::now_utc,
    ) {
        Ok(export) => export,
        // `except ValueError as e: raise click.ClickException(str(e))`. An
        // Internal error is what would have propagated as a traceback, so it
        // stays an `Err` rather than being dressed up as a friendly message.
        Err(ExportError::Value(message)) => return Ok(click_exception(&message)),
        Err(err @ ExportError::Internal(_)) => return Err(anyhow::anyhow!("{err}")),
    };

    // `conn.close()` happens in the `finally`, BEFORE the write — so a store
    // that cannot be closed never blocks the file from being written, and the
    // write itself holds no database handle. Dropping the store here reproduces
    // the ordering (DIV-259's lesson: an open handle at exit is an artifact).
    drop(conn);

    match safe_write_text(std::path::Path::new(&args.output), &export.text, args.force) {
        Ok(()) => {}
        Err(WriteError::Exists(message)) => return Ok(click_exception(&message)),
        Err(err @ WriteError::Io(_)) => return Err(anyhow::anyhow!("{err}")),
    }

    // `click.echo(f"  wrote {output}")` — the RAW argument, not the resolved
    // path, so a relative `-o out.csv` prints `  wrote out.csv`.
    Ok(Output::ok(format!("  wrote {}\n", args.output)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use clap::Parser as _;

    use super::*;

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        args: ExportArgs,
    }

    /// A fresh scratch directory. `tempfile` is not in the lock and the
    /// campaign builds offline, so this is `cache.rs`'s idiom with a counter —
    /// tests run in parallel inside one process and `process::id()` alone
    /// collides.
    fn scratch(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "stax-export-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn parse(argv: &[&str]) -> ExportArgs {
        let mut all = vec!["x"];
        all.extend_from_slice(argv);
        Wrap::try_parse_from(all).expect("parse").args
    }

    #[test]
    fn format_and_output_are_both_required() {
        assert!(Wrap::try_parse_from(["x"]).is_err());
        assert!(Wrap::try_parse_from(["x", "-f", "csv"]).is_err());
        assert!(Wrap::try_parse_from(["x", "-o", "out.csv"]).is_err());
        assert!(Wrap::try_parse_from(["x", "-f", "csv", "-o", "out.csv"]).is_ok());
    }

    #[test]
    fn the_two_choices_are_the_decorators() {
        assert!(Wrap::try_parse_from(["x", "-f", "text", "-o", "o"]).is_err());
        assert!(Wrap::try_parse_from(["x", "-f", "json", "-o", "o", "-p", "7days"]).is_err());
        let args = parse(&["-f", "json", "-o", "o", "-p", "all"]);
        assert_eq!(args.period.as_deref(), Some("all"));
    }

    #[test]
    fn an_omitted_period_is_none_which_is_the_rollup() {
        let args = parse(&["-f", "csv", "-o", "o"]);
        assert!(
            args.period.is_none(),
            "`default=None` selects build_multi_period_export"
        );
        assert!(!args.force);
    }

    #[test]
    fn the_repeatables_take_real_slugs() {
        let args = parse(&[
            "-f",
            "csv",
            "-o",
            "o",
            "--project",
            "-Users-yad-dev-a",
            "--exclude",
            "-Users-yad-dev-b",
        ]);
        assert_eq!(args.include, vec!["-Users-yad-dev-a".to_owned()]);
        assert_eq!(args.exclude, vec!["-Users-yad-dev-b".to_owned()]);
    }

    #[test]
    fn the_refusal_is_a_click_exception_at_exit_one() {
        let dir = scratch("refuse");
        let target = dir.as_path().join("out.csv");
        std::fs::write(&target, "old").expect("seed");
        let err = safe_write_text(&target, "new", false).expect_err("refuses");
        let WriteError::Exists(message) = err else {
            panic!("expected the FileExistsError shape");
        };
        let out = click_exception(&message);
        assert_eq!(out.code, 1, "ClickException, not BadParameter");
        assert!(out.stdout.is_empty());
        assert!(
            out.stderr
                .ends_with(" already exists. Pass --force to overwrite.\n"),
            "{}",
            out.stderr
        );
        // …and the file is untouched.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    #[test]
    fn force_overwrites_and_leaves_no_tmp_behind() {
        let dir = scratch("force");
        let target = dir.as_path().join("out.csv");
        std::fs::write(&target, "old").expect("seed");
        safe_write_text(&target, "new", true).expect("force writes");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(
            !dir.as_path().join("out.csv.tmp").exists(),
            "`Path.replace` moved the temp file, it did not copy it"
        );
        let entries: Vec<_> = std::fs::read_dir(dir.as_path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one file: {entries:?}");
    }

    #[test]
    fn a_missing_parent_is_created() {
        let dir = scratch("parents");
        let target = dir.as_path().join("a").join("b").join("out.json");
        safe_write_text(&target, "{}", false).expect("mkdir parents=True");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}");
    }

    #[test]
    fn a_symlink_target_is_refused_even_with_force() {
        let dir = scratch("symlink");
        let real = dir.as_path().join("real.csv");
        std::fs::write(&real, "real").expect("seed");
        let link = dir.as_path().join("link.csv");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        for force in [false, true] {
            let err = safe_write_text(&link, "new", force).expect_err("refuses");
            let WriteError::Exists(message) = err else {
                panic!("expected FileExistsError");
            };
            assert!(
                message.starts_with("Refusing to write through symlink: "),
                "{message}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "real",
            "the link's target is never followed"
        );
    }

    #[test]
    fn a_dangling_symlink_is_still_a_symlink() {
        // `is_symlink()` is true where `exists()` is false — the order of the
        // two checks in `safe_write_text` is what makes this the symlink
        // message rather than a silent write through the link.
        let dir = scratch("dangling");
        let link = dir.as_path().join("dangling.csv");
        std::os::unix::fs::symlink(dir.as_path().join("nope"), &link).expect("symlink");
        let err = safe_write_text(&link, "x", true).expect_err("refuses");
        assert!(matches!(err, WriteError::Exists(ref m) if m.contains("through symlink:")));
    }

    #[test]
    fn a_symlinked_temp_path_is_refused_after_the_target_checks() {
        let dir = scratch("tmplink");
        let target = dir.as_path().join("out.csv");
        let tmp = dir.as_path().join("out.csv.tmp");
        std::os::unix::fs::symlink(dir.as_path().join("elsewhere"), &tmp).expect("symlink");
        let err = safe_write_text(&target, "x", false).expect_err("refuses");
        let WriteError::Exists(message) = err else {
            panic!("expected FileExistsError");
        };
        assert!(
            message.starts_with("Refusing to write through symlink temp: "),
            "{message}"
        );
        assert!(!target.exists(), "nothing was written");
    }

    #[test]
    fn the_temp_name_is_the_whole_path_plus_tmp() {
        // `p.with_suffix(p.suffix + ".tmp")` reads like suffix surgery and is
        // a plain append — proven by writing through each shape and asserting
        // no residue is left under any OTHER name.
        for name in ["out.csv", "out", "a.tar.gz", ".hidden"] {
            let dir = scratch("tmpname");
            let target = dir.as_path().join(name);
            safe_write_text(&target, "x", false).expect("writes");
            let mut entries: Vec<String> = std::fs::read_dir(dir.as_path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            entries.sort();
            assert_eq!(entries, vec![name.to_owned()], "residue for {name}");
        }
    }
}
