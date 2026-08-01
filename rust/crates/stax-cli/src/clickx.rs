//! The two Click error renderings tranche 4 needed and `click.rs` does not carry.
//!
//! [`crate::click::UsageError`] pins one shape — `click.BadParameter(msg,
//! param_hint=…)`, rendered `Error: Invalid value for KEY: msg` — because that
//! is the only one wave 1 and tranche 1 raised. The `skills` / `docs` /
//! `recommend` family raises two more, and both are byte-visible:
//!
//! ```text
//! Error: Invalid value: unknown audience 'bogus'; choose one of …
//! Error: --scope user is global; pass --project SLUG or --projects A,B,C …
//! ```
//!
//! The first is `BadParameter` with **no** hint (`format_message` drops the
//! `for KEY` clause); the second is a bare `click.UsageError`, which has no
//! `Invalid value` prefix at all. Getting either wrong is a divergence on the
//! most common thing a user does with a new command: mistype it.
//!
//! Kept in its own module rather than added to `click.rs` because tranches 2
//! and 3 are editing that crate concurrently — a shared-file edit for eight
//! lines of formatting buys a merge conflict and nothing else. The three
//! renderings should be folded into one module when the wave lands.

use crate::click::{Output, PROGRAM};

/// The `Usage:` + `Try '…'` header every Click `UsageError` prints.
fn header(command_path: &str, arg_spec: &str) -> String {
    format!(
        "Usage: {PROGRAM} {command_path} {arg_spec}\n\
         Try '{PROGRAM} {command_path} --help' for help.\n\n"
    )
}

/// `raise click.BadParameter(message)` — no `param_hint`.
#[must_use]
pub fn bad_parameter_no_hint(command_path: &str, arg_spec: &str, message: &str) -> Output {
    Output {
        stdout: String::new(),
        stderr: format!(
            "{}Error: Invalid value: {message}\n",
            header(command_path, arg_spec)
        ),
        code: 2,
    }
}

/// `raise click.UsageError(message)` — the message, with no prefix.
#[must_use]
pub fn usage_error(command_path: &str, arg_spec: &str, message: &str) -> Output {
    Output {
        stdout: String::new(),
        stderr: format!("{}Error: {message}\n", header(command_path, arg_spec)),
        code: 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hintless_bad_parameter_drops_the_for_clause() {
        let output = bad_parameter_no_hint("docs list", "[OPTIONS]", "unknown audience 'x'");
        assert_eq!(output.code, 2);
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            concat!(
                "Usage: stax docs list [OPTIONS]\n",
                "Try 'stax docs list --help' for help.\n",
                "\n",
                "Error: Invalid value: unknown audience 'x'\n",
            )
        );
    }

    #[test]
    fn a_bare_usage_error_has_no_invalid_value_prefix() {
        let output = usage_error("skills generate", "[OPTIONS]", "--scope user is global");
        assert!(output.stderr.ends_with("\nError: --scope user is global\n"));
        assert!(!output.stderr.contains("Invalid value"));
    }
}
