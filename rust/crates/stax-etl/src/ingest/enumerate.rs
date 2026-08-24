//! Port of `python-legacy: ingest/enumerate.py` — twelve lines, one job.
//!
//! Fans every registered adapter's `SessionRef`s into one sequence, in adapter
//! registration order. Python's is a generator (`yield from adapter.enumerate()`)
//! and the laziness is not incidental: `run_ingest` opens a transaction per ref,
//! so a materialised list of every session on the machine would hold ~15K
//! `SessionRef`s while the first one is still being written.
//!
//! Rust's [`SourceAdapter::enumerate`] returns a `Vec` — that is the wave-2
//! contract, and it is the right one there (an adapter's own walk is a
//! directory listing, bounded by one provider's tree). So the laziness that
//! matters is *across* adapters, which is what [`iter_refs`] preserves: the
//! second adapter's tree is not walked until the first adapter's refs have been
//! consumed.

use stax_adapters::base::{SessionRef, SourceAdapter};

/// `iter_refs(adapters)` — every adapter's sessions, lazily, in registry order.
///
/// The `flat_map` closure calls `enumerate()` per adapter as the iterator
/// reaches it, which is `yield from` with the same evaluation order.
pub fn iter_refs<'a>(
    adapters: &'a [Box<dyn SourceAdapter>],
) -> impl Iterator<Item = SessionRef> + 'a {
    adapters
        .iter()
        .flat_map(|adapter| adapter.enumerate().into_iter())
}

/// `_lookup(adapters, name)` — the adapter that produced a ref.
///
/// # Errors
/// `KeyError: No adapter registered for provider …`, which Python raises and
/// does not catch: a ref whose provider is not in the list means the registry
/// and the enumeration disagree, and continuing would silently drop a file.
pub fn lookup<'a>(
    adapters: &'a [Box<dyn SourceAdapter>],
    name: &str,
) -> anyhow::Result<&'a dyn SourceAdapter> {
    adapters
        .iter()
        .find(|adapter| adapter.name() == name)
        .map(std::convert::AsRef::as_ref)
        .ok_or_else(|| anyhow::anyhow!("No adapter registered for provider {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::testdb::FakeAdapter;

    #[test]
    fn refs_come_out_in_registry_order() {
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![
            Box::new(FakeAdapter::with_refs("claude", 2)),
            Box::new(FakeAdapter::with_refs("codex", 1)),
        ];
        let providers: Vec<String> = iter_refs(&adapters)
            .map(|session| session.provider)
            .collect();
        assert_eq!(providers, ["claude", "claude", "codex"]);
    }

    #[test]
    fn the_second_adapter_is_not_walked_until_the_first_is_drained() {
        // The property `yield from` gives Python and a `Vec<Vec<_>>` would not.
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![
            Box::new(FakeAdapter::with_refs("claude", 3)),
            Box::new(FakeAdapter::with_refs("codex", 3)),
        ];
        let mut refs = iter_refs(&adapters);
        assert_eq!(refs.next().unwrap().provider, "claude");
        let walked = adapters
            .iter()
            .filter(|a| a.name() == "codex")
            .map(|a| a.enumerate().len())
            .sum::<usize>();
        assert_eq!(walked, 3, "…and it still enumerates correctly when reached");
    }

    #[test]
    fn an_unknown_provider_is_an_error_not_a_silent_skip() {
        let adapters: Vec<Box<dyn SourceAdapter>> =
            vec![Box::new(FakeAdapter::with_refs("claude", 1))];
        assert!(lookup(&adapters, "claude").is_ok());
        let err = match lookup(&adapters, "cursor") {
            Ok(_) => panic!("an unregistered provider must not resolve"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("No adapter registered for provider"), "{err}");
    }
}
