//! `stax anchor` — the agent-continuity surface (`RS-1-030`..`RS-1-032`).
//!
//! Three verbs over the append-only sidecar in [`stax_core::anchor`]:
//!
//! ```text
//! stax anchor set <key> [<text>…] [--file <path>]   # append; stdin when neither
//! stax anchor get [<key>] [--json]                  # newest per key, or one key
//! stax anchor log <key> [--json]                    # one key, oldest → newest
//! ```
//!
//! Everything ambient — `$STAX_ANCHOR_DB`, `$CLAUDE_SESSION_ID`, the working
//! directory, the clock — is read exactly once, here at the edge, and handed to
//! the core as arguments (the wave-1 pattern law: `set_var` is unsafe in Rust
//! 2024 and the workspace forbids unsafe, so nothing below this file may consult
//! the environment).
//!
//! REGISTRATION — this wave's architect owns `lib.rs`, so the four edits it
//! needs are stated here rather than made:
//!
//! ```text
//! mod anchor;                                                   // beside `mod store;`
//! pub use anchor::{AnchorArgs, AnchorCommand, run_anchor};      // beside the status re-export
//!
//! pub enum Command {
//!     /// Keyed, append-only campaign state that survives a context rotation.
//!     Anchor(AnchorArgs),
//!     Store(StoreArgs),
//! }
//!
//! match &cli.command {
//!     Command::Anchor(args) => run_anchor(args),
//!     Command::Store(args) => run_store(args),
//! }
//! ```
//!
//! `lib.rs`'s existing `status_takes_an_optional_store_path` test destructures
//! `Command` irrefutably (`let Command::Store(args) = &cli.command;`); adding a
//! second variant turns that into a `let … else` or a `match`. Two lines, and
//! [`stax_core::anchor`] needs `pub mod anchor;` in its own `lib.rs`.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use stax_core::anchor::{
    ANCHOR_DB_ENV, AnchorDb, Clock, EnvelopeCommand, SESSION_HINT_ENV, SystemClock, render_json,
    render_set_receipt, render_text, resolve_db_path,
};

/// `stax anchor` — keyed, append-only campaign state.
#[derive(Debug, Args)]
pub struct AnchorArgs {
    /// Anchor sidecar to use. Defaults to `$STAX_ANCHOR_DB`, else
    /// `./.stax-anchors.db` in the working directory.
    ///
    /// `global` so both spellings work: an agent writes
    /// `anchor --db X set k v` as readily as `anchor set k v --db X`.
    #[arg(long, value_name = "PATH", global = true)]
    pub db: Option<PathBuf>,

    /// The verb.
    #[command(subcommand)]
    pub command: AnchorCommand,
}

/// The three anchor verbs.
#[derive(Debug, Subcommand)]
pub enum AnchorCommand {
    /// Append an anchor under `<key>`.
    Set(AnchorSetArgs),
    /// Print the newest anchor per key, or one key's newest.
    Get(AnchorGetArgs),
    /// Print one key's whole history, oldest first.
    Log(AnchorLogArgs),
}

/// Arguments for `stax anchor set`.
#[derive(Debug, Args)]
pub struct AnchorSetArgs {
    /// The key to append under — `architect-state`, `wave-state`, …
    pub key: String,

    /// The body. Unquoted words are joined with single spaces, so
    /// `anchor set wave-state wave 1 fanning out` needs no quoting.
    ///
    /// A plain multi-value positional rather than `trailing_var_arg`: the
    /// latter would swallow a trailing `--db` into the body.
    #[arg(value_name = "TEXT")]
    pub text: Vec<String>,

    /// Read the body from a file instead, byte-verbatim.
    #[arg(long, value_name = "PATH", conflicts_with = "text")]
    pub file: Option<PathBuf>,
}

/// Arguments for `stax anchor get`.
#[derive(Debug, Args)]
pub struct AnchorGetArgs {
    /// One key. Omit it for every key's newest anchor, key-sorted.
    pub key: Option<String>,

    /// Emit the `staxtrace.anchor/1` envelope instead of text.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `stax anchor log`.
#[derive(Debug, Args)]
pub struct AnchorLogArgs {
    /// The key whose history to print.
    pub key: String,

