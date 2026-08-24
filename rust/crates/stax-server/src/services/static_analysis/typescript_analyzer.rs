//! TypeScript static analyzer — type errors / lint count.
//!
//! Port of `python-legacy: services/static_analysis/typescript_analyzer.py`
//! (196 lines). The reference shells out to `tsc` (TypeScript compiler) and
//! `eslint`; both are expected to arrive with the user's project toolchain —
//! there is no library to depend on, in Python or here. When neither is
//! present the runner skips the file with a warning recorded in the row's
//! `details_json`.
//!
//! Metrics produced:
//!
//! * `type_completeness` — derived from `tsc --noEmit` error count. Lower
//!   error count ⇒ higher completeness; the value is
//!   `1.0 - min(1, errors / 10)` so it lives in `[0, 1]` like the Python
//!   analyzer's metric.
//! * `lint_count` — number of `eslint` problems on the file.
//!
//! Complexity is **deferred**, exactly as the reference defers it — there is
//! no clean cross-toolchain answer for TS/JS complexity.
//!
//! # Recorded divergences
//!
//! * Child output is decoded lossily; `text=True` in CPython would raise on
//!   undecodable bytes, which the reference never handles anyway.
//! * The `eslint JSON parse failed:` warning carries `serde_json`'s wording
//!   where the reference embeds `json.JSONDecodeError`'s. The prefix — what
//!   the runner and tests key on — is identical.
//! * `round(score, 3)` is scaled-round here, banker's in CPython. Every
//!   reachable score is an exact tenth (`1.0 - n/10`), where the two agree.

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// `_TIMEOUT_S = 60`.
const TIMEOUT_S: u64 = 60;

/// `ALL_METRICS = ("lint_count", "type_completeness")`.
pub const ALL_METRICS: [&str; 2] = ["lint_count", "type_completeness"];

/// `shutil.which(program) is not None` — a PATH walk, no fallback to cwd.
fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

/// `shutil.which`'s per-candidate test: a regular file with execute access.
#[cfg(unix)]
fn is_executable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    candidate.is_file()
        && candidate
            .metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(candidate: &Path) -> bool {
    candidate.is_file()
}

fn tsc_available() -> bool {
    on_path("tsc")
}

fn eslint_available() -> bool {
    on_path("eslint")
}

/// `available() -> (have_any, "; ".join(reasons))`.
///
/// True when *either* tool is present — each metric degrades independently,
/// so one tool is enough to produce one row.
#[must_use]
pub fn available() -> (bool, String) {
    availability(tsc_available(), eslint_available())
}

/// The pure core of [`available`], split out so the tools-absent shape is
/// testable on a machine that happens to have the toolchain installed.
fn availability(have_tsc: bool, have_eslint: bool) -> (bool, String) {
    let mut parts: Vec<&str> = Vec::new();
    let mut have_any = false;
    if have_tsc {
        have_any = true;
    } else {
        parts.push("tsc not on PATH (npm install -g typescript)");
    }
    if have_eslint {
        have_any = true;
    } else {
        parts.push("eslint not on PATH (npm install -g eslint)");
    }
    (have_any, parts.join("; "))
}

/// What one `subprocess.run(..., timeout=60, check=False)` call can come to.
enum Run {
    Done {
        returncode: i32,
        stdout: String,
        stderr: String,
    },
    /// `subprocess.TimeoutExpired`.
    TimedOut,
    /// `OSError` / `FileNotFoundError` — the child never ran.
    Failed(std::io::Error),
}

/// CPython's `Popen.returncode` — the exit code, or the negative signal
/// number on Unix when the child died to a signal.
fn returncode_of(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(-1)
    }
}

/// `subprocess.run(cmd, capture_output=True, text=True, timeout=60,
/// check=False)`, with the timeout hand-rolled: `std::process` has none, so
/// the child is polled against a deadline and killed past it.
///
/// Both pipes are drained on threads *while* polling — a child that fills a
/// pipe buffer would otherwise never exit and turn the timeout into the only
/// path out.
fn run_captured(mut cmd: Command) -> Run {
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Run::Failed(e),
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + Duration::from_secs(TIMEOUT_S);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Run::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Run::Failed(e);
            }
        }
    };
    Run::Done {
        returncode: returncode_of(status),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }
}

