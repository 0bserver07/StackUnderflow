//! Conformance checking against the shipped `staxtrace.memory/1` schema.
//!
//! A port of `scripts/check_memory_contract.py` — the checker CI runs. It reads
//! `contracts/staxtrace-memory-v1/schema.json` **unchanged**; nothing about
//! the contract is transcribed into Rust, which is the whole point of RS-1-026:
//! the oracle stays one file, honoured by two implementations.
//!
//! The same deliberately-small JSON-Schema 2020-12 subset: local `$ref`,
//! `oneOf`, `type`, `required`, `properties`, `items`, `const`, `enum`. Unknown
//! keywords are ignored and objects are open — an unknown ADDITIVE field is
//! never visited, so it is preserved and ignored, never rejected. Porting the
//! *subset* rather than pulling a full JSON-Schema crate is on purpose: a
//! stricter validator would reject envelopes the Python side accepts, and that
//! divergence would be invisible until an agent's field got dropped in the wild.

use serde_json::Value;

/// Validate `instance` against `schema`; an empty vec means valid.
///
/// `root` is the top-level schema document, used to resolve `$ref`. `path` is
/// the JSON-pointer-ish prefix used in messages (start with `"$"`), so the error
/// strings read like the Python checker's.
#[must_use]
pub fn validate(instance: &Value, schema: &Value, root: &Value, path: &str) -> Vec<String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return match resolve_ref(reference, root) {
            Some(target) => validate(instance, target, root, path),
            None => vec![format!("{path}: unresolvable $ref {reference:?}")],
        };
    }

    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matched = branches
            .iter()
            .filter(|sub| validate(instance, sub, root, path).is_empty())
            .count();
        if matched == 1 {
            return Vec::new();
        }
        return vec![format!(
            "{path}: expected exactly one oneOf branch to match, {matched} did"
        )];
    }

    let mut errors = Vec::new();
    if let Some(expected) = schema.get("const")
        && instance != expected
    {
        errors.push(format!("{path}: const mismatch: {instance} != {expected}"));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(instance)
    {
        errors.push(format!("{path}: {instance} not in enum {allowed:?}"));
    }
    if let Some(types) = schema.get("type") {
        let names: Vec<&str> = match types {
            Value::String(name) => vec![name.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.iter().any(|name| type_matches(name, instance)) {
            errors.push(format!(
                "{path}: expected type {types}, got {}",
                type_name(instance)
            ));
        }
    }

    if let Some(object) = instance.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    errors.push(format!("{path}: missing required property '{key}'"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, subschema) in properties {
                // Unknown keys are intentionally not visited.
                if let Some(child) = object.get(key) {
                    errors.extend(validate(child, subschema, root, &format!("{path}.{key}")));
                }
            }
        }
    }
    if let (Some(items), Some(subschema)) = (instance.as_array(), schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            errors.extend(validate(item, subschema, root, &format!("{path}[{index}]")));
        }
    }
    errors
}

/// Resolve a local JSON pointer (`#` or `#/$defs/Name`).
fn resolve_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference == "#" {
        return Some(root);
    }
    let rest = reference.strip_prefix("#/")?;
    let mut node = root;
    for part in rest.split('/') {
        let key = part.replace("~1", "/").replace("~0", "~");
        node = node.get(&key)?;
    }
    Some(node)
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        // Python excludes bool from the numeric types (bool subclasses int);
        // serde_json already keeps Bool and Number apart, so `is_i64/is_u64`
        // is the same predicate.
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                "integer"
            } else {
                "float"
            }
        }
        Value::Null => "null",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "oneOf": [{"$ref": "#/$defs/a"}, {"$ref": "#/$defs/b"}],
            "$defs": {
                "a": {
                    "type": "object",
                    "required": ["kind", "n"],
                    "properties": {
                        "kind": {"const": "a"},
                        "n": {"type": "integer"},
                        "rows": {"type": "array", "items": {"type": "object"}}
                    }
                },
                "b": {
                    "type": "object",
                    "required": ["kind", "error"],
                    "properties": {
                        "kind": {"enum": ["b", "b2"]},
                        "error": {"type": "string"}
                    }
                }
            }
        })
    }

    fn check(instance: &Value) -> Vec<String> {
        let root = schema();
        validate(instance, &root, &root, "$")
    }

    #[test]
    fn one_of_needs_exactly_one_branch() {
        assert!(check(&json!({"kind": "a", "n": 1})).is_empty());
        assert!(check(&json!({"kind": "b", "error": "boom"})).is_empty());
        // Neither branch: the required keys are missing from both.
        assert_eq!(check(&json!({"kind": "a"})).len(), 1);
    }

    #[test]
    fn unknown_keys_are_open_and_unvisited() {
        assert!(check(&json!({"kind": "a", "n": 1, "x_future": {"deep": [1]}})).is_empty());
    }

    #[test]
    fn wrong_types_and_missing_fields_bite() {
        // "seven" is not an integer -> branch a fails; branch b needs `error`.
        assert_eq!(check(&json!({"kind": "a", "n": "seven"})).len(), 1);
        assert_eq!(check(&json!({"kind": "nope", "n": 1})).len(), 1);
    }

    #[test]
    fn booleans_are_not_integers() {
        let root = json!({"type": "integer"});
        assert!(!validate(&json!(true), &root, &root, "$").is_empty());
        assert!(validate(&json!(7), &root, &root, "$").is_empty());
    }

    #[test]
    fn items_are_validated_positionally() {
        let root = json!({"type": "array", "items": {"type": "object"}});
        let errors = validate(&json!([{}, 3, {}]), &root, &root, "$");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("$[1]:"), "{errors:?}");
    }

    #[test]
    fn escaped_pointer_segments_resolve() {
        let root = json!({"$defs": {"a/b": {"type": "string"}}, "$ref": "#/$defs/a~1b"});
        assert!(validate(&json!("x"), &root, &root, "$").is_empty());
        assert!(!validate(&json!(1), &root, &root, "$").is_empty());
    }
}
