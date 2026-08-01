//! The hook surface — the layer that runs inside a coding agent's hook budget.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `hooks/` package. The
//! budget is the whole point. Python's CLI process floor alone is 159 ms
//! (measured; `rust/PERF.md`), so every hook the reference installs pays that
//! before it reads a single row, and the design is bent around it — bounded
//! sidecar reads, a 250 ms `busy_timeout` that would rather skip than wait, a
//! 1.5 s subprocess deadline for a query that takes milliseconds. Wave 0 measured
//! this port's process floor at **0.94 ms**, 176× lower, and wave 6 measures the
//! whole fire end to end rather than asserting it.
//!
//! ## The nine entry points
//!
//! `hooks/` exposes exactly nine hook ids, in four families. Enumerated here
//! because "the inject hook" is three of nine, and the other six are where the
//! writes and the subprocesses live:
//!
//! | family | id | event | matcher | what it does |
//! |---|---|---|---|---|
//! | capture | `stackunderflow-post-tool-use` | PostToolUse | `Bash` | **writes** a `failure` row |
//! | capture | `stackunderflow-user-prompt` | UserPromptSubmit | — | **writes** a `correction` row |
//! | capture | `stackunderflow-stop` | Stop | — | **writes** a `boundary` row + session totals |
//! | capture | `stackunderflow-pre-compact` | PreCompact | — | **writes** a `snapshot` row |
//! | inject | `stackunderflow-inject-session-start` | SessionStart | — | reads: the project digest |
//! | inject | `stackunderflow-inject-user-prompt` | UserPromptSubmit | — | reads: matching past decisions |
//! | inject | `stackunderflow-inject-pre-tool-use` | PreToolUse | `Edit|Write|MultiEdit` | reads: the file's failure modes |
//! | recall | `stackunderflow-pretool-recall` | PreToolUse | `Edit|Write|Bash` | **spawns** `memory file --json` |
//! | nudge | `stackunderflow-posttool-nudge` | PostToolUse | `Bash` | reads two JSON sidecars |
//!
//! Capture and injection are independently opt-in (`hooks install` /
//! `hooks install --inject`); the proactive governance layer the last two ride
//! on is opt-in again on top of that, and off by default.
//!
//! ## Is the hook path read-only?
//!
//! No, and the answer is per-family. Recorded rather than assumed:
//!
//! * **Injection — read-only here, read-WRITE in the reference (DIV-200).**
//!   `inject._connect` goes through `store.db.connect`, which opens read-write
//!   and issues `PRAGMA journal_mode = WAL`. This port opens
//!   `SQLITE_OPEN_READ_ONLY`. Invisible on stdout.
//! * **Capture — writes, by design, in both (DIV-201).** Ported bug-for-bug,
//!   including the `CREATE TABLE IF NOT EXISTS` self-heal that runs on every
//!   recorded fire.
//! * **Recall — read-only itself, but it spawns a process** that is the Python
//!   CLI and therefore writes whatever that writes (DIV-026's telemetry).
//! * **Nudge / governance — never `store.db`,** by explicit design in the
//!   reference: two small locked JSON files in the app dir, so a hook cannot
//!   contend with the ingest writer. Preserved exactly.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod env;
pub mod handlers;
pub mod inject;
pub mod install;
pub mod jsonerr;
pub mod patterns;
pub mod proactive;
pub mod pystr;
pub mod recall;
pub mod repair;
pub mod templates;

pub use env::HookEnv;
pub use handlers::{Fired, run};
pub use inject::build_injection;
pub use recall::build_recall;
