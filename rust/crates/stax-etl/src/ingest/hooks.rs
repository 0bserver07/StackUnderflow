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

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::Connection;

/// The filesystem roots a hook reads, injected.
///
/// Python's Claude hook calls `_claude_home()` *inside* the method, so the two
/// environment inputs (`$CLAUDE_CONFIG_DIR`, `$HOME`) are read at hook time and
/// a caller scopes the whole pass by setting `HOME` — which is exactly what the
/// parity harness does. [`HookEnv::live`] is that read, and it is what
/// [`super::run_ingest`] passes.
///
/// It is a *parameter* rather than a call inside the hook because finding 5 —
/// `set_var` is `unsafe` under Rust 2024 and this workspace forbids `unsafe` —
/// makes pure-function-plus-injection law for the campaign: a test cannot move
/// `$HOME`, so a hook that resolved its own root could only ever be exercised
/// against the developer's real one. That is not hypothetical: the first cut of
/// this file scanned the maintainer's 1.1 GB `~/.claude/projects` on every
/// `cargo test`, and took 5.3 s to do it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEnv {
    /// `_claude_home()` — `$CLAUDE_CONFIG_DIR` or `$HOME/.claude`.
    pub claude_root: PathBuf,
}

impl HookEnv {
    /// Read the live process environment, as the reference does per call.
    #[must_use]
    pub fn live() -> Self {
        Self {
            claude_root: stax_adapters::claude::claude_home(),
        }
    }

    /// Point the Claude hook at an explicit root.
    #[must_use]
    pub fn rooted(claude_root: impl Into<PathBuf>) -> Self {
        Self {
            claude_root: claude_root.into(),
        }
    }
}

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
    fn materialize_metadata(&self, conn: &Connection, env: &HookEnv) -> Result<()>;
}

/// `ClaudeAdapter.materialize_metadata` — agent-team metadata + commit outcomes.
///
/// # The body, and where each half lives
///
/// Python's body is two calls, in this order:
///
/// ```text
/// materialize_team_metadata(conn, claude_root=_claude_home(), provider=self.name)
/// link_commits_to_sessions(conn)
/// ```
///
/// | Python module | lines | TASKS-RS item | ported to |
/// |---|---|---|---|
/// | `adapters/claude_teams.py` | 926 | RS-2-004 | [`crate::ingest::teams`] |
/// | `services/outcome_attribution.py` (the ingest half) | 257 | RS-5-025 | [`crate::ingest::outcomes`] |
///
/// **This is DIV-042, closed.** From wave 4 until now the body was a documented
/// stub and the gate *counted* the gap — 41 of 162 sessions carried team
/// metadata in Python and 0 in the port on the 1 GB corpus, with the four
/// `sessions` columns excluded from the diff and the count printed instead. Both
/// halves are ported, the exclusion is gone, and the columns are compared like
/// every other.
///
/// # `_claude_home()` is read per call, not captured
///
/// Python calls `_claude_home()` inside the method, so `$CLAUDE_CONFIG_DIR` and
/// `$HOME` are read at hook time. That read is [`HookEnv::live`], made by
/// [`super::run_ingest`] on every pass — see [`HookEnv`] for why it is a
/// parameter here and not a call.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeHook;

