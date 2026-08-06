//! Python static analyzer — complexity / lint count / type completeness.
//!
//! Port of `stackunderflow/services/static_analysis/python_analyzer.py` (303
//! lines). Each metric is independently optional: a missing tool, empty file,
//! or parse failure means the metric is *absent* from `metrics` and the reason
//! lives in `warnings` — never a panic, never an `Err`.
//!
//! # Recorded divergence — library imports become CLI shell-outs
//!
//! The reference runs two of its three probes **in-process**: `complexity` via
//! `radon.complexity.cc_visit(content)` (line 135) and `type_completeness` via
//! a stdlib `ast` walk (lines 229–255); `mypy` is probed by `import mypy.api`
//! (line 89) and never executed. A Rust process cannot import Python
//! libraries, so every probe here is a subprocess:
//!
//! * `complexity` shells out to `radon cc -j <file>` and parses its JSON — the
//!   same per-block `complexity` integers `cc_visit` returns (functions and
//!   classes at the top level; methods and closures stay nested in both).
//! * `type_completeness` runs `python3 -c` with a line-for-line transcription
//!   of the reference's AST walk ([`TYPE_COMPLETENESS_SCRIPT`]) — the walk
//!   *is* CPython's `ast`, so CPython executes it and the counts match by
//!   construction.
//! * availability probes spawn `<tool> --version` instead of importing
//!   (`radon`, `mypy`) or `shutil.which` (`ruff`); an unspawnable binary is
//!   "not installed", with the reference's reason strings verbatim.
//! * the reference caps only its one subprocess (`ruff`) at `_TIMEOUT_S`
//!   (line 48); here every tool is a subprocess, so all three carry the same
//!   60s cap.
//!
//! Warning texts mirror the reference's f-strings; where the embedded error
//! differs by construction (serde's JSON error wording vs `json.JSONDecodeError`,
//! an OS spawn error vs Python's `OSError`), the prefix is kept and the tail is
//! honest about its origin.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
// Python's `round(x, n)` — correct decimal rounding, ties to even — and the
// float-vs-int JSON distinction. One home for both (wave-5 law).
use stax_etl::stats::aggregator::{jf, round_py};

use super::FileMetrics;

/// `_TIMEOUT_S` (reference line 48) — hard cap on each subprocess. The
/// reference applies it to `ruff` only (its other passes are in-process); here
/// every tool is a subprocess, so all inherit the same 60s budget.
const TIMEOUT_S: u64 = 60;

/// `ALL_METRICS` (reference line 52) — the closed list of metrics this
/// analyzer can produce. Mirrors the runner's metric keys minus `coverage`
/// (deferred to Spec 22).
pub const ALL_METRICS: [&str; 3] = ["complexity", "lint_count", "type_completeness"];

/// One metric pass: `(value, details, warning)` — the reference's per-metric
/// return shape (e.g. `_complexity`, line 122).
type MetricOutcome = (Option<Value>, Map<String, Value>, Option<String>);

// ── availability ──────────────────────────────────────────────────────────

/// Can `<bin> --version` be spawned and exit 0?
///
/// The divergence probe: the reference imports `radon` / `mypy` as modules
/// (lines 74–92) and `shutil.which`es `ruff` (line 83); a foreign process can
/// only ask the CLI. Absent binary ⇒ `false`, same conclusion either way.
fn cli_available(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// `available()` (reference lines 95–116) — `(any_metric_available, reason)`.
///
/// The runner invokes the analyzer when the bool is true; `reason` summarises
/// what's missing so the CLI can warn the user once per language.
#[must_use]
pub fn available() -> (bool, String) {
    availability_summary(
        cli_available("radon"),
        cli_available("ruff"),
        cli_available("mypy"),
    )
}

/// The pure composition of the three probes — reference lines 102–116, with
/// its reason strings verbatim.
fn availability_summary(radon: bool, ruff: bool, mypy: bool) -> (bool, String) {
    let mut parts: Vec<&str> = Vec::new();
    let mut have_any = false;
    if radon {
        have_any = true;
    } else {
        parts.push("radon not installed (pip install 'stackunderflow[analysis]')");
    }
    if ruff {
        have_any = true;
    } else {
        parts.push("ruff not on PATH (pip install ruff)");
    }
    if mypy {
        have_any = true;
    } else {
        parts.push("mypy not installed (pip install 'stackunderflow[analysis]')");
    }
    (have_any, parts.join("; "))
}

// ── subprocess plumbing ───────────────────────────────────────────────────

/// Why a subprocess produced no output.
enum RunError {
    /// The binary could not be started.
    Spawn(std::io::Error),
    /// `subprocess.TimeoutExpired` — killed at [`TIMEOUT_S`].
    Timeout,
}

/// A finished subprocess: `subprocess.run(capture_output=True, text=True)`.
struct RunOutput {
    /// Exit code; `-1` stands in for death-by-signal (Python reports `-SIG`).
    code: i32,
    stdout: String,
    stderr: String,
}

/// Drain one child pipe on its own thread, so a full pipe never deadlocks the
/// wait loop below (the same hazard `subprocess.run` solves internally).
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    })
}

