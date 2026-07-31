//! The curated capability table — `adapters/capabilities.json`, loaded as data.
//!
//! This is the port of `services/support_matrix.py`'s loader half
//! (`_load_capabilities`, `SCHEMA`, `FIELDS`, `STATUSES`, `FIDELITY_LEVELS`).
//! The **same file** feeds both implementations: nothing here transcribes a
//! provider name, a label, or a fidelity flag into Rust, because the whole point
//! of that file is that agent names are data (`adapters/__init__.py:14`).
//!
//! The path is injected rather than discovered. There is no `importlib.resources`
//! in Rust and an `include_str!` would freeze a build-time copy — a hazard when
//! the parity harness must prove both implementations read the identical bytes.
//! [`default_path`] resolves a repo layout from an injected root, and
//! [`path_from_env`] is the pure resolver the CLI wires up.
//!
//! Not ported here: the *introspection* half (`discover_adapters`,
//! `support_matrix`, `render_markdown`). Rust's registry is compile-time
//! ([`crate::registry`]), so "which adapters exist" is a different question
//! there; the rendering belongs with the CLI command that prints it (wave 8).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::pyval;

/// The envelope schema `services/support_matrix.py` publishes.
pub const SCHEMA: &str = "stackunderflow.support-matrix/1";

/// The schema string the data file itself carries.
pub const FILE_SCHEMA: &str = "stackunderflow.adapter-capabilities/1";

/// The environment variable that re-points the loader at another copy of the
/// table. Named for the Python package, not the port, because it points at a
/// file that ships with the Python package.
pub const CAPABILITIES_PATH_ENV: &str = "STACKUNDERFLOW_CAPABILITIES";

/// The data file's path relative to the repository root.
pub const CAPABILITIES_RELATIVE_PATH: &str = "stackunderflow/adapters/capabilities.json";

/// A canonical record field, in the display order `FIELDS` declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    /// Message text — prompts, responses, and tool text.
    ContentText,
    /// Input / output / cache token counts.
    Tokens,
    /// Per-message USD cost attribution.
    Cost,
    /// Names of the tools / functions invoked.
    ToolCalls,
    /// Tool result / output text.
    ToolOutput,
    /// Reasoning / thinking token split (v026 attribution).
    Reasoning,
    /// Files created or edited, attributable per session.
    FileTouches,
}

/// Every [`Field`], in `FIELDS` display order.
pub const FIELDS: [Field; 7] = [
    Field::ContentText,
    Field::Tokens,
    Field::Cost,
    Field::ToolCalls,
    Field::ToolOutput,
    Field::Reasoning,
    Field::FileTouches,
];

impl Field {
    /// The JSON key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentText => "content_text",
            Self::Tokens => "tokens",
            Self::Cost => "cost",
            Self::ToolCalls => "tool_calls",
            Self::ToolOutput => "tool_output",
            Self::Reasoning => "reasoning",
            Self::FileTouches => "file_touches",
        }
    }

    /// The one-line description `FIELDS` carries alongside the key.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::ContentText => "Message text — prompts, responses, and tool text",
            Self::Tokens => "Input / output / cache token counts",
            Self::Cost => "Per-message USD cost attribution",
            Self::ToolCalls => "Names of the tools / functions invoked",
            Self::ToolOutput => "Tool result / output text",
            Self::Reasoning => "Reasoning / thinking token split (v026 attribution)",
            Self::FileTouches => "Files created or edited, attributable per session",
        }
    }

    fn parse(key: &str) -> Option<Self> {
        FIELDS.into_iter().find(|field| field.as_str() == key)
    }
}

/// How well a provider captures a [`Field`] (`FIDELITY_LEVELS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fidelity {
    /// Captured completely and structurally.
    Full,
    /// Numeric values read directly from the source.
    Exact,
    /// Derived / approximated.
    Estimated,
    /// Captured but incomplete or unstructured.
    Partial,
    /// Not captured. `captured` reads false.
    #[default]
    None,
}

impl Fidelity {
    /// The JSON spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Exact => "exact",
            Self::Estimated => "estimated",
            Self::Partial => "partial",
            Self::None => "none",
        }
    }

    /// The invariant every consumer relies on: `captured == (fidelity != none)`
    /// (`support_matrix.py:48`).
    #[must_use]
    pub const fn captured(self) -> bool {
        !matches!(self, Self::None)
    }

    fn parse(value: &str) -> Option<Self> {
        [
            Self::Full,
            Self::Exact,
            Self::Estimated,
            Self::Partial,
            Self::None,
        ]
        .into_iter()
        .find(|level| level.as_str() == value)
    }
}

