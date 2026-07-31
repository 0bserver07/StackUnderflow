//! The real `adapters/capabilities.json`, loaded by the Rust loader.
//!
//! The file is DATA shipped with the Python package and read verbatim here —
//! no provider name, label, or fidelity flag is transcribed into Rust. These
//! tests assert that the *whole* table loads (the mission's bar: every
//! registered provider key in the file loads), that the invariants
//! `services/support_matrix.py` documents hold, and that the registry cannot
//! drift away from the table.

mod support;

use stax_adapters::capabilities::{Capabilities, FIELDS, Field, ResumeScope, Status};
use stax_adapters::{Capabilities as _CapabilitiesAlias, registered_names};
use support::{
    assert_same_lines, fixture, note_missing_reference, reference_python, run_python_reference,
};

fn table() -> Capabilities {
    let path = fixture("stackunderflow/adapters/capabilities.json");
    assert!(
        path.is_file(),
        "missing capability table at {}",
        path.display()
    );
    Capabilities::load(&path).expect("the shipped capability table must load")
}

#[test]
fn every_provider_row_in_the_shipped_table_loads() {
    let table = table();
    assert_eq!(
        table.schema(),
        stax_adapters::capabilities::FILE_SCHEMA,
        "the data file's schema string"
    );
    // 20 providers ship today; the assertion is on "all of them", not on a
    // hardcoded list of names — the count is derived from the file itself.
    assert_eq!(table.len(), table.providers().len());
    assert!(
        table.len() >= 20,
        "expected the full provider set, got {}",
        table.len()
    );
    for cap in table.iter() {
        assert!(!cap.provider.is_empty());
        assert!(!cap.label.is_empty(), "{} has no label", cap.provider);
        // Every canonical field is materialised, never silently omitted.
        assert_eq!(
            cap.fields.len(),
            FIELDS.len(),
            "{} field count",
            cap.provider
        );
        for field in FIELDS {
            // The one invariant every consumer relies on.
            assert_eq!(
                cap.captures(field),
                cap.field_fidelity(field).captured(),
                "{} / {}",
                cap.provider,
                field.as_str()
            );
        }
    }
}

#[test]
fn every_registered_adapter_has_a_capabilities_row() {
    // Python's drift test asserts the table covers exactly the introspected
    // adapter set. Rust carries 2 of 20 providers today, so this is the half
    // that can hold now — and it is the half that matters: an adapter must not
    // ship without an honest fidelity row.
    let table = table();
    for name in registered_names() {
        let cap = table
            .get(&name)
            .unwrap_or_else(|| panic!("registered adapter {name:?} has no capabilities row"));
        assert_eq!(cap.provider, name);
    }
}

#[test]
fn a_session_scoped_resume_template_renders_and_a_latest_scoped_one_does_not() {
    let table = table();
    let mut session_scoped = 0;
    let mut latest_scoped = 0;
    for cap in table.iter() {
        let Some(resume) = &cap.resume else { continue };
        match resume.scope {
            ResumeScope::Session => {
                session_scoped += 1;
                let rendered = table
                    .resume_command(&cap.provider, "abc-123")
                    .unwrap_or_else(|| panic!("{} should render", cap.provider));
                assert!(
                    rendered.contains("abc-123"),
                    "{} rendered without the id: {rendered}",
                    cap.provider
                );
                assert!(!rendered.contains("{session_id}"));
            }
            ResumeScope::Latest => {
                latest_scoped += 1;
                // A latest-scope CLI has nowhere to put an id; inventing a flag
                // would print a command that does not work.
                assert_eq!(table.resume_command(&cap.provider, "abc-123"), None);
            }
        }
    }
    assert!(
        session_scoped >= 3,
        "the table ships session-scoped templates"
    );
    assert!(
        latest_scoped >= 1,
        "the table ships a latest-scoped template"
    );
}

#[test]
fn claude_and_codex_rows_say_what_the_adapters_actually_do() {
    // Spot-check the two providers this batch ports, against their own rows.
    let table = table();
    let claude = table.get("claude").expect("claude row");
    assert_eq!(claude.status, Status::Supported);
    assert!(claude.emits_usage_events);
    // Anthropic usage reports no reasoning split, and the adapter never
    // fabricates one.
    assert!(!claude.captures(Field::Reasoning));
    assert!(claude.captures(Field::Tokens));

    let codex = table.get("codex").expect("codex row");
    assert_eq!(codex.status, Status::Supported);
    assert!(codex.emits_usage_events);
    // The codex normalizer keeps the reasoning split.
    assert!(codex.captures(Field::Reasoning));
}

#[test]
fn a_provider_that_cannot_bill_is_marked_and_the_unknown_default_is_true() {
    let table = table();
    let exempt: Vec<&str> = table
        .iter()
        .filter(|cap| !cap.emits_usage_events)
        .map(|cap| cap.provider.as_str())
        .collect();
    assert!(
        !exempt.is_empty(),
        "the adapter↔normalizer parity check reads this flag; the table should \
         carry at least one non-billable source"
    );
    for provider in &exempt {
        let cap = table.get(provider).expect("row");
        assert!(
            !cap.notes.is_empty(),
            "{provider} claims it cannot bill but gives no reason"
        );
    }
    // An undocumented provider is assumed to bill, so a missing row surfaces as
    // a gap rather than as a silent exemption.
    assert!(table.emits_usage_events("not-a-provider"));
}

#[test]
fn the_rust_loader_reads_the_table_exactly_as_python_does() {
    if reference_python().is_none() {
        note_missing_reference("the_rust_loader_reads_the_table_exactly_as_python_does");
        return;
    }
    // Compares the *loaded* tables — defaults applied, unset fields filled in —
    // not the file's literal bytes. Python's `_CAPABILITIES` resolves through
    // `importlib.resources` to the same worktree file this test hands the Rust
    // loader.
    let table = table();
    let rust: String = table
        .iter()
        .map(|cap| stax_adapters::dump::capability_line(cap) + "\n")
        .collect();
    assert_same_lines(
        "capabilities table",
        &run_python_reference(&["capabilities"]),
        &rust,
    );
}

#[test]
fn the_public_alias_is_the_same_type() {
    // `stax_adapters::Capabilities` is re-exported for callers in later waves.
    let _: fn(&std::path::Path) -> anyhow::Result<_CapabilitiesAlias> = Capabilities::load;
}