/// `subprocess.run(cmd, capture_output=True, text=True, timeout=_TIMEOUT_S,
/// check=False)` (reference lines 176–178), std-only: spawn, feed optional
/// stdin, poll `try_wait` against a deadline, kill on expiry.
fn run_with_timeout(cmd: &mut Command, stdin_data: Option<&str>) -> Result<RunOutput, RunError> {
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    if let (Some(data), Some(mut pipe)) = (stdin_data, child.stdin.take()) {
        let owned = data.to_owned();
        std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = pipe.write_all(owned.as_bytes());
            // Dropping the pipe closes the child's stdin (EOF).
        });
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + Duration::from_secs(TIMEOUT_S);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(RunOutput {
                    code: status.code().unwrap_or(-1),
                    stdout: stdout.join().unwrap_or_default(),
                    stderr: stderr.join().unwrap_or_default(),
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Spawn(error));
            }
        }
    }
}

/// `proc.stderr.strip()[:200]` (reference line 185) — Python slices
/// characters, so this takes chars, not bytes.
fn stderr_excerpt(stderr: &str) -> String {
    stderr.trim().chars().take(200).collect()
}

// ── per-metric implementations ────────────────────────────────────────────

/// `_complexity` (reference lines 122–150) — average cyclomatic complexity
/// across the file's blocks, via `radon cc -j` instead of `cc_visit`.
///
/// Empty file or zero-function file returns an absent metric with
/// `{"functions": 0}` — meaningfully absent rather than spuriously zero.
fn complexity(file_path: &Path) -> MetricOutcome {
    if !cli_available("radon") {
        return (None, Map::new(), Some("radon not installed".to_owned()));
    }
    let mut cmd = Command::new("radon");
    cmd.args(["cc", "-j"]).arg(file_path);
    let out = match run_with_timeout(&mut cmd, None) {
        Ok(out) => out,
        Err(RunError::Timeout) => {
            return (
                None,
                Map::new(),
                Some(format!("radon timed out after {TIMEOUT_S}s")),
            );
        }
        Err(RunError::Spawn(error)) => {
            return (
                None,
                Map::new(),
                // The CLI analogue of "radon import failed: {e}" (line 133).
                Some(format!("radon failed to start: {error}")),
            );
        }
    };
    if out.code != 0 {
        return (
            None,
            Map::new(),
            Some(format!(
                "radon exit {}: {}",
                out.code,
                stderr_excerpt(&out.stderr)
            )),
        );
    }
    parse_radon_cc_json(&out.stdout)
}

