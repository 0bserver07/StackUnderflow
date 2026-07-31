//! `PostIngestHook` — the port of Python's
//! `getattr(adapter, "materialize_metadata", None)`.
//!
//! # The architect's binding decision
//!
//! ARCHITECT-STATE, "ARCHITECT DECISIONS (E's questions, binding)", item 1:
//! *materialize_metadata becomes a PostIngestHook trait owned by
//! stax-etl/stax-core — adapters stay storage-free.*
//!
//! That is what this module is. Python discovers the capability with `getattr`
//! on the adapter instance, so the hook lives *on* the adapter and an adapter
//! that has one necessarily knows about `sqlite3`, the store schema, and (for
//! Claude) two service modules. Rust has no `getattr`; expressing the same shape
//! as a default trait method on [`stax_adapters::base::SourceAdapter`] would
//! have dragged `Connection` into the adapter contract and made every one of the
//! twenty providers a storage-aware type to serve the one that uses it.
//!
//! Inverting it costs one lookup — [`for_provider`] keyed on the provider name
//! rather than a method on the instance — and buys back the layering: the
//! adapters crate still depends on nothing but `anyhow` + `rusqlite`, and the
//! hook that needs the store lives in the crate that owns the store writes.
//!
//! # The fence is the contract
//!
//! `run_ingest` wraps each call in `try/except Exception` and logs a warning:
//! *"a metadata hook must never break ingest"*. Ported literally — [`run_all`]
//! turns an `Err` into a note and keeps going, so a hook that fails costs its
//! own metadata and nothing else.

use anyhow::Result;
use rusqlite::Connection;

/// One adapter's post-ingest materialisation step.
///
/// Called once per ingest pass, **after** every file has been written and
/// committed — not per file. Implementations must be idempotent and cheap on a
/// machine with nothing to materialise: Python's Claude hook is documented as "a
/// machine with no agent-teams activity is a no-op".
pub trait PostIngestHook: Send + Sync {
    /// The provider whose adapter owns this hook (`adapter.name`).
    fn provider(&self) -> &'static str;

    /// Materialise this provider's out-of-band metadata into the store.
    ///
    /// # Errors
    /// Anything. [`run_all`] fences every call, so an error costs this hook's
    /// metadata and never the ingest pass.
    fn materialize_metadata(&self, conn: &Connection) -> Result<()>;
}

/// `ClaudeAdapter.materialize_metadata` — agent-team metadata + commit outcomes.
///
/// # STUB, and exactly why
///
/// Python's body is two calls:
///
/// ```text
/// materialize_team_metadata(conn, claude_root=_claude_home(), provider=self.name)
/// link_commits_to_sessions(conn)
/// ```
///
/// Both targets are **unported items in other waves**, and neither is wave 4's
/// to write:
///
/// | Python module | lines | TASKS-RS item | wave |
/// |---|---|---|---|
/// | `adapters/claude_teams.py` | 926 | RS-2-004 (open) | 2 |
/// | `services/outcome_attribution.py` | 257 | RS-5-025 (open) | 5 |
///
/// What wave 4 owes is the *seam*, and the seam is complete: the trait, the
/// registry, the fenced dispatch in [`run_all`], and the wiring in
/// `run_ingest` that calls it after the sweep. When RS-2-004 and RS-5-025 land,
/// [`ClaudeHook::materialize_metadata`] becomes the two calls above and nothing
/// else in the ingest layer moves.
///
/// The stub returns `Ok(())` rather than an error on purpose. Python's hook on a
/// machine with no `~/.claude/teams` is *also* a no-op that returns `None`, so
/// "did the ingest pass succeed" answers the same on both sides today; an error
/// here would make the pass report a failure Python does not report, which is a
/// louder lie than a documented no-op.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeHook;

impl PostIngestHook for ClaudeHook {
    fn provider(&self) -> &'static str {
        "claude"
    }

    fn materialize_metadata(&self, _conn: &Connection) -> Result<()> {
        // WAVE 6/5 SEAM — see the type docs. `materialize_team_metadata`
        // (RS-2-004, 926 ln) and `link_commits_to_sessions` (RS-5-025, 257 ln)
        // go here, in that order.
        Ok(())
    }
}

/// Every hook this build carries.
///
/// One entry, because Python has one: `materialize_metadata` is defined on
/// `ClaudeAdapter` and on no other adapter (`grep -rn 'def
/// materialize_metadata' stackunderflow/adapters/` → one hit).
static HOOKS: [&dyn PostIngestHook; 1] = [&ClaudeHook];

/// The hook registered for `provider`, or `None` — `getattr(adapter,
/// "materialize_metadata", None)`.
#[must_use]
pub fn for_provider(provider: &str) -> Option<&'static dyn PostIngestHook> {
    HOOKS
        .iter()
        .find(|hook| hook.provider() == provider)
        .copied()
}

/// Run every hook whose provider appears in `providers`, fencing each one.
///
/// `providers` is the adapter list `run_ingest` was handed, in registry order —
/// Python iterates the same list and `getattr`s each element, so a provider that
/// is not registered on this machine never has its hook run even if the code for
/// it is compiled in.
///
/// Returns the `_logger.warning` lines Python would have emitted.
pub fn run_all(conn: &Connection, providers: &[&str]) -> Vec<String> {
    let mut notes = Vec::new();
    for provider in providers {
        let Some(hook) = for_provider(provider) else {
            continue;
        };
        if let Err(err) = hook.materialize_metadata(conn) {
            notes.push(format!("materialize_metadata failed for {provider}: {err}"));
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_is_the_only_provider_with_a_hook() {
        assert!(for_provider("claude").is_some());
        for absent in ["codex", "cursor", "gemini", "antigravity", ""] {
            assert!(for_provider(absent).is_none(), "{absent} grew a hook");
        }
    }

    #[test]
    fn a_provider_that_is_not_registered_never_runs_its_hook() {
        let conn = Connection::open_in_memory().unwrap();
        // "claude" compiled in, but not in the pass's adapter list.
        assert!(run_all(&conn, &["codex"]).is_empty());
        assert!(run_all(&conn, &["claude"]).is_empty(), "the stub succeeds");
    }

    #[test]
    fn a_failing_hook_is_a_note_not_a_failed_pass() {
        struct Exploding;
        impl PostIngestHook for Exploding {
            fn provider(&self) -> &'static str {
                "boom"
            }
            fn materialize_metadata(&self, _conn: &Connection) -> Result<()> {
                anyhow::bail!("no ~/.claude/teams")
            }
        }
        let conn = Connection::open_in_memory().unwrap();
        // Exercised directly: `run_all` reads the static registry, and the point
        // being pinned is the fence, which is the same three lines.
        let hook = Exploding;
        let mut notes = Vec::new();
        if let Err(err) = hook.materialize_metadata(&conn) {
            notes.push(format!(
                "materialize_metadata failed for {}: {err}",
                hook.provider()
            ));
        }
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("no ~/.claude/teams"), "{notes:?}");
    }
}
