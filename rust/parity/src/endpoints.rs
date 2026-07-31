//! The endpoint differ: boot both servers, walk a case file, diff the bytes.
//!
//! The CLI gate (`parity-cli.sh`, gate 4) proved the shape works: run the two
//! implementations against the *same* store state and diff stdout byte for
//! byte. This is that gate for HTTP.
//!
//! # What it compares, and why that list
//!
//! * **status** — the cheapest divergence to ship and the loudest to a client.
//! * **`content-type`** — the one header that is a contract rather than an
//!   artefact. `content-length` is derived from the body (so the body diff
//!   already covers it), `date` / `server` / `etag` / `last-modified` are
//!   per-process or per-inode and would only ever produce noise.
//! * **body bytes** — the whole point. Not "the same JSON": the same bytes.
//!   Key order, float presentation and `ensure_ascii` are all invisible to a
//!   parsed comparison and all of them have already bitten this campaign.
//!
//! # Why the cases run in order
//!
//! `deps.current_log_path` is server state that `POST /api/project-by-dir`
//! writes and every project-scoped `GET` reads. A differ that shuffled its
//! cases, or ran them concurrently, would be diffing two different sessions.
//! So: one case at a time, Python first (it is also the side that may migrate
//! the shared store), then Rust.
//!
//! # Known-open cases
//!
//! An id prefixed `!` is **reported but not failed**. That is not a snooze
//! button — the differ prints every one of them under a loud banner with its
//! diff, so an unported endpoint is visible in the gate output rather than
//! absent from the case file. It is the difference between "we know this is
//! open" and "nobody looked".

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::http::{self, Response};

/// One row of the case file.
#[derive(Debug, Clone)]
pub struct Case {
    /// Stable id, used for the diff filename.
    pub id: String,
    /// `GET` / `POST` / …
    pub method: String,
    /// Path plus query string, sent verbatim.
    pub target: String,
    /// JSON request body, or `None`.
    pub body: Option<String>,
    /// Reported but never failed — an endpoint this wave has not ported yet.
    pub known_open: bool,
}

/// The verdict for one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Status, `content-type` and every body byte agree.
    Identical,
    /// A real difference, with a rendered explanation.
    Divergent(String),
    /// A `!`-marked case that differs — counted separately, never fatal.
    KnownOpen(String),
    /// Neither side could be reached; the harness failed, not the port.
    Error(String),
}

/// One case's outcome plus the timings, so the report can carry both.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The case this describes.
    pub case: Case,
    /// The verdict.
    pub verdict: Verdict,
    /// Milliseconds the Python side took.
    pub py_ms: u128,
    /// Milliseconds the Rust side took.
    pub rs_ms: u128,
}

/// Parse a case file.
///
/// Format, `|`-separated, `#` comments and blank lines ignored:
///
/// ```text
/// <id> | <METHOD> | <path-and-query> | <json-body or ->
/// ```
///
/// # Errors
/// A row with fewer than three fields — a typo that would otherwise silently
/// shrink the matrix.
pub fn parse_cases(text: &str) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('|').map(str::trim).collect();
        if fields.len() < 3 {
            return Err(format!(
                "case file line {}: expected `id | METHOD | target [| body]`, got {line:?}",
                lineno + 1
            ));
        }
        let (known_open, id) = match fields[0].strip_prefix('!') {
            Some(rest) => (true, rest.trim().to_owned()),
            None => (false, fields[0].to_owned()),
        };
        let body = fields
            .get(3)
            .copied()
            .filter(|b| !b.is_empty() && *b != "-")
            .map(str::to_owned);
        cases.push(Case {
            id,
            method: fields[1].to_ascii_uppercase(),
            target: fields[2].to_owned(),
            body,
            known_open,
        });
    }
    Ok(cases)
}

