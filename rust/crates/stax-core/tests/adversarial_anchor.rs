//! Adversarial pins for the anchor sidecar's durability under a fleet.
//!
//! `anchor` exists so 10–20 concurrent agents can write campaign state that
//! survives a context rotation (`rust/ARCHITECT-STATE.md`: "Fan-out 10–20 agents
//! at architect discretion"). Concurrency is therefore the feature's *operating
//! envelope*, not an edge case — and the wave-1 landing tested it single-writer.
//!
//! [`contention`] is the finding: with concurrent writers and a body large
//! enough that a write transaction outlives the next writer's attempt to start
//! one, appends fail with `SQLITE_BUSY` and the state an agent believed it
//! anchored is simply gone. The sidecar opens with rusqlite's defaults —
//! rollback journal, no `PRAGMA journal_mode = WAL`, no `PRAGMA busy_timeout`,
//! and a deferred `BEGIN` — and a deferred transaction that reads before it
//! writes gets `SQLITE_BUSY` *returned immediately*, with the busy handler never
//! consulted, when another connection already holds `RESERVED`. That is the
//! classic SQLite lock-upgrade deadlock-avoidance path; `BEGIN IMMEDIATE` plus
//! WAL plus an explicit `busy_timeout` is the standard closure.
//!
//! Measured out-of-process, which is how agents actually call it (16 processes ×
//! 12 appends of a 256 KB body): **171 of 192 rows landed — 21 appends lost,
//! 10.9%**, each with `stax: appending anchor "big" to …: database is locked:
//! Error code 5`. Nine `anchor get` calls failed the same way, so reads are not
//! isolated from writers either. Reproduce out-of-process with:
//!
//! ```sh
//! A=$(mktemp -d); head -c 262144 /dev/urandom | base64 -w0 > "$A/body.txt"
//! for p in $(seq 1 16); do
//!   ( for i in $(seq 1 12); do
//!       rust/target/release/stax anchor --db "$A/c.db" set big --file "$A/body.txt" \
//!         >/dev/null 2>>"$A/err" || echo lost >>"$A/lost"
//!     done ) &
//! done; wait
//! sqlite3 "$A/c.db" 'select count(*) from anchors'   # expect 192
//! wc -l < "$A/lost"                                  # observed 21
//! ```
//!
//! Run: `cargo test -p stax-core --test adversarial_anchor`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use stax_core::anchor::{AnchorDb, SystemClock};

/// A fresh scratch directory per test — no cleanup, these are tiny.
fn scratch(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("stax-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// One writer per thread, each with its own connection — the same isolation
/// separate processes get, minus the process-spawn cost. Returns
/// `(rows that landed, failure messages)`.
fn hammer(path: &Path, writers: usize, per_writer: usize, body: &str) -> (usize, Vec<String>) {
    // Create up front so every writer races on INSERT, not on schema setup.
    AnchorDb::open_or_create(path).expect("create the sidecar");

    let failures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let landed = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for writer in 0..writers {
            let failures = Arc::clone(&failures);
            let landed = Arc::clone(&landed);
            scope.spawn(move || {
                for attempt in 0..per_writer {
                    let db = match AnchorDb::open_or_create(path) {
                        Ok(db) => db,
                        Err(error) => {
                            failures
                                .lock()
                                .expect("lock")
                                .push(format!("open: {error:#}"));
                            continue;
                        }
                    };
                    match db.append("fleet", body, Some(&format!("w{writer}")), &SystemClock) {
                        Ok(_) => {
                            landed.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => failures
                            .lock()
                            .expect("lock")
                            .push(format!("append w{writer}/{attempt}: {error:#}")),
                    }
                }
            });
        }
    });
    let failures = Arc::try_unwrap(failures)
        .expect("all writers joined")
        .into_inner()
        .expect("lock");
    (landed.load(Ordering::Relaxed), failures)
}

/// Writes that must not be lost when a fleet writes at once.
mod contention {
    use super::{hammer, scratch};

    /// FAILS TODAY once the body is big enough to hold the write lock.
    #[test]
    fn every_append_survives_a_fleet_of_writers() {
        let dir = scratch("anchor-contended-big");
        let body = "x".repeat(256 * 1024);
        let (writers, per_writer) = (16, 12);
        let (landed, failures) = hammer(&dir.join("contended.db"), writers, per_writer, &body);
        let expected = writers * per_writer;
        assert_eq!(
            landed,
            expected,
            "{} of {expected} appends were LOST under contention; first failures:\n  {}",
            expected - landed,
            failures
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }

    /// The same shape with a small body usually passes — which is exactly why
    /// the single-writer landing looked clean. Kept as a control: if this ever
    /// starts failing too, the loss is worse than measured, not better.
    #[test]
    fn small_bodies_are_the_easy_case_that_hid_the_problem() {
        let dir = scratch("anchor-contended-small");
        let (landed, failures) = hammer(&dir.join("contended.db"), 12, 40, "small body");
        assert_eq!(
            landed,
            12 * 40,
            "even small bodies lose appends now; failures:\n  {}",
            failures
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
}

/// Guarantees the landing states, re-checked adversarially. These hold.
mod guarantees {
    use super::{AnchorDb, SystemClock, scratch};

    /// A read must never bring the sidecar into existence — a `SessionStart`
    /// hook that runs `anchor get` in every repo must not litter.
    #[test]
    fn reads_never_create_the_sidecar() {
        let dir = scratch("anchor-noread");
        let path = dir.join("absent.db");
        assert!(
            AnchorDb::open_existing(&path)
                .expect("absent is not an error")
                .is_none()
        );
        assert!(!path.exists(), "open_existing created {}", path.display());
    }

    /// Append-only is enforced by trigger, so `UPDATE`/`DELETE` are refused —
    /// but note for the ledger that `DROP TABLE anchors` is *not*, so the
    /// guarantee is against accident, not against a hostile local writer.
    #[test]
    fn update_and_delete_are_refused_but_drop_table_is_not() {
        let dir = scratch("anchor-append-only");
        let path = dir.join("a.db");
        let db = AnchorDb::open_or_create(&path).expect("create");
        db.append("k", "v", None, &SystemClock).expect("append");
        drop(db);

        let conn = rusqlite::Connection::open(&path).expect("reopen");
        assert!(conn.execute("UPDATE anchors SET body = 'x'", []).is_err());
        assert!(conn.execute("DELETE FROM anchors", []).is_err());
        assert!(
            conn.execute("DROP TABLE anchors", []).is_ok(),
            "if DROP is refused too, tighten this note in the ledger"
        );
    }

    /// An empty or whitespace-only body is refused, so a failed `--file` read
    /// cannot masquerade as anchored state.
    #[test]
    fn empty_bodies_and_keys_are_refused() {
        let dir = scratch("anchor-empty");
        let db = AnchorDb::open_or_create(&dir.join("a.db")).expect("create");
        assert!(db.append("k", "", None, &SystemClock).is_err());
        assert!(db.append("k", "   \n\t ", None, &SystemClock).is_err());
        assert!(db.append("", "body", None, &SystemClock).is_err());
    }
}
