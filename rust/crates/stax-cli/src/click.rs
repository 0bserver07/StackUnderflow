//! The two Click surfaces a hand-written command has to reproduce.
//!
//! clap owns the *parser's* errors (`--limit notanint`), and those are recorded
//! divergences already (D-2 in `PARITY-wave1-resume.md`). What this module owns
//! is the error a command **body** raises: `click.BadParameter(msg,
//! param_hint=…)`, which Click renders through `UsageError.show()` as
//!
//! ```text
//! Usage: stax cfg set [OPTIONS] KEY VALUE
//! Try 'stackunderflow cfg set --help' for help.
//!
//! Error: Invalid value for KEY: <message>
//! ```
//!
//! …on **stderr**, with exit code **2** and an empty stdout. Every byte of that
//! is Click's, including the blank line, so it lives in one place rather than
//! being re-spelled at each raise site (`memory.rs` predates this module and
//! keeps its own copy for the `--since` funnel).
//!
//! The program name is `stax` here and `stackunderflow` there; `parity-cli.sh`
//! normalises exactly the `Usage:` and `Try '…'` lines, scoped, and counts each
//! time it fires. Message *bodies* are never normalised — a literal
//! `stackunderflow` inside a hint (`cfg set`'s "use ``stax plan set``")
//! is Python's string and is emitted verbatim (DIV-237).

use std::process::ExitCode;

/// A `click.UsageError` in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    /// `ctx.command_path` minus the program name — e.g. `cfg set`.
    pub command_path: String,
    /// The `[OPTIONS] KEY VALUE` tail of the usage line.
    pub arg_spec: String,
    /// `param_hint` — `KEY`, `VALUE`, `--since`, …
    pub param_hint: String,
    /// The `BadParameter` message.
    pub message: String,
}

impl UsageError {
    /// Build a `BadParameter`.
    #[must_use]
    pub fn bad_parameter(
        command_path: &str,
        arg_spec: &str,
        param_hint: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            command_path: command_path.to_owned(),
            arg_spec: arg_spec.to_owned(),
            param_hint: param_hint.to_owned(),
            message: message.into(),
        }
    }

    /// The exact stderr bytes Click writes, program name included.
    #[must_use]
    pub fn render(&self, program: &str) -> String {
        format!(
            "Usage: {program} {path} {spec}\nTry '{program} {path} --help' for help.\n\nError: Invalid value for {hint}: {message}\n",
            path = self.command_path,
            spec = self.arg_spec,
            hint = self.param_hint,
            message = self.message,
        )
    }
}

/// What a ported command body returns: bytes, and the process's exit code.
///
/// Returned rather than printed so a command is testable without a subprocess
/// — the same shape `memory::Output` has, kept separate because that one is
/// wave 1's and its `code` semantics (0/1/2) are already pinned by 55 cases.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    /// Everything the command wrote to stdout.
    pub stdout: String,
    /// Everything the command wrote to stderr.
    pub stderr: String,
    /// The process exit code.
    pub code: i32,
}

impl Output {
    /// A successful run that printed `stdout`.
    #[must_use]
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            code: 0,
        }
    }

    /// `sys.exit(1)` after printing `stdout` — the `backup` failure shape.
    #[must_use]
    pub fn exit1(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            code: 1,
        }
    }

    /// A `UsageError`: empty stdout, Click's block on stderr, exit 2.
    #[must_use]
    pub fn usage(error: &UsageError, program: &str) -> Self {
        Self {
            stdout: String::new(),
            stderr: error.render(program),
            code: 2,
        }
    }

    /// Write the bytes and hand back the process's exit code.
    #[must_use]
    pub fn emit(&self) -> ExitCode {
        use std::io::Write as _;
        print!("{}", self.stdout);
        let _ = std::io::stdout().flush();
        if !self.stderr.is_empty() {
            eprint!("{}", self.stderr);
        }
        ExitCode::from(u8::try_from(self.code).unwrap_or(1))
    }
}

/// The program name this binary prints into `Usage:` lines.
pub const PROGRAM: &str = "stax";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bad_parameter_block_is_clicks_byte_for_byte() {
        let error = UsageError::bad_parameter(
            "cfg set",
            "[OPTIONS] KEY VALUE",
            "KEY",
            "Unknown key 'bogus'. Valid: a, b",
        );
        assert_eq!(
            error.render("stackunderflow"),
            concat!(
                "Usage: stackunderflow cfg set [OPTIONS] KEY VALUE\n",
                "Try 'stackunderflow cfg set --help' for help.\n",
                "\n",
                "Error: Invalid value for KEY: Unknown key 'bogus'. Valid: a, b\n",
            )
        );
    }

    #[test]
    fn the_program_name_is_the_only_thing_that_changes() {
        let error = UsageError::bad_parameter("cfg set", "[OPTIONS] KEY VALUE", "VALUE", "nope");
        let theirs = error.render("stackunderflow");
        let ours = error.render(PROGRAM);
        assert_ne!(theirs, ours);
        assert_eq!(theirs.replace("stackunderflow", "stax"), ours);
    }

    #[test]
    fn a_usage_error_leaves_stdout_empty_and_exits_two() {
        let error = UsageError::bad_parameter("cfg set", "[OPTIONS] KEY VALUE", "KEY", "x");
        let output = Output::usage(&error, PROGRAM);
        assert!(output.stdout.is_empty());
        assert_eq!(output.code, 2);
    }
}