/// The Python helpers' `(value, details, warning)` triple, verbatim: value
/// present ⇒ the metric lands, warning present ⇒ the reason lands.
type MetricOutcome = (Option<Value>, Map<String, Value>, Option<String>);

/// `_type_completeness_via_tsc` — run `tsc --noEmit` and translate the error
/// count into a `[0, 1]` score.
fn type_completeness_via_tsc(file_path: &Path) -> MetricOutcome {
    if !tsc_available() {
        return (None, Map::new(), Some("tsc not on PATH".to_owned()));
    }
    let mut cmd = Command::new("tsc");
    cmd.args([
        "--noEmit",
        "--pretty",
        "false",
        // `--target` is best-effort; without a project tsconfig the compiler
        // defaults to ES3, which floods the baseline error count with
        // "object spread requires ES2015" noise. ES2020 is a reasonable
        // middle that doesn't introduce JSX/strict assumptions.
        "--target",
        "es2020",
        "--module",
        "esnext",
        "--moduleResolution",
        "node",
        "--allowJs",
        "--skipLibCheck",
    ])
    .arg(file_path);
    match run_captured(cmd) {
        Run::TimedOut => (
            None,
            Map::new(),
            Some(format!("tsc timed out after {TIMEOUT_S}s")),
        ),
        Run::Failed(e) => (None, Map::new(), Some(format!("tsc failed to start: {e}"))),
        // tsc returns non-zero when there are type errors. We don't
        // differentiate "error" from "config issue" because the cap below
        // pegs the score at 0 for any 10+ findings — same outcome as a
        // complete failure.
        Run::Done { stdout, .. } => {
            let (value, details) = parse_tsc_stdout(&stdout);
            (Some(value), details, None)
        }
    }
}

/// The parse half of `_type_completeness_via_tsc`: count `error TS` lines and
/// map `[0, ∞)` errors to `[1, 0]` completeness, rounded to three decimals.
fn parse_tsc_stdout(stdout: &str) -> (Value, Map<String, Value>) {
    let error_count = stdout
        .lines()
        .filter(|line| line.contains("error TS"))
        .count();
    let score = 1.0 - (error_count as f64 / 10.0).min(1.0);
    let score = (score * 1000.0).round() / 1000.0;
    let mut details = Map::new();
    details.insert("type_errors".to_owned(), Value::from(error_count));
    (Value::from(score), details)
}

/// `_lint_count` — count of `eslint` problems for the file.
fn lint_count(file_path: &Path) -> MetricOutcome {
    if !eslint_available() {
        return (None, Map::new(), Some("eslint not on PATH".to_owned()));
    }
    let mut cmd = Command::new("eslint");
    cmd.args([
        "--format",
        "json",
        // Minimal default config — same rationale as ruff's --isolated. We
        // don't want the host project's .eslintrc.json polluting the
        // baseline.
        "--no-eslintrc",
    ])
    .arg(file_path);
    match run_captured(cmd) {
        Run::TimedOut => (
            None,
            Map::new(),
            Some(format!("eslint timed out after {TIMEOUT_S}s")),
        ),
        Run::Failed(e) => (
            None,
            Map::new(),
            Some(format!("eslint failed to start: {e}")),
        ),
        Run::Done {
            returncode,
            stdout,
            stderr,
        } => {
            // eslint exits 0 (clean), 1 (problems), 2 (config error).
            if !matches!(returncode, 0 | 1) {
                let snippet: String = stderr.trim().chars().take(200).collect();
                return (
                    None,
                    Map::new(),
                    Some(format!("eslint exit {returncode}: {snippet}")),
                );
            }
            match parse_eslint_stdout(&stdout) {
                Ok((value, details)) => (Some(value), details, None),
                Err(warning) => (None, Map::new(), Some(warning)),
            }
        }
    }
}

