//! The `staxtrace.memory/1` agent-output envelope.
//!
//! A port of `python-legacy: cli_helpers/agent_output.py` — same field names,
//! same insertion order, same `chars/4 + 1` token estimate, same rule that a
//! command's documented extras (`memory file` adds `risk`; `memory ask` adds
//! `note` and `vector_used`) may never shadow a core field. The contract's own
//! words, which this module has to keep true:
//!
//! > **Deterministic** — same store + same query → byte-identical JSON. The
//! > envelope keys are emitted in a fixed insertion order and `results` keeps
//! > the order the discovery layer produced.
//!
//! Like the Python module this one is **pure**: it builds and returns values, it
//! never prints and it never opens a store. The CLI owns stdout.
//!
//! Two shapes, distinguished exactly as `contracts/staxtrace-memory-v1/schema.json`
//! distinguishes them: a success envelope carries `results`, an error envelope
//! carries `error` instead and means the process exited non-zero.
//!
//! Unknown keys survive a round-trip. That is not politeness, it is phase 2 of
//! the shipped conformance checker: "an unknown ADDITIVE field is never visited,
//! so it is preserved and ignored, never rejected." A typed port that dropped
//! them would pass every schema check and still break the contract.

use std::fmt;

use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::pyjson;

/// Bumps only on a breaking change to the envelope or a `results[]` row shape.
pub const MEMORY_SCHEMA_VERSION: u32 = 1;

/// The frozen contract id every envelope carries.
pub const MEMORY_SCHEMA: &str = "staxtrace.memory/1";

/// The pre-rename spelling of [`MEMORY_SCHEMA`], still accepted by every reader.
///
/// The envelope is a wire contract: it crosses machines (`--at`, `observe`) and
/// is parsed by scripts this project does not own. A machine running an older
/// `stax` still ANSWERS with this string, so a reader that only knew the new
/// name would reject a perfectly valid response from a peer that simply has not
/// rebuilt yet. Same shape, same version — only the name moved.
pub const MEMORY_SCHEMA_LEGACY: &str = "stackunderflow.memory/1";

/// Does `schema` name this envelope, in either generation?
#[must_use]
pub fn is_memory_schema(schema: &str) -> bool {
    schema == MEMORY_SCHEMA || schema == MEMORY_SCHEMA_LEGACY
}

/// The eight fields every success envelope carries, in emission order.
///
/// Mirrors `agent_output._CORE_FIELDS`; an extra may not use these names.
pub const CORE_FIELDS: [&str; 8] = [
    "schema",
    "command",
    "query",
    "results",
    "result_count",
    "token_estimate",
    "budget",
    "truncated",
];

// ── command ─────────────────────────────────────────────────────────────────

/// Which agent-facing command produced an envelope.
///
/// The schema calls this enum ADDITIVE — "a new producer appends a value
/// (existing consumers pinned to the old set still validate every envelope they
/// were built for)". [`MemoryCommand::Other`] is how this port stays a consumer
/// that does not break on a value minted after it was compiled; [`is_known`]
/// asks the question the schema's `enum` asks.
///
/// [`is_known`]: MemoryCommand::is_known
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemoryCommand {
    Decisions,
    File,
    Worked,
    Sessions,
    Ask,
    ContextReplay,
    /// A producer this build has never heard of — carried verbatim.
    Other(String),
}

impl MemoryCommand {
    /// The wire spelling (`context-replay`, not `ContextReplay`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Decisions => "decisions",
            Self::File => "file",
            Self::Worked => "worked",
            Self::Sessions => "sessions",
            Self::Ask => "ask",
            Self::ContextReplay => "context-replay",
            Self::Other(raw) => raw,
        }
    }

    /// Whether this value is in the schema's `enum` as of this build.
    #[must_use]
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Other(_))
    }

    /// Every value the shipped schema enumerates, in schema order.
    #[must_use]
    pub fn known() -> Vec<Self> {
        vec![
            Self::Decisions,
            Self::File,
            Self::Worked,
            Self::Sessions,
            Self::Ask,
            Self::ContextReplay,
        ]
    }
}

