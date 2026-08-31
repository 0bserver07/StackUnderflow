//! stax-audit — Spec 28's egress audit: the signature catalog and the D1
//! config detector.
//!
//! The catalog is declarative JSON, one file per agent under `signatures/` at
//! the repo top (Spec 28 §6, following the `adapters/capabilities.json`
//! pattern). The engine evaluates signatures against on-disk artifacts and
//! emits [`EgressFinding`]s in the §5 shape. Postures are honest by
//! construction: a missing veto is `at_risk`, a present one is `protected`,
//! anything unreadable or unmodeled is `unknown` — never `safe` (§8.3).

#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

mod d1;
mod parse;

pub use d1::{AgentCoverage, AuditReport, ScanContext, run_d1};

/// Finding severity, ordered — render sorts descending on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

/// Spec 28 §5 posture. There is deliberately no `Safe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Posture {
    AtRisk,
    Occurred,
    Protected,
    Unknown,
}

/// Which detector produced a finding (§5; `wire` reserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Detector {
    Config,
    Event,
    Transcript,
    Wire,
}

/// A reproducible pointer to what fired (§5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    /// Path of the artifact, with the home prefix printed as `~`.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub snippet: String,
}

/// The one shape every detector emits (Spec 28 §5). D2's `scope` block joins
/// when the event detector lands; omitting it now keeps the JSON stable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressFinding {
    pub provider: String,
    pub detector: Detector,
    pub signature_id: String,
    pub severity: Severity,
    pub posture: Posture,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// The exact veto line. Suggested, never applied (§1 out-of-scope).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Artifact format a check knows how to read. `TomlLite` is the hand-rolled
/// sections + `key = value` subset (no new parser dependency); anything it
/// cannot read degrades the check to `unknown`, never to silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    Json,
    TomlLite,
    Env,
}

/// One D1 check: read `file`, walk `key`, compare against the value lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    /// Stable id, `<agent>.<name>` (dedupe key with provider, §5).
    pub id: String,
    /// Artifact path; `~/` resolves against the scan home.
    pub file: String,
    pub format: Format,
    /// Dotted path inside the artifact (`telemetry.trace_upload`), or the
    /// variable name for `env` artifacts.
    pub key: String,
    /// Values that mean "configured to upload" → `at_risk`.
    pub uploading_when: Vec<serde_json::Value>,
    /// Values that mean the veto is present → `protected`. A value matching
    /// neither list reports `unknown` (out-of-range, fail-closed).
    #[serde(default)]
    pub safe_when: Vec<serde_json::Value>,
    /// If true, a missing key means the veto is absent → `at_risk` (the Grok
    /// shape: unset `trace_upload` means uploads follow the remote flag, §0.3).
    #[serde(default)]
    pub at_risk_when_unset: bool,
    pub title: String,
    /// The exact remediation line. Mandatory: a warning without the fix is
    /// noise (§4.1).
    pub veto: String,
    pub severity: Severity,
}

/// One agent's signature file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSignature {
    pub agent: String,
    pub version: u32,
    /// Directories whose existence means "this agent is on the machine" —
    /// the denominator of the audit header line.
    pub detect_dirs: Vec<String>,
    /// Present when the signature is not yet verified against the agent's
    /// real, current config format: the agent still counts as detected, and
    /// the audit reports posture `unknown` with this reason. Honest > wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    #[serde(default)]
    pub checks: Vec<Check>,
}

/// Parse and validate one signature file (fail-closed: invalid = error).
pub fn catalog_from_str(s: &str) -> Result<AgentSignature> {
    let sig: AgentSignature = serde_json::from_str(s)
        .map_err(|e| anyhow::anyhow!("signature does not parse: {e}"))?;
    validate(&sig)?;
    Ok(sig)
}

fn validate(sig: &AgentSignature) -> Result<()> {
    if sig.agent.trim().is_empty() {
        bail!("signature has an empty agent name");
    }
    if sig.detect_dirs.is_empty() {
        bail!("{}: detect_dirs is empty — the audit header needs a denominator", sig.agent);
    }
    if sig.checks.is_empty() && sig.pending.is_none() {
        bail!(
            "{}: a live signature needs at least one check (or an explicit `pending` reason)",
            sig.agent
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for check in &sig.checks {
        if !seen.insert(check.id.as_str()) {
            bail!("{}: duplicate check id {}", sig.agent, check.id);
        }
        let prefix = format!("{}.", sig.agent);
        if !check.id.starts_with(&prefix) {
            bail!("{}: check id {} must start with {prefix}", sig.agent, check.id);
        }
        if check.file.trim().is_empty() {
            bail!("{}: check {} names no file artifact", sig.agent, check.id);
        }
        if check.key.trim().is_empty() {
            bail!("{}: check {} has an empty key", sig.agent, check.id);
        }
        if check.title.trim().is_empty() {
            bail!("{}: check {} has an empty title", sig.agent, check.id);
        }
        if check.veto.trim().is_empty() {
            bail!(
                "{}: check {} has an empty veto — a warning without the fix is noise",
                sig.agent,
                check.id
            );
        }
        if check.uploading_when.is_empty() && !check.at_risk_when_unset {
            bail!("{}: check {} can never fire", sig.agent, check.id);
        }
    }
    Ok(())
}

/// Load and validate every `*.json` signature in a directory (sorted by file
/// name, so output order is deterministic).
pub fn catalog_from_dir(dir: &Path) -> Result<Vec<AgentSignature>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read signature dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let mut catalog = Vec::new();
    let mut agents = std::collections::BTreeSet::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let sig = catalog_from_str(&text)
            .with_context(|| format!("invalid signature {}", path.display()))?;
        if !agents.insert(sig.agent.clone()) {
            bail!("duplicate agent {} in {}", sig.agent, path.display());
        }
        catalog.push(sig);
    }
    Ok(catalog)
}

/// The signature files compiled into the binary. An installed `stax` far from
/// any checkout audits with these (the wave-10 embedded-copies pattern); a
/// checkout or `--signatures <dir>` overrides them.
const EMBEDDED: &[(&str, &str)] = &[
    ("claude", include_str!("../../../../signatures/claude.json")),
    ("codex", include_str!("../../../../signatures/codex.json")),
    ("copilot", include_str!("../../../../signatures/copilot.json")),
    ("cursor", include_str!("../../../../signatures/cursor.json")),
    ("gemini", include_str!("../../../../signatures/gemini.json")),
    ("grok", include_str!("../../../../signatures/grok.json")),
];

/// The catalog compiled into the binary, validated on load like any other.
pub fn embedded_catalog() -> Result<Vec<AgentSignature>> {
    let mut catalog = Vec::new();
    for (name, text) in EMBEDDED {
        let sig = catalog_from_str(text)
            .with_context(|| format!("embedded signature {name} is invalid"))?;
        if sig.agent != *name {
            bail!("embedded signature {name} declares agent {}", sig.agent);
        }
        catalog.push(sig);
    }
    Ok(catalog)
}
