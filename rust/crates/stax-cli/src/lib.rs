//! The command surface: clap, and eventually the 79 commands Python exposes.
//!
//! Charter (`docs/specs/rust-port.md` §3): port `cli.py` — the same command
//! names, the same flags, the same output shapes, so a script written against
//! the Python CLI keeps working when `stax` replaces it. Wave 8 owns the long
//! tail and the `--help`-tree diff; wave 0 ships exactly one command, `store` (né `status` — DIV-025 ruling),
//! whose entire job is to be checkable against Python on the real store.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod anchor;
mod ask;
mod backup;
mod cache;
mod cfg;
mod click;
mod discovery;
mod embeddings;
mod memory;
mod pyclock;
mod resume;
pub mod settings;
mod status;
mod store;
mod sync;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

pub use anchor::{AnchorArgs, AnchorCommand, run_anchor};
pub use ask::{hybrid_env_from_process, run_ask};
pub use backup::{BackupArgs, BackupVerb, run_backup};
pub use cache::{ClearCacheArgs, run_clear_cache};
pub use cfg::{CfgArgs, CfgVerb, ConfigArgs, ConfigVerb, ModelAliasVerb, run_cfg, run_config};
pub use click::{Output, UsageError};
pub use discovery::{
    ActionWorkedArgs, FailureModesArgs, InPathArgs, PastDecisionsArgs, TouchingFileArgs,
    run_action_worked, run_failure_modes, run_in_path, run_past_decisions, run_touching_file,
};
pub use memory::{MemoryArgs, MemoryVerb, run_memory};
pub use resume::{ResumeArgs, ResumeEnv, run_resume};
pub use status::{StatusArgs, run_status};
pub use store::{StoreArgs, render_store, run_store};
pub use sync::{SyncArgs, SyncInitArgs, SyncJsonArgs, SyncVerb, run_sync};

/// `stax` — the Rust port of StackUnderflow.
///
/// `about` is set explicitly rather than taken from this doc comment: the
/// `--help`-tree differ compares the root summary against Click's, and Click
/// prints `cli.__doc__`. Matching it is drop-in parity (the P0 directive); the
/// string is the maintainer's to change, in `cli.py` first.
#[derive(Debug, Parser)]
#[command(
    name = "stax",
    version,
    about = "StackUnderflow — a local-first knowledge base for your AI coding sessions.",
    long_about = None
)]
pub struct Cli {
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every command `stax` understands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Keyed, append-only campaign state that survives a context rotation.
    Anchor(AnchorArgs),
    /// Back up and restore session data from every registered coding agent.
    Backup(BackupArgs),
    /// View or change persistent settings.
    Cfg(CfgArgs),
    /// Clear cached data.  Use ``start --fresh`` for a clean boot.
    #[command(name = "clear-cache")]
    ClearCache(ClearCacheArgs),
    /// The pre-`cfg` spelling, kept working and kept out of every listing.
    ///
    /// `about = ""` is deliberate and is a parity fact, not an oversight:
    /// `config_compat` and its three subcommands have **no docstring** in
    /// `cli.py`, so Click prints no summary at all. A helpful Rust summary here
    /// would be the only text in the tree the reference does not have.
    #[command(hide = true, about = "", long_about = None)]
    Config(ConfigArgs),
    /// List sessions where editing FILE led to a follow-up correction.
    ///
    /// Surfaces the sessions where a past edit to FILE was followed by the
    /// user reporting it broke, the agent reverting it (``git revert`` /
    /// ``git reset --hard`` / ``git checkout --``), or a complaint — each
    /// with the evidence (the triggering message) plus an
    /// ``outcome_confidence`` in [0.0, 1.0]. Rows below ``--min-confidence``
    /// (default 0.5) are filtered out. The companion of
    /// ``find-sessions-where-action-worked``: use this to learn why an edit
    /// went wrong, that one to learn how a successful change was done.
    FindFailureModesForFile(FailureModesArgs),
    /// List sessions whose project root is PATH or any ancestor of PATH.
    ///
    /// Useful when an agent is working in /a/b/c and wants to know what
    /// has happened in the project rooted at /a/b. The match is
    /// ancestor-only — projects rooted *below* PATH do not match.
    FindSessionsInPath(InPathArgs),
    /// List sessions where FILE shows up in tool calls or message text.
    FindSessionsTouchingFile(TouchingFileArgs),
    /// List sessions where ACTION was performed and the next user turn confirmed it worked.
    ///
    /// ACTION is matched as a substring against tool calls and message text,
    /// so it can be a tool name ("Edit"), a file fragment ("cost.py"), or a
    /// phrase from the conversation ("add caching"). For each session the
    /// *last* matching message is the anchor; the outcome is inferred from
    /// the following user turns (an explicit "thanks"/"that worked", an
    /// agent revert command, or — at lower confidence — no signal at all
    /// before the session ended). Each row carries an ``outcome_confidence``
    /// in [0.0, 1.0]; rows below ``--min-confidence`` (default 0.5) are
    /// filtered out. Pair with ``find-failure-modes-for-file`` to see where
    /// an edit went wrong.
    FindSessionsWhereActionWorked(ActionWorkedArgs),
    /// Ask the local store what past sessions already know.
    ///
    /// ``memory`` is the agent-facing namespace: one set of commands, one
    /// output contract. Run any subcommand with ``--json`` to get the stable,
    /// token-bounded agent-output envelope an agent can splice straight into
    /// its context window; without it you get a human-readable summary.
    ///
    /// Every subcommand shares ``--format`` / ``--json``, ``--project``,
    /// ``--since``, ``--limit`` and ``--context-budget``. ``--project``
    /// defaults to the current directory's project when StackUnderflow
    /// recognises it, so these commands Just Work when run inside a repo.
    Memory(MemoryArgs),
    /// Session/resume ids for every coding agent under PATH (default: cwd).
    ///
    /// Groups recent sessions by provider and renders each agent's real resume
    /// invocation (templates are data in ``adapters/capabilities.json``, verified
    /// against the actual CLIs — e.g. ``claude --resume <id>``, ``codex resume
    /// <id>``). Matching is bidirectional: standing inside a project finds it,
    /// and giving a workspace folder lists every project underneath. Read-only;
    /// agents whose CLI has no known resume command still list their session
    /// ids.
    Resume(ResumeArgs),
    /// Substring-search QUERY across past message content; return matching
    /// sessions.
    SearchPastDecisions(PastDecisionsArgs),
    /// Compact one-liner: today + month cost and message counts.
    Status(StatusArgs),
    /// Open the store read-only and print its schema version and row counts.
    Store(StoreArgs),
    /// Encrypted, bring-your-own-bucket backup of your analytics aggregates
    /// (opt-in).
    Sync(SyncArgs),
}