impl From<&str> for MemoryCommand {
    fn from(raw: &str) -> Self {
        match raw {
            "decisions" => Self::Decisions,
            "file" => Self::File,
            "worked" => Self::Worked,
            "sessions" => Self::Sessions,
            "ask" => Self::Ask,
            "context-replay" => Self::ContextReplay,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for MemoryCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for MemoryCommand {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MemoryCommand {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from(raw.as_str()))
    }
}

// ── envelopes ───────────────────────────────────────────────────────────────

/// The token-bounded result envelope an agent splices into its context window.
#[derive(Debug, Clone, PartialEq)]
pub struct SuccessEnvelope {
    /// Always [`MEMORY_SCHEMA`]; kept as a field because it is on the wire.
    pub schema: String,
    pub command: MemoryCommand,
    /// Command-specific echo of the resolved inputs. Key order is the wire's.
    pub query: Map<String, Value>,
    /// Product-shaped rows in the order the discovery layer produced them.
    pub results: Vec<Value>,
    /// `len(results)` after budget packing.
    pub result_count: u64,
    /// `chars/4 + 1` estimate of `results`.
    pub token_estimate: u64,
    /// The `--context-budget` that was enforced (0 = packing disabled).
    pub budget: i64,
    /// True when the budget dropped at least one otherwise-matching row.
    pub truncated: bool,
    /// Documented extras (`risk`, `note`, `vector_used`) and any additive field
    /// a newer producer added — emitted after the core eight, in input order.
    pub extra: Map<String, Value>,
}

/// Emitted on a non-zero exit: stdout is this, not a result envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorEnvelope {
    pub schema: String,
    pub command: MemoryCommand,
    pub query: Map<String, Value>,
    pub error: String,
    /// Additive fields a newer producer added; empty for every envelope the
    /// current Python builder emits.
    pub extra: Map<String, Value>,
}

/// Either shape of the contract.
#[derive(Debug, Clone, PartialEq)]
pub enum MemoryEnvelope {
    Success(SuccessEnvelope),
    Error(ErrorEnvelope),
}

impl Serialize for SuccessEnvelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Hand-written rather than derived-with-flatten so the emission order is
        // stated once, here, and reads the same as `build_envelope`'s dict
        // literal in Python. Order IS the contract.
        let mut map = serializer.serialize_map(Some(CORE_FIELDS.len() + self.extra.len()))?;
        map.serialize_entry("schema", &self.schema)?;
        map.serialize_entry("command", &self.command)?;
        map.serialize_entry("query", &self.query)?;
        map.serialize_entry("results", &self.results)?;
        map.serialize_entry("result_count", &self.result_count)?;
        map.serialize_entry("token_estimate", &self.token_estimate)?;
        map.serialize_entry("budget", &self.budget)?;
        map.serialize_entry("truncated", &self.truncated)?;
        for (key, value) in &self.extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl Serialize for ErrorEnvelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(4 + self.extra.len()))?;
        map.serialize_entry("schema", &self.schema)?;
        map.serialize_entry("command", &self.command)?;
        map.serialize_entry("query", &self.query)?;
        map.serialize_entry("error", &self.error)?;
        for (key, value) in &self.extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl Serialize for MemoryEnvelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Success(env) => env.serialize(serializer),
            Self::Error(env) => env.serialize(serializer),
        }
    }
}

// ── builders (the Python module's three public functions) ───────────────────

/// Assemble the standard agent-output envelope — `agent_output.build_envelope`.
///
/// `token_estimate` is always recomputed from the final `results` so it
/// describes exactly what the caller receives, whichever packing path produced
/// them. `extra` is applied last and cannot shadow a core field.
#[must_use]
pub fn build_envelope(
    command: MemoryCommand,
    query: Map<String, Value>,
    results: Vec<Value>,
    budget: i64,
    truncated: bool,
    extra: Map<String, Value>,
) -> SuccessEnvelope {
    let result_count = results.len() as u64;
    let token_estimate = pyjson::estimate_tokens(&results);
    let extra = extra
        .into_iter()
        .filter(|(key, _)| !CORE_FIELDS.contains(&key.as_str()))
        .collect();
    SuccessEnvelope {
        schema: MEMORY_SCHEMA.to_owned(),
        command,
        query,
        results,
        result_count,
        token_estimate,
        budget,
        truncated,
        extra,
    }
}