/// Parse `radon cc -j` output: `{"<file>": [block, …]}` on success,
/// `{"<file>": {"error": "…"}}` when radon could not parse the source — the
/// CLI's spelling of the `SyntaxError` that `cc_visit` raises (reference
/// line 136 maps it to `"radon parse failed: {e}"`).
fn parse_radon_cc_json(stdout: &str) -> MetricOutcome {
    let parsed: Value = match serde_json::from_str(stdout) {
        Ok(value) => value,
        Err(error) => {
            return (
                None,
                Map::new(),
                Some(format!("radon JSON parse failed: {error}")),
            );
        }
    };
    let Some(by_file) = parsed.as_object() else {
        return (
            None,
            Map::new(),
            Some("radon JSON not an object".to_owned()),
        );
    };
    // One file in, one key out.
    let Some(entry) = by_file.values().next() else {
        return (
            None,
            Map::new(),
            Some("radon JSON missing file entry".to_owned()),
        );
    };
    if let Some(message) = entry.get("error").and_then(Value::as_str) {
        return (
            None,
            Map::new(),
            Some(format!("radon parse failed: {message}")),
        );
    }
    let Some(blocks) = entry.as_array() else {
        return (
            None,
            Map::new(),
            Some("radon JSON file entry not a list".to_owned()),
        );
    };
    // `cmps = [float(r.complexity) for r in results]` (line 140).
    let cmps: Vec<f64> = blocks
        .iter()
        .filter_map(|block| block.get("complexity").and_then(Value::as_f64))
        .collect();
    if cmps.is_empty() {
        // `if not results: return None, {"functions": 0}, None` (line 138).
        let mut details = Map::new();
        details.insert("functions".to_owned(), Value::from(0));
        return (None, details, None);
    }
    let avg = cmps.iter().sum::<f64>() / cmps.len() as f64;
    let max = cmps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min = cmps.iter().copied().fold(f64::INFINITY, f64::min);
    let mut details = Map::new();
    details.insert("functions".to_owned(), Value::from(cmps.len()));
    // `int(max(cmps))` / `int(min(cmps))` (lines 146–147) — truncation, which
    // the saturating `as` cast performs for these small finite values.
    #[allow(clippy::cast_possible_truncation)]
    {
        details.insert("max_complexity".to_owned(), Value::from(max as i64));
        details.insert("min_complexity".to_owned(), Value::from(min as i64));
    }
    // `round(avg, 3)` (line 143) — CPython's decimal ties-to-even round.
    (Some(jf(round_py(avg, 3))), details, None)
}

/// `_lint_count` (reference lines 153–204) — count of `ruff` findings, with
/// the top-3 rule ids in details. The count is a float because the schema's
/// `REAL` carries everything.
fn lint_count(file_path: &Path) -> MetricOutcome {
    if !cli_available("ruff") {
        return (None, Map::new(), Some("ruff not on PATH".to_owned()));
    }
    let mut cmd = Command::new("ruff");
    // `--isolated`: don't follow project config — a *baseline* lint count
    // comparable across pre/post snapshots (reference lines 168–173).
    cmd.args(["check", "--output-format=json", "--no-cache", "--isolated"])
        .arg(file_path);
    let out = match run_with_timeout(&mut cmd, None) {
        Ok(out) => out,
        Err(RunError::Timeout) => {
            return (
                None,
                Map::new(),
                Some(format!("ruff timed out after {TIMEOUT_S}s")),
            );
        }
        Err(RunError::Spawn(error)) => {
            return (
                None,
                Map::new(),
                Some(format!("ruff failed to start: {error}")),
            );
        }
    };
    // ruff exit code: 0 = clean, 1 = findings, 2 = error (line 183).
    if out.code != 0 && out.code != 1 {
        return (
            None,
            Map::new(),
            Some(format!(
                "ruff exit {}: {}",
                out.code,
                stderr_excerpt(&out.stderr)
            )),
        );
    }
    parse_ruff_json(&out.stdout)
}

/// Parse `ruff check --output-format=json` stdout — reference lines 186–204.
///
/// Ties in the rule ranking keep first-seen order: Python's stable `sorted`
/// over dict-insertion order (line 199), mirrored here by a stable sort over
/// an insertion-ordered `Vec`.
fn parse_ruff_json(stdout: &str) -> MetricOutcome {
    let findings: Value = if stdout.trim().is_empty() {
        Value::Array(Vec::new())
    } else {
        match serde_json::from_str(stdout) {
            Ok(value) => value,
            Err(error) => {
                return (
                    None,
                    Map::new(),
                    Some(format!("ruff JSON parse failed: {error}")),
                );
            }
        }
    };
    let Some(findings) = findings.as_array() else {
        return (None, Map::new(), Some("ruff JSON not a list".to_owned()));
    };
    let mut rule_freq: Vec<(String, u64)> = Vec::new();
    for finding in findings {
        // Non-dict entries and non-string codes are skipped, but every entry
        // still counts toward the total (lines 193–198 vs line 201).
        let Some(code) = finding.get("code").and_then(Value::as_str) else {
            continue;
        };
        match rule_freq.iter_mut().find(|(name, _)| name == code) {
            Some((_, count)) => *count += 1,
            None => rule_freq.push((code.to_owned(), 1)),
        }
    }
    // Stable sort ⇒ ties keep insertion order, i.e. first-seen rule first.
    rule_freq.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    let top_rules: Vec<Value> = rule_freq
        .iter()
        .take(3)
        .map(|(code, count)| {
            let mut rule = Map::new();
            rule.insert("code".to_owned(), Value::from(code.clone()));
            rule.insert("count".to_owned(), Value::from(*count));
            Value::Object(rule)
        })
        .collect();
    let mut details = Map::new();
    details.insert("top_rules".to_owned(), Value::Array(top_rules));
    // `float(len(findings))` (line 201).
    #[allow(clippy::cast_precision_loss)]
    (Some(jf(findings.len() as f64)), details, None)
}

