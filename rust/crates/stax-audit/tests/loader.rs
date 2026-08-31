//! B1 — the signature catalog loads, validates, and fails closed.
//!
//! Spec 28 §6: signatures are declarative data following the
//! `adapters/capabilities.json` pattern. A catalog that does not validate is a
//! load error, never a silently-skipped check (§4.4 fail-closed).

use stax_audit::{Severity, catalog_from_str};

const TESTAGENT: &str = r#"{
  "agent": "testagent",
  "version": 1,
  "detect_dirs": ["~/.testagent"],
  "checks": [
    {
      "id": "testagent.telemetry",
      "file": "~/.testagent/settings.json",
      "format": "json",
      "key": "telemetry.enabled",
      "uploading_when": [true],
      "title": "testagent uploads telemetry",
      "veto": "set telemetry.enabled=false in ~/.testagent/settings.json",
      "severity": "high"
    }
  ]
}"#;

#[test]
fn loads_a_signature_and_its_check() {
    let sig = catalog_from_str(TESTAGENT).expect("valid signature must load");
    assert_eq!(sig.agent, "testagent");
    assert_eq!(sig.checks.len(), 1);
    let check = &sig.checks[0];
    assert_eq!(check.id, "testagent.telemetry");
    assert_eq!(check.severity, Severity::High);
    assert!(check.veto.contains("telemetry.enabled=false"));
}

#[test]
fn duplicate_check_ids_are_a_load_error() {
    let two = r#"{
  "agent": "testagent",
  "version": 1,
  "detect_dirs": ["~/.testagent"],
  "checks": [
    { "id": "testagent.telemetry", "file": "~/.t/a.json", "format": "json",
      "key": "a", "uploading_when": [true], "title": "a", "veto": "va", "severity": "high" },
    { "id": "testagent.telemetry", "file": "~/.t/b.json", "format": "json",
      "key": "b", "uploading_when": [true], "title": "b", "veto": "vb", "severity": "low" }
  ]
}"#;
    let err = catalog_from_str(two).unwrap_err().to_string();
    assert!(err.contains("duplicate"), "got: {err}");
    assert!(err.contains("testagent.telemetry"), "got: {err}");
}

#[test]
fn missing_veto_is_a_load_error() {
    let noveto = TESTAGENT.replace(
        r#""veto": "set telemetry.enabled=false in ~/.testagent/settings.json","#,
        r#""veto": "","#,
    );
    let err = catalog_from_str(&noveto).unwrap_err().to_string();
    assert!(err.contains("veto"), "got: {err}");
}

#[test]
fn unknown_severity_is_a_load_error() {
    let bad = TESTAGENT.replace(r#""severity": "high""#, r#""severity": "shrug""#);
    assert!(catalog_from_str(&bad).is_err());
}

#[test]
fn a_check_needs_file_or_env_and_a_key_for_files() {
    let neither = TESTAGENT.replace(r#""file": "~/.testagent/settings.json","#, "");
    let err = catalog_from_str(&neither).unwrap_err().to_string();
    assert!(err.contains("file") || err.contains("env"), "got: {err}");
}

#[test]
fn embedded_catalog_loads_and_every_agent_validates() {
    let catalog = stax_audit::embedded_catalog().expect("shipped signatures must validate");
    assert!(
        catalog.iter().any(|s| s.agent == "grok"),
        "the incident-verified grok signature must ship"
    );
    for sig in &catalog {
        assert!(
            !sig.detect_dirs.is_empty(),
            "{}: needs detect_dirs",
            sig.agent
        );
    }
}