/// The adapter status vocabulary (`STATUSES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Full-stream ingest, broadly validated.
    Supported,
    /// Captures a deliberately reduced dataset.
    Partial,
    /// On by default and functional, pending broad validation.
    Beta,
}

impl Status {
    /// The JSON spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Partial => "partial",
            Self::Beta => "beta",
        }
    }

    /// The display weight `_STATUS_ORDER` assigns: supported → beta → partial.
    #[must_use]
    pub const fn order(self) -> u8 {
        match self {
            Self::Supported => 0,
            Self::Beta => 1,
            Self::Partial => 2,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::Supported, Self::Partial, Self::Beta]
            .into_iter()
            .find(|status| status.as_str() == value)
    }
}

/// How far a resume command reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeScope {
    /// The command takes a session id: `{session_id}` is substituted.
    Session,
    /// The command resumes the latest session only; no id can be passed.
    Latest,
}

impl ResumeScope {
    /// The JSON spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Latest => "latest",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "session" => Some(Self::Session),
            "latest" => Some(Self::Latest),
            _ => None,
        }
    }
}

/// A provider's resume invocation, as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    /// The command template, e.g. `claude --resume {session_id}`.
    pub command: String,
    /// Whether the template takes a session id.
    pub scope: ResumeScope,
    /// How the entry was verified (free text; absent on some rows).
    pub verified: Option<String>,
    /// An extra caveat, present on `grok` only today.
    pub note: Option<String>,
}

impl Resume {
    /// Render the template for `session_id`, mirroring `cli.py:6395`.
    ///
    /// `None` for a [`ResumeScope::Latest`] provider — a latest-scope CLI has
    /// nowhere to put an id, and inventing a flag would print a command that
    /// does not work.
    #[must_use]
    pub fn render(&self, session_id: &str) -> Option<String> {
        match self.scope {
            ResumeScope::Session => Some(self.command.replace("{session_id}", session_id)),
            ResumeScope::Latest => None,
        }
    }
}

/// One provider's curated row.
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterCapability {
    /// The provider key (the JSON object key).
    pub provider: String,
    /// Human label, e.g. `Claude Code`.
    pub label: String,
    /// Status vocabulary.
    pub status: Status,
    /// Resume invocation, when one is known.
    pub resume: Option<Resume>,
    /// Whether this source can ever yield billable usage events.
    pub emits_usage_events: bool,
    /// Free-text caveats.
    pub notes: String,
    /// Where in the adapter/normalizer source the row was read from.
    pub basis: String,
    /// Per-field fidelity; every [`Field`] is present, defaulting to
    /// [`Fidelity::None`].
    pub fields: BTreeMap<Field, Fidelity>,
}

impl AdapterCapability {
    /// The fidelity for `field` (`support_matrix.py:field_fidelity`).
    #[must_use]
    pub fn field_fidelity(&self, field: Field) -> Fidelity {
        self.fields.get(&field).copied().unwrap_or(Fidelity::None)
    }

    /// Whether `field` is captured at all (`support_matrix.py:captures`).
    #[must_use]
    pub fn captures(&self, field: Field) -> bool {
        self.field_fidelity(field).captured()
    }
}

/// The whole loaded table.
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    entries: BTreeMap<String, AdapterCapability>,
    schema: String,
}

