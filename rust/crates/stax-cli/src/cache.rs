//! `stax clear-cache` — `cli.py:809`–`:818` plus
//! `infra/cursor_cache.clear_cache`.
//!
//! Three things about this ten-line command are load-bearing:
//!
//! * **`PROJECT` is accepted and ignored.** The argument is declared
//!   (`required=False`) and never read by the body. Dropping it from the port
//!   would turn `stax clear-cache foo` into a usage error where Python exits 0.
//! * **The first line is conditional on a filesystem fact.** It prints only
//!   when `$STACKUNDERFLOW_HOME/cache/cursor-results.json` existed and was
//!   removed — which makes the command *non-idempotent in its output*: the
//!   second run prints two lines where the first printed three. The parity
//!   rows therefore run on case-local homes (`@home:` in `cases.txt`), one
//!   seeded with the file and one without, so both branches are proven and
//!   neither implementation can be measured against the other's leftovers.
//! * **The hint names `stackunderflow`.** It is a literal inside the message
//!   body, not a usage line, and it is emitted verbatim (DIV-237).

use anyhow::Result;
use clap::Args;
use stax_core::settings::app_dir;

use crate::click::Output;

/// `stax clear-cache [PROJECT]`.
#[derive(Debug, Args)]
pub struct ClearCacheArgs {
    /// Accepted for compatibility and ignored, exactly as the reference does.
    #[arg(value_name = "PROJECT")]
    pub project: Option<String>,
}

/// Run `clear-cache`.
///
/// # Errors
/// A filesystem failure other than "the file is not there".
pub fn run_clear_cache(_args: &ClearCacheArgs) -> Result<Output> {
    Ok(render(clear_cursor_cache(&cursor_cache_path())))
}

/// `infra.cursor_cache._default_cache_path()`.
#[must_use]
pub fn cursor_cache_path() -> std::path::PathBuf {
    app_dir().join("cache").join("cursor-results.json")
}

/// `infra.cursor_cache.clear_cache` — `True` when a file was removed.
///
/// Python tests `exists()` first and swallows an `OSError` from the `unlink`,
/// returning `False`. Both halves matter: a directory at that path reports
/// `exists()` and then fails to unlink, which is `False` and no output line.
#[must_use]
pub fn clear_cursor_cache(path: &std::path::Path) -> bool {
    if !path.exists() {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

/// The three (or two) lines.
#[must_use]
pub fn render(cleared: bool) -> Output {
    let mut out = String::new();
    if cleared {
        out.push_str("  cursor parse cache cleared.\n");
    }
    out.push_str("  in-memory cache is cleared on restart.\n");
    out.push_str("  use `stackunderflow start --fresh` to also wipe the disk cache.\n");
    Output::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_miss_path_prints_two_lines() {
        assert_eq!(
            render(false).stdout,
            concat!(
                "  in-memory cache is cleared on restart.\n",
                "  use `stackunderflow start --fresh` to also wipe the disk cache.\n",
            )
        );
    }

    #[test]
    fn the_hit_path_prepends_the_cursor_line() {
        assert_eq!(
            render(true).stdout,
            concat!(
                "  cursor parse cache cleared.\n",
                "  in-memory cache is cleared on restart.\n",
                "  use `stackunderflow start --fresh` to also wipe the disk cache.\n",
            )
        );
    }

    #[test]
    fn the_hint_keeps_the_reference_program_name() {
        // DIV-237: this is a message body, not a `Usage:` line, so the harness
        // does not (and must not) normalise it. If the port ever prints `stax`
        // here it diverges on every invocation.
        assert!(
            render(false)
                .stdout
                .contains("`stackunderflow start --fresh`")
        );
    }

    #[test]
    fn clearing_is_true_once_and_false_after() {
        let dir = std::env::temp_dir().join(format!("stax-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cursor-results.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(clear_cursor_cache(&path));
        assert!(!clear_cursor_cache(&path));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_at_the_cache_path_is_a_miss_not_a_crash() {
        let dir = std::env::temp_dir().join(format!("stax-cache-dir-{}", std::process::id()));
        let path = dir.join("cursor-results.json");
        std::fs::create_dir_all(&path).unwrap();
        assert!(!clear_cursor_cache(&path));
        std::fs::remove_dir_all(&dir).ok();
    }
}