/// Assemble the error envelope emitted alongside a non-zero exit —
/// `agent_output.build_error_envelope`.
#[must_use]
pub fn build_error_envelope(
    command: MemoryCommand,
    query: Map<String, Value>,
    error: impl Into<String>,
) -> ErrorEnvelope {
    ErrorEnvelope {
        schema: MEMORY_SCHEMA.to_owned(),
        command,
        query,
        error: error.into(),
        extra: Map::new(),
    }
}

/// Serialise to the canonical JSON string — `agent_output.render`.
///
/// `indent=2`, key order fixed by the builders, deterministic. Implemented once
/// here and exposed on all three envelope types so a caller never has to know
/// which one it holds.
#[must_use]
pub fn render<T: Serialize + ?Sized>(envelope: &T) -> String {
    pyjson::dumps_pretty(envelope)
}

/// [`render`] plus the newline `click.echo` writes — the exact stdout bytes,
/// which is what the golden fixture files contain.
#[must_use]
pub fn render_line<T: Serialize + ?Sized>(envelope: &T) -> String {
    let mut out = render(envelope);
    out.push('\n');
    out
}

impl SuccessEnvelope {
    /// See [`render`].
    #[must_use]
    pub fn render(&self) -> String {
        render(self)
    }

    /// See [`render_line`].
    #[must_use]
    pub fn render_line(&self) -> String {
        render_line(self)
    }
}

impl ErrorEnvelope {
    /// See [`render`].
    #[must_use]
    pub fn render(&self) -> String {
        render(self)
    }

    /// See [`render_line`].
    #[must_use]
    pub fn render_line(&self) -> String {
        render_line(self)
    }
}

impl MemoryEnvelope {
    /// See [`render`].
    #[must_use]
    pub fn render(&self) -> String {
        render(self)
    }

    /// See [`render_line`].
    #[must_use]
    pub fn render_line(&self) -> String {
        render_line(self)
    }

    /// The `command` field, whichever shape this is.
    #[must_use]
    pub fn command(&self) -> &MemoryCommand {
        match self {
            Self::Success(env) => &env.command,
            Self::Error(env) => &env.command,
        }
    }

    /// The `schema` tag, whichever shape this is.
    #[must_use]
    pub fn schema(&self) -> &str {
        match self {
            Self::Success(env) => &env.schema,
            Self::Error(env) => &env.schema,
        }
    }

    /// Parse the stdout of a `--json` run.
    ///
    /// Accepts the trailing newline `click.echo` adds.
    ///
    /// # Errors
    ///
    /// [`EnvelopeError::Json`] when the text is not JSON, otherwise the first
    /// structural problem found.
    pub fn from_json(text: &str) -> Result<Self, EnvelopeError> {
        Self::from_value(pyjson::loads(text)?)
    }

    /// Parse an already-decoded envelope.
    ///
    /// The success/error split follows the schema's `oneOf`: `error` present
    /// and `results` absent is the error branch, everything else is read as a
    /// success envelope (and fails loudly if it is not one).
    ///
    /// # Errors
    ///
    /// [`EnvelopeError`] describing the first missing or mistyped field.
    pub fn from_value(value: Value) -> Result<Self, EnvelopeError> {
        let Value::Object(mut map) = value else {
            return Err(EnvelopeError::NotAnObject);
        };
        let schema = take_string(&mut map, "schema")?;
        let command = MemoryCommand::from(take_string(&mut map, "command")?.as_str());
        let query = take_object(&mut map, "query")?;

        if !map.contains_key("results") && map.contains_key("error") {
            let error = take_string(&mut map, "error")?;
            return Ok(Self::Error(ErrorEnvelope {
                schema,
                command,
                query,
                error,
                extra: map,
            }));
        }

        let results = take_array(&mut map, "results")?;
        let result_count = take_u64(&mut map, "result_count")?;
        let token_estimate = take_u64(&mut map, "token_estimate")?;
        let budget = take_i64(&mut map, "budget")?;
        let truncated = take_bool(&mut map, "truncated")?;
        Ok(Self::Success(SuccessEnvelope {
            schema,
            command,
            query,
            results,
            result_count,
            token_estimate,
            budget,
            truncated,
            extra: map,
        }))
    }
}