/// Line-for-line transcription of `_type_completeness`'s AST walk (reference
/// lines 229–263), executed by `python3 -c` because the walk *is* CPython's
/// `ast`. Source arrives on stdin; `sys.argv[1]` is the filename that the
/// reference passes to `ast.parse` for error messages (line 230). Prints one
/// JSON object: `{"functions": N, "typed": M}` or `{"error": "…"}`.
const TYPE_COMPLETENESS_SCRIPT: &str = r#"
import ast, json, sys

src = sys.stdin.read()
filename = sys.argv[1] if len(sys.argv) > 1 else "<file>"
try:
    tree = ast.parse(src, filename)
except SyntaxError as e:
    print(json.dumps({"error": str(e)}))
    raise SystemExit(0)
total = 0
typed = 0
for node in ast.walk(tree):
    if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        continue
    total += 1
    args = node.args
    positional = list(args.args)
    if positional and positional[0].arg in ("self", "cls"):
        positional = positional[1:]
    all_params = (
        positional
        + list(args.kwonlyargs)
        + ([args.vararg] if args.vararg else [])
        + ([args.kwarg] if args.kwarg else [])
    )
    all_typed = all(p.annotation is not None for p in all_params)
    return_typed = node.returns is not None
    if all_typed and return_typed:
        typed += 1
print(json.dumps({"functions": total, "typed": typed}))
"#;

/// `_type_completeness` (reference lines 207–263) — ratio of fully typed
/// function signatures (all args annotated, `self`/`cls` exempt, return
/// annotated), in `[0, 1]`, raw counts in details. No-function files report
/// the metric as absent rather than an arbitrary 1.0.
fn type_completeness(file_path: &Path, content: &str) -> MetricOutcome {
    // `if not content.strip(): return None, {}, None` (line 223) — before any
    // subprocess is spawned.
    if content.trim().is_empty() {
        return (None, Map::new(), None);
    }
    let mut cmd = Command::new("python3");
    cmd.arg("-c")
        .arg(TYPE_COMPLETENESS_SCRIPT)
        .arg(file_path.as_os_str());
    let out = match run_with_timeout(&mut cmd, Some(content)) {
        Ok(out) => out,
        Err(RunError::Timeout) => {
            return (
                None,
                Map::new(),
                Some(format!("python3 timed out after {TIMEOUT_S}s")),
            );
        }
        Err(RunError::Spawn(error)) => {
            return (
                None,
                Map::new(),
                // The CLI analogue of "ast unavailable" (line 228): no CPython
                // to run the walk.
                Some(format!("python3 failed to start: {error}")),
            );
        }
    };
    if out.code != 0 {
        return (
            None,
            Map::new(),
            Some(format!(
                "python3 exit {}: {}",
                out.code,
                stderr_excerpt(&out.stderr)
            )),
        );
    }
    parse_type_completeness_json(&out.stdout)
}

/// Parse [`TYPE_COMPLETENESS_SCRIPT`]'s one-object stdout — the counting is
/// reference lines 256–263, the `SyntaxError` branch is line 232's
/// `"AST parse failed: {e}"`.
fn parse_type_completeness_json(stdout: &str) -> MetricOutcome {
    let parsed: Value = match serde_json::from_str(stdout.trim()) {
        Ok(value) => value,
        Err(error) => {
            return (
                None,
                Map::new(),
                Some(format!("python3 JSON parse failed: {error}")),
            );
        }
    };
    if let Some(message) = parsed.get("error").and_then(Value::as_str) {
        return (
            None,
            Map::new(),
            Some(format!("AST parse failed: {message}")),
        );
    }
    let total = parsed.get("functions").and_then(Value::as_u64).unwrap_or(0);
    let typed = parsed.get("typed").and_then(Value::as_u64).unwrap_or(0);
    if total == 0 {
        // `return None, {"functions": 0}, None` (line 257).
        let mut details = Map::new();
        details.insert("functions".to_owned(), Value::from(0));
        return (None, details, None);
    }
    let mut details = Map::new();
    details.insert("functions".to_owned(), Value::from(total));
    details.insert("typed_functions".to_owned(), Value::from(typed));
    #[allow(clippy::cast_precision_loss)]
    let ratio = typed as f64 / total as f64;
    // `round(ratio, 3)` (line 260).
    (Some(jf(round_py(ratio, 3))), details, None)
}