/// The parse half of `_lint_count`: eslint's `--format json` document is one
/// entry per file (we always pass exactly one file in); the metric is the
/// total message count and the details are the three most frequent rules.
fn parse_eslint_stdout(stdout: &str) -> Result<(Value, Map<String, Value>), String> {
    let data: Value = if stdout.trim().is_empty() {
        Value::Array(Vec::new())
    } else {
        serde_json::from_str(stdout).map_err(|e| format!("eslint JSON parse failed: {e}"))?
    };
    let Value::Array(entries) = data else {
        return Err("eslint JSON not a list".to_owned());
    };
    let mut total: u64 = 0;
    // First-seen order, so the stable sort below breaks count ties the way
    // CPython's insertion-ordered dict + stable `sorted` does.
    let mut rule_freq: Vec<(String, u64)> = Vec::new();
    for entry in &entries {
        let Some(messages) = entry.get("messages").and_then(Value::as_array) else {
            continue;
        };
        for message in messages {
            if !message.is_object() {
                continue;
            }
            total += 1;
            if let Some(rule) = message.get("ruleId").and_then(Value::as_str) {
                if let Some(slot) = rule_freq.iter_mut().find(|(code, _)| code == rule) {
                    slot.1 += 1;
                } else {
                    rule_freq.push((rule.to_owned(), 1));
                }
            }
        }
    }
    rule_freq.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    let top_rules: Vec<Value> = rule_freq
        .into_iter()
        .take(3)
        .map(|(code, count)| serde_json::json!({"code": code, "count": count}))
        .collect();
    let mut details = Map::new();
    details.insert("top_rules".to_owned(), Value::Array(top_rules));
    // `float(total)` — the reference stores the count as a float, so the
    // details_json round-trip renders `5.0`, not `5`.
    Ok((Value::from(total as f64), details))
}