/// Run one case against both servers and judge it.
#[must_use]
pub fn run_case(case: &Case, py_port: u16, rs_port: u16, timeout: Duration) -> Outcome {
    let body = case.body.as_deref().map(str::as_bytes);

    let py_started = std::time::Instant::now();
    let py = http::request(py_port, &case.method, &case.target, body, timeout);
    let py_ms = py_started.elapsed().as_millis();

    let rs_started = std::time::Instant::now();
    let rs = http::request(rs_port, &case.method, &case.target, body, timeout);
    let rs_ms = rs_started.elapsed().as_millis();

    let verdict = match (py, rs) {
        (Ok(py), Ok(rs)) => judge(case, &py, &rs),
        (Err(err), _) => Verdict::Error(format!("python side: {err}")),
        (_, Err(err)) => Verdict::Error(format!("rust side: {err}")),
    };
    Outcome {
        case: case.clone(),
        verdict,
        py_ms,
        rs_ms,
    }
}

fn judge(case: &Case, py: &Response, rs: &Response) -> Verdict {
    let mut problems = String::new();
    if py.status != rs.status {
        let _ = writeln!(
            problems,
            "  status      python {} vs rust {}",
            py.status, rs.status
        );
    }
    let py_ct = py.header("content-type").unwrap_or("<absent>");
    let rs_ct = rs.header("content-type").unwrap_or("<absent>");
    if py_ct != rs_ct {
        let _ = writeln!(
            problems,
            "  content-type python {py_ct:?} vs rust {rs_ct:?}"
        );
    }
    if py.body != rs.body {
        let _ = writeln!(
            problems,
            "  body        {} bytes vs {} bytes; {}",
            py.body.len(),
            rs.body.len(),
            describe_body_diff(&py.body, &rs.body)
        );
    }
    if problems.is_empty() {
        return Verdict::Identical;
    }
    if case.known_open {
        Verdict::KnownOpen(problems)
    } else {
        Verdict::Divergent(problems)
    }
}

/// The first byte offset where two bodies differ, with a window of context.
///
/// A whole-body dump of a multi-megabyte `/api/projects` response is unusable
/// in a gate log; the offset plus 60 bytes either side is what a human actually
/// needs to name the field. The full bodies are written to disk alongside.
#[must_use]
pub fn describe_body_diff(py: &[u8], rs: &[u8]) -> String {
    let at = py
        .iter()
        .zip(rs.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| py.len().min(rs.len()));
    let start = at.saturating_sub(60);
    format!(
        "first difference at byte {at}\n    python …{}…\n    rust   …{}…",
        String::from_utf8_lossy(&py[start..(at + 60).min(py.len())]),
        String::from_utf8_lossy(&rs[start..(at + 60).min(rs.len())]),
    )
}

/// Write both bodies for a divergent case so the diff survives the gate log.
///
/// # Errors
/// Any filesystem failure — the caller reports it rather than losing the run.
pub fn dump_bodies(dir: &Path, case: &Case, py: &[u8], rs: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(format!("{}.python", case.id)), py)?;
    std::fs::write(dir.join(format!("{}.rust", case.id)), rs)?;
    Ok(())
}

/// Counts for the closing tally.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Byte-identical cases.
    pub identical: usize,
    /// Real divergences — these fail the gate.
    pub divergent: usize,
    /// `!`-marked cases that differ — reported, never fatal.
    pub known_open: usize,
    /// Harness failures (a server that would not answer).
    pub errors: usize,
}

impl Tally {
    /// Fold one outcome in.
    pub fn add(&mut self, verdict: &Verdict) {
        match verdict {
            Verdict::Identical => self.identical += 1,
            Verdict::Divergent(_) => self.divergent += 1,
            Verdict::KnownOpen(_) => self.known_open += 1,
            Verdict::Error(_) => self.errors += 1,
        }
    }

