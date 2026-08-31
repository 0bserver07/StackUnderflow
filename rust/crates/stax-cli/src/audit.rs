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
}

pub fn run_audit(args: &AuditArgs) -> Result<Output> {
    let catalog = match &args.signatures {
        Some(dir) => stax_audit::catalog_from_dir(dir)?,
        None => stax_audit::embedded_catalog()?,
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate agent configs"))?;
    let report = stax_audit::run_d1(&catalog, &stax_audit::ScanContext { home });

    let at_risk = report
        .findings
        .iter()
        .any(|f| matches!(f.posture, stax_audit::Posture::AtRisk));

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
