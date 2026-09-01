//! D3 — the transcript exfil detector (Spec 28 §4-D3).
//!
//! Rides transcripts the store already ingested, so it covers every provider
//! at once — and by construction it cannot see a client-side upload the agent
//! never ran as a command (the Grok case). That asymmetry is why D1/D2/D3 all
//! exist; the audit's coverage line says so out loud.
//!
//! The engine is pure — invocations in, findings out — so the rules are tested
//! without a store, and the CLI owns all the SQL.

use crate::{Detector, EgressFinding, Evidence, Posture, Severity};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One tool call from a session transcript, normalized across providers.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub session_id: String,
    pub provider: String,
    /// Ordering within the session — the secret→network window counts these.
    pub seq: i64,
    /// "Bash", "Read", "Edit", … — command rules only look at shell tools.
    pub tool_name: String,
    /// The shell command, empty for non-command tools.
    pub command: String,
    /// The path a file tool touched, if any.
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    /// A program invoked with any of `argv_any` (empty = any invocation).
    Program,
    /// A literal phrase anywhere in the command.
    Phrase,
    /// A hostname anywhere in the command.
    Host,
    /// Something from `left_any` piped into something from `right_any`.
    Pipeline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleFamily {
    pub id: String,
    pub kind: RuleKind,
    #[serde(default)]
    pub programs: Vec<String>,
    #[serde(default)]
    pub argv_any: Vec<String>,
    #[serde(default)]
    pub phrases: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub left_any: Vec<String>,
    #[serde(default)]
    pub right_any: Vec<String>,
    /// Only fire when the command names a host that is not allow-listed.
    #[serde(default)]
    pub requires_remote: bool,
    pub severity: Severity,
    pub title: String,
    pub veto: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptRules {
    pub version: u32,
    pub allow_hosts: Vec<String>,
    pub secret_path_markers: Vec<String>,
    /// How many invocations after a secret read still count as a chain.
    pub secret_window: i64,
    pub families: Vec<RuleFamily>,
}

const EMBEDDED_RULES: &str = include_str!("../../../../signatures/transcript-rules.json");

/// The rules compiled into the binary, validated on load.
pub fn transcript_rules() -> Result<TranscriptRules> {
    let rules: TranscriptRules = serde_json::from_str(EMBEDDED_RULES)
        .map_err(|e| anyhow::anyhow!("transcript rules do not parse: {e}"))?;
    if rules.families.is_empty() {
        bail!("transcript rules carry no families");
    }
    for fam in &rules.families {
        if !fam.id.starts_with("d3.") {
            bail!("{}: rule ids are namespaced d3.*", fam.id);
        }
        if fam.veto.trim().is_empty() || fam.title.trim().is_empty() {
            bail!("{}: every rule needs a title and a veto", fam.id);
        }
    }
    Ok(rules)
}

/// Shell tools whose payload is a command line. Everything else is context.
const COMMAND_TOOLS: &[&str] = &[
    "Bash",
    "bash",
    "shell",
    "run_terminal_cmd",
    "execute_command",
];

/// Scan a session's invocations. Findings dedupe to one per
/// (session, rule), carrying the hit count and the first command as evidence.
pub fn run_d3(rules: &TranscriptRules, calls: &[Invocation]) -> Vec<EgressFinding> {
    let mut hits: BTreeMap<String, Hit> = BTreeMap::new();
    let mut last_secret_read: BTreeMap<String, i64> = BTreeMap::new();

    for call in calls {
        if let Some(path) = &call.file_path
            && rules
                .secret_path_markers
                .iter()
                .any(|m| path.contains(m.as_str()))
        {
            last_secret_read.insert(call.session_id.clone(), call.seq);
        }
        if !COMMAND_TOOLS.contains(&call.tool_name.as_str()) || call.command.trim().is_empty() {
            continue;
        }
        for segment in segments(&call.command) {
            let remote = mentions_remote(segment, &rules.allow_hosts);
            for fam in &rules.families {
                if !matches(fam, segment, &call.command, remote) {
                    continue;
                }
                // A secret read shortly before a network command is the
                // pattern worth waking someone for (§4-D3, top severity).
                let chained = last_secret_read
                    .get(&call.session_id)
                    .is_some_and(|seq| call.seq - seq <= rules.secret_window);
                let severity = if chained {
                    Severity::Critical
                } else {
                    fam.severity
                };

                let entry = hits.entry(fam.id.clone()).or_insert_with(|| Hit {
                    provider: call.provider.clone(),
                    severity,
                    count: 0,
                    sessions: std::collections::BTreeSet::new(),
                    evidence: segment.trim().to_string(),
                    chained,
                });
                entry.count += 1;
                entry.sessions.insert(call.session_id.clone());
                if severity > entry.severity {
                    entry.severity = severity;
                    entry.chained = chained;
                    entry.evidence = segment.trim().to_string();
                }
            }
        }
    }

    let by_id: BTreeMap<&str, &RuleFamily> =
        rules.families.iter().map(|f| (f.id.as_str(), f)).collect();
    hits.into_iter()
        .map(|(rule_id, hit)| {
            let fam = by_id[rule_id.as_str()];
            let sessions = hit.sessions.len();
            let plural = if hit.count == 1 { "" } else { "s" };
            let spread = if sessions == 1 {
                format!("{} command{plural}, 1 session", hit.count)
            } else {
                format!("{} command{plural} across {sessions} sessions", hit.count)
            };
            let chain = if hit.chained {
                ", one right after reading a secret-shaped file"
            } else {
                ""
            };
            EgressFinding {
                provider: hit.provider,
                detector: Detector::Transcript,
                signature_id: rule_id,
                severity: hit.severity,
                posture: Posture::Occurred,
                title: format!("{} ({spread}{chain})", fam.title),
                evidence: Some(Evidence {
                    path: format!(
                        "{} session{}",
                        sessions,
                        if sessions == 1 { "" } else { "s" }
                    ),
                    line: None,
                    snippet: clip_command(&hit.evidence),
                }),
                remediation: Some(fam.veto.clone()),
                session_id: hit.sessions.iter().next().cloned(),
            }
        })
        .collect()
}

/// Split a payload into individual commands: newlines and shell separators.
/// A flag nine lines down belongs to a different command — conflating them
/// made `git tag -d …; curl …` look like an upload.
fn segments(command: &str) -> Vec<&str> {
    command
        .split(['\n', ';'])
        .flat_map(|line| line.split("&&"))
        .flat_map(|line| line.split("||"))
        .flat_map(|line| line.split('|'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

struct Hit {
    provider: String,
    severity: Severity,
    count: usize,
    sessions: std::collections::BTreeSet<String>,
    evidence: String,
    chained: bool,
}

/// `segment` is one command, original case (curl flags are case-sensitive:
/// `-d` uploads, `-D` dumps headers; `-F` posts a form, `-f` fails quietly).
/// `whole` is the full payload — only the pipeline rule may span segments.
fn matches(fam: &RuleFamily, segment: &str, whole: &str, remote: bool) -> bool {
    if fam.requires_remote && !remote {
        return false;
    }
    let lower = segment.to_lowercase();
    match fam.kind {
        RuleKind::Program => {
            if !fam.programs.iter().any(|p| invokes(segment, p)) {
                return false;
            }
            if !fam.argv_any.is_empty() && !fam.argv_any.iter().any(|a| has_flag(segment, a)) {
                return false;
            }
            // Copy tools take a direction: the remote must be the
            // DESTINATION. `scp host:file ./local` is a fetch, not an exfil.
            if fam
                .programs
                .iter()
                .any(|p| COPY_TOOLS.contains(&p.as_str()))
            {
                return remote_is_destination(segment, fam);
            }
            true
        }
        RuleKind::Phrase => fam
            .phrases
            .iter()
            .any(|p| lower.contains(&p.to_lowercase())),
        RuleKind::Host => {
            // Naming a paste host is not sending to one — require an actual
            // upload flag in the same command.
            fam.hosts.iter().any(|h| lower.contains(&h.to_lowercase()))
                && UPLOAD_FLAGS.iter().any(|f| has_flag(segment, f))
        }
        RuleKind::Pipeline => whole
            .split(['\n', ';'])
            .any(|line| pipes_into(fam, &line.to_lowercase())),
    }
}

/// Does one line pack something on the left of a pipe and push it on the
/// right? Scoped to a single line: a download on line 2 and an unrelated
/// `| tail` on line 3 is not an exfiltration pipeline.
fn pipes_into(fam: &RuleFamily, line_lower: &str) -> bool {
    let Some(pipe) = line_lower.find('|') else {
        return false;
    };
    let (left, right) = line_lower.split_at(pipe);
    fam.left_any
        .iter()
        .any(|l| left.contains(&l.to_lowercase()))
        && fam
            .right_any
            .iter()
            .any(|r| right.contains(&r.to_lowercase()))
}

/// Tools whose remote argument may be either source or destination.
const COPY_TOOLS: &[&str] = &["scp", "rsync", "sftp"];

/// Flags that mean "this command sends a payload".
const UPLOAD_FLAGS: &[&str] = &[
    "-T",
    "--upload-file",
    "-F",
    "--form",
    "-d",
    "--data",
    "--data-binary",
    "--data-raw",
    "--post-file",
    "--post-data",
];

/// For a copy tool, is the remote the LAST positional argument?
fn remote_is_destination(segment: &str, fam: &RuleFamily) -> bool {
    let positional: Vec<&str> = segment
        .split_whitespace()
        .skip_while(|t| !fam.programs.iter().any(|p| token_is_program(t, p)))
        .skip(1)
        .filter(|t| !t.starts_with('-'))
        .collect();
    // Skip quoted -e "ssh …" payloads and other option values by simply
    // asking whether the FINAL argument is the remote one.
    positional
        .last()
        .is_some_and(|last| looks_remote_target(last))
}

fn looks_remote_target(token: &str) -> bool {
    let token = token.trim_matches(['"', '\'']);
    if token.contains("://") {
        return true;
    }
    token
        .split_once('@')
        .is_some_and(|(user, rest)| !user.is_empty() && rest.contains(':'))
        || token.split_once(':').is_some_and(|(host, path)| {
            !host.is_empty() && !host.contains('/') && path.starts_with(['/', '~'])
        })
}

fn token_is_program(token: &str, program: &str) -> bool {
    token == program || token.ends_with(&format!("/{program}"))
}

/// Is `program` invoked in this segment — as the first real token?
/// Substring alone would match `curl-wrapper.sh`; `FOO=bar` prefixes are
/// assignments, not the command.
fn invokes(segment: &str, program: &str) -> bool {
    segment
        .split_whitespace()
        .find(|t| !t.contains('='))
        .is_some_and(|first| token_is_program(first, program))
}

/// Flag present as its own token (`-d`), or a multi-token phrase (`-X POST`).
/// CASE-SENSITIVE by design.
fn has_flag(segment: &str, flag: &str) -> bool {
    if flag.contains(' ') {
        return segment.contains(flag);
    }
    segment
        .split_whitespace()
        .any(|t| t == flag || t.split_once('=').is_some_and(|(head, _)| head == flag))
}

/// Does the segment name a host that is not allow-listed? Covers
/// `scheme://host/…`, `user@host:path`, and object-store URIs.
fn mentions_remote(segment: &str, allow: &[String]) -> bool {
    let allowed = |host: &str| {
        let host = host.to_lowercase();
        allow.iter().any(|a| {
            host == a.to_lowercase() || host.starts_with(&format!("{}:", a.to_lowercase()))
        })
    };
    for token in segment.split_whitespace() {
        let token = token.trim_matches(['"', '\'', '(', ')']);
        if token.contains('=') && !token.contains("://") {
            continue; // a plain assignment, not a target
        }
        if let Some((_, rest)) = token.split_once("://") {
            let host = rest.split(['/', '?']).next().unwrap_or("");
            if !host.is_empty() && !allowed(host) {
                return true;
            }
        }
        if let Some((user, rest)) = token.split_once('@')
            && !user.is_empty()
            && let Some((host, _)) = rest.split_once(':')
            && !host.is_empty()
            && !allowed(host)
        {
            return true;
        }
        if token.starts_with("s3://") || token.starts_with("gs://") {
            return true;
        }
    }
    false
}

/// Evidence stays a single readable line — transcripts contain heredocs.
fn clip_command(command: &str) -> String {
    let one_line: String = command.split('\n').next().unwrap_or(command).into();
    let chars: Vec<char> = one_line.chars().collect();
    if chars.len() <= 160 {
        return one_line;
    }
    let mut out: String = chars[..159].iter().collect();
    out.push('…');
    out
}
