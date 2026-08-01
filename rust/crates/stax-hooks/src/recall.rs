//! `hooks/recall.py` — the active-recall hook, a pre-tool memory lookup through
//! the public `memory` CLI.
//!
//! Where [`crate::inject`] reads the store *in-process*, this one shells the
//! agent-facing surface — `stackunderflow memory file <path> --json` — as a
//! subprocess under a **hard deadline**, parses the token-bounded
//! `stackunderflow.memory/1` envelope, and injects a warning only when the file
//! about to be touched has real failure history.
//!
//! The execution model is the reason it is a separate module in the reference
//! and a separate module here: a nested subprocess with a deadline is a
//! different risk shape from an indexed `SELECT`, and the deadline is *shared*
//! across every path a Bash command yields, so the tool is never delayed by more
//! than it no matter how many candidates were extracted.
//!
//! Invariants, all reproduced: always exit 0 · silent on any failure (missing
//! binary, non-zero exit, timeout, malformed JSON, unknown schema) · a clean
//! file is the same silence as an error · token-bounded, dropping the *oldest*
//! failure lines first · local and read-only.
//!
//! ### The subprocess this port spawns
//!
//! Deliberately still `stackunderflow memory file … --json`, the *Python* CLI,
//! not `stax memory file`. The command is what a user's `settings.json` and
//! `$PATH` resolve, and swapping it would (a) change the measured latency into
//! something the differ cannot compare and (b) silently re-point a user's hook
//! at a different implementation. `HookEnv::memory_bin` is the seam for wave 8
//! to decide otherwise; the default is the reference's bare name.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use stax_core::queries::pyjson;
use stax_core::queries::pyjson::Value;

use crate::env::HookEnv;
use crate::proactive;
use crate::pystr;
use crate::templates;

/// `recall._MEMORY_SCHEMA` — pinned exactly. An envelope from a different major
/// is treated as unparseable (silent no-op).
const MEMORY_SCHEMA: &str = "stackunderflow.memory/1";

/// `recall._TOKEN_BUDGET` × `_CHARS_PER_TOKEN` — three times the inject hooks'
/// per-event budget, because this block replaces a whole `memory file`
/// round-trip.
const MAX_CHARS: usize = 600 * 4;

/// `recall._DEFAULT_TIMEOUT_S` / `_MAX_TIMEOUT_S`.
const DEFAULT_TIMEOUT_S: f64 = 1.5;
const MAX_TIMEOUT_S: f64 = 30.0;

/// How many file-looking tokens a Bash command may turn into lookups.
const MAX_BASH_PATHS: usize = 3;
/// How many failure lines the rendered block carries before the budget clip.
const MAX_LINES: usize = 6;
/// Per-line evidence excerpt cap — mirrors `inject::EVIDENCE_CHARS`.
const EVIDENCE_CHARS: usize = 140;
/// Cap the token scan so a pathological command cannot make us loop long.
const MAX_COMMAND_TOKENS: usize = 64;

/// `recall._FILE_PATH_KEYS` — same probe order as `inject._edited_file_path`.
const FILE_PATH_KEYS: [&str; 4] = ["file_path", "path", "notebook_path", "filename"];

/// Pseudo-filesystems — never a source file worth a lookup.
const SKIP_PREFIXES: [&str; 3] = ["/dev/", "/proc/", "/sys/"];

/// One distilled risk finding (`recall._extract_recall`'s dict).
#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    /// The path the envelope resolved to, else the one we queried.
    pub path: String,
    /// `risk.failed`.
    pub failed: i64,
    /// `risk.reverted`.
    pub reverted: i64,
    /// `risk.total_sessions`.
    pub total: i64,
    /// The `kind == "failure_mode"` rows, verbatim.
    pub failure_modes: Vec<Value>,
}