/// Parse this process's arguments and run the requested command.
///
/// # Errors
/// Whatever the command returns. Argument-parsing failures exit the process
/// through clap, as they do for every clap program.
pub fn run() -> Result<ExitCode> {
    dispatch(&Cli::parse())
}

/// Run an already-parsed [`Cli`].
///
/// Wave-1 commands print as they go and signal failure through `Err` (or, for
/// `memory`, through `std::process::exit`); wave-8 commands return their bytes
/// and their exit code as an [`Output`] so they are testable without a
/// subprocess. Both shapes end up as an [`ExitCode`] here.
///
/// # Errors
/// Whatever the command returns.
pub fn dispatch(cli: &Cli) -> Result<ExitCode> {
    let code = match &cli.command {
        Command::Anchor(args) => run_anchor(args).map(|()| ExitCode::SUCCESS)?,
        Command::Backup(args) => run_backup(args)?.emit(),
        Command::Cfg(args) => run_cfg(args)?.emit(),
        Command::ClearCache(args) => run_clear_cache(args)?.emit(),
        Command::Config(args) => run_config(args)?.emit(),
        Command::FindFailureModesForFile(args) => {
            run_failure_modes(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::FindSessionsInPath(args) => run_in_path(args).map(|()| ExitCode::SUCCESS)?,
        Command::FindSessionsTouchingFile(args) => {
            run_touching_file(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::FindSessionsWhereActionWorked(args) => {
            run_action_worked(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::Memory(args) => run_memory(args).map(|()| ExitCode::SUCCESS)?,
        Command::Resume(args) => run_resume(args).map(|()| ExitCode::SUCCESS)?,
        Command::SearchPastDecisions(args) => {
            run_past_decisions(args).map(|()| ExitCode::SUCCESS)?
        }
        Command::Status(args) => run_status(args)?.emit(),
        Command::Store(args) => run_store(args).map(|()| ExitCode::SUCCESS)?,
        Command::Sync(args) => run_sync(args).map(|()| ExitCode::SUCCESS)?,
    };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_clap_definition_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn store_takes_an_optional_store_path() {
        let cli = Cli::try_parse_from(["stax", "store"]).expect("bare store parses");
        let Command::Store(args) = &cli.command else {
            panic!("expected store");
        };
        assert!(args.store.is_none());

        let cli = Cli::try_parse_from(["stax", "store", "--store", "/data/su/store.db"])
            .expect("--store parses");
        let Command::Store(args) = &cli.command else {
            panic!("expected store");
        };
        assert_eq!(
            args.store.as_deref(),
            Some(std::path::Path::new("/data/su/store.db"))
        );
    }

    #[test]
    fn the_binary_is_named_stax() {
        assert_eq!(Cli::command().get_name(), "stax");
    }
}
