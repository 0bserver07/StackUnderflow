//! `infra/egress.py` — the shape guard every outbound structured body crosses.
//!
//! Deliberately **a guard, not a redactor**: the project preserves transcript
//! text at rest and does send the content it must (you cannot embed text without
//! sending it). The guard's job is narrower — make the set of top-level keys
//! that cross the network boundary an explicit, reviewed **allowlist**, and give
//! the leak tests their primitives.
//!
//! Three properties are the whole design and each is ported deliberately:
//!
//! * **Allowlist, never denylist.** A denylist has to anticipate every bad key;
//!   an allowlist fails *closed* on the unknown.
//! * **Cheap.** An O(keys) set-membership check on the hot embed path — no I/O,
//!   no network, no serialization on the success path.
//! * **Never echoes values.** A violation names the offending *keys*, never
//!   their values, so a rejected body can be logged without leaking whatever the
//!   stray key was carrying.
//!
//! It sits in this crate rather than an `infra` one because `TASKS-RS` files it
//! here (RS-7-001) with the note "redaction before anything leaves the box" —
//! sync is the other thing that leaves the box, and one crate owning the
//! boundary is the point.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// `OLLAMA_EMBED_KEYS` — `POST /api/embeddings` takes exactly these.
///
/// The prompt IS transcript text and crosses by design; no other field should
/// ever accompany it.
pub const OLLAMA_EMBED_KEYS: &[&str] = &["model", "prompt"];

/// `OLLAMA_CHAT_KEYS` — `POST /api/chat`.
///
/// No free-form `context` / `metadata` / `env` field is permitted — those are
/// exactly the shapes a leak would ride in on.
pub const OLLAMA_CHAT_KEYS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "tools",
    "options",
    "keep_alive",
    "format",
    "think",
];

/// `EgressViolation` — a body carried a top-level key outside its allowlist.
///
/// The message names the disallowed keys and the allowed set — never the
/// values — so it is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressViolation(pub String);

impl std::fmt::Display for EgressViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EgressViolation {}

/// `guard_json_body(body, allow=…, kind=…)`.
///
/// # Errors
/// [`EgressViolation`] with the reference's message. Note both key lists are
/// rendered by CPython's `repr` of a `list[str]` — `['a', 'b']`, with a space
/// after each comma and single quotes — because that is what an f-string of a
/// `sorted(...)` produces, and this message is asserted by the leak tests.
pub fn guard_json_body(
    body: &Map<String, Value>,
    allow: &[&str],
    kind: &str,
) -> Result<Map<String, Value>, EgressViolation> {
    let allowed: BTreeSet<&str> = allow.iter().copied().collect();
    // `sorted(k for k in body if k not in allow)`.
    let stray: Vec<&str> = {
        let mut keys: Vec<&str> = body
            .keys()
            .map(String::as_str)
            .filter(|key| !allowed.contains(key))
            .collect();
        keys.sort_unstable();
        keys
    };
    if !stray.is_empty() {
        return Err(EgressViolation(format!(
            "{kind}: {} disallowed top-level key(s) would cross the network boundary: {}; \
             allowed: {}",
            stray.len(),
            py_list_repr(&stray),
            py_list_repr(&allowed.iter().copied().collect::<Vec<_>>())
        )));
    }
    Ok(body.clone())
}

/// `serialize(body)` — deterministic JSON for substring leak-scanning.
///
/// `sort_keys` + `default=str` mirror how the real request bodies are encoded on
/// the wire, so scanning this string is a faithful proxy for "does this appear
/// in what we would send". Key *order* never affects substring presence; the
/// sort just makes the output stable for assertions.
///
/// A bare `json.dumps` also means the DEFAULT separators `(", ", ": ")` and
/// `ensure_ascii=True` — the `dumps_py_default` writer, not the compact one.
#[must_use]
pub fn serialize(body: &Value) -> String {
    stax_memory::pyjson::dumps_py_default(&sorted_deep(body))
}

/// `scan(serialized, needles)` — the needles that appear as substrings.
///
/// Deliberately dumb (plain substring containment): the corpus supplies
/// concrete synthetic secrets, so there is no clever pattern to get subtly wrong
/// and no false negative from a regex missing an edge case. Empty needles are
/// dropped (`if n and n in serialized`) — otherwise every scan would report a
/// leak of `""`.
#[must_use]
pub fn scan(serialized: &str, needles: &[String]) -> Vec<String> {
    needles
        .iter()
        .filter(|needle| !needle.is_empty() && serialized.contains(needle.as_str()))
        .cloned()
        .collect()
}

