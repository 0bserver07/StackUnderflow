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
use crate::{claude, codex};

/// Every adapter this build carries, in Python's registration order (module
/// name, then class name) — `adapters.registered()`.
///
/// Wave 2 lands `claude` + `codex`; the remaining 18 providers append here.
#[must_use]
pub fn registered() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude::ClaudeAdapter::new()),
        Box::new(codex::CodexAdapter::new()),
    ]
}

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
        assert_eq!(registered_names(), vec!["claude", "codex"]);
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