impl Capabilities {
    /// Load and validate the table at `path`.
    ///
    /// Validation is `_load_capabilities`'s, kept strict on purpose: an unknown
    /// status, a malformed `resume` block, an unknown field key, or an unknown
    /// fidelity is an error, not a shrug. Those branches are marked
    /// `pragma: no cover` in Python precisely because a healthy tree never hits
    /// them — the table is curated, and a typo in it must be loud.
    ///
    /// # Errors
    /// Unreadable file, invalid JSON, a missing `adapters` object, or any of the
    /// validation failures above.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading capabilities table {}", path.display()))?;
        Self::from_str(&text)
            .with_context(|| format!("parsing capabilities table {}", path.display()))
    }

    /// Parse and validate an in-memory copy of the table.
    ///
    /// # Errors
    /// As [`Capabilities::load`].
    #[allow(
        clippy::should_implement_trait,
        reason = "this is a fallible domain parse, not std::str::FromStr's \
        infallible-until-Err contract; naming it `from_str` keeps it next to \
        `load` for readers coming from the Python loader"
    )]
    pub fn from_str(text: &str) -> Result<Self> {
        let raw: Value = serde_json::from_str(text).context("capabilities.json is not JSON")?;
        let schema = raw
            .get("schema")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(adapters) = raw.get("adapters").and_then(Value::as_object) else {
            bail!("capabilities.json has no `adapters` object");
        };
        let mut entries = BTreeMap::new();
        for (name, entry) in adapters {
            entries.insert(name.clone(), parse_entry(name, entry)?);
        }
        Ok(Self { entries, schema })
    }

    /// The `schema` string the file declared.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// One provider's row, or `None` when the table does not carry it.
    #[must_use]
    pub fn get(&self, provider: &str) -> Option<&AdapterCapability> {
        self.entries.get(provider)
    }

    /// Every provider key, sorted.
    #[must_use]
    pub fn providers(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// Every row, sorted by provider key.
    pub fn iter(&self) -> impl Iterator<Item = &AdapterCapability> {
        self.entries.values()
    }

    /// How many providers the table documents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table is empty (never true for a healthy tree).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `provider` can ever emit billable usage events.
    ///
    /// Unknown providers read `true`, matching
    /// `cap.get("emits_usage_events", True)` at every Python call site: the
    /// adapter↔normalizer parity check must treat an undocumented provider as
    /// *expected to bill*, so a missing row shows up as a gap rather than as a
    /// silent exemption.
    #[must_use]
    pub fn emits_usage_events(&self, provider: &str) -> bool {
        self.get(provider).is_none_or(|cap| cap.emits_usage_events)
    }

    /// The rendered resume command for `provider`/`session_id`, when one exists.
    #[must_use]
    pub fn resume_command(&self, provider: &str, session_id: &str) -> Option<String> {
        self.get(provider)?.resume.as_ref()?.render(session_id)
    }

    /// The fidelity of `field` for `provider`; unknown providers read
    /// [`Fidelity::None`] (`support_matrix.py:field_fidelity`).
    #[must_use]
    pub fn field_fidelity(&self, provider: &str, field: Field) -> Fidelity {
        self.get(provider)
            .map_or(Fidelity::None, |cap| cap.field_fidelity(field))
    }

    /// Whether `provider` captures `field` at all.
    #[must_use]
    pub fn captures(&self, provider: &str, field: Field) -> bool {
        self.field_fidelity(provider, field).captured()
    }
}

impl<'a> IntoIterator for &'a Capabilities {
    type Item = &'a AdapterCapability;
    type IntoIter = std::collections::btree_map::Values<'a, String, AdapterCapability>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.values()
    }
}

fn parse_entry(name: &str, entry: &Value) -> Result<AdapterCapability> {
    let Some(entry) = entry.as_object() else {
        bail!("adapter {name:?} is not an object");
    };
    let status = entry
        .get("status")
        .and_then(Value::as_str)
        .and_then(Status::parse)
        .with_context(|| {
            let raw = entry
                .get("status")
                .map_or_else(|| "<missing>".to_string(), pyval::py_repr);
            format!("unknown status {raw} for adapter {name:?}")
        })?;
    let label = entry
        .get("label")
        .and_then(Value::as_str)
        .with_context(|| format!("adapter {name:?} has no label"))?
        .to_string();
    let resume = match entry.get("resume") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_resume(name, value)?),
    };
    let mut fields: BTreeMap<Field, Fidelity> =
        FIELDS.into_iter().map(|f| (f, Fidelity::None)).collect();
    if let Some(raw_fields) = entry.get("fields") {
        let Some(raw_fields) = raw_fields.as_object() else {
            bail!("adapter {name:?} has a non-object `fields`");
        };
        for (key, value) in raw_fields {
            let field = Field::parse(key)
                .with_context(|| format!("unknown support-matrix field: {key:?}"))?;
            let level = value
                .as_str()
                .and_then(Fidelity::parse)
                .with_context(|| format!("unknown fidelity for field {key:?}"))?;
            fields.insert(field, level);
        }
    }
    Ok(AdapterCapability {
        provider: name.to_string(),
        label,
        status,
        resume,
        // `bool(entry.get("emits_usage_events", True))` — Python truthiness, so
        // a non-bool value coerces rather than failing.
        emits_usage_events: entry.get("emits_usage_events").is_none_or(pyval::py_truthy),
        notes: entry
            .get("notes")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        basis: entry
            .get("basis")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        fields,
    })
}

