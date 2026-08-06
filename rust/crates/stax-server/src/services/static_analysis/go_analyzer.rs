//! Go static analyzer — vet errors / cyclomatic complexity.
//!
//! Port of `stackunderflow/services/static_analysis/go_analyzer.py`. Shells
//! out to `go vet` and `gocyclo` (both must be on `PATH` — when absent the
//! analyzer skips cleanly with a recorded warning, same contract as the TS
//! analyzer).
//!
//! Metrics produced:
//!
//! * `lint_count` — number of `go vet` findings on the file (a Go-vet finding
//!   is functionally a lint hit).
//! * `complexity` — average cyclomatic complexity from `gocyclo`.
//!
//! Coverage (`go test -coverprofile`) is **deferred** — needs test runner
//! sandboxing, same as the Python coverage path. Type completeness isn't a
//! meaningful Go metric (the language requires types) so it's absent from
//! this analyzer.
//!
//! Subprocess handling follows the reference's `subprocess.run(...,
//! capture_output=True, text=True, timeout=60, check=False)`: a timeout or a
//! failed spawn becomes a warning string, never a panic or an `Err`. The
//! drain-then-poll shape is the same bargain `stax-etl`'s `run_git` strikes —
//! pipes are read on helper threads so a chatty child can't deadlock
//! `try_wait`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// `_TIMEOUT_S = 60`.
const TIMEOUT_S: u64 = 60;

/// `ALL_METRICS = ("complexity", "lint_count")`.
pub const ALL_METRICS: [&str; 2] = ["complexity", "lint_count"];

/// The reference's per-metric 3-tuple: `(value, details, warning)`. A `None`
/// value with a warning is a skipped metric; a `None` value with no warning is
/// "nothing to measure" (gocyclo saw zero functions).
type MetricOutcome = (Option<f64>, Map<String, Value>, Option<String>);

/// `available() -> tuple[bool, str]` — true if *either* tool is present, with
/// one semicolon-joined reason line per absent tool.
#[must_use]
pub fn available() -> (bool, String) {
    available_given(tool_on_path("go"), tool_on_path("gocyclo"))
}

/// The pure half of [`available`], split out so the reason strings are
/// testable on a machine that happens to have a tool installed.
fn available_given(have_go: bool, have_gocyclo: bool) -> (bool, String) {
    let mut parts: Vec<&str> = Vec::new();
    let mut have_any = false;
    if have_go {
        have_any = true;
    } else {
        parts.push("go not on PATH (install Go: https://go.dev/dl)");
    }
    if have_gocyclo {
        have_any = true;
    } else {
        parts.push("gocyclo not on PATH (go install github.com/fzipp/gocyclo/cmd/gocyclo@latest)");
    }
    (have_any, parts.join("; "))
}

/// Run every available metric over `content`.
///
/// `content` is unused (`_ = content` in the reference) — both tools read the
/// file from disk; the argument exists for the analyzer contract.
#[must_use]
pub fn analyze(path: &Path, content: &str) -> super::FileMetrics {
    let _ = content;
    let mut out = super::FileMetrics::default();

    let (cx_value, cx_details, cx_warn) = complexity_via_gocyclo(path);
    if let Some(value) = cx_value {
        out.metrics
            .insert("complexity".to_owned(), Value::from(value));
    }
    // `if cx_details:` — an empty dict is falsy, so `{"functions": 0}` (a
    // non-empty dict holding a zero) IS recorded, exactly as in Python.
    if !cx_details.is_empty() {
        out.details
            .insert("complexity".to_owned(), Value::Object(cx_details));
    }
    if let Some(warn) = cx_warn {
        out.warnings.push(format!("complexity: {warn}"));
    }

    let (lint_value, lint_details, lint_warn) = lint_count_via_go_vet(path);
    if let Some(value) = lint_value {
        out.metrics
            .insert("lint_count".to_owned(), Value::from(value));
    }
    if !lint_details.is_empty() {
        out.details
            .insert("lint_count".to_owned(), Value::Object(lint_details));
    }
    if let Some(warn) = lint_warn {
        out.warnings.push(format!("lint_count: {warn}"));
    }

    out
}

