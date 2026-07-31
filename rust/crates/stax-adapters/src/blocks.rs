//! The content-block vocabulary the JSONL adapters share.
//!
//! `pi._message_text`, `openclaw._message_text` and `droid._message_text` are
//! byte-for-byte the same eleven lines of Python, and their
//! `_tools_from_content` siblings differ only in which block `type` values count
//! as a tool call. Copying that into three Rust modules is how the three drift;
//! it lives here once, with the accepted block types passed in as data.
//!
//! [`crate::codex`] carries its own private copy from the batch that landed it —
//! same semantics, and deliberately left alone: the foundation is the spec, and
//! re-pointing it at this module would be a change to a proven parity surface
//! for no behavioural gain.

use serde_json::Value;

/// Concatenate the text of a message's content blocks (`_message_text`).
///
/// | shape | result |
/// |---|---|
/// | `"plain string"` | itself, verbatim |
/// | `[{"text": "a"}, {"text": "b"}]` | `"a\nb"` |
/// | `[{"text": ""}]` / `[{"text": 7}]` / `[{}]` | contributes nothing — **no** empty piece, so no stray newline |
/// | `["bare", ""]` | both appended, empty string included |
/// | anything else (number, object, `null`, absent) | `""` |
///
/// The asymmetry in row three is the one thing a reader gets wrong from memory:
/// [`crate::claude`]'s version pushes an empty piece for a missing `text` key
/// (and so costs a newline), this family does not.
#[must_use]
pub fn message_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut pieces: Vec<&str> = Vec::new();
    for block in items {
        if let Some(map) = block.as_object() {
            if let Some(text) = map.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                pieces.push(text);
            }
        } else if let Some(text) = block.as_str() {
            // No truthiness guard on this branch in Python: a bare `""` block
            // is appended and does cost a newline.
            pieces.push(text);
        }
    }
    pieces.join("\n")
}

/// Tool names invoked in this turn (`_tools_from_content`).
///
/// `kinds` is the block `type` allowlist each adapter declares — `["tool_use"]`
/// for droid, `["toolCall", "tool_use"]` for pi, `["tool_use", "toolCall"]` for
/// openclaw. A non-string or empty `name` is dropped, so the result is always
/// the `tuple[str, ...]` the record contract promises.
#[must_use]
pub fn tool_names(content: Option<&Value>, kinds: &[&str]) -> Vec<String> {
    let Some(items) = content.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for block in items {
        let Some(map) = block.as_object() else {
            continue;
        };
        let Some(kind) = map.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !kinds.contains(&kind) {
            continue;
        }
        if let Some(name) = map.get("name").and_then(Value::as_str)
            && !name.is_empty()
        {
            names.push(name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_concatenation_matches_the_python_helper() {
        assert_eq!(message_text(Some(&json!("plain"))), "plain");
        assert_eq!(
            message_text(Some(&json!([{"type": "text", "text": "a"}, {"text": "b"}]))),
            "a\nb"
        );
        // A block with no usable text contributes nothing at all — contrast
        // the claude adapter, where a missing key still costs a newline.
        assert_eq!(
            message_text(Some(
                &json!([{"text": "a"}, {"type": "text"}, {"text": ""}, {"text": 7}])
            )),
            "a"
        );
        assert_eq!(
            message_text(Some(&json!(["bare", "", "tail"]))),
            "bare\n\ntail"
        );
        assert_eq!(message_text(Some(&json!(42))), "");
        assert_eq!(message_text(Some(&json!({"text": "x"}))), "");
        assert_eq!(message_text(Some(&json!(null))), "");
        assert_eq!(message_text(None), "");
    }

    #[test]
    fn tool_names_respect_the_declared_block_types() {
        let content = json!([
            {"type": "tool_use", "name": "Edit"},
            {"type": "toolCall", "name": "write_file"},
            {"type": "text", "name": "NotATool"},
            {"type": "tool_use", "name": ""},
            {"type": "tool_use"},
            {"type": "tool_use", "name": 7},
            "bare",
        ]);
        assert_eq!(tool_names(Some(&content), &["tool_use"]), vec!["Edit"]);
        assert_eq!(
            tool_names(Some(&content), &["toolCall", "tool_use"]),
            vec!["Edit", "write_file"]
        );
        assert!(tool_names(Some(&json!("plain")), &["tool_use"]).is_empty());
        assert!(tool_names(None, &["tool_use"]).is_empty());
    }
}