fn parse_resume(name: &str, value: &Value) -> Result<Resume> {
    let malformed = || format!("malformed resume entry for adapter {name:?}");
    let block = value.as_object().with_context(malformed)?;
    let command = block
        .get("command")
        .and_then(Value::as_str)
        .with_context(malformed)?
        .to_string();
    let scope = block
        .get("scope")
        .and_then(Value::as_str)
        .and_then(ResumeScope::parse)
        .with_context(malformed)?;
    Ok(Resume {
        command,
        scope,
        verified: block
            .get("verified")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        note: block
            .get("note")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

/// The table's path under `repo_root`.
#[must_use]
pub fn default_path(repo_root: &Path) -> PathBuf {
    repo_root.join(CAPABILITIES_RELATIVE_PATH)
}

/// Resolve the table's path from an injected environment.
///
/// `$STACKUNDERFLOW_CAPABILITIES` wins when set and non-empty; otherwise the
/// repo layout under `repo_root`. Pure by construction — the campaign forbids
/// `set_var` (Rust 2024 makes it `unsafe`), so the environment is a parameter,
/// exactly as in `stax_core::settings::resolve_app_dir`.
#[must_use]
pub fn path_from_env(raw: Option<&OsStr>, repo_root: &Path) -> PathBuf {
    match raw.filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => default_path(repo_root),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_path_wins_over_the_repo_layout() {
        let root = Path::new("/repo");
        assert_eq!(
            path_from_env(None, root),
            Path::new("/repo/stackunderflow/adapters/capabilities.json")
        );
        assert_eq!(
            path_from_env(Some(OsStr::new("")), root),
            Path::new("/repo/stackunderflow/adapters/capabilities.json")
        );
        assert_eq!(
            path_from_env(Some(OsStr::new("/elsewhere/caps.json")), root),
            Path::new("/elsewhere/caps.json")
        );
    }

    #[test]
    fn an_unknown_status_is_an_error_not_a_shrug() {
        let err =
            Capabilities::from_str(r#"{"adapters": {"x": {"label": "X", "status": "shipped"}}}"#)
                .expect_err("unknown status must fail");
        assert!(format!("{err:#}").contains("unknown status"), "{err:#}");
    }

    #[test]
    fn an_unknown_field_or_fidelity_is_an_error() {
        let err = Capabilities::from_str(
            r#"{"adapters": {"x": {"label": "X", "status": "beta",
                "fields": {"nonesuch": "full"}}}}"#,
        )
        .expect_err("unknown field must fail");
        assert!(
            format!("{err:#}").contains("unknown support-matrix field"),
            "{err:#}"
        );

        let err = Capabilities::from_str(
            r#"{"adapters": {"x": {"label": "X", "status": "beta",
                "fields": {"tokens": "vibes"}}}}"#,
        )
        .expect_err("unknown fidelity must fail");
        assert!(format!("{err:#}").contains("unknown fidelity"), "{err:#}");
    }

    #[test]
    fn a_malformed_resume_block_is_an_error() {
        for bad in [
            r#"{"scope": "session"}"#,
            r#"{"command": "x {session_id}", "scope": "everything"}"#,
            r#""claude --resume""#,
        ] {
            let json = format!(
                r#"{{"adapters": {{"x": {{"label": "X", "status": "beta", "resume": {bad}}}}}}}"#
            );
            let err = Capabilities::from_str(&json).expect_err("malformed resume must fail");
            assert!(
                format!("{err:#}").contains("malformed resume entry"),
                "{err:#}"
            );
        }
    }

    #[test]
    fn unset_fields_default_to_none_and_emits_defaults_to_true() {
        let caps = Capabilities::from_str(
            r#"{"adapters": {"x": {"label": "X", "status": "beta",
                "fields": {"tokens": "exact"}}}}"#,
        )
        .expect("loads");
        let entry = caps.get("x").expect("entry");
        assert_eq!(entry.field_fidelity(Field::Tokens), Fidelity::Exact);
        assert_eq!(entry.field_fidelity(Field::Cost), Fidelity::None);
        assert!(entry.captures(Field::Tokens));
        assert!(!entry.captures(Field::Cost));
        assert!(entry.emits_usage_events);
        // Every canonical field is present, never silently omitted.
        assert_eq!(entry.fields.len(), FIELDS.len());
    }

    #[test]
    fn resume_renders_only_for_session_scope() {
        let session = Resume {
            command: "claude --resume {session_id}".to_string(),
            scope: ResumeScope::Session,
            verified: None,
            note: None,
        };
        assert_eq!(
            session.render("abc").as_deref(),
            Some("claude --resume abc")
        );
        let latest = Resume {
            command: "grok --continue".to_string(),
            scope: ResumeScope::Latest,
            verified: None,
            note: None,
        };
        assert_eq!(latest.render("abc"), None);
    }

    #[test]
    fn an_unknown_provider_is_assumed_to_bill() {
        let caps = Capabilities::from_str(r#"{"adapters": {}}"#).expect("loads");
        assert!(caps.emits_usage_events("nonesuch"));
        assert_eq!(caps.field_fidelity("nonesuch", Field::Cost), Fidelity::None);
        assert!(caps.is_empty());
    }
}
