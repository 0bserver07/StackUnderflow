//! D1 — the config detector: evaluate every signature against a home
//! directory, emit findings + coverage. Preventive: needs no session (§4.1).

use crate::{AgentSignature, Detector, EgressFinding, Evidence, Posture, Severity};
use std::path::PathBuf;

/// All `~/` in signatures resolve against this home. Tests hand a synthetic
/// one; the CLI hands the real one.
pub struct ScanContext {
    pub home: PathBuf,
}

/// Per-agent audit accounting — the header line's denominator and the honest
/// tail ("2 artifacts unreadable") both come from here.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentCoverage {
    pub agent: String,
    pub detected: bool,
    pub at_risk: usize,
    pub protected: usize,
    pub unknown: usize,
    pub skipped_artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
}

/// What one D1 run produces.
#[derive(Debug, serde::Serialize)]
pub struct AuditReport {
    pub findings: Vec<EgressFinding>,
    pub coverage: Vec<AgentCoverage>,
}

pub fn run_d1(catalog: &[AgentSignature], ctx: &ScanContext) -> AuditReport {
    let mut findings = Vec::new();
    let mut coverage = Vec::new();

    for sig in catalog {
        let detected = sig.detect_dirs.iter().any(|d| resolve(ctx, d).exists());
        let mut cov = AgentCoverage {
            agent: sig.agent.clone(),
            detected,
            at_risk: 0,
            protected: 0,
            unknown: 0,
            skipped_artifacts: Vec::new(),
            pending: sig.pending.clone(),
        };
        if !detected {
            coverage.push(cov);
            continue;
        }

        if let Some(reason) = &sig.pending {
            findings.push(EgressFinding {
                provider: sig.agent.clone(),
                detector: Detector::Config,
                signature_id: format!("{}.pending", sig.agent),
                severity: Severity::Info,
                posture: Posture::Unknown,
                title: format!(
                    "{} detected, no verified signature yet — posture unknown, not safe ({reason})",
                    sig.agent
                ),
                evidence: None,
                remediation: None,
                session_id: None,
            });
            cov.unknown += 1;
            coverage.push(cov);
            continue;
        }

        for check in &sig.checks {
            let path = resolve(ctx, &check.file);
            if !path.is_file() {
                if check.at_risk_when_unset {
                    cov.at_risk += 1;
                    findings.push(finding(
                        sig,
                        check,
                        Posture::AtRisk,
                        check.severity,
                        check.title.clone(),
                        Some(Evidence {
                            path: check.file.clone(),
                            line: None,
                            snippet: format!("{} is unset ({} not present)", check.key, check.file),
                        }),
                    ));
                } else {
                    cov.skipped_artifacts.push(check.file.clone());
                }
                continue;
            }

            let readable = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|text| crate::parse::read_artifact(&text, check.format));
            let tree = match readable {
                Ok(tree) => tree,
                Err(err) => {
                    cov.unknown += 1;
                    cov.skipped_artifacts.push(check.file.clone());
                    findings.push(finding(
                        sig,
                        check,
                        Posture::Unknown,
                        Severity::Info,
                        format!("{} — artifact cannot be read", check.title),
                        Some(Evidence {
                            path: check.file.clone(),
                            line: None,
                            snippet: format!("{err}"),
                        }),
                    ));
                    continue;
                }
            };

            match crate::parse::lookup(&tree, &check.key) {
                None => {
                    if check.at_risk_when_unset {
                        cov.at_risk += 1;
                        findings.push(finding(
                            sig,
                            check,
                            Posture::AtRisk,
                            check.severity,
                            check.title.clone(),
                            Some(Evidence {
                                path: check.file.clone(),
                                line: None,
                                snippet: format!("{} is unset (no local veto)", check.key),
                            }),
                        ));
                    }
                }
                Some(value) => {
                    let snippet = format!("{} = {value}", check.key);
                    if check.uploading_when.iter().any(|u| u == value) {
                        cov.at_risk += 1;
                        findings.push(finding(
                            sig,
                            check,
                            Posture::AtRisk,
                            check.severity,
                            check.title.clone(),
                            Some(Evidence {
                                path: check.file.clone(),
                                line: None,
                                snippet,
                            }),
                        ));
                    } else if check.safe_when.iter().any(|s| s == value) {
                        cov.protected += 1;
                    } else {
                        cov.unknown += 1;
                        findings.push(finding(
                            sig,
                            check,
                            Posture::Unknown,
                            Severity::Info,
                            format!("{} — unrecognized value", check.title),
                            Some(Evidence {
                                path: check.file.clone(),
                                line: None,
                                snippet,
                            }),
                        ));
                    }
                }
            }
        }
        coverage.push(cov);
    }

    AuditReport { findings, coverage }
}

fn finding(
    sig: &AgentSignature,
    check: &crate::Check,
    posture: Posture,
    severity: Severity,
    title: String,
    evidence: Option<Evidence>,
) -> EgressFinding {
    EgressFinding {
        provider: sig.agent.clone(),
        detector: Detector::Config,
        signature_id: check.id.clone(),
        severity,
        posture,
        title,
        evidence,
        remediation: Some(check.veto.clone()),
        session_id: None,
    }
}

/// `~/x` resolves under the scan home; anything else is taken as
/// home-relative too, so a synthetic home can never escape itself.
fn resolve(ctx: &ScanContext, path: &str) -> PathBuf {
    let rel = path.strip_prefix("~/").unwrap_or(path);
    ctx.home.join(rel)
}
