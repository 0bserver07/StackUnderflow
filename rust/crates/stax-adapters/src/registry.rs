//! The adapter registry — the port of `adapters/__init__.py`'s self-discovery.
//!
//! Python walks its own package at import time and registers every class with a
//! non-empty `name` and callable `enumerate`/`read`. The point of that design is
//! stated in its docstring: *"there is no import list to extend, no opt-in flag,
//! and no way to ship an adapter that silently never registers"* — silent
//! absence is how 13 agents' data went dark under the old beta gating.
//!
//! Rust reaches the same guarantee at compile time instead of at import time.
//! TASKS-RS RS-2-001 suggested an `inventory`/`linkme` link-section registry;
//! this port deliberately does not use one, because it would not remove the
//! central edit it exists to remove: a new provider needs a `mod` declaration in
//! `lib.rs` no matter what, so the "one central list" is unavoidable and a
//! second registration mechanism only adds a way for the two to disagree. What
//! replaces Python's loudness is a test:
//! `every_registered_adapter_has_a_capabilities_row` fails the build if a
//! provider is registered without a curated row, and (once all 20 land) the
//! reverse.
//!
//! **Adding a provider is two lines:** `mod <name>;` in `lib.rs` and one entry
//! in [`registered`].

use crate::base::SourceAdapter;
use crate::cline::{ClineFamilyAdapter, Variant};
use crate::{
    antigravity, claude, codex, continue_ext, copilot, cursor, droid, gemini, grok, kiro, openclaw,
    opencode, pi, qwen,
};

/// Every adapter this build carries, in Python's registration order (module
/// name, then class name) — `adapters.registered()`.
///
/// The order is not cosmetic: it is `sorted(pkgutil.iter_modules())` and then
/// `sorted(vars(module))`, which is why the three Cline-family providers sit
/// between `claude` and `codex` (module `cline`) in class-name order, and why
/// `cursor` follows `codex`.
#[must_use]
pub fn registered() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(antigravity::AntigravityAdapter::new()),
        Box::new(claude::ClaudeAdapter::new()),
        Box::new(ClineFamilyAdapter::new(Variant::Cline)),
        Box::new(ClineFamilyAdapter::new(Variant::KiloCode)),
        Box::new(ClineFamilyAdapter::new(Variant::RooCode)),
        Box::new(codex::CodexAdapter::new()),
        Box::new(continue_ext::ContinueAdapter::new()),
        Box::new(copilot::CopilotAdapter::new()),
        Box::new(cursor::CursorAdapter::new()),
        Box::new(droid::DroidAdapter::new()),
        Box::new(gemini::GeminiAdapter::new()),
        Box::new(grok::GrokAdapter::new()),
        Box::new(kiro::KiroAdapter::new()),
        Box::new(openclaw::OpenClawAdapter::new()),
        Box::new(opencode::OpenCodeAdapter::new()),
        Box::new(pi::PiAdapter::new()),
        Box::new(qwen::QwenAdapter::new()),
    ]
}

/// The full twenty, in the order Python's module walk yields them.
///
/// The canonical order lives here as data so a partially-landed build can still
/// assert it is *in* order without hardcoding how many providers exist yet —
/// see `registration_order_is_a_prefix_free_subsequence_of_pythons`.
pub const PYTHON_WALK_ORDER: [&str; 20] = [
    "antigravity",
    "claude",
    "cline",
    "kilocode",
    "roocode",
    "codeium",
    "codex",
    "continue",
    "copilot",
    "cursor",
    "cursor-agent",
    "droid",
    "gemini",
    "grok",
    "hermes",
    "kiro",
    "openclaw",
    "opencode",
    "pi",
    "qwen",
];

/// The provider keys this build carries, in registration order.
#[must_use]
pub fn registered_names() -> Vec<String> {
    registered()
        .iter()
        .map(|adapter| adapter.name().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_order_matches_pythons_module_walk() {
        assert_eq!(
            registered_names(),
            vec![
                "antigravity",
                "claude",
                "cline",
                "kilocode",
                "roocode",
                "codex",
                "continue",
                "copilot",
                "cursor",
                "droid",
                "gemini",
                "grok",
                "kiro",
                "openclaw",
                "opencode",
                "pi",
                "qwen",
            ]
        );
    }

    #[test]
    fn registration_order_is_a_subsequence_of_pythons_module_walk() {
        // The exact-list assertion above is a merge point every stamp-out batch
        // edits. This one is not: it holds for any partially-landed build, and
        // it is the assertion that actually encodes *why* the order is what it
        // is (`sorted(pkgutil.iter_modules())`, then `sorted(vars(module))`).
        let mut expected = PYTHON_WALK_ORDER.iter();
        for name in registered_names() {
            assert!(
                expected.any(|candidate| *candidate == name),
                "{name:?} is registered out of Python's module-walk order \
                 (or is not in PYTHON_WALK_ORDER at all)"
            );
        }
    }

    #[test]
    fn the_canonical_order_is_itself_well_formed() {
        let mut unique: Vec<&str> = PYTHON_WALK_ORDER.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), PYTHON_WALK_ORDER.len(), "duplicate provider");
    }

    #[test]
    fn every_adapter_names_itself_exactly_once() {
        let names = registered_names();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            names.len(),
            unique.len(),
            "duplicate provider key: {names:?}"
        );
        assert!(names.iter().all(|name| !name.is_empty()));
    }
}