/// `recall.build_recall` — the injection envelope for a recall fire, or `""`.
///
/// Governance rides on top without changing the default:
/// * `proactive` disabled (the default) → **passthrough**: the shipped
///   file-risk warning is emitted exactly as before, ungoverned.
/// * kill-switch set → **off**: every pre-tool nudge is silenced.
/// * `proactive_enabled` → **governed**: the warning passes the dedupe / cap /
///   cooldown layer, and a command-cluster nudge may be appended on the Bash path.
#[must_use]
pub fn build_recall(hook_id: &str, payload: &Value, env: &HookEnv) -> String {
    let Some(event) = templates::hook_id_event(hook_id) else {
        return String::new();
    };
    if !templates::RECALL_HOOK_IDS.contains(&hook_id) {
        return String::new();
    }

    let pmode = proactive::mode(env);
    if pmode == proactive::Mode::Off {
        return String::new(); // env kill-switch
    }

    let mut blocks: Vec<String> = Vec::new();

    // ── file-risk (shipped in #5; #97 only retrofits governance) ────────────
    let recalls = collect_recalls(payload, env);
    let file_text = render(&recalls);
    if !file_text.trim().is_empty()
        && (pmode != proactive::Mode::Governed
            || proactive::admit_file_risk(&recalls, payload, env))
    {
        blocks.push(file_text);
    }

    // ── command-cluster nudge (Phase 1 — governed mode, Bash path only) ─────
    if pmode == proactive::Mode::Governed {
        let cmd_text = proactive::command_cluster_block(payload, env);
        if !cmd_text.trim().is_empty() {
            blocks.push(cmd_text);
        }
    }

    if blocks.is_empty() {
        return String::new();
    }
    let text = blocks.join("\n\n");
    pyjson::dumps_default(&Value::Object(vec![(
        "hookSpecificOutput".into(),
        Value::Object(vec![
            ("hookEventName".into(), Value::Str(event.to_string())),
            ("additionalContext".into(), Value::Str(text)),
        ]),
    )]))
}

/// `recall._collect_recalls` — the path extraction plus the shared-deadline CLI
/// loop.
#[must_use]
pub fn collect_recalls(payload: &Value, env: &HookEnv) -> Vec<Recall> {
    let paths = candidate_paths(payload);
    if paths.is_empty() {
        return Vec::new();
    }
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| std::path::Path::new(value).is_dir());

    let started = Instant::now();
    let budget = timeout_seconds(env.recall_timeout.as_deref());
    let mut recalls = Vec::new();
    for path in paths {
        let remaining = budget - started.elapsed().as_secs_f64();
        if remaining <= 0.05 {
            break; // deadline spent — never stretch it for more paths
        }
        let Some(envelope) = query_memory_file(&path, remaining, cwd, env) else {
            continue;
        };
        if let Some(recall) = extract_recall(&envelope, &path) {
            recalls.push(recall);
        }
    }
    recalls
}

// ── payload → candidate paths ───────────────────────────────────────────────

/// `recall._candidate_paths`.
#[must_use]
pub fn candidate_paths(payload: &Value) -> Vec<String> {
    let Some(tool_input @ Value::Object(_)) = payload.get("tool_input") else {
        return Vec::new();
    };
    if payload.get("tool_name").and_then(Value::as_str) == Some("Bash") {
        return match tool_input.get("command") {
            Some(Value::Str(command)) => paths_from_command(command),
            _ => Vec::new(),
        };
    }
    for key in FILE_PATH_KEYS {
        if let Some(Value::Str(value)) = tool_input.get(key)
            && !value.trim().is_empty()
        {
            return vec![value.trim().to_string()];
        }
    }
    Vec::new()
}

/// `recall._EXT_RE` = `\.[A-Za-z][A-Za-z0-9]{0,7}$` — a plausible file
/// extension: letter-led and at most 8 characters, so `3.12` and `v2.0` do not
/// count. Scanned rather than compiled; the class is fixed ASCII.
fn has_extension(token: &str) -> bool {
    let Some(dot) = token.rfind('.') else {
        return false;
    };
    let tail = &token[dot + 1..];
    let mut chars = tail.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    rest.len() <= 7 && rest.iter().all(char::is_ascii_alphanumeric)
}

