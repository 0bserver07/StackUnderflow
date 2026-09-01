//! `stax audit` — Spec 28's D1 config audit, the preventive detector: which
//! coding agents on this machine are configured (or lack the veto) to send
//! your code or telemetry off-box, and the exact line that stops each one.

use crate::click::Output;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Emit findings + coverage as JSON (machines, CI).
    #[arg(long)]
    pub json: bool,

    /// Read signatures from a directory instead of the compiled-in catalog.
    #[arg(long, value_name = "DIR")]
    pub signatures: Option<PathBuf>,

    /// Exit 2 when any finding is at risk (CI gate).
    #[arg(long)]
    pub strict: bool,

    /// Skip the transcript scan (D3) and audit configs only.
    #[arg(long)]
    pub configs_only: bool,

    /// How many recent tool-carrying messages D3 reads.
    #[arg(long, value_name = "N")]
    pub window: Option<i64>,
}

pub fn run_audit(args: &AuditArgs) -> Result<Output> {
    let catalog = match &args.signatures {
        Some(dir) => stax_audit::catalog_from_dir(dir)?,
        None => stax_audit::embedded_catalog()?,
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate agent configs"))?;
    let mut report = stax_audit::run_d1(&catalog, &stax_audit::ScanContext { home });

    // D3 rides the transcripts the store already ingested, so it covers every
    // provider at once — including agents with no config signature.
    if !args.configs_only {
        let window = args
            .window
            .unwrap_or(crate::audit_transcripts::DEFAULT_WINDOW);
        let scan = crate::audit_transcripts::collect(window);
        match scan.unavailable {
            Some(reason) => report.transcript_note = Some(reason),
            None => {
                let rules = stax_audit::transcript_rules()?;
                report
                    .findings
                    .extend(stax_audit::run_d3(&rules, &scan.invocations));
                report.transcript_note = Some(format!(
                    "{} sessions scanned ({} tool calls, newest {window} messages)",
                    scan.sessions,
                    scan.invocations.len()
                ));
            }
        }
    }

    let at_risk = report.findings.iter().any(|f| {
        matches!(
            f.posture,
            stax_audit::Posture::AtRisk | stax_audit::Posture::Occurred
        )
    });

    let body = if args.json {
        let mut json = serde_json::to_string_pretty(&report)?;
        json.push('\n');
        json
    } else {
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.parse().ok())
            .unwrap_or(100);
        stax_audit::render_table(&report, width)
    };

    Ok(Output {
        stdout: body,
        stderr: String::new(),
        code: if args.strict && at_risk { 2 } else { 0 },
    })
}