// ── orchestration ─────────────────────────────────────────────────────────

/// Fold one metric pass into the result — the three identical blocks of the
/// reference's `analyze` (lines 279–301): value into `metrics` when present,
/// non-empty details into `details`, warning prefixed with the metric name.
fn fold(out: &mut FileMetrics, name: &str, outcome: MetricOutcome) {
    let (value, details, warning) = outcome;
    if let Some(value) = value {
        out.metrics.insert(name.to_owned(), value);
    }
    if !details.is_empty() {
        out.details.insert(name.to_owned(), Value::Object(details));
    }
    if let Some(warning) = warning {
        out.warnings.push(format!("{name}: {warning}"));
    }
}

/// `analyze` (reference lines 269–303) — run every available metric.
///
/// `path` is the on-disk location of the file (the runner has already written
/// `content` there); the shell-out tools need a real path, while
/// `type_completeness` analyses `content` itself, exactly as the reference
/// passes `content` to `ast.parse`. `metrics` only carries keys whose pass
/// succeeded; everything else is reported in `warnings`.
#[must_use]
pub fn analyze(path: &Path, content: &str) -> FileMetrics {
    let mut out = FileMetrics::default();
    fold(&mut out, "complexity", complexity(path));
    fold(&mut out, "lint_count", lint_count(path));
    fold(
        &mut out,
        "type_completeness",
        type_completeness(path, content),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Canned `radon cc -j` output, shaped as the CLI emits it: one key per
    // file, blocks with `complexity` ints, classes alongside functions.
    const RADON_OK: &str = r#"{"/tmp/pass/f.py": [
        {"type": "function", "rank": "A", "name": "a", "lineno": 1, "complexity": 3},
        {"type": "function", "rank": "B", "name": "b", "lineno": 9, "complexity": 7},
        {"type": "class", "rank": "A", "name": "C", "lineno": 20, "complexity": 2, "methods": []}
    ]}"#;

    #[test]
    fn radon_average_is_rounded_to_three_and_details_carry_the_extremes() {
        let (value, details, warning) = parse_radon_cc_json(RADON_OK);
        assert_eq!(value, Some(json!(4.0))); // (3 + 7 + 2) / 3
        assert_eq!(details.get("functions"), Some(&json!(3)));
        assert_eq!(details.get("max_complexity"), Some(&json!(7)));
        assert_eq!(details.get("min_complexity"), Some(&json!(2)));
        assert_eq!(warning, None);
    }

    #[test]
    fn radon_rounding_is_pythons_decimal_round_not_f64_round() {
        // avg(1, 2, 2) = 5/3 → CPython round(5/3, 3) == 1.667.
        let canned = r#"{"f.py": [{"complexity": 1}, {"complexity": 2}, {"complexity": 2}]}"#;
        let (value, _, warning) = parse_radon_cc_json(canned);
        assert_eq!(value, Some(json!(1.667)));
        assert_eq!(warning, None);
    }

    #[test]
    fn radon_zero_blocks_reports_absent_metric_with_functions_zero() {
        // `if not results: return None, {"functions": 0}, None` (line 138).
        let (value, details, warning) = parse_radon_cc_json(r#"{"empty.py": []}"#);
        assert_eq!(value, None);
        assert_eq!(details.get("functions"), Some(&json!(0)));
        assert_eq!(warning, None);
    }

    #[test]
    fn radon_error_entry_maps_to_the_references_parse_failed_warning() {
        // The CLI's spelling of `cc_visit`'s SyntaxError.
        let canned = r#"{"bad.py": {"error": "invalid syntax (<unknown>, line 1)"}}"#;
        let (value, details, warning) = parse_radon_cc_json(canned);
        assert_eq!(value, None);
        assert!(details.is_empty());
        assert_eq!(
            warning.as_deref(),
            Some("radon parse failed: invalid syntax (<unknown>, line 1)")
        );
    }

    #[test]
    fn ruff_counts_every_finding_but_ranks_only_string_codes_ties_stable() {
        // F401 and E501 tie at 2 — first-seen order wins (stable sort over
        // insertion order, mirroring Python's stable `sorted` over the dict).
        // The `"code": null` entry still counts toward the total (line 201).
        let canned = r#"[
            {"code": "F401"}, {"code": "E501"}, {"code": "E501"},
            {"code": null}, {"code": "W291"}, {"code": "F401"}
        ]"#;
        let (value, details, warning) = parse_ruff_json(canned);
        assert_eq!(value, Some(json!(6.0)));
        assert_eq!(
            details.get("top_rules"),
            Some(&json!([
                {"code": "F401", "count": 2},
                {"code": "E501", "count": 2},
                {"code": "W291", "count": 1}
            ]))
        );
        assert_eq!(warning, None);
    }

    #[test]
    fn ruff_empty_stdout_is_zero_findings_and_non_list_json_is_a_warning() {
        // `json.loads(proc.stdout) if proc.stdout.strip() else []` (line 187).
        let (value, details, warning) = parse_ruff_json("   \n");
        assert_eq!(value, Some(json!(0.0)));
        assert_eq!(details.get("top_rules"), Some(&json!([])));
        assert_eq!(warning, None);

        let (value, _, warning) = parse_ruff_json(r#"{"not": "a list"}"#);
        assert_eq!(value, None);
        assert_eq!(warning.as_deref(), Some("ruff JSON not a list"));
    }

    #[test]
    fn type_completeness_ratio_rounds_and_details_carry_the_counts() {
        let (value, details, warning) =
            parse_type_completeness_json("{\"functions\": 3, \"typed\": 1}\n");
        assert_eq!(value, Some(json!(0.333))); // round(1/3, 3)
        assert_eq!(details.get("functions"), Some(&json!(3)));
        assert_eq!(details.get("typed_functions"), Some(&json!(1)));
        assert_eq!(warning, None);
    }

    #[test]
    fn type_completeness_zero_functions_and_syntax_errors_match_the_reference() {
        // `if total == 0: return None, {"functions": 0}, None` (line 257).
        let (value, details, warning) =
            parse_type_completeness_json(r#"{"functions": 0, "typed": 0}"#);
        assert_eq!(value, None);
        assert_eq!(details.get("functions"), Some(&json!(0)));
        assert_eq!(warning, None);

        // `"AST parse failed: {e}"` (line 232).
        let (value, _, warning) =
            parse_type_completeness_json(r#"{"error": "invalid syntax (f.py, line 2)"}"#);
        assert_eq!(value, None);
        assert_eq!(
            warning.as_deref(),
            Some("AST parse failed: invalid syntax (f.py, line 2)")
        );
    }

    #[test]
    fn blank_content_skips_type_completeness_before_any_subprocess() {
        // `if not content.strip(): return None, {}, None` (line 223) — no
        // python3 spawn, so this passes on a machine with no tools at all.
        let (value, details, warning) = type_completeness(Path::new("/tmp/x.py"), "  \n\t ");
        assert_eq!(value, None);
        assert!(details.is_empty());
        assert_eq!(warning, None);
    }

    #[test]
    fn availability_is_false_only_when_all_three_probes_fail() {
        // The probe itself: an unspawnable binary is "not installed".
        assert!(!cli_available("stax-no-such-tool-2026-xyzzy"));

        let (have_any, reason) = availability_summary(false, false, false);
        assert!(!have_any);
        assert_eq!(
            reason,
            "radon not installed (pip install 'stackunderflow[analysis]'); \
             ruff not on PATH (pip install ruff); \
             mypy not installed (pip install 'stackunderflow[analysis]')"
        );

        // One tool is enough (reference: "at least one metric can run").
        let (have_any, reason) = availability_summary(false, true, false);
        assert!(have_any);
        assert_eq!(
            reason,
            "radon not installed (pip install 'stackunderflow[analysis]'); \
             mypy not installed (pip install 'stackunderflow[analysis]')"
        );

        let (have_any, reason) = availability_summary(true, true, true);
        assert!(have_any);
        assert_eq!(reason, "");
    }
}