/// `_lint_count_via_go_vet` — count of `go vet` issues on the file.
fn lint_count_via_go_vet(file_path: &Path) -> MetricOutcome {
    if !tool_on_path("go") {
        return (None, Map::new(), Some("go not on PATH".to_owned()));
    }
    let mut cmd = Command::new("go");
    cmd.arg("vet").arg(file_path);
    let proc = match run_captured("go vet", &mut cmd) {
        Ok(proc) => proc,
        Err(warn) => return (None, Map::new(), Some(warn)),
    };
    // `go vet` writes findings to stderr, exits non-zero on findings — the
    // reference never checks the exit code here, and neither does this.
    parse_go_vet_stderr(&proc.stderr)
}

/// The parse half of `_lint_count_via_go_vet`, on the captured stderr.
fn parse_go_vet_stderr(raw_stderr: &str) -> MetricOutcome {
    // A bare-file invocation in a tmpdir without a go.mod often errors
    // "no Go files in <dir>" before vet even runs — we treat that as
    // "no findings observable" rather than a hard failure.
    let err = raw_stderr.trim();
    if err.contains("no Go files") || err.contains("go.mod file not found") {
        return (
            None,
            Map::new(),
            Some("go vet requires a module context (no go.mod in tmpdir)".to_owned()),
        );
    }
    // `if line and not line.startswith(("#", "go: "))` — lines are NOT
    // trimmed individually, so an indented `# header` would still count.
    let findings = err
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("go: "))
        .count();
    let mut details = Map::new();
    details.insert(
        "vet_lines".to_owned(),
        Value::from(u64::try_from(findings.min(5)).unwrap_or(5)),
    );
    // `float(len(findings))` — a count small enough that the cast is exact.
    #[allow(clippy::cast_precision_loss)]
    let count = findings as f64;
    (Some(count), details, None)
}

/// `_complexity_via_gocyclo` — average cyclomatic complexity via `gocyclo`.
fn complexity_via_gocyclo(file_path: &Path) -> MetricOutcome {
    if !tool_on_path("gocyclo") {
        return (None, Map::new(), Some("gocyclo not on PATH".to_owned()));
    }
    let mut cmd = Command::new("gocyclo");
    cmd.arg("-avg").arg(file_path);
    let proc = match run_captured("gocyclo", &mut cmd) {
        Ok(proc) => proc,
        Err(warn) => return (None, Map::new(), Some(warn)),
    };
    if proc.code != 0 {
        // `proc.stderr.strip()[:200]` — Python slices characters, so `take`
        // on `chars`, not `truncate` on bytes.
        let head: String = proc.stderr.trim().chars().take(200).collect();
        return (
            None,
            Map::new(),
            Some(format!("gocyclo exit {}: {head}", proc.code)),
        );
    }
    parse_gocyclo_stdout(&proc.stdout)
}

/// The parse half of `_complexity_via_gocyclo`, on the captured stdout.
///
/// gocyclo's last line with "Average:" is what we want; every other line is
/// per-function, "`<complexity> <package> <func> <pos>`".
fn parse_gocyclo_stdout(raw_stdout: &str) -> MetricOutcome {
    let out = raw_stdout.trim();
    let mut avg: Option<f64> = None;
    let mut func_count: u64 = 0;
    for line in out.lines() {
        let line = line.trim();
        if line.to_ascii_lowercase().starts_with("average:") {
            // `float(line.split(":", 1)[1].strip())`, with any parse failure
            // collapsing to `None` (the reference's IndexError/ValueError).
            avg = line
                .split_once(':')
                .and_then(|(_, tail)| tail.trim().parse::<f64>().ok());
        } else if line
            .split_whitespace()
            .next()
            // `parts[0].isdigit()` — `split_whitespace` never yields "", so
            // `all` over an empty token can't vacuously pass.
            .is_some_and(|first| first.chars().all(|c| c.is_ascii_digit()))
        {
            func_count += 1;
        }
    }
    if avg.is_none() && func_count == 0 {
        let mut details = Map::new();
        details.insert("functions".to_owned(), Value::from(0_u64));
        return (None, details, None);
    }
    let mut details = Map::new();
    details.insert("functions".to_owned(), Value::from(func_count));
    (avg.map(round3), details, None)
}