    /// The process exit code: 0 clean, 1 divergence, 2 harness failure.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        if self.errors > 0 {
            2
        } else if self.divergent > 0 {
            1
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_parse_with_and_without_bodies() {
        let cases = parse_cases(
            "# a comment\n\
             P-list | GET | /api/projects | -\n\
             \n\
             P-set  | POST | /api/project-by-dir | {\"dir_name\":\"x\"}\n\
             !D-stats | GET | /api/stats\n",
        )
        .expect("parses");
        assert_eq!(cases.len(), 3);
        assert_eq!(cases[0].id, "P-list");
        assert!(cases[0].body.is_none());
        assert_eq!(cases[1].method, "POST");
        assert_eq!(cases[1].body.as_deref(), Some("{\"dir_name\":\"x\"}"));
        assert!(!cases[1].known_open);
        assert_eq!(cases[2].id, "D-stats");
        assert!(cases[2].known_open);
    }

    #[test]
    fn a_short_row_is_an_error_not_a_silently_smaller_matrix() {
        assert!(parse_cases("oops | GET").is_err());
    }

    fn response(status: u16, content_type: &str, body: &str) -> Response {
        Response {
            status,
            headers: vec![("content-type".to_owned(), content_type.to_owned())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn case(known_open: bool) -> Case {
        Case {
            id: "X".to_owned(),
            method: "GET".to_owned(),
            target: "/api/projects".to_owned(),
            body: None,
            known_open,
        }
    }

    #[test]
    fn identical_responses_are_identical() {
        let py = response(200, "application/json", r#"{"a":1}"#);
        let rs = response(200, "application/json", r#"{"a":1}"#);
        assert_eq!(judge(&case(false), &py, &rs), Verdict::Identical);
    }

    #[test]
    fn key_order_alone_is_a_divergence() {
        // The exact class of difference a parsed comparison would call equal.
        let py = response(200, "application/json", r#"{"a":1,"b":2}"#);
        let rs = response(200, "application/json", r#"{"b":2,"a":1}"#);
        assert!(matches!(
            judge(&case(false), &py, &rs),
            Verdict::Divergent(_)
        ));
    }

    #[test]
    fn float_presentation_alone_is_a_divergence() {
        // ryu's `1e16` against CPython's `1e+16` — same value, different bytes.
        let py = response(200, "application/json", r#"{"n":1e+16}"#);
        let rs = response(200, "application/json", r#"{"n":1e16}"#);
        assert!(matches!(
            judge(&case(false), &py, &rs),
            Verdict::Divergent(_)
        ));
    }

    #[test]
    fn a_charset_on_the_content_type_is_a_divergence() {
        let py = response(200, "application/json", "{}");
        let rs = response(200, "application/json; charset=utf-8", "{}");
        assert!(matches!(
            judge(&case(false), &py, &rs),
            Verdict::Divergent(_)
        ));
    }

    #[test]
    fn a_known_open_case_is_reported_not_failed() {
        let py = response(200, "application/json", "{}");
        let rs = response(404, "application/json", r#"{"detail":"Not Found"}"#);
        assert!(matches!(
            judge(&case(true), &py, &rs),
            Verdict::KnownOpen(_)
        ));
    }

    #[test]
    fn the_exit_code_ranks_harness_failure_above_divergence() {
        let mut tally = Tally::default();
        tally.add(&Verdict::Identical);
        assert_eq!(tally.exit_code(), 0);
        tally.add(&Verdict::KnownOpen(String::new()));
        assert_eq!(tally.exit_code(), 0, "known-open never fails the gate");
        tally.add(&Verdict::Divergent(String::new()));
        assert_eq!(tally.exit_code(), 1);
        tally.add(&Verdict::Error(String::new()));
        assert_eq!(tally.exit_code(), 2);
    }

    #[test]
    fn the_diff_description_names_the_first_differing_byte() {
        let text = describe_body_diff(br#"{"a":1,"b":2}"#, br#"{"a":1,"b":3}"#);
        assert!(text.contains("first difference at byte 11"), "{text}");
    }
}