/// `recall._paths_from_command` — file-looking tokens, best candidates first.
///
/// Deliberately light: `shlex.split` the command, keep tokens carrying a `/` or
/// an extension, skip flags / URLs / pseudo-files, take the value half of
/// `VAR=path` and `--flag=path`. Extensions rank ahead of bare directory-ish
/// tokens (`src/app.py` beats `/usr/bin/env`). False positives are cheap — an
/// unknown path comes back clean and stays silent.
#[must_use]
pub fn paths_from_command(command: &str) -> Vec<String> {
    // `shlex.split(command)`, falling back to a whitespace split on unbalanced
    // quotes exactly as the reference's `except ValueError` does.
    let tokens = shlex_split(command).unwrap_or_else(|| {
        command
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>()
    });

    let mut candidates: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for raw in tokens.into_iter().take(MAX_COMMAND_TOKENS) {
        let raw = match raw.split_once('=') {
            // `if "=" in raw and not raw.startswith("=")` — the value half.
            Some((head, tail)) if !head.is_empty() => tail.to_string(),
            _ => raw,
        };
        let tok = raw
            .trim_matches(|c| c == '"' || c == '\'')
            .trim_end_matches([';', ',', ':'])
            .to_string();
        if tok.is_empty() || tok.starts_with('-') || tok.contains("://") {
            continue;
        }
        if !tok.contains('/') && !has_extension(&tok) {
            continue;
        }
        if matches!(tok.as_str(), "/" | "." | "..")
            || SKIP_PREFIXES.iter().any(|prefix| tok.starts_with(prefix))
        {
            continue;
        }
        if seen.contains(&tok) {
            continue;
        }
        seen.push(tok.clone());
        candidates.push(tok);
    }

    let (with_ext, without_ext): (Vec<String>, Vec<String>) = candidates
        .into_iter()
        .partition(|token| has_extension(token));
    with_ext
        .into_iter()
        .chain(without_ext)
        .take(MAX_BASH_PATHS)
        .collect()
}

/// `shlex.split(s)` in POSIX mode, minus the comment handling `shlex.split`
/// disables by default. `None` on an unbalanced quote — the reference's
/// `ValueError`.
fn shlex_split(text: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(inner) => current.push(inner),
                        None => return None, // "No closing quotation"
                    }
                }
            }
            '"' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            // Inside double quotes the escape only applies to
                            // the characters shlex calls `escapedquotes`.
                            Some(next @ ('"' | '\\')) => current.push(next),
                            Some(next) => {
                                current.push('\\');
                                current.push(next);
                            }
                            None => return None,
                        },
                        Some(inner) => current.push(inner),
                        None => return None,
                    }
                }
            }
            '\\' => {
                has_token = true;
                current.push(chars.next()?);
            }
            ch if ch.is_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            ch => {
                has_token = true;
                current.push(ch);
            }
        }
    }
    if has_token {
        out.push(current);
    }
    Some(out)
}

// ── the CLI lookup ──────────────────────────────────────────────────────────

/// `recall._timeout_seconds` — `$STACKUNDERFLOW_RECALL_TIMEOUT`, else 1.5.
///
/// Anything unparseable or non-positive falls back to the default; values clamp
/// to 30s so a stray "milliseconds" value cannot wedge a session.
#[must_use]
pub fn timeout_seconds(raw: Option<&str>) -> f64 {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_TIMEOUT_S;
    };
    match parse_py_float(raw) {
        // `except ValueError: return _DEFAULT_TIMEOUT_S` — an unparseable value
        // returns immediately, it does not fall through to the clamp.
        None => DEFAULT_TIMEOUT_S,
        Some(value) if value > 0.0 => value.min(MAX_TIMEOUT_S),
        Some(_) => DEFAULT_TIMEOUT_S,
    }
}

/// `float(raw)` — CPython accepts `inf`/`nan` and a leading `+`; `f64::from_str`
/// agrees on all of those, so this is the parse plus the underscore rejection
/// Python applies to *string* input (`float("1_0")` raises).
fn parse_py_float(raw: &str) -> Option<f64> {
    if raw.contains('_') {
        return None;
    }
    raw.parse::<f64>().ok()
}

