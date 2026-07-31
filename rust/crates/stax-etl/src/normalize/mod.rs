//! The normalize layer — per-provider `messages → usage_events` transforms.
//!
//! Port of `stackunderflow/etl/normalize/`. Twenty registry keys over seventeen
//! transforms: `kilocode` and `roocode` are `ClineNormalizer` subclasses that
//! override nothing but their key, and `omp` is a `provider_aliases` entry on
//! `PiNormalizer`.
//!
//! # The registry is a table, not a walk
//!
//! Python's registry is **self-discovering**: `_discover_and_register` walks the
//! package with `pkgutil`, imports every module, and registers every concrete
//! `Normalizer` subclass whose `provider_name` is non-empty (first-wins on a
//! duplicate, aliases after their class). It became a walk because the old
//! hand-written block shipped cursor-agent under the wrong key for months and
//! silently stranded every one of its rows.
//!
//! Rust has no import-time reflection, so the walk cannot be ported — this is a
//! table. The gap that motivated the walk is closed differently: the table is
//! diffed against the *live Python registry* by
//! `tests/registry_parity.rs`, which shells out to the reference interpreter and
//! compares key-for-key and key→module. A key added on either side and not the
//! other fails that test. A comment claiming the keys match would not.
//!
//! # What a normalizer is allowed to do
//!
//! Yield 0..N events for one row. Skipping is normal and heavily used: the
//! silent bare `return`s are the current contract (see [`claude`]), and
//! reproducing them is what keeps the Rust event count equal to Python's rather
//! than helpfully larger.

pub mod base;
pub mod claude;
pub mod cline;
pub mod codeium;
pub mod codex;
pub mod continue_ext;
pub mod copilot;
pub mod cursor;
pub mod cursor_agent;
pub mod droid;
pub mod gemini;
pub mod grok;
pub mod hermes;
pub mod kiro;
pub mod openclaw;
pub mod opencode;
pub mod pass;
pub mod pi;
pub mod qwen;
pub mod row;
pub mod text;

pub use base::{
    CostArgs, CostSource, EventSpec, NormalizeContext, Normalizer, UsageEvent, day_from_ts,
};
pub use row::{MsgRow, PyRaise};

use claude::ClaudeNormalizer;
use cline::ClineNormalizer;
use codeium::CodeiumNormalizer;
use codex::CodexNormalizer;
use continue_ext::ContinueNormalizer;
use copilot::CopilotNormalizer;
use cursor::CursorNormalizer;
use cursor_agent::CursorAgentNormalizer;
use droid::DroidNormalizer;
use gemini::GeminiNormalizer;
use grok::GrokNormalizer;
use hermes::HermesNormalizer;
use kiro::KiroNormalizer;
use openclaw::OpenClawNormalizer;
use opencode::OpenCodeNormalizer;
use pi::PiNormalizer;
use qwen::QwenNormalizer;

// The instances. Zero-sized but for `ClineNormalizer`'s key, so `static` costs
// nothing and lets `get` hand back a `&'static dyn Normalizer`.
static CLAUDE: ClaudeNormalizer = ClaudeNormalizer;
static CLINE: ClineNormalizer = ClineNormalizer::cline();
static CODEIUM: CodeiumNormalizer = CodeiumNormalizer;
static CODEX: CodexNormalizer = CodexNormalizer;
static CONTINUE: ContinueNormalizer = ContinueNormalizer;
static COPILOT: CopilotNormalizer = CopilotNormalizer;
static CURSOR: CursorNormalizer = CursorNormalizer;
static CURSOR_AGENT: CursorAgentNormalizer = CursorAgentNormalizer;
static DROID: DroidNormalizer = DroidNormalizer;
static GEMINI: GeminiNormalizer = GeminiNormalizer;
static GROK: GrokNormalizer = GrokNormalizer;
static HERMES: HermesNormalizer = HermesNormalizer;
static KILOCODE: ClineNormalizer = ClineNormalizer::kilocode();
static KIRO: KiroNormalizer = KiroNormalizer;
static OPENCLAW: OpenClawNormalizer = OpenClawNormalizer;
static OPENCODE: OpenCodeNormalizer = OpenCodeNormalizer;
static PI: PiNormalizer = PiNormalizer;
static QWEN: QwenNormalizer = QwenNormalizer;
static ROOCODE: ClineNormalizer = ClineNormalizer::roocode();

/// The registry, in the order `_discover_and_register` fills it: modules sorted
/// by name, and a class's aliases immediately after the class.
///
/// `pi` is followed by `omp` for that reason — not alphabetically.
const REGISTRY: [(&str, &dyn Normalizer); 20] = [
    ("claude", &CLAUDE),
    ("cline", &CLINE),
    ("codeium", &CODEIUM),
    ("codex", &CODEX),
    ("continue", &CONTINUE),
    ("copilot", &COPILOT),
    ("cursor", &CURSOR),
    ("cursor-agent", &CURSOR_AGENT),
    ("droid", &DROID),
    ("gemini", &GEMINI),
    ("grok", &GROK),
    ("hermes", &HERMES),
    ("kilocode", &KILOCODE),
    ("kiro", &KIRO),
    ("openclaw", &OPENCLAW),
    ("opencode", &OPENCODE),
    ("pi", &PI),
    ("omp", &PI),
    ("qwen", &QWEN),
    ("roocode", &ROOCODE),
];

/// `normalize.all()` — every registered `(provider, normalizer)` in
/// registration order.
#[must_use]
pub fn all() -> Vec<(&'static str, &'static dyn Normalizer)> {
    REGISTRY.to_vec()
}