    /// Emit the `staxtrace.anchor/1` envelope instead of text.
    #[arg(long)]
    pub json: bool,
}

/// Run `stax anchor …`.
///
/// # Errors
/// When the sidecar cannot be opened, the body is empty or unreadable, or a
/// query fails. A non-zero exit is the contract for an empty body
/// (`RS-1-030`): the process exits through `main`'s `ExitCode::FAILURE`.
pub fn run_anchor(args: &AnchorArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving the working directory")?;
    let configured = std::env::var_os(ANCHOR_DB_ENV);
    let db_path = resolve_db_path(
        args.db.as_deref(),
        configured.as_deref(),
        &cwd,
        home_dir().as_deref(),
    );

    match &args.command {
        AnchorCommand::Set(set) => run_set(set, &db_path, &SystemClock, session_hint().as_deref()),
        AnchorCommand::Get(get) => run_get(get, &db_path, &SystemClock),
        AnchorCommand::Log(log) => run_log(log, &db_path, &SystemClock),
    }
}

/// `anchor set` — append, then print a one-line receipt.
fn run_set(
    args: &AnchorSetArgs,
    db_path: &Path,
    clock: &dyn Clock,
    session_hint: Option<&str>,
) -> Result<()> {
    let body = read_body(args)?;
    let db = AnchorDb::open_or_create(db_path)?;
    let stored = db.append(&args.key, &body, session_hint, clock)?;
    print!("{}", render_set_receipt(&stored, db.path()));
    Ok(())
}

/// `anchor get` — newest per key, or one key's newest.
fn run_get(args: &AnchorGetArgs, db_path: &Path, clock: &dyn Clock) -> Result<()> {
    let anchors = match AnchorDb::open_existing(db_path)? {
        None => Vec::new(),
        Some(db) => match &args.key {
            Some(key) => db.newest(key)?.into_iter().collect(),
            None => db.newest_per_key()?,
        },
    };
    emit(
        EnvelopeCommand::Get,
        args.json,
        db_path,
        args.key.as_deref(),
        &anchors,
        clock,
    );
    Ok(())
}

/// `anchor log` — one key, oldest → newest.
fn run_log(args: &AnchorLogArgs, db_path: &Path, clock: &dyn Clock) -> Result<()> {
    let anchors = match AnchorDb::open_existing(db_path)? {
        None => Vec::new(),
        Some(db) => db.history(&args.key)?,
    };
    emit(
        EnvelopeCommand::Log,
        args.json,
        db_path,
        Some(args.key.as_str()),
        &anchors,
        clock,
    );
    Ok(())
}

/// Print a result set, as the envelope or as text.
///
/// An empty result is a success either way — an unknown key and a directory
/// with no sidecar both read as "nothing anchored", which is what keeps a
/// `SessionStart` hook from failing a session. In text mode the reason goes to
/// *stderr* so stdout stays exactly the anchored bytes and
/// `anchor get <key> > file` round-trips; in `--json` mode stdout is the
/// envelope and stderr stays silent, so a machine consumer sees one clean
/// stream.
fn emit(
    command: EnvelopeCommand,
    json: bool,
    db_path: &Path,
    key: Option<&str>,
    anchors: &[stax_core::anchor::Anchor],
    clock: &dyn Clock,
) {
    if json {
        print!(
            "{}",
            render_json(command, db_path, &clock.now(), key, anchors)
        );
    } else if anchors.is_empty() {
        eprintln!("no anchors in {}", db_path.display());
    } else {
        print!("{}", render_text(anchors));
    }
}

/// The body for `anchor set`: the positional text, `--file`, else stdin.
///
/// Stdin is only consulted when it is a pipe. A bare `anchor set key` at a
/// terminal is a mistake, and blocking on a tty is the worst possible answer for
/// the caller this feature exists for — an agent would hang until it timed out.
fn read_body(args: &AnchorSetArgs) -> Result<String> {
    if let Some(path) = &args.file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("reading the anchor body from {}", path.display()));
    }
    if !args.text.is_empty() {
        return Ok(args.text.join(" "));
    }
    let stdin = std::io::stdin();
    anyhow::ensure!(
        !stdin.is_terminal(),
        "no body given: pass <TEXT>, --file <PATH>, or pipe the body on stdin"
    );
    std::io::read_to_string(stdin).context("reading the anchor body from stdin")
}

/// The session breadcrumb, best-effort: `$CLAUDE_SESSION_ID` when a live agent
/// session exports it, `None` otherwise. Nothing keys off it.
fn session_hint() -> Option<String> {
    std::env::var(SESSION_HINT_ENV).ok()
}