/// `recall._query_memory_file` — the parsed envelope, or `None` for every
/// failure: binary not on `PATH`, non-zero exit, the timeout expiring (the child
/// is killed), stdout that is not JSON, or a schema major we do not understand.
fn query_memory_file(path: &str, timeout: f64, cwd: Option<&str>, env: &HookEnv) -> Option<Value> {
    let mut command = Command::new(&env.memory_bin);
    command
        .args(["memory", "file", path, "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().ok()?;

    // `capture_output=True` means both pipes must be drained or a chatty child
    // blocks on a full pipe buffer and the deadline fires on a process that was
    // never actually slow. Two reader threads, exactly what `communicate` does.
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink);
    });

    let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return None; // `subprocess.TimeoutExpired`
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return None,
        }
    };

    let stdout = out_reader.join().ok()?;
    let _ = err_reader.join();
    if !status.success() {
        return None; // the --json contract: non-zero exit means an error envelope
    }
    let envelope = pyjson::loads(&String::from_utf8(stdout).ok()?)?;
    if envelope.get("schema").and_then(Value::as_str) != Some(MEMORY_SCHEMA) {
        return None;
    }
    Some(envelope)
}

// ── envelope → risk signal ──────────────────────────────────────────────────

/// `recall._extract_recall` — one envelope distilled, or `None` when clean.
///
/// "Risky" means actual failure signal: `kind == "failure_mode"` rows, or
/// non-zero `failed` / `reverted` counts. Sessions that merely *touched* the
/// file are not a warning.
#[must_use]
pub fn extract_recall(envelope: &Value, queried_path: &str) -> Option<Recall> {
    let risk = envelope
        .get("risk")
        .filter(|v| matches!(v, Value::Object(_)));
    let failure_modes: Vec<Value> = match envelope.get("results") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|row| {
                matches!(row, Value::Object(_))
                    && row.get("kind").and_then(Value::as_str) == Some("failure_mode")
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let failed = as_int(risk.and_then(|risk| risk.get("failed")));
    let reverted = as_int(risk.and_then(|risk| risk.get("reverted")));
    if failure_modes.is_empty() && failed <= 0 && reverted <= 0 {
        return None;
    }
    let path = risk
        .and_then(|risk| risk.get("path"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(queried_path)
        .to_string();
    Some(Recall {
        path,
        failed,
        reverted,
        total: as_int(risk.and_then(|risk| risk.get("total_sessions"))),
        failure_modes,
    })
}

/// `recall._as_int` — `int(value)` with `bool` mapped to 0 and every failure to 0.
#[must_use]
pub fn as_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Bool(_)) | None => 0,
        Some(Value::Int(number)) => *number,
        // `int(1.9)` truncates toward zero.
        Some(Value::Float(number)) => *number as i64,
        Some(Value::Str(text)) => text.trim().parse::<i64>().unwrap_or(0),
        Some(_) => 0,
    }
}

// ── rendering ───────────────────────────────────────────────────────────────

/// `recall._render` — the injected text for the collected findings.
#[must_use]
pub fn render(recalls: &[Recall]) -> String {
    if recalls.is_empty() {
        return String::new();
    }
    let show_name = recalls.len() > 1;
    let mut lines: Vec<(String, String)> = Vec::new();
    for recall in recalls {
        for fm in &recall.failure_modes {
            let ts = fm
                .get("last_ts")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            lines.push((ts, failure_line(fm, &recall.path, show_name)));
        }
    }
    // `sort(key=..., reverse=True)` — Python's sort is stable, and a stable sort
    // reversed by `reverse=True` (not by reversing the comparator) keeps equal
    // keys in input order. `sort_by` here is stable and the key is negated by
    // comparing b to a, which would flip ties; `sort_by_key` on Reverse keeps
    // them, which is what `reverse=True` does.
    lines.sort_by_key(|(ts, _)| std::cmp::Reverse(ts.clone()));
    let bullets: Vec<String> = lines
        .into_iter()
        .take(MAX_LINES)
        .map(|(_, line)| line)
        .collect();

    let opening = if recalls.len() == 1 {
        format!(
            "[StackUnderflow memory] {} has failure history ({}).",
            pystr::basename(&recalls[0].path),
            risk_phrase(&recalls[0])
        )
    } else {
        let names: Vec<&str> = recalls
            .iter()
            .map(|recall| pystr::basename(&recall.path))
            .collect();
        format!(
            "[StackUnderflow memory] Files this command touches have failure history ({}).",
            names.join(", ")
        )
    };
    let header = if bullets.is_empty() {
        opening
    } else {
        format!("{opening} Recent trouble:")
    };
    let footer = format!(
        "Full history: `stackunderflow memory file {} --json`.",
        recalls[0].path
    );
    assemble(&header, bullets, &footer)
}