/// `round(avg, 3)`. Python rounds the decimal expansion half-to-even where
/// `f64::round` is half-away-from-zero; an exact `.0005` tie is not
/// representable in the binary floats gocyclo prints, so the divergence has no
/// observable input.
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// One finished tool invocation, `text=True`-shaped.
struct Captured {
    /// `proc.returncode`. Python reports a signal death as `-SIGNUM`; that
    /// arrives here as `code() == None` and collapses to `-1` — a recorded
    /// divergence that can only surface inside the `gocyclo exit {code}`
    /// warning text.
    code: i32,
    stdout: String,
    stderr: String,
}

/// `subprocess.run(cmd, capture_output=True, text=True, timeout=60,
/// check=False)`.
///
/// `Err` is the warning string the reference builds in its two `except` arms:
/// `"{label} timed out after 60s"` and `"{label} failed to start: {e}"`. Both
/// pipes are drained on helper threads before the `try_wait` poll — a vet run
/// over a finding-heavy file can exceed the 64 KB pipe buffer, and polling
/// without reading would deadlock exactly where Python's `communicate()` does
/// not.
fn run_captured(label: &str, cmd: &mut Command) -> Result<Captured, String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{label} failed to start: {e}"))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + Duration::from_secs(TIMEOUT_S);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            // The wait itself failing is the closest Rust gets to the
            // reference's `OSError` arm.
            Err(e) => return Err(format!("{label} failed to start: {e}")),
        }
        if Instant::now() >= deadline {
            // `subprocess.TimeoutExpired`: kill, reap, discard partial output.
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout.join();
            let _ = stderr.join();
            return Err(format!("{label} timed out after {TIMEOUT_S}s"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // `text=True` decodes with the locale codec (UTF-8 on every machine this
    // runs on). Lossy here: a stray invalid byte in tool output should mangle
    // one character, not abort the metric.
    let stdout = String::from_utf8_lossy(&stdout.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned();
    Ok(Captured {
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

/// Drain one pipe to completion on a helper thread.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

/// `shutil.which(name) is not None` — is the tool on `PATH` at all.
fn tool_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_reason_strings_cover_all_four_tool_combinations() {
        assert_eq!(available_given(true, true), (true, String::new()));

        let (ok, reason) = available_given(false, true);
        assert!(ok, "one tool present is still available");
        assert_eq!(reason, "go not on PATH (install Go: https://go.dev/dl)");

        let (ok, reason) = available_given(true, false);
        assert!(ok);
        assert_eq!(
            reason,
            "gocyclo not on PATH (go install github.com/fzipp/gocyclo/cmd/gocyclo@latest)"
        );

        let (ok, reason) = available_given(false, false);
        assert!(!ok);
        assert_eq!(
            reason,
            "go not on PATH (install Go: https://go.dev/dl); \
             gocyclo not on PATH (go install github.com/fzipp/gocyclo/cmd/gocyclo@latest)"
        );
    }

    #[test]
    fn gocyclo_function_lines_are_counted_and_the_average_is_rounded() {
        let out = "9 store (Loader).Load internal/store/load.go:41:1\n\
                   4 store helper internal/store/load.go:12:1\n\
                   Average: 6.5\n";
        let (value, details, warn) = parse_gocyclo_stdout(out);
        assert_eq!(value, Some(6.5));
        assert_eq!(details.get("functions"), Some(&Value::from(2_u64)));
        assert_eq!(warn, None);

        // `round(avg, 3)` on a longer decimal tail.
        let (value, _, _) = parse_gocyclo_stdout("3 p f a.go:1:1\nAverage: 6.66666\n");
        assert_eq!(value, Some(6.667));
    }

    #[test]
    fn gocyclo_with_no_functions_and_no_average_reports_nothing_but_details() {
        let (value, details, warn) = parse_gocyclo_stdout("");
        assert_eq!(value, None);
        assert_eq!(details.get("functions"), Some(&Value::from(0_u64)));
        assert_eq!(warn, None, "an empty file is not an error");
    }

    #[test]
    fn gocyclo_with_a_malformed_average_line_keeps_the_function_count() {
        let (value, details, warn) = parse_gocyclo_stdout("7 p f a.go:1:1\nAverage: n/a\n");
        assert_eq!(value, None, "float() would raise; avg collapses to None");
        assert_eq!(details.get("functions"), Some(&Value::from(1_u64)));
        assert_eq!(warn, None);
    }

    #[test]
    fn go_vet_counts_findings_and_skips_header_and_toolchain_lines() {
        let err = "# example.com/tmp\n\
                   ./main.go:10:2: unreachable code\n\
                   ./main.go:14:6: fmt.Sprintf format %d has arg s of wrong type string\n\
                   go: downloading example.com/dep v1.0.0\n";
        let (value, details, warn) = parse_go_vet_stderr(err);
        assert_eq!(value, Some(2.0));
        assert_eq!(details.get("vet_lines"), Some(&Value::from(2_u64)));
        assert_eq!(warn, None);
    }

    #[test]
    fn go_vet_without_a_module_context_is_a_skip_not_a_count() {
        for err in [
            "go: no Go files in /tmp/session-reconstruction",
            "go.mod file not found in current directory or any parent directory",
        ] {
            let (value, details, warn) = parse_go_vet_stderr(err);
            assert_eq!(value, None, "{err:?} must not be counted as findings");
            assert!(details.is_empty());
            assert_eq!(
                warn.as_deref(),
                Some("go vet requires a module context (no go.mod in tmpdir)")
            );
        }
    }

    #[test]
    fn go_vet_details_cap_vet_lines_at_five_while_the_metric_keeps_the_count() {
        let err = (0..7)
            .map(|i| format!("./a.go:{i}:1: shadowed variable"))
            .collect::<Vec<_>>()
            .join("\n");
        let (value, details, warn) = parse_go_vet_stderr(&err);
        assert_eq!(value, Some(7.0));
        assert_eq!(details.get("vet_lines"), Some(&Value::from(5_u64)));
        assert_eq!(warn, None);
    }

    #[test]
    fn analyze_without_the_tools_skips_cleanly_with_one_warning_per_metric() {
        // Each half is asserted only where its tool is genuinely absent (the
        // contract this test pins); where a tool happens to be installed its
        // half runs for real against a nonexistent path and is not asserted.
        let out = analyze(
            Path::new("/nonexistent/session-tmpdir/main.go"),
            "package main\n",
        );
        if !tool_on_path("gocyclo") {
            assert!(
                out.warnings
                    .contains(&"complexity: gocyclo not on PATH".to_owned()),
                "warnings were {:?}",
                out.warnings
            );
            assert!(!out.metrics.contains_key("complexity"));
            assert!(!out.details.contains_key("complexity"));
        }
        if !tool_on_path("go") {
            assert!(
                out.warnings
                    .contains(&"lint_count: go not on PATH".to_owned()),
                "warnings were {:?}",
                out.warnings
            );
            assert!(!out.metrics.contains_key("lint_count"));
            assert!(!out.details.contains_key("lint_count"));
        }
    }
}