/// The user's home directory, for `~` in `$STAX_ANCHOR_DB`.
fn home_dir() -> Option<PathBuf> {
    #[allow(
        deprecated,
        reason = "std::env::home_dir is the platform-correct answer \
        on the 1.97.1 pin; the 2018-era deprecation is scheduled for removal upstream"
    )]
    std::env::home_dir()
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    /// A standalone parser over [`AnchorArgs`], so the clap surface is pinned
    /// here rather than through `lib.rs` — which this wave's architect owns.
    /// `stax anchor …` and `anchor …` parse identically below the verb.
    #[derive(Debug, Parser)]
    #[command(name = "anchor")]
    struct AnchorCli {
        #[command(flatten)]
        anchor: AnchorArgs,
    }

    fn parse(argv: &[&str]) -> AnchorArgs {
        AnchorCli::try_parse_from(argv)
            .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"))
            .anchor
    }

    #[test]
    fn the_clap_definition_is_well_formed() {
        AnchorCli::command().debug_assert();
    }

    #[test]
    fn set_joins_unquoted_words_and_keeps_a_quoted_argument_whole() {
        let args = parse(&["anchor", "set", "wave-state", "wave", "1", "fanning", "out"]);
        let AnchorCommand::Set(set) = &args.command else {
            panic!("expected set");
        };
        assert_eq!(set.key, "wave-state");
        assert_eq!(set.text, ["wave", "1", "fanning", "out"]);
        assert_eq!(read_body(set).expect("a body"), "wave 1 fanning out");

        let args = parse(&["anchor", "set", "wave-state", "wave 0 gated 69fb328"]);
        let AnchorCommand::Set(set) = &args.command else {
            panic!("expected set");
        };
        assert_eq!(read_body(set).expect("a body"), "wave 0 gated 69fb328");
    }

    #[test]
    fn the_db_flag_is_global_so_it_may_precede_or_follow_the_verb() {
        for argv in [
            ["anchor", "--db", "/w/a.db", "get"].as_slice(),
            ["anchor", "get", "--db", "/w/a.db"].as_slice(),
        ] {
            let args = parse(argv);
            assert_eq!(args.db.as_deref(), Some(Path::new("/w/a.db")), "{argv:?}");
        }
    }

    #[test]
    fn set_still_sees_the_db_flag_after_its_body() {
        // The body is a multi-value positional rather than `trailing_var_arg`
        // precisely so this keeps working: a trailing-var-arg body would
        // swallow `--db` and anchor the literal text "… --db /w/a.db".
        let args = parse(&["anchor", "set", "k", "some", "body", "--db", "/w/a.db"]);
        assert_eq!(args.db.as_deref(), Some(Path::new("/w/a.db")));
        let AnchorCommand::Set(set) = &args.command else {
            panic!("expected set");
        };
        assert_eq!(read_body(set).expect("a body"), "some body");
    }

    #[test]
    fn a_body_and_a_file_are_mutually_exclusive() {
        let error = AnchorCli::try_parse_from(["anchor", "set", "k", "text", "--file", "/w/b.md"])
            .expect_err("--file and <TEXT> must conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn get_takes_an_optional_key_and_log_requires_one() {
        let args = parse(&["anchor", "get"]);
        let AnchorCommand::Get(get) = &args.command else {
            panic!("expected get");
        };
        assert!(get.key.is_none() && !get.json);

        let args = parse(&["anchor", "get", "architect-state", "--json"]);
        let AnchorCommand::Get(get) = &args.command else {
            panic!("expected get");
        };
        assert_eq!(get.key.as_deref(), Some("architect-state"));
        assert!(get.json);

        let args = parse(&["anchor", "log", "architect-state", "--json"]);
        let AnchorCommand::Log(log) = &args.command else {
            panic!("expected log");
        };
        assert_eq!(log.key, "architect-state");
        assert!(log.json);

        let error = AnchorCli::try_parse_from(["anchor", "log"]).expect_err("log needs a key");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    fn set_args(text: &[&str], file: Option<&str>) -> AnchorSetArgs {
        AnchorSetArgs {
            key: "k".to_string(),
            text: text.iter().map(ToString::to_string).collect(),
            file: file.map(PathBuf::from),
        }
    }

    #[test]
    fn unquoted_words_become_one_space_separated_body() {
        let body = read_body(&set_args(&["wave", "1", "fanning", "out"], None))
            .expect("a positional body needs no io");
        assert_eq!(body, "wave 1 fanning out");
    }

    #[test]
    fn a_single_quoted_argument_is_kept_exactly() {
        let body = read_body(&set_args(&["wave 0  gated  69fb328"], None)).expect("a body");
        assert_eq!(body, "wave 0  gated  69fb328");
    }

    #[test]
    fn a_missing_file_is_an_error_that_names_the_path() {
        let error = read_body(&set_args(&[], Some("/nonexistent/anchor-body.md")))
            .expect_err("a missing --file must fail");
        assert!(
            error.to_string().contains("/nonexistent/anchor-body.md"),
            "{error}"
        );
    }
}
