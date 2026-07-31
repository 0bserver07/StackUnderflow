//! Canonical line-oriented dumps — the Rust half of the parity harness.
//!
//! Parity is the definition of done (`docs/specs/rust-port.md` §5), so both
//! implementations need one agreed serialization to be diffed *as text*. That is
//! what this module is: a [`SessionRef`] or [`Record`] rendered as a single
//! compact JSON object with a fixed key order, matched line-for-line by
//! `crates/stax-adapters/parity/python_reference.py`.
//!
//! Two deliberate choices keep the diff honest rather than merely green:
//!
//! * **Floats are emitted as Python's `repr`** ([`crate::pyval::py_float_str`]),
//!   not as JSON numbers. `file_mtime` is the one float in the contract, and a
//!   JSON encoder is free to render `1.7e9` differently on each side; comparing
//!   the two `repr`s tests the mtime *and* the formatter.
//! * **`raw` is emitted as a string** holding the re-serialized source object.
//!   That is exactly what the ingest writer stores in `messages.raw_json`
//!   (`json.dumps(rec.raw, default=str)`), so the diff covers key order — which
//!   is why this crate builds `serde_json` with `preserve_order`.
//!
//! The dumps are used by the `stax-adapter-parity` binary and by
//! `tests/parity.rs`; sharing them means the harness cannot pass because the two
//! callers agreed with each other and not with Python.

use serde_json::{Map, Value};

use crate::base::{Record, SessionRef};
use crate::pyval;

/// One capability row as a tab-separated line.
///
/// Deliberately not JSON: this compares the *loaded* table (defaults applied,
/// fields filled in) rather than the file's literal bytes, and a flat line makes
/// a mismatch legible in a test failure.
#[must_use]
pub fn capability_line(cap: &crate::capabilities::AdapterCapability) -> String {
    let fields = crate::capabilities::FIELDS
        .into_iter()
        .map(|field| format!("{}:{}", field.as_str(), cap.field_fidelity(field).as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let (scope, command) = cap.resume.as_ref().map_or(("-", "-"), |resume| {
        (resume.scope.as_str(), resume.command.as_str())
    });
    format!(
        "{}\t{}\t{}\t{}\t{scope}\t{command}\t{fields}",
        cap.provider,
        cap.label,
        cap.status.as_str(),
        cap.emits_usage_events,
    )
}

/// Sort refs into the harness's canonical order.
///
/// Python walks project directories in `readdir` order, which no two
/// filesystems agree on (see the divergence note on
/// [`crate::claude::ClaudeAdapter::enumerate`]), so both sides sort by
/// `(project_slug, session_id, file_path)` before the diff. Ordering is the one
/// thing the harness deliberately does not compare.
pub fn sort_refs(refs: &mut [SessionRef]) {
    refs.sort_by(|a, b| {
        (&a.project_slug, &a.session_id, &a.file_path).cmp(&(
            &b.project_slug,
            &b.session_id,
            &b.file_path,
        ))
    });
}

/// One [`SessionRef`] as a canonical JSON line.
#[must_use]
pub fn ref_line(session: &SessionRef) -> String {
    let mut out = Map::new();
    out.insert("provider".into(), session.provider.clone().into());
    out.insert("project_slug".into(), session.project_slug.clone().into());
    out.insert("session_id".into(), session.session_id.clone().into());
    out.insert(
        "file_path".into(),
        session.file_path.to_string_lossy().into_owned().into(),
    );
    out.insert(
        "file_mtime".into(),
        pyval::py_float_str(session.file_mtime).into(),
    );
    out.insert("file_size".into(), session.file_size.into());
    out.insert("source_kind".into(), session.source_kind.as_str().into());
    out.insert(
        "source_hint".into(),
        session
            .source_hint
            .clone()
            .map_or(Value::Null, Value::Object),
    );
    Value::Object(out).to_string()
}

/// One [`Record`] as a canonical JSON line.
#[must_use]
pub fn record_line(record: &Record) -> String {
    let mut out = Map::new();
    out.insert("provider".into(), record.provider.clone().into());
    out.insert("session_id".into(), record.session_id.clone().into());
    out.insert("seq".into(), record.seq.into());
    out.insert("timestamp".into(), record.timestamp.clone().into());
    out.insert("role".into(), record.role.clone().into());
    out.insert(
        "model".into(),
        record.model.clone().map_or(Value::Null, Value::from),
    );
    out.insert("input_tokens".into(), record.input_tokens.into());
    out.insert("output_tokens".into(), record.output_tokens.into());
    out.insert(
        "cache_create_tokens".into(),
        record.cache_create_tokens.into(),
    );
    out.insert("cache_read_tokens".into(), record.cache_read_tokens.into());
    out.insert("content_text".into(), record.content_text.clone().into());
    out.insert("tools".into(), record.tools.clone().into());
    out.insert(
        "cwd".into(),
        record.cwd.clone().map_or(Value::Null, Value::from),
    );
    out.insert("is_sidechain".into(), record.is_sidechain.into());
    out.insert("uuid".into(), record.uuid.clone().into());
    out.insert(
        "parent_uuid".into(),
        record.parent_uuid.clone().map_or(Value::Null, Value::from),
    );
    out.insert("speed".into(), record.speed.as_str().into());
    // The `raw_json` column's exact contents, key order included.
    out.insert("raw".into(), record.raw.to_string().into());
    Value::Object(out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{SourceKind, Speed};
    use serde_json::json;

    #[test]
    fn ref_line_key_order_is_fixed() {
        let session = SessionRef::file("claude", "-a", "s1", "/tmp/s1.jsonl", 1.5, 12);
        assert_eq!(
            ref_line(&session),
            r#"{"provider":"claude","project_slug":"-a","session_id":"s1","file_path":"/tmp/s1.jsonl","file_mtime":"1.5","file_size":12,"source_kind":"file","source_hint":null}"#
        );
    }

    #[test]
    fn record_line_carries_raw_verbatim_with_source_key_order() {
        let record = Record {
            provider: "codex".into(),
            session_id: "s".into(),
            seq: 7,
            timestamp: "2026-01-01T00:00:00Z".into(),
            role: "assistant".into(),
            model: Some("gpt-5.4".into()),
            input_tokens: 1,
            output_tokens: 2,
            cache_create_tokens: 3,
            cache_read_tokens: 4,
            content_text: "hi".into(),
            tools: vec!["Bash".into()],
            cwd: None,
            is_sidechain: false,
            uuid: "s:7".into(),
            parent_uuid: None,
            raw: json!({"z": 1, "a": 2}),
            speed: Speed::Standard,
        };
        let line = record_line(&record);
        assert!(line.contains(r#""raw":"{\"z\":1,\"a\":2}""#), "{line}");
        assert!(line.starts_with(r#"{"provider":"codex","session_id":"s","seq":7,"#));
        assert_eq!(SourceKind::File.as_str(), "file");
    }
}