// ── errors ──────────────────────────────────────────────────────────────────

/// Why an envelope could not be read.
#[derive(Debug)]
pub enum EnvelopeError {
    /// The text was not valid JSON.
    Json(serde_json::Error),
    /// The document was not a JSON object.
    NotAnObject,
    /// A required field was absent.
    Missing(&'static str),
    /// A required field had the wrong JSON type.
    WrongType {
        field: &'static str,
        want: &'static str,
    },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "not JSON: {err}"),
            Self::NotAnObject => f.write_str("envelope is not a JSON object"),
            Self::Missing(field) => write!(f, "missing required property {field:?}"),
            Self::WrongType { field, want } => {
                write!(f, "property {field:?} is not {want}")
            }
        }
    }
}

impl std::error::Error for EnvelopeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for EnvelopeError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn take(map: &mut Map<String, Value>, field: &'static str) -> Result<Value, EnvelopeError> {
    map.shift_remove(field).ok_or(EnvelopeError::Missing(field))
}

fn take_string(map: &mut Map<String, Value>, field: &'static str) -> Result<String, EnvelopeError> {
    match take(map, field)? {
        Value::String(text) => Ok(text),
        _ => Err(EnvelopeError::WrongType {
            field,
            want: "a string",
        }),
    }
}

fn take_object(
    map: &mut Map<String, Value>,
    field: &'static str,
) -> Result<Map<String, Value>, EnvelopeError> {
    match take(map, field)? {
        Value::Object(inner) => Ok(inner),
        _ => Err(EnvelopeError::WrongType {
            field,
            want: "an object",
        }),
    }
}

fn take_array(
    map: &mut Map<String, Value>,
    field: &'static str,
) -> Result<Vec<Value>, EnvelopeError> {
    match take(map, field)? {
        Value::Array(items) => Ok(items),
        _ => Err(EnvelopeError::WrongType {
            field,
            want: "an array",
        }),
    }
}

fn take_u64(map: &mut Map<String, Value>, field: &'static str) -> Result<u64, EnvelopeError> {
    take(map, field)?.as_u64().ok_or(EnvelopeError::WrongType {
        field,
        want: "a non-negative integer",
    })
}

fn take_i64(map: &mut Map<String, Value>, field: &'static str) -> Result<i64, EnvelopeError> {
    take(map, field)?.as_i64().ok_or(EnvelopeError::WrongType {
        field,
        want: "an integer",
    })
}