/// Run every available metric over `content`.
#[must_use]
pub fn analyze(path: &Path, content: &str) -> super::FileMetrics {
    // Touch `content` so the path argument matches the on-disk write the
    // runner already did. Both tools want a real file.
    let _ = content;
    let mut out = super::FileMetrics::default();

    let (tc_value, tc_details, tc_warn) = type_completeness_via_tsc(path);
    if let Some(value) = tc_value {
        out.metrics.insert("type_completeness".to_owned(), value);
    }
    if !tc_details.is_empty() {
        out.details
            .insert("type_completeness".to_owned(), Value::Object(tc_details));
    }
    if let Some(warning) = tc_warn {
        out.warnings.push(format!("type_completeness: {warning}"));
    }

    let (lint_value, lint_details, lint_warn) = lint_count(path);
    if let Some(value) = lint_value {
        out.metrics.insert("lint_count".to_owned(), value);
    }
    if !lint_details.is_empty() {
        out.details
            .insert("lint_count".to_owned(), Value::Object(lint_details));
    }
    if let Some(warning) = lint_warn {
        out.warnings.push(format!("lint_count: {warning}"));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape `tsc --noEmit --pretty false` emits per diagnostic:
    /// `file(line,col): error TSNNNN: message`.
    const TSC_TWO_ERRORS: &str = "\
src/app.ts(3,5): error TS2322: Type 'string' is not assignable to type 'number'.\n\
src/app.ts(10,1): error TS2304: Cannot find name 'bar'.\n";

    #[test]
    fn tsc_counts_error_lines_and_scores_them() {
        let (value, details) = parse_tsc_stdout(TSC_TWO_ERRORS);
        assert_eq!(value, json!(0.8));
        assert_eq!(details.get("type_errors"), Some(&json!(2)));
    }

    #[test]
    fn tsc_clean_output_scores_one_and_ignores_non_error_lines() {
        // Version banners and bare notes carry no `error TS` and don't count.
        let (value, details) = parse_tsc_stdout("Version 5.4.5\nFound 0 errors.\n");
        assert_eq!(value, json!(1.0));
        assert_eq!(details.get("type_errors"), Some(&json!(0)));
        let (value, _) = parse_tsc_stdout("");
        assert_eq!(value, json!(1.0));
    }

    #[test]
    fn tsc_ten_or_more_errors_pin_the_score_at_zero() {
        let stdout: String = (0..12)
            .map(|n| format!("f.ts({n},1): error TS2304: Cannot find name 'x{n}'.\n"))
            .collect();
        let (value, details) = parse_tsc_stdout(&stdout);
        assert_eq!(value, json!(0.0));
        assert_eq!(details.get("type_errors"), Some(&json!(12)));
    }

    #[test]
    fn tsc_score_lands_on_exact_tenths_after_rounding() {
        // 1.0 - 3/10 is 0.7000000000000001 in f64; `round(score, 3)` in the
        // reference makes it 0.7, and so does the scaled round here.
        let stdout: String = "a.ts(1,1): error TS1005: ';' expected.\n".repeat(3);
        let (value, _) = parse_tsc_stdout(&stdout);
        assert_eq!(value, json!(0.7));
    }

    #[test]
    fn eslint_counts_messages_and_ranks_the_top_three_rules() {
        // One file entry, six problems: a null ruleId (parse error) counts
        // toward the total but never ranks; count ties keep first-seen order.
        let stdout = json!([{
            "filePath": "/tmp/f.ts",
            "messages": [
                {"ruleId": "no-unused-vars", "severity": 2, "message": "'a' is defined but never used."},
                {"ruleId": "no-unused-vars", "severity": 2, "message": "'b' is defined but never used."},
                {"ruleId": "semi", "severity": 1, "message": "Missing semicolon."},
                {"ruleId": "eqeqeq", "severity": 1, "message": "Expected '===' and instead saw '=='."},
                {"ruleId": "curly", "severity": 1, "message": "Expected { after 'if' condition."},
                {"ruleId": null, "severity": 2, "message": "Parsing error: Unexpected token"}
            ]
        }])
        .to_string();
        let (value, details) = parse_eslint_stdout(&stdout).expect("valid eslint JSON");
        assert_eq!(value, json!(6.0));
        assert_eq!(
            details.get("top_rules"),
            Some(&json!([
                {"code": "no-unused-vars", "count": 2},
                {"code": "semi", "count": 1},
                {"code": "eqeqeq", "count": 1},
            ]))
        );
    }

    #[test]
    fn eslint_skips_malformed_entries_the_way_the_reference_does() {
        // Non-dict entries, entries without a messages list, and non-dict
        // messages are all skipped without failing the parse.
        let stdout = json!([
            "not-a-dict",
            {"filePath": "/tmp/a.ts"},
            {"filePath": "/tmp/b.ts", "messages": "not-a-list"},
            {"filePath": "/tmp/c.ts", "messages": ["not-a-dict", {"ruleId": "semi"}]}
        ])
        .to_string();
        let (value, details) = parse_eslint_stdout(&stdout).expect("valid eslint JSON");
        assert_eq!(value, json!(1.0));
        assert_eq!(
            details.get("top_rules"),
            Some(&json!([{"code": "semi", "count": 1}]))
        );
    }

    #[test]
    fn eslint_empty_stdout_is_zero_problems() {
        let (value, details) = parse_eslint_stdout("  \n").expect("empty is an empty list");
        assert_eq!(value, json!(0.0));
        assert_eq!(details.get("top_rules"), Some(&json!([])));
    }

    #[test]
    fn eslint_bad_json_and_non_list_json_become_warnings() {
        let err = parse_eslint_stdout("not json at all").expect_err("must not parse");
        assert!(err.starts_with("eslint JSON parse failed: "), "got: {err}");
        let err = parse_eslint_stdout(r#"{"messages": []}"#).expect_err("a dict is not a list");
        assert_eq!(err, "eslint JSON not a list");
    }

    #[test]
    fn availability_reports_each_missing_tool_by_name() {
        assert_eq!(availability(true, true), (true, String::new()));
        assert_eq!(
            availability(true, false),
            (
                true,
                "eslint not on PATH (npm install -g eslint)".to_owned()
            )
        );
        assert_eq!(
            availability(false, true),
            (
                true,
                "tsc not on PATH (npm install -g typescript)".to_owned()
            )
        );
        assert_eq!(
            availability(false, false),
            (
                false,
                "tsc not on PATH (npm install -g typescript); \
                 eslint not on PATH (npm install -g eslint)"
                    .to_owned()
            )
        );
    }

    #[test]
    fn analyze_with_tools_absent_skips_both_metrics_and_records_why() {
        if tsc_available() || eslint_available() {
            // The toolchain happens to be installed here; the absent path is
            // pinned by `availability_reports_each_missing_tool_by_name`.
            return;
        }
        let out = analyze(Path::new("/nonexistent/file.ts"), "let x = 1;\n");
        assert!(out.metrics.is_empty());
        assert!(out.details.is_empty());
        assert_eq!(
            out.warnings,
            vec![
                "type_completeness: tsc not on PATH".to_owned(),
                "lint_count: eslint not on PATH".to_owned(),
            ]
        );
    }
}