impl PostIngestHook for ClaudeHook {
    fn provider(&self) -> &'static str {
        "claude"
    }

    fn materialize_metadata(&self, conn: &Connection, env: &HookEnv) -> Result<()> {
        crate::ingest::teams::materialize_team_metadata(conn, &env.claude_root, self.provider())?;
        crate::ingest::outcomes::link_commits_to_sessions(conn)?;
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
pub fn run_all(conn: &Connection, providers: &[&str], env: &HookEnv) -> Vec<String> {
    let mut notes = Vec::new();
    for provider in providers {
        let Some(hook) = for_provider(provider) else {
            continue;
        };
        if let Err(err) = hook.materialize_metadata(conn, env) {
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

    /// A scratch `~/.claude` with one team-shaped transcript in it — enough that
    /// the Claude hook has real work to do and reaches the store.
    fn scratch_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stax-hooks-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("projects/-p/LEAD.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "{\"type\":\"assistant\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":\
             {\"content\":[{\"type\":\"tool_use\",\"name\":\"TeamCreate\",\"input\":\
             {\"team_name\":\"t\",\"description\":\"d\"}}]}}\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_provider_that_is_not_registered_never_runs_its_hook() {
        // An EMPTY in-memory database: the claude hook reaches a `SELECT` on a
        // `sessions` table that does not exist, so a note here would prove it
        // ran. There is none, which proves it did not.
        let home = scratch_home("unregistered");
        let conn = Connection::open_in_memory().unwrap();
        assert!(run_all(&conn, &["codex"], &HookEnv::rooted(&home)).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_claude_hook_runs_through_the_registry_and_reaches_the_store() {
        // The dispatch `run_ingest` depends on, proven end to end but cheaply:
        // an in-memory store with the ingest schema, a three-line scratch home,
        // and the four columns read back.
        let home = scratch_home("dispatch");
        let conn = crate::ingest::testdb::store();
        conn.execute(
            "INSERT INTO projects (provider, slug, path, display_name, first_seen, last_modified) \
             VALUES ('claude', '-p', '/p', 'p', 0.0, 0.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (project_id, session_id, first_ts, last_ts, message_count) \
             VALUES (1, 'LEAD', NULL, NULL, 0)",
            [],
        )
        .unwrap();

        let notes = run_all(&conn, &["claude"], &HookEnv::rooted(&home));
        assert!(notes.is_empty(), "{notes:?}");
        let (team_id, role): (Option<String>, Option<String>) = conn
            .query_row("SELECT team_id, agent_role FROM sessions", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(team_id.as_deref(), Some("t"));
        assert_eq!(role.as_deref(), Some("lead"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_store_the_hook_cannot_write_is_a_note_and_not_a_failed_pass() {
        // The fence, through the registry, on a database with no tables at all.
        //
        // The two halves of the hook body fail DIFFERENTLY here, and both
        // answers are the reference's. `materialize_team_metadata` catches
        // `sqlite3.Error` itself — it rolls back and returns a zeroed report —
        // so it contributes nothing. `link_commits_to_sessions` catches
        // nothing, so its `no such table: sessions` propagates to the fence and
        // becomes exactly one warning line, which is where `run_ingest`'s
        // `try/except Exception` around the hook leaves it.
        let home = scratch_home("fence");
        let conn = Connection::open_in_memory().unwrap();
        let notes = run_all(&conn, &["claude"], &HookEnv::rooted(&home));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("materialize_metadata failed for claude:"),
            "{notes:?}"
        );
        assert!(notes[0].contains("no such table"), "{notes:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_failing_hook_is_a_note_not_a_failed_pass() {
        struct Exploding;
        impl PostIngestHook for Exploding {
            fn provider(&self) -> &'static str {
                "boom"
            }
            fn materialize_metadata(&self, _conn: &Connection, _env: &HookEnv) -> Result<()> {
                anyhow::bail!("no ~/.claude/teams")
            }
        }
        let conn = Connection::open_in_memory().unwrap();
        // Exercised directly: `run_all` reads the static registry, and the point
        // being pinned is the fence, which is the same three lines.
        let hook = Exploding;
        let mut notes = Vec::new();
        if let Err(err) = hook.materialize_metadata(&conn, &HookEnv::live()) {
            notes.push(format!(
                "materialize_metadata failed for {}: {err}",
                hook.provider()
            ));
        }
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("no ~/.claude/teams"), "{notes:?}");
    }

    #[test]
    fn the_live_env_is_the_claude_home_the_adapters_read() {
        assert_eq!(
            HookEnv::live().claude_root,
            stax_adapters::claude::claude_home(),
            "the hook and the adapter must not disagree about where ~/.claude is"
        );
    }
}