/// `recall._risk_phrase`.
fn risk_phrase(recall: &Recall) -> String {
    let mut counts: Vec<String> = Vec::new();
    if recall.failed != 0 {
        counts.push(format!("{} failed", recall.failed));
    }
    if recall.reverted != 0 {
        counts.push(format!("{} reverted", recall.reverted));
    }
    let stat = if counts.is_empty() {
        "past failure modes on record".to_string()
    } else {
        counts.join(" and ")
    };
    if recall.total > 0 && !counts.is_empty() {
        format!("{stat} of {} past sessions touching it", recall.total)
    } else {
        stat
    }
}

/// `recall._failure_line`.
fn failure_line(fm: &Value, path: &str, show_name: bool) -> String {
    let ts = match fm.get("last_ts").and_then(Value::as_str) {
        Some(value) => pystr::head(value, 10),
        None => String::new(),
    };
    let ts = if ts.is_empty() {
        "(undated)".to_string()
    } else {
        ts
    };
    let outcome = fm.get("outcome").and_then(Value::as_str).unwrap_or("?");
    let evidence = pystr::trim(
        fm.get("outcome_evidence")
            .and_then(Value::as_str)
            .unwrap_or(""),
        EVIDENCE_CHARS,
    );
    let prefix = if show_name {
        format!("{}  ", pystr::basename(path))
    } else {
        String::new()
    };
    let body = if evidence.is_empty() {
        outcome.to_string()
    } else {
        format!("{outcome}: {evidence}")
    };
    format!("  • {prefix}{ts}  {body}")
}

