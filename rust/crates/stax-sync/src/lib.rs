//! Multi-device sync: device identity, the shard format, the merge overlay, and
//! the transports.
//!
//! Charter (`docs/specs/rust-port.md` §3): port the Python `sync/` package — the
//! `ObjectStore` abstraction with its s3 and ssh transports, the shard format
//! (unchanged, so sync-hub and issue #100 stay untouched), and the
//! zero-knowledge encryption layer. Encryption gets *simpler* here (§2.5):
//! Python wraps `rage` through `pyrage`, so this crate calls `age` directly and
//! the ciphertext is interoperable by construction rather than by agreement.
//!
//! # Module map (one per reference file)
//!
//! | Reference | Here | What it owns |
//! |---|---|---|
//! | `sync/keys.py` | [`keys`] | identity resolution (env → keychain → `0600` file), fingerprints |
//! | `sync/cipher.py` | [`cipher`] | `age` encrypt/decrypt |
//! | `sync/serialize.py` | [`serialize`] | canonical shard bytes, the content hash, the `(provider, slug)` re-key |
//! | `sync/bucket.py` | [`bucket`] | the four-method `ObjectStore`, URL parsing, scheme dispatch |
//! | `sync/ssh_store.py` | [`ssh_store`] | the ssh transport — **argv construction separated from execution** |
//! | `sync/runner.py` | [`runner`] | identity/outbox tables, `push`, `pull`, `status` |
//! | `sync/merge.py` | [`merge`] | the `local UNION ALL <mart>_remote` overlay |
//! | `infra/egress.py` | [`egress`] | the outbound-body shape guard (RS-7-001) |
//! | `cli.py::_replicate_backup` | [`replicate`] | the rsync-over-ssh argv for `backup create --to` |
//!
//! [`pyvalue`] and [`pyerr`] are this port's own: a SQLite cell that keeps
//! Python's storage classes (because those bytes are hashed), and CPython's
//! exception *messages* (because `pull` interpolates them into warnings that are
//! a byte contract).
//!
//! # Default OFF, and the port keeps it that way
//!
//! With no `sync_identity` row there is no network, no credentials and no work:
//! [`runner::status`] returns before it builds a single shard, and
//! [`merge::merged_overview`] is only reachable through `?scope=all-devices`. A
//! store with sync off behaves exactly as if the feature were absent — which is
//! why the differ's disabled-store section is as large as its enabled one.
//!
//! # What this crate does not run
//!
//! The wave-6 brief forbids live ssh against a remote host, so
//! [`ssh_store::SSHObjectStore`] is built over a [`ssh_store::Transport`]: the
//! real one spawns `ssh`, and [`ssh_store::LocalShellTransport`] runs the *same
//! remote command string* under `sh -c` against a scratch directory. Every argv
//! is also a value ([`ssh_store::RemoteInvocation`],
//! [`replicate::ReplicationPlan`]) so the differ compares the exact bytes that
//! would reach `execve`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bucket;
pub mod cipher;
pub mod egress;
pub mod keys;
pub mod merge;
pub mod pyerr;
pub mod pyvalue;
pub mod replicate;
pub mod runner;
pub mod serialize;
pub mod ssh_store;
