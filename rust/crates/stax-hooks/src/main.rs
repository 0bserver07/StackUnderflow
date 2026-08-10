//! `stax-hooks run <id>` — the process Claude Code spawns.
//!
//! (The reference spelled it `stackunderflow hooks run <id>`; since the split
//! that names a program a Rust-only install does not have, so the settings
//! file points here — see `templates::canonical_command`.)
//!
//! Argv and stdin contract, copied from `cli.py:hooks_run_cmd`:
//!
//! ```text
//! stax-hooks [hooks] run <hook_id> [--capture-content]
//! ```
//!
//! * the payload arrives as JSON on **stdin**, and is read only when stdin is
//!   not a TTY (`if not sys.stdin.isatty()`),
//! * anything unreadable, blank, non-JSON or not-an-object becomes `{}`,
//! * the result goes to **stdout**, newline-terminated, and the exit code is
//!   **always 0** — `sys.exit(_run(...))` where `run` returns `0` unconditionally.
//!
//! No `clap`. Not for speed — clap parses in microseconds — but because this
//! binary has exactly one verb and a hand-rolled parse is the difference between
//! a `--help` tree that must be diffed against Python's and one that does not
//! exist. `stax hooks run` for humans is wave 8's item (RS-8-056); this is the
//! thing the settings file points at.

use std::io::{IsTerminal as _, Read as _, Write as _};

use stax_core::queries::pyjson;
use stax_hooks::{HookEnv, run};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some((hook_id, capture_content)) = parse_argv(&argv) else {
        // Click exits 2 on a usage error and prints to stderr. This binary is
        // never invoked by a human, so the message is short and the code is
        // Click's, not 0 — a malformed *installation* must be visible.
        eprintln!("usage: stax-hooks hooks run <hook_id> [--capture-content]");
        std::process::exit(2);
    };

    let payload = read_payload();
    let fired = run(
        &hook_id,
        &payload,
        capture_content,
        &HookEnv::from_process(),
    );
    if !fired.stdout.is_empty() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(fired.stdout.as_bytes());
        let _ = out.flush();
    }
    // `hooks.run` is documented to return 0 in every path, and Claude Code
    // treats a non-zero PreToolUse hook as a *block*. Never anything else.
    std::process::exit(0);
}

/// `hooks run <id> [--capture-content]`, with the `hooks` prefix optional so the
/// binary can be invoked either as the drop-in or directly.
fn parse_argv(argv: &[String]) -> Option<(String, bool)> {
    let mut rest: Vec<&str> = argv.iter().map(String::as_str).collect();
    if rest.first() == Some(&"hooks") {
        rest.remove(0);
    }
    if rest.first() != Some(&"run") {
        return None;
    }
    rest.remove(0);

    let mut hook_id: Option<&str> = None;
    let mut capture_content = false;
    for token in rest {
        match token {
            "--capture-content" => capture_content = true,
            other if other.starts_with('-') => return None,
            other if hook_id.is_none() => hook_id = Some(other),
            _ => return None, // Click rejects a second positional argument
        }
    }
    hook_id.map(|id| (id.to_string(), capture_content))
}

/// The reference's stdin read, including its two swallowed failures.
fn read_payload() -> pyjson::Value {
    let empty = pyjson::Value::Object(Vec::new());
    let mut raw = String::new();
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        // `except (OSError, ValueError): raw = ""` — including invalid UTF-8,
        // which is where CPython raises a `UnicodeDecodeError` (a `ValueError`).
        let mut bytes = Vec::new();
        if stdin.lock().read_to_end(&mut bytes).is_ok() {
            raw = String::from_utf8(bytes).unwrap_or_default();
        }
    }
    if raw.trim().is_empty() {
        return empty;
    }
    match pyjson::loads(&raw) {
        // `if not isinstance(payload, dict): payload = {}`.
        Some(value @ pyjson::Value::Object(_)) => value,
        _ => empty,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_argv;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn the_drop_in_shape_parses() {
        assert_eq!(
            parse_argv(&argv(&["hooks", "run", "stackunderflow-stop"])),
            Some(("stackunderflow-stop".into(), false))
        );
        assert_eq!(
            parse_argv(&argv(&[
                "hooks",
                "run",
                "stackunderflow-user-prompt",
                "--capture-content"
            ])),
            Some(("stackunderflow-user-prompt".into(), true))
        );
        // The `hooks` prefix is optional.
        assert_eq!(
            parse_argv(&argv(&["run", "stackunderflow-stop"])),
            Some(("stackunderflow-stop".into(), false))
        );
    }

    #[test]
    fn a_malformed_invocation_is_a_usage_error() {
        assert_eq!(parse_argv(&argv(&[])), None);
        assert_eq!(parse_argv(&argv(&["hooks"])), None);
        assert_eq!(parse_argv(&argv(&["hooks", "run"])), None);
        assert_eq!(parse_argv(&argv(&["hooks", "status"])), None);
        assert_eq!(parse_argv(&argv(&["hooks", "run", "a", "b"])), None);
        assert_eq!(parse_argv(&argv(&["hooks", "run", "--nope"])), None);
    }
}