fn take_bool(map: &mut Map<String, Value>, field: &'static str) -> Result<bool, EnvelopeError> {
    match take(map, field)? {
        Value::Bool(flag) => Ok(flag),
        _ => Err(EnvelopeError::WrongType {
            field,
            want: "a boolean",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn query() -> Map<String, Value> {
        json!({"text": "retry", "project": null, "since": null, "limit": 20})
            .as_object()
            .expect("object")
            .clone()
    }

    #[test]
    fn schema_is_versioned() {
        assert_eq!(MEMORY_SCHEMA, "staxtrace.memory/1");
        assert_eq!(
            MEMORY_SCHEMA,
            format!("staxtrace.memory/{MEMORY_SCHEMA_VERSION}")
        );
    }

    #[test]
    fn build_envelope_has_the_eight_core_fields() {
        let env = build_envelope(
            MemoryCommand::Decisions,
            query(),
            vec![json!({"session_id": "s1"})],
            2000,
            false,
            Map::new(),
        );
        let rendered = pyjson::loads(&env.render()).expect("valid JSON");
        let keys: Vec<&str> = rendered
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, CORE_FIELDS);
        assert_eq!(env.result_count, 1);
        assert_eq!(env.budget, 2000);
        assert!(!env.truncated);
    }

    #[test]
    fn token_estimate_describes_results_not_the_envelope() {
        let results = vec![json!({"session_id": "s", "snippet": "y".repeat(400)})];
        let env = build_envelope(
            MemoryCommand::Decisions,
            Map::new(),
            results.clone(),
            2000,
            false,
            Map::new(),
        );
        assert_eq!(env.token_estimate, pyjson::estimate_tokens(&results));
        assert!(env.token_estimate > 50);
    }

    #[test]
    fn extra_cannot_shadow_core_fields() {
        let extra = json!({"results": [{"injected": "nope"}], "schema": "evil", "risk": {"r": 1}})
            .as_object()
            .expect("object")
            .clone();
        let env = build_envelope(
            MemoryCommand::File,
            Map::new(),
            vec![json!({"real": 1})],
            2000,
            false,
            extra,
        );
        assert_eq!(env.results, vec![json!({"real": 1})]);
        assert_eq!(env.schema, MEMORY_SCHEMA);
        assert_eq!(env.extra.keys().collect::<Vec<_>>(), ["risk"]);
    }

    #[test]
    fn error_envelope_is_not_a_result_envelope() {
        let env = build_error_envelope(
            MemoryCommand::Decisions,
            query(),
            "Invalid since value 'garbage'",
        );
        let wrapped = MemoryEnvelope::Error(env);
        let parsed = pyjson::loads(&wrapped.render()).expect("valid JSON");
        let obj = parsed.as_object().expect("object");
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            ["schema", "command", "query", "error"]
        );
        assert!(!obj.contains_key("results"));
    }

    #[test]
    fn round_trip_preserves_unknown_additive_fields() {
        // Phase 2 of the shipped checker, as a type-level property.
        let text = concat!(
            "{\n  \"schema\": \"staxtrace.memory/1\",\n  \"command\": \"ask\",\n",
            "  \"query\": {},\n  \"results\": [],\n  \"result_count\": 0,\n",
            "  \"token_estimate\": 1,\n  \"budget\": 2000,\n  \"truncated\": false,\n",
            "  \"note\": \"n\",\n  \"x_future_additive_field\": {\n",
            "    \"added_later\": [\n      1,\n      2,\n      3\n    ]\n  }\n}"
        );
        let env = MemoryEnvelope::from_json(text).expect("parses");
        assert_eq!(env.render(), text);
    }

    #[test]
    fn unknown_command_is_carried_not_rejected() {
        let cmd = MemoryCommand::from("minted-later");
        assert!(!cmd.is_known());
        assert_eq!(cmd.as_str(), "minted-later");
        assert!(MemoryCommand::known().iter().all(MemoryCommand::is_known));
        assert_eq!(
            MemoryCommand::from("context-replay"),
            MemoryCommand::ContextReplay
        );
    }

    #[test]
    fn missing_required_field_is_an_error_not_a_default() {
        let err = MemoryEnvelope::from_json(r#"{"schema": "staxtrace.memory/1"}"#)
            .expect_err("command is required");
        assert!(matches!(err, EnvelopeError::Missing("command")), "{err}");
    }

    #[test]
    fn error_branch_needs_error_and_no_results() {
        let err_env = MemoryEnvelope::from_json(
            r#"{"schema":"s","command":"ask","query":{},"error":"boom"}"#,
        )
        .expect("parses");
        assert!(matches!(err_env, MemoryEnvelope::Error(_)));
        // `results` present wins even when `error` is too — the success branch
        // is the one the schema requires `results` for.
        let both = MemoryEnvelope::from_json(
            r#"{"schema":"s","command":"ask","query":{},"results":[],"result_count":0,
                "token_estimate":1,"budget":0,"truncated":false,"error":"boom"}"#,
        )
        .expect("parses");
        assert!(matches!(both, MemoryEnvelope::Success(_)));
    }
}
