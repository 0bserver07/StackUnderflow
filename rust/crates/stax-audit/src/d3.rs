//! D3 — the transcript exfil detector (Spec 28 §4-D3).
//!
//! Rides transcripts the store already ingested, so it covers every provider
//! at once — and by construction it cannot see a client-side upload the agent
//! never ran as a command (the Grok case). That asymmetry is why D1/D2/D3 all
//! exist; the audit's coverage line says so out loud.
//!
//! The engine is pure — invocations in, findings out — so the rules are tested
//! without a store, and the CLI owns all the SQL.

use crate::{Detector, Evidence, EgressFinding, Posture, Severity};
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
const COMMAND_TOOLS: &[&str] = &["Bash", "bash", "shell", "run_terminal_cmd", "execute_command"];

/// Scan a session's invocations. Findings dedupe to one per
/// (session, rule), carrying the hit count and the first command as evidence.
pub fn run_d3(rules: &TranscriptRules, calls: &[Invocation]) -> Vec<EgressFinding> {
    let mut hits: BTreeMap<(String, String), Hit> = BTreeMap::new();
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
        let lower = call.command.to_lowercase();
        let remote = mentions_remote(&lower, &rules.allow_hosts);

        for fam in &rules.families {
            if !matches(fam, &lower, remote) {
                continue;
            }
            // A secret read shortly before a network command is the pattern
            // worth waking someone for (§4-D3, highest severity).
            let chained = last_secret_read
                .get(&call.session_id)
                .is_some_and(|seq| call.seq - seq <= rules.secret_window);
            let severity = if chained { Severity::Critical } else { fam.severity };

            let entry = hits
                .entry((call.session_id.clone(), fam.id.clone()))
                .or_insert_with(|| Hit {
                    provider: call.provider.clone(),
                    severity,
                    count: 0,
                    first_command: call.command.clone(),
                    chained,
                });
            entry.count += 1;
            if severity > entry.severity {
                entry.severity = severity;
                entry.chained = chained;
                entry.first_command = call.command.clone();
            }
        }
    }

    let by_id: BTreeMap<&str, &RuleFamily> =
        rules.families.iter().map(|f| (f.id.as_str(), f)).collect();
    hits.into_iter()
        .map(|((session, rule_id), hit)| {
            let fam = by_id[rule_id.as_str()];
            let plural = if hit.count == 1 { "" } else { "s" };
            let chain = if hit.chained {
                " right after reading a secret-shaped file"
            } else {
                ""
            };
            EgressFinding {
                provider: hit.provider,
                detector: Detector::Transcript,
                signature_id: rule_id,
                severity: hit.severity,
                posture: Posture::Occurred,
                title: format!("{} ({} command{plural}{chain})", fam.title, hit.count),
                evidence: Some(Evidence {
                    path: format!("session {session}"),
                    line: None,
                    snippet: clip_command(&hit.first_command),
                }),
                remediation: Some(fam.veto.clone()),
                session_id: Some(session),
            }
        })
        .collect()
}

struct Hit {
    provider: String,
    severity: Severity,
    count: usize,
    first_command: String,
    chained: bool,
}

fn matches(fam: &RuleFamily, lower: &str, remote: bool) -> bool {
    if fam.requires_remote && !remote {
        return false;
    }
    match fam.kind {
        RuleKind::Program => {
            if !fam.programs.iter().any(|p| invokes(lower, p)) {
                return false;
            }
            fam.argv_any.is_empty() || fam.argv_any.iter().any(|a| has_flag(lower, a))
        }
        RuleKind::Phrase => fam.phrases.iter().any(|p| lower.contains(&p.to_lowercase())),
        RuleKind::Host => fam.hosts.iter().any(|h| lower.contains(&h.to_lowercase())),
        RuleKind::Pipeline => {
            let Some(pipe) = lower.find('|') else {
                return false;
            };
            let (left, right) = lower.split_at(pipe);
            fam.left_any.iter().any(|l| left.contains(&l.to_lowercase()))
                && fam.right_any.iter().any(|r| right.contains(&r.to_lowercase()))
        }
    }
}

/// Is `program` invoked here — at the start, or after a shell separator?
/// Substring alone would match `securely-curl-free.sh`.
fn invokes(lower: &str, program: &str) -> bool {
    lower.split(['|', ';', '&', '\n', '(']).any(|seg| {
        seg.split_whitespace()
            .find(|t| !t.contains('=')) // skip `FOO=bar` prefixes
            .is_some_and(|first| first == program || first.ends_with(&format!("/{program}")))
    })
}

/// Flag present as its own token (`-d`), or as a multi-token phrase
/// (`-X POST`) — never as a fragment of a longer flag.
fn has_flag(lower: &str, flag: &str) -> bool {
    let flag = flag.to_lowercase();
    if flag.contains(' ') {
        return lower.contains(&flag);
    }
    lower.split_whitespace().any(|t| {
        t == flag || t.split_once('=').is_some_and(|(head, _)| head == flag)
    })
}

/// Does the command name a host that is not allow-listed? Covers
/// `scheme://host/…` and `user@host:path`.
fn mentions_remote(lower: &str, allow: &[String]) -> bool {
    let allowed = |host: &str| {
        allow
            .iter()
            .any(|a| host == a.to_lowercase() || host.starts_with(&format!("{}:", a.to_lowercase())))
    };
    for token in lower.split_whitespace() {
        if let Some((_, rest)) = token.split_once("://") {
            let host = rest.split(['/', '?']).next().unwrap_or("");
            if !host.is_empty() && !allowed(host) {
                return true;
            }
        }
        // `user@host:path` — an scp target, not an email in prose.
        if let Some((user, rest)) = token.split_once('@')
            && !user.is_empty()
            && let Some((host, _)) = rest.split_once(':')
            && !host.is_empty()
            && !allowed(host)
        {
            return true;
        }
        // `s3://` and friends carry no host but are still off-box.
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