/// `normalize.get(provider)` — the registered transform, or `None`.
#[must_use]
pub fn get(provider: &str) -> Option<&'static dyn Normalizer> {
    REGISTRY
        .iter()
        .find(|(key, _)| *key == provider)
        .map(|(_, normalizer)| *normalizer)
}

/// `normalize.registered_providers()` — the keys, sorted.
///
/// `backfill._run_normalizers` builds its `WHERE p.provider IN (…)` list from
/// exactly this, so the order is part of the SQL the two implementations issue.
#[must_use]
pub fn registered_providers() -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = REGISTRY.iter().map(|(key, _)| *key).collect();
    keys.sort_unstable();
    keys
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Fixtures shared by the per-provider unit tests.

    use stax_core::queries::pyjson::Value as PyValue;

    use super::base::NormalizeContext;
    use super::row::MsgRow;

    /// The context `stackunderflow etl backfill` runs in — real
    /// `data/models.toml`, no price book (DIV-016).
    pub fn ctx() -> NormalizeContext {
        NormalizeContext::unprimed(&crate::pricing::test_support::manifest_path())
            .expect("the checked-in models.toml parses")
    }

    /// A joined row with the columns `_run_normalizers` selects, tokens zeroed.
    pub fn assistant_row(provider: &str, model: &str) -> MsgRow {
        MsgRow::new()
            .with("id", PyValue::Int(1))
            .with("provider", PyValue::Str(provider.to_string()))
            .with("project_id", PyValue::Int(42))
            .with("session_id", PyValue::Str("sess-1".to_string()))
            .with(
                "timestamp",
                PyValue::Str("2026-04-25T12:00:00+00:00".to_string()),
            )
            .with("role", PyValue::Str("assistant".to_string()))
            .with("model", PyValue::Str(model.to_string()))
            .with("input_tokens", PyValue::Int(0))
            .with("output_tokens", PyValue::Int(0))
            .with("cache_read_tokens", PyValue::Int(0))
            .with("cache_create_tokens", PyValue::Int(0))
            .with("content_text", PyValue::Str(String::new()))
            .with("raw_json", PyValue::Str("{}".to_string()))
            .with("speed", PyValue::Str("standard".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stax_core::queries::pyjson::Value as PyValue;
    use test_support::{assistant_row, ctx};

    #[test]
    fn twenty_keys_over_seventeen_transforms() {
        assert_eq!(REGISTRY.len(), 20);
        assert_eq!(registered_providers().len(), 20);
        let mut keys = registered_providers();
        keys.dedup();
        assert_eq!(keys.len(), 20, "no key may be registered twice");
    }

    #[test]
    fn the_three_cline_keys_and_the_two_pi_keys_share_their_transforms() {
        for key in ["cline", "kilocode", "roocode"] {
            assert_eq!(get(key).expect("registered").provider_name(), key);
        }
        // The Pi alias is the exception: `omp` resolves to a normalizer whose
        // `provider_name` — and therefore whose pricer key — is `pi`.
        assert_eq!(get("omp").expect("registered").provider_name(), "pi");
        assert_eq!(get("pi").expect("registered").provider_name(), "pi");
    }

    #[test]
    fn every_key_answers_to_its_own_name_except_the_alias() {
        for (key, normalizer) in all() {
            if key == "omp" {
                continue;
            }
            assert_eq!(
                normalizer.provider_name(),
                key,
                "{key} must route through its own pricer"
            );
        }
    }

    #[test]
    fn an_unregistered_provider_is_none_not_a_panic() {
        assert!(
            get("antigravity").is_none(),
            "antigravity has no normalizer"
        );
        assert!(get("").is_none());
        assert!(get("CLAUDE").is_none(), "keys are case-sensitive");
    }

    #[test]
    fn every_normalizer_drops_a_user_role_row() {
        // The Python suite's `test_beta_normalizer_user_role_yields_no_events`,
        // widened to all twenty keys.
        let ctx = ctx();
        for (key, normalizer) in all() {
            let row = assistant_row(key, "claude-sonnet-4-5-20250929")
                .with("role", PyValue::Str("user".into()))
                .with("input_tokens", PyValue::Int(100))
                .with("output_tokens", PyValue::Int(100))
                .with("content_text", PyValue::Str("user msg".into()));
            assert!(
                normalizer.normalize(&ctx, &row).unwrap().is_empty(),
                "{key}: user-role rows must yield zero events"
            );
        }
    }

    #[test]
    fn every_normalizer_survives_malformed_raw_json() {
        // The Python suite's `test_beta_normalizer_malformed_raw_json_does_not_
        // raise`. "Survives" means no panic and no raise — a yielded event is
        // allowed, and is checked for canonical shape.
        let ctx = ctx();
        for (key, normalizer) in all() {
            let row = assistant_row(key, "")
                .with("model", PyValue::Str(String::new()))
                .with(
                    "raw_json",
                    PyValue::Str("this is not valid json {{{".into()),
                );
            let events = normalizer
                .normalize(&ctx, &row)
                .unwrap_or_else(|raise| panic!("{key} raised on malformed raw_json: {raise}"));
            for event in events {
                assert!(event.cost_usd >= 0.0, "{key}");
                assert!(event.input_tokens >= 0, "{key}");
                assert!(
                    ["standard", "fast"].contains(&event.speed.as_str()),
                    "{key}"
                );
            }
        }
    }

    #[test]
    fn every_normalizer_leaves_a_bare_row_alone() {
        // No columns at all — the degenerate case a synthetic test can build.
        let ctx = ctx();
        for (key, normalizer) in all() {
            let events = normalizer
                .normalize(&ctx, &MsgRow::new())
                .unwrap_or_else(|raise| panic!("{key} raised on an empty row: {raise}"));
            assert!(events.is_empty(), "{key} invented an event from nothing");
        }
    }
}
