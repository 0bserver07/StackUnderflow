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
mod discovery;
mod embeddings;
mod memory;
mod resume;
mod store;

use anyhow::Result;
use clap::{Parser, Subcommand};

pub use anchor::{AnchorArgs, AnchorCommand, run_anchor};
pub use ask::{hybrid_env_from_process, run_ask};
pub use discovery::{
    ActionWorkedArgs, FailureModesArgs, InPathArgs, PastDecisionsArgs, TouchingFileArgs,
    run_action_worked, run_failure_modes, run_in_path, run_past_decisions, run_touching_file,
};
pub use memory::{MemoryArgs, MemoryVerb, run_memory};
pub use resume::{ResumeArgs, ResumeEnv, run_resume};
pub use store::{StoreArgs, render_store, run_store};

/// `stax` — the Rust port of StackUnderflow.
#[derive(Debug, Parser)]
#[command(name = "stax", version, about, long_about = None)]
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
    /// List sessions where editing FILE led to a follow-up correction.
    FindFailureModesForFile(FailureModesArgs),
    /// List sessions whose project root is PATH or any ancestor of PATH.
    FindSessionsInPath(InPathArgs),
    /// List sessions where FILE shows up in tool calls or message text.
    FindSessionsTouchingFile(TouchingFileArgs),
    /// List sessions where ACTION was performed and the next user turn
    /// confirmed it worked.
    FindSessionsWhereActionWorked(ActionWorkedArgs),
    /// Ask the local store what past sessions already know.
    Memory(MemoryArgs),
    /// Session/resume ids for every coding agent under PATH (default: cwd).
    ///
    /// Groups recent sessions by provider and renders each agent's real resume
    /// invocation (templates are data in `adapters/capabilities.json`, verified
    /// against the actual CLIs — e.g. `claude --resume <id>`, `codex resume
    /// <id>`). Matching is bidirectional: standing inside a project finds it,
    /// and giving a workspace folder lists every project underneath. Read-only;
    /// agents whose CLI has no known resume command still list their session
    /// ids.
    Resume(ResumeArgs),
    /// Substring-search QUERY across past message content; return matching
    /// sessions.
    SearchPastDecisions(PastDecisionsArgs),
    /// Open the store read-only and print its schema version and row counts.
    Store(StoreArgs),
}

/// Parse this process's arguments and run the requested command.
///
/// # Errors
/// Whatever the command returns. Argument-parsing failures exit the process
/// through clap, as they do for every clap program.
pub fn run() -> Result<()> {
    dispatch(&Cli::parse())
}

/// Run an already-parsed [`Cli`].
///
/// # Errors
/// Whatever the command returns.
pub fn dispatch(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Anchor(args) => run_anchor(args),
        Command::FindFailureModesForFile(args) => run_failure_modes(args),
        Command::FindSessionsInPath(args) => run_in_path(args),
        Command::FindSessionsTouchingFile(args) => run_touching_file(args),
        Command::FindSessionsWhereActionWorked(args) => run_action_worked(args),
        Command::Memory(args) => run_memory(args),
        Command::Resume(args) => run_resume(args),
        Command::SearchPastDecisions(args) => run_past_decisions(args),
        Command::Store(args) => run_store(args),
    }
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