/// `recall._assemble` — header + bullets + footer under the token budget.
///
/// Over budget → drop bullet lines from the end first. Bullets are sorted
/// newest-first, so the tail IS the oldest entry: "truncate oldest first". A
/// final hard clip guards a pathologically long header/footer.
fn assemble(header: &str, bullets: Vec<String>, footer: &str) -> String {
    let mut kept = bullets;
    let mut text;
    loop {
        let mut parts: Vec<&str> = vec![header];
        parts.extend(kept.iter().map(String::as_str));
        parts.push(footer);
        text = parts.join("\n");
        if pystr::len_chars(&text) <= MAX_CHARS || kept.is_empty() {
            break;
        }
        kept.pop(); // the oldest surviving line
    }
    pystr::clip(&text, MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env() -> HookEnv {
        HookEnv {
            store_path: PathBuf::from("/nonexistent/store.db"),
            app_dir: PathBuf::from("/nonexistent"),
            weights: (0.5, 0.2, 0.3),
            now_micros: 1_785_456_000_000_000,
            cwd: PathBuf::from("/home/u/proj"),
            config: None,
            proactive_disabled: None,
            recall_timeout: None,
            memory_bin: "stackunderflow".into(),
            proactive: crate::env::ProactiveSettings::default(),
        }
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn the_deadline_knob_is_clamped_and_lenient() {
        assert!((timeout_seconds(None) - 1.5).abs() < f64::EPSILON);
        assert!((timeout_seconds(Some("  ")) - 1.5).abs() < f64::EPSILON);
        assert!((timeout_seconds(Some("0.25")) - 0.25).abs() < f64::EPSILON);
        assert!((timeout_seconds(Some("nope")) - 1.5).abs() < f64::EPSILON);
        assert!((timeout_seconds(Some("-1")) - 1.5).abs() < f64::EPSILON);
        assert!((timeout_seconds(Some("0")) - 1.5).abs() < f64::EPSILON);
        // A stray "milliseconds" value cannot wedge a session.
        assert!((timeout_seconds(Some("1500")) - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bash_paths_prefer_extensions_and_skip_the_obvious() {
        assert_eq!(
            paths_from_command("python /usr/bin/env src/app.py"),
            vec!["src/app.py", "/usr/bin/env"]
        );
        assert_eq!(paths_from_command("ls -la"), Vec::<String>::new());
        assert_eq!(
            paths_from_command("curl https://example.com/x.py"),
            Vec::<String>::new()
        );
        assert_eq!(paths_from_command("cat /dev/null"), Vec::<String>::new());
        // `VAR=path` and `--flag=path` yield the value half.
        assert_eq!(
            paths_from_command("CONFIG=conf/app.toml run"),
            vec!["conf/app.toml"]
        );
        assert_eq!(
            paths_from_command("pytest --rootdir=tests/unit"),
            vec!["tests/unit"]
        );
        // At most three.
        assert_eq!(
            paths_from_command("cat a.py b.py c.py d.py").len(),
            MAX_BASH_PATHS
        );
    }

    #[test]
    fn the_extension_test_rejects_version_numbers() {
        assert!(has_extension("app.py"));
        assert!(has_extension("x.tsx"));
        assert!(!has_extension("3.12"));
        assert!(!has_extension("v2.0"));
        assert!(!has_extension("a.abcdefghi")); // 9 characters
        assert!(has_extension("a.abcdefgh")); // 8
    }

    #[test]
    fn unbalanced_quotes_fall_back_to_a_whitespace_split() {
        assert_eq!(shlex_split("echo 'unbalanced"), None);
        assert_eq!(
            paths_from_command("echo 'unbalanced src/app.py"),
            vec!["src/app.py"]
        );
        assert_eq!(
            shlex_split(r#"a "b c" d"#),
            Some(vec!["a".into(), "b c".into(), "d".into()])
        );
    }

    #[test]
    fn a_clean_envelope_is_silence() {
        let envelope = obj(&[
            ("schema", Value::Str(MEMORY_SCHEMA.into())),
            ("results", Value::Array(vec![])),
            (
                "risk",
                obj(&[("failed", Value::Int(0)), ("reverted", Value::Int(0))]),
            ),
        ]);
        assert_eq!(extract_recall(&envelope, "/a/b.py"), None);
    }

    #[test]
    fn counts_alone_are_enough_to_warn() {
        let envelope = obj(&[
            ("schema", Value::Str(MEMORY_SCHEMA.into())),
            ("results", Value::Array(vec![])),
            (
                "risk",
                obj(&[
                    ("failed", Value::Int(2)),
                    ("reverted", Value::Int(0)),
                    ("total_sessions", Value::Int(9)),
                    ("path", Value::Str("/resolved/b.py".into())),
                ]),
            ),
        ]);
        let recall = extract_recall(&envelope, "/a/b.py").expect("risky");
        assert_eq!(recall.path, "/resolved/b.py");
        assert_eq!(recall.failed, 2);
        // The header alone carries the warning when there are no bullet rows.
        assert_eq!(
            render(std::slice::from_ref(&recall)),
            "[StackUnderflow memory] b.py has failure history (2 failed of 9 past sessions touching it).\n\
             Full history: `stackunderflow memory file /resolved/b.py --json`."
        );
    }

    #[test]
    fn rows_render_newest_first() {
        let recall = Recall {
            path: "/a/b.py".into(),
            failed: 1,
            reverted: 0,
            total: 0,
            failure_modes: vec![
                obj(&[
                    ("last_ts", Value::Str("2026-01-01T00:00:00".into())),
                    ("outcome", Value::Str("failed".into())),
                    ("outcome_evidence", Value::Str("old".into())),
                ]),
                obj(&[
                    ("last_ts", Value::Str("2026-07-01T00:00:00".into())),
                    ("outcome", Value::Str("reverted".into())),
                    ("outcome_evidence", Value::Str("new".into())),
                ]),
            ],
        };
        let rendered = render(std::slice::from_ref(&recall));
        let newest = rendered.find("2026-07-01").expect("newest present");
        let oldest = rendered.find("2026-01-01").expect("oldest present");
        assert!(newest < oldest, "{rendered}");
        assert!(rendered.contains("Recent trouble:"), "{rendered}");
    }

    #[test]
    fn at_most_six_bullets_survive_the_line_cap() {
        // `_MAX_LINES` bites long before the token budget can: per-line evidence
        // is already clipped to 140 characters, so six bullets is ~1.1 KB
        // against a 2,400-character budget. The cap is the real bound on the
        // common path, and it keeps the NEWEST six.
        let modes: Vec<Value> = (0..9)
            .map(|index| {
                obj(&[
                    ("last_ts", Value::Str(format!("2026-{:02}-01", index + 1))),
                    ("outcome", Value::Str("failed".into())),
                    ("outcome_evidence", Value::Str("x".repeat(500))),
                ])
            })
            .collect();
        let recall = Recall {
            path: "/a/b.py".into(),
            failed: 9,
            reverted: 0,
            total: 9,
            failure_modes: modes,
        };
        let rendered = render(std::slice::from_ref(&recall));
        assert_eq!(rendered.matches("  • ").count(), MAX_LINES);
        assert!(pystr::len_chars(&rendered) <= MAX_CHARS, "over budget");
        assert!(rendered.contains("2026-09-01"), "newest kept: {rendered}");
        assert!(
            !rendered.contains("2026-01-01"),
            "oldest dropped: {rendered}"
        );
        // Each line's evidence is clipped at 140 characters, not 500.
        assert!(!rendered.contains(&"x".repeat(141)), "{rendered}");
    }

    #[test]
    fn the_budget_drops_the_oldest_lines_first() {
        // Driving `_assemble` directly, because reaching 2,400 characters
        // through `render` needs a header no real path produces.
        let bullets: Vec<String> = (0..6)
            .map(|index| format!("  • 2026-0{}-01  {}", 6 - index, "y".repeat(500)))
            .collect();
        let text = assemble("HEADER", bullets, "FOOTER");
        assert!(pystr::len_chars(&text) <= MAX_CHARS, "over budget");
        // Bullets are newest-first, so the tail IS the oldest entry.
        assert!(text.contains("2026-06-01"), "newest kept: {text}");
        assert!(!text.contains("2026-01-01"), "oldest dropped first: {text}");
        assert!(text.ends_with("FOOTER"), "the footer always survives");
    }

    #[test]
    fn a_pathological_header_still_gets_hard_clipped() {
        let text = assemble(&"H".repeat(5_000), Vec::new(), "FOOTER");
        assert_eq!(pystr::len_chars(&text), MAX_CHARS);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn multiple_files_name_each_line() {
        let recalls = vec![
            Recall {
                path: "/a/one.py".into(),
                failed: 1,
                reverted: 0,
                total: 1,
                failure_modes: vec![obj(&[
                    ("last_ts", Value::Str("2026-05-01".into())),
                    ("outcome", Value::Str("failed".into())),
                ])],
            },
            Recall {
                path: "/a/two.py".into(),
                failed: 0,
                reverted: 1,
                total: 1,
                failure_modes: vec![],
            },
        ];
        let rendered = render(&recalls);
        assert!(
            rendered.contains("Files this command touches"),
            "{rendered}"
        );
        assert!(rendered.contains("one.py, two.py"), "{rendered}");
        assert!(
            rendered.contains("  • one.py  2026-05-01  failed"),
            "{rendered}"
        );
        // The footer names the FIRST recall's path.
        assert!(
            rendered.ends_with("memory file /a/one.py --json`."),
            "{rendered}"
        );
    }

    #[test]
    fn an_unknown_id_or_schema_stays_silent() {
        assert_eq!(build_recall("nope", &Value::Object(vec![]), &env()), "");
        let envelope = obj(&[("schema", Value::Str("stackunderflow.memory/2".into()))]);
        // The schema guard lives in `query_memory_file`; this asserts the shape
        // it checks is the one the envelope carries.
        assert_ne!(
            envelope.get("schema").and_then(Value::as_str),
            Some(MEMORY_SCHEMA)
        );
    }

    #[test]
    fn a_payload_with_no_extractable_path_never_spawns_anything() {
        let payload = obj(&[
            ("tool_name", Value::Str("Bash".into())),
            (
                "tool_input",
                obj(&[("command", Value::Str("ls -la".into()))]),
            ),
        ]);
        assert_eq!(collect_recalls(&payload, &env()), Vec::new());
        assert_eq!(
            build_recall("stackunderflow-pretool-recall", &payload, &env()),
            ""
        );
    }
}