/// `sort_keys=True` applied recursively, as CPython's encoder does.
fn sorted_deep(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), sorted_deep(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_deep).collect()),
        other => other.clone(),
    }
}

/// `repr(list_of_str)` — `['a', 'b']`.
fn py_list_repr(items: &[&str]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|item| stax_core::queries::paths::py_repr(item))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn a_conforming_embed_body_passes_through_unchanged() {
        let payload = body(&[
            ("model", Value::from("nomic-embed-text")),
            ("prompt", Value::from("some transcript text")),
        ]);
        assert_eq!(
            guard_json_body(&payload, OLLAMA_EMBED_KEYS, "ollama/embeddings").expect("allowed"),
            payload
        );
    }

    #[test]
    fn an_empty_body_passes_because_an_allowlist_bounds_what_may_appear() {
        assert!(guard_json_body(&Map::new(), OLLAMA_EMBED_KEYS, "ollama/embeddings").is_ok());
    }

    #[test]
    fn one_stray_key_fails_closed_with_the_references_message() {
        let payload = body(&[
            ("model", Value::from("m")),
            ("prompt", Value::from("p")),
            ("env", Value::from("SECRET=1")),
        ]);
        let err =
            guard_json_body(&payload, OLLAMA_EMBED_KEYS, "ollama/embeddings").expect_err("stray");
        assert_eq!(
            err.0,
            "ollama/embeddings: 1 disallowed top-level key(s) would cross the network \
             boundary: ['env']; allowed: ['model', 'prompt']"
        );
        // …and the VALUE is nowhere in it.
        assert!(!err.0.contains("SECRET=1"), "{}", err.0);
    }

    #[test]
    fn stray_keys_are_sorted_and_counted() {
        let payload = body(&[
            ("zeta", Value::Null),
            ("alpha", Value::Null),
            ("model", Value::from("m")),
        ]);
        let err = guard_json_body(&payload, OLLAMA_CHAT_KEYS, "ollama/chat").expect_err("stray");
        assert!(
            err.0
                .starts_with("ollama/chat: 2 disallowed top-level key(s)"),
            "{}",
            err.0
        );
        assert!(err.0.contains("['alpha', 'zeta']"), "{}", err.0);
    }

    #[test]
    fn the_chat_allowlist_is_the_eight_documented_knobs() {
        assert_eq!(OLLAMA_CHAT_KEYS.len(), 8);
        for forbidden in ["context", "metadata", "env", "system_prompt"] {
            assert!(!OLLAMA_CHAT_KEYS.contains(&forbidden), "{forbidden}");
        }
    }

    #[test]
    fn serialize_sorts_recursively_and_uses_the_bare_dumps_layout() {
        let payload = serde_json::json!({
            "prompt": "p",
            "model": "m",
            "nested": {"b": 1, "a": [ {"y": 2, "x": 1} ]},
        });
        assert_eq!(
            serialize(&payload),
            r#"{"model": "m", "nested": {"a": [{"x": 1, "y": 2}], "b": 1}, "prompt": "p"}"#
        );
    }

    #[test]
    fn scan_finds_substrings_and_ignores_the_empty_needle() {
        let text = serialize(&serde_json::json!({"prompt": "token sk-live-123 here"}));
        assert_eq!(
            scan(
                &text,
                &["sk-live-123".to_owned(), String::new(), "absent".to_owned()]
            ),
            vec!["sk-live-123".to_owned()]
        );
        assert!(scan(&text, &[]).is_empty());
    }

    #[test]
    fn a_secret_hiding_in_a_nested_value_is_still_found() {
        // The reason `scan` is substring-based rather than key-based: a leak
        // does not have to arrive at the top level.
        let text = serialize(&serde_json::json!({
            "messages": [{"role": "user", "content": "my key is sk-deep-9"}],
        }));
        assert_eq!(
            scan(&text, &["sk-deep-9".to_owned()]),
            vec!["sk-deep-9".to_owned()]
        );
    }
}
