//! Codeium — the port of `stackunderflow/adapters/codeium.py`.
//!
//! A **discovery-only stub**, and deliberately so. The Codeium plugin keeps its
//! chat state under `~/.codeium/` as protobuf-encoded binary blobs with no
//! published `.proto`; the module docstring on the Python side records the
//! decision and its three reasons (no schema, a plugin-specific message shape,
//! and data on the reference machine that predates the project). Until a stable
//! parser exists, `enumerate()` and `read()` yield nothing.
//!
//! ## Why it is registered at all
//!
//! Because the alternative is silence. `adapters/__init__.py` states the rule in
//! its own docstring — *"there is no import list to extend, no opt-in flag, and
//! no way to ship an adapter that silently never registers"* — after 13 agents'
//! data went dark under the old beta gating. A registered-but-inert adapter is a
//! provider key the support matrix can carry an honest `partial` /
//! `emits_usage_events: false` row for, which is strictly more information than
//! an absent one.
//!
//! ## The one capability it does carry
//!
//! [`SourceAdapter::source_roots`] returns `~/.codeium`, and
//! [`SourceAdapter::watch_paths`] stays empty. That asymmetry is the Python
//! original's, not an oversight: `codeium.py` declares `source_roots` and no
//! `watch_paths`, so `backup create` copies the tree (449 MB of blobs a future
//! parser will want) while the ETL watcher never wakes for a provider that
//! cannot produce a record.

use std::path::{Path, PathBuf};

use crate::base::{Record, SessionRef, SourceAdapter, home_dir};

/// The provider key.
pub const NAME: &str = "codeium";

/// The discovery path (`_CODEIUM_ROOT`), relative to the home directory.
///
/// Kept as a constant for the same reason Python keeps the module-level path:
/// so a future implementer can grep for it. `enumerate` does not walk it.
pub const ROOT_RELATIVE: &str = ".codeium";

/// The Codeium source adapter (`CodeiumAdapter`) — registered, inert.
#[derive(Debug, Clone)]
pub struct CodeiumAdapter {
    root: PathBuf,
}

impl Default for CodeiumAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeiumAdapter {
    /// `~/.codeium`, resolved once at construction.
    #[must_use]
    pub fn new() -> Self {
        Self::with_optional_root(None)
    }

    /// Inject the discovery root — `CodeiumAdapter(root=…)`.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The Python constructor exactly: `root or Path.home() / ".codeium"`.
    #[must_use]
    pub fn with_optional_root(root: Option<PathBuf>) -> Self {
        Self {
            root: root.unwrap_or_else(|| home_dir().unwrap_or_default().join(ROOT_RELATIVE)),
        }
    }

    /// The discovery root this adapter would read.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl SourceAdapter for CodeiumAdapter {
    fn name(&self) -> &str {
        NAME
    }

    /// Nothing — the stub's whole contract.
    ///
    /// Python spells this as a generator whose body is `return` followed by an
    /// unreachable `yield`; the Rust signature makes the empty result the only
    /// thing that can be written. A populated `~/.codeium` yields exactly as
    /// much as an absent one: zero refs, no error.
    fn enumerate(&self) -> Vec<SessionRef> {
        Vec::new()
    }

    /// A no-op. Unreachable in practice — [`enumerate`](Self::enumerate) never
    /// hands out a ref to read — and harmless if some caller synthesises one.
    fn read_into(&self, _session: &SessionRef, _since_offset: i64, _sink: &mut dyn FnMut(Record)) {}

    /// `~/.codeium` — what `backup create` copies (`source_roots`).
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_yields_nothing_from_a_populated_tree() {
        // The point of the test is that *content* changes nothing: a real
        // `~/.codeium` is 449 MB of blobs and still enumerates empty.
        let dir = std::env::temp_dir().join(format!("stax-codeium-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("database/chat")).expect("scratch");
        std::fs::write(dir.join("database/chat/state.pb"), b"\x08\x01\x12\x04blob").expect("blob");
        std::fs::write(dir.join("config.json"), b"{}").expect("config");

        let adapter = CodeiumAdapter::with_root(&dir);
        assert!(adapter.enumerate().is_empty());
        // …and a synthesised ref reads to nothing rather than panicking.
        let ref_ = SessionRef::file(NAME, "-p", "s", dir.join("config.json"), 0.0, 2);
        assert!(adapter.read(&ref_, 0).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_sees_the_root_and_the_watcher_does_not() {
        // The asymmetry is `codeium.py`'s: `source_roots` is declared,
        // `watch_paths` is not, so the default `source_roots -> watch_paths`
        // fallback must NOT be what answers here.
        let adapter = CodeiumAdapter::with_root("/tmp/stax-codeium-root");
        assert_eq!(
            adapter.source_roots(),
            vec![PathBuf::from("/tmp/stax-codeium-root")]
        );
        assert!(adapter.watch_paths().is_empty());
        assert_eq!(adapter.name(), "codeium");
    }

    #[test]
    fn the_default_root_is_the_home_dotfile() {
        let adapter = CodeiumAdapter::new();
        assert!(
            adapter.root().ends_with(ROOT_RELATIVE),
            "{}",
            adapter.root().display()
        );
    }
}
