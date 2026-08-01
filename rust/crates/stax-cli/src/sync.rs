//! `stax sync` — `cli.py`'s `sync` group: `init`, `push`, `pull`, `status`.
//!
//! Opt-in, client-side-encrypted, bring-your-own-destination backup of the
//! analytics aggregates. **Default OFF**: with no `sync_identity` row there is
//! no network, no credentials, and `status` returns before it builds a shard.
//!
//! Every string below is a byte contract. `sync-parity.sh`'s `cli/*` rows run
//! both binaries on the same store copy and `cmp` their stdout, so the two
//! leading spaces, the column alignment of `device:` / `key fingerprint:` and
//! the literal program name inside `"Run: stackunderflow sync init …"` are all
//! compared rather than approximated. That last one is a *literal* in `cli.py`,
//! not `sys.argv[0]`, so the port prints `stackunderflow` too — bug-for-bug,
//! and it will keep saying so until the maintainer renames the string.
//!
//! # The optional-dependency check, and what it means for a compiled binary
//!
//! `_sync_missing_deps` asks whether `pyrage` and (scheme-dependent) `boto3` are
//! importable. A Rust binary has no import step, so the port answers the same
//! question about *compiled-in capabilities*:
//!
//! * `pyrage` → the `age` crate, always present. Never reported missing.
//! * `boto3`  → an S3 client, which this build does not carry (DIV-213). Any
//!   destination `requires_boto3` answers `True` for therefore reports it.
//!
//! On the parity host — where `boto3` is not installed either — the two
//! implementations produce identical output for identical input, including the
//! install hint. That is parity by agreement on the facts, not by coincidence.

use anyhow::Result;
use clap::{Args, Subcommand};
use stax_sync::keys::SecretSources;
use stax_sync::{bucket, keys, runner, ssh_store};

/// `stax sync` — the verb group.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Which sync verb to run.
    #[command(subcommand)]
    pub verb: SyncVerb,
}

/// The four `sync` subcommands, in `cli.py`'s declaration order.
#[derive(Debug, Subcommand)]
pub enum SyncVerb {
    /// Generate this device's encryption key and record the bucket destination.
    Init(SyncInitArgs),
    /// Encrypt and upload changed aggregate shards to your bucket.
    Push,
    /// Fetch and merge every OTHER device's encrypted aggregates from your
    /// bucket.
    Pull(SyncJsonArgs),
    /// Show sync configuration and how many shards are pending upload (local
    /// only).
    Status(SyncJsonArgs),
}

/// `sync init`'s flags.
#[derive(Debug, Args)]
pub struct SyncInitArgs {
    /// Sync destination: s3://my-bucket[/prefix] for any S3-compatible store,
    /// or ssh://[user@]host[:port]/abs/path to sync between machines you own
    /// with no bucket at all
    #[arg(long = "bucket", value_name = "BUCKET_URL")]
    pub bucket_url: String,
    /// Custom object-store endpoint URL (set it for non-default storage
    /// providers)
    #[arg(long = "endpoint", value_name = "ENDPOINT_URL")]
    pub endpoint_url: Option<String>,
    /// Replace an existing sync key on this device (destroys access to data
    /// encrypted under the old key — back it up first)
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

/// The `--json` flag `pull` and `status` share.
#[derive(Debug, Args)]
pub struct SyncJsonArgs {
    /// Emit machine-readable JSON
    #[arg(long = "json", default_value_t = false)]
    pub as_json: bool,
}

/// `_SYNC_INSTALL_HINT`.
const SYNC_INSTALL_HINT: &str = "  This command needs optional dependencies that aren't installed.\n\
                                 \x20 Install them with:  pip install 'stackunderflow[sync]'";

/// `_sync_missing_deps(need_bucket=…, bucket_url=…)`.
///
/// See the module docs for how a compiled binary answers an import question.
/// The reference's shape is preserved exactly, including that a `None`
/// `bucket_url` leaves `need_bucket` at whatever the caller passed — which for
/// `sync init` is `False`, so `init` never asks about the bucket dependency.
fn missing_deps(need_bucket: bool, bucket_url: Option<&str>) -> Vec<&'static str> {
    let mut missing = Vec::new();
    // `pyrage` is the `age` crate here: compiled in, never missing.
    let need_bucket = match (need_bucket, bucket_url) {
        (true, Some(url)) => bucket::requires_boto3(url),
        (flag, _) => flag,
    };
    if need_bucket {
        // DIV-213: no S3 client in this build.
        missing.push("boto3");
    }
    missing
}

/// Exit the way `sys.exit(1)` does: the message is already on stdout, and
/// nothing goes to stderr.
///
/// `stax_cli::run`'s `Err` path prints `stax: {error}` to stderr, which click
/// does not, so a verb that means "exit 1 quietly" cannot return `Err`.
fn exit_1() -> ! {
    use std::io::Write as _;

    let _ = std::io::stdout().flush();
    std::process::exit(1);
}

/// Run `stax sync …`.
///
/// # Errors
/// Only for failures the reference would let propagate as a traceback; every
/// handled failure prints and exits 1.
pub fn run_sync(args: &SyncArgs) -> Result<()> {
    let store_path = stax_core::settings::store_path();
    let state_dir = stax_core::settings::app_dir();
    match &args.verb {
        SyncVerb::Init(init) => run_init(&store_path, &state_dir, init),
        SyncVerb::Push => run_push(&store_path, &state_dir),
        SyncVerb::Pull(flags) => run_pull(&store_path, &state_dir, flags.as_json),
        SyncVerb::Status(flags) => run_status(&store_path, flags.as_json),
    }
}

fn run_status(store_path: &std::path::Path, as_json: bool) -> Result<()> {
    let conn = runner::open_store(store_path)?;
    let state = runner::status(&conn)?;
    drop(conn);

    if as_json {
        // `click.echo(json.dumps(st.as_dict(), indent=2))`.
        println!("{}", stax_memory::pyjson::dumps_pretty(&state.to_json()));
        return Ok(());
    }

    if !state.enabled {
        println!("  Sync: off (no key on this device).");
        println!("  Enable with: stackunderflow sync init --bucket s3://your-bucket");
        return Ok(());
    }

    println!("  Sync: on");
    println!(
        "    device:          {}",
        show(state.device_uuid.as_deref())
    );
    println!(
        "    key fingerprint: {}",
        show(state.fingerprint.as_deref())
    );
    println!("    bucket:          {}", show(state.bucket_url.as_deref()));
    // `if st.endpoint_url:` — falsy, so an empty string is skipped too.
    if let Some(endpoint) = state
        .endpoint_url
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        println!("    endpoint:        {endpoint}");
    }
    println!("    shards (local):  {}", state.shard_count);
    println!("    pending upload:  {}", state.pending.len());
    // `st.last_push_ts or 'never'` — falsy, so an empty string is also `never`.
    println!(
        "    last push:       {}",
        state
            .last_push_ts
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("never")
    );
    Ok(())
}

/// `f"{value}"` where `value` may be `None` — CPython prints `None`.
fn show(value: Option<&str>) -> &str {
    value.unwrap_or("None")
}

const NOT_CONFIGURED: &str =
    "  Sync is not configured. Run: stackunderflow sync init --bucket s3://your-bucket";

fn run_push(store_path: &std::path::Path, state_dir: &std::path::Path) -> Result<()> {
    let conn = runner::open_store(store_path)?;
    if !runner::is_enabled(&conn)? {
        println!("{NOT_CONFIGURED}");
        exit_1();
    }
    // The dependency check happens HERE, not before opening the store, because
    // whether the bucket dependency is needed depends on the configured scheme.
    let identity = runner::load_identity(&conn)?;
    if !missing_deps(true, identity.as_ref().map(|id| id.bucket_url.as_str())).is_empty() {
        println!("{SYNC_INSTALL_HINT}");
        exit_1();
    }
    let sources = SecretSources::from_process();
    let result = match runner::run_push(&conn, state_dir, &sources, None) {
        Ok(result) => result,
        Err(err) => {
            println!("  sync push failed: {err}");
            exit_1();
        }
    };
    drop(conn);

    if result.uploaded == 0 {
        println!(
            "  Up to date — {} shard(s) unchanged, nothing to upload.",
            result.skipped
        );
    } else {
        // `mb = result.bytes_uploaded / (1 << 20)` then `f"{mb:.2f}"` — CPython
        // formats with round-half-to-even, which is Rust's `{:.2}` too.
        let mb = result.bytes_uploaded as f64 / f64::from(1_u32 << 20);
        println!(
            "  Pushed {} shard(s) ({mb:.2} MB); {} unchanged.",
            result.uploaded, result.skipped
        );
        println!("  Generation {}. Manifest committed.", result.generation);
    }
    Ok(())
}

fn run_pull(
    store_path: &std::path::Path,
    state_dir: &std::path::Path,
    as_json: bool,
) -> Result<()> {
    let conn = runner::open_store(store_path)?;
    if !runner::is_enabled(&conn)? {
        println!("{NOT_CONFIGURED}");
        exit_1();
    }
    let identity = runner::load_identity(&conn)?;
    if !missing_deps(true, identity.as_ref().map(|id| id.bucket_url.as_str())).is_empty() {
        println!("{SYNC_INSTALL_HINT}");
        exit_1();
    }
    let sources = SecretSources::from_process();
    let result = match runner::run_pull(&conn, state_dir, &sources, None) {
        Ok(result) => result,
        Err(err) => {
            println!("  sync pull failed: {err}");
            exit_1();
        }
    };
    drop(conn);

    if as_json {
        println!("{}", stax_memory::pyjson::dumps_pretty(&result.to_json()));
        return Ok(());
    }

    if result.devices_seen == 0 {
        println!("  No other devices found in the bucket yet.");
    } else if result.shards_ingested == 0 {
        println!(
            "  Up to date — {} peer(s), nothing new to pull.",
            result.devices_seen
        );
    } else {
        println!(
            "  Pulled {} shard(s) from {} peer(s); {} unchanged.",
            result.shards_ingested, result.devices_seen, result.skipped
        );
        println!("  Merged view: /api/sync/overview?scope=all-devices");
    }
    for warning in &result.warnings {
        println!("  warning: {warning}");
    }
    Ok(())
}

fn run_init(
    store_path: &std::path::Path,
    state_dir: &std::path::Path,
    args: &SyncInitArgs,
) -> Result<()> {
    if !missing_deps(false, None).is_empty() {
        println!("{SYNC_INSTALL_HINT}");
        exit_1();
    }

    // Validate the destination HERE so a typo fails at `init` rather than at
    // the first `push`, by which point a key has been generated and shown.
    let scheme = bucket::scheme_of(&args.bucket_url);
    if !bucket::SUPPORTED_SCHEMES.contains(&scheme.as_str()) {
        println!("  Unsupported sync destination: {}", args.bucket_url);
        println!("  Expected s3://bucket[/prefix] or ssh://[user@]host[:port]/abs/path");
        exit_1();
    }
    if scheme == "ssh"
        && let Err(message) = ssh_store::parse_ssh_url(&args.bucket_url)
    {
        println!("  Invalid ssh destination: {message}");
        exit_1();
    }

    let conn = runner::open_store(store_path)?;
    let existing = runner::load_identity(&conn)?;
    if let Some(existing) = &existing
        && !args.force
    {
        println!("  Sync is already configured on this device.");
        println!("    device:          {}", existing.device_uuid);
        println!("    key fingerprint: {}", existing.key_fingerprint);
        println!("  Re-running will NOT change the key. To replace it, back up the");
        println!("  current key first, then re-run with --force (this destroys access");
        println!("  to any data already encrypted under the old key).");
        exit_1();
    }

    let identity = keys::generate_identity();
    keys::store_secret_file(&identity.secret, state_dir)?;
    let device_uuid = existing.map_or_else(runner::new_device_uuid, |row| row.device_uuid);
    runner::write_identity(
        &conn,
        &runner::Identity {
            device_uuid: device_uuid.clone(),
            key_fingerprint: identity.fingerprint.clone(),
            bucket_url: args.bucket_url.clone(),
            endpoint_url: args.endpoint_url.clone(),
            layout_version: 1,
            created_at: runner::utcnow_iso(),
        },
    )?;
    print_init_banner(&identity, &device_uuid, &args.bucket_url, state_dir);
    drop(conn);
    Ok(())
}

/// `_print_sync_init_banner` — the loud, unmissable key-loss warning.
///
/// Shown once, and the only place the secret is ever printed. Non-differable by
/// construction (the key is random), which is exactly why every *other* `init`
/// path has a corpus row.
fn print_init_banner(
    identity: &keys::AgeIdentity,
    device_uuid: &str,
    bucket_url: &str,
    state_dir: &std::path::Path,
) {
    let line = format!("  {}", "─".repeat(64));
    println!("{line}");
    println!("  SYNC ENCRYPTION KEY — READ THIS BEFORE YOU CONTINUE");
    println!("{line}");
    println!(
        "  This device just generated a private encryption key. Everything\n  \
         pushed to your bucket is encrypted with it, and NOTHING can decrypt\n  \
         it without this key — not the bucket host, not this project, no one.\n"
    );
    println!(
        "  IF YOU LOSE THIS KEY, THE OFF-SITE COPY IS UNRECOVERABLE CIPHERTEXT.\n  \
         There is no reset, no recovery, no backdoor. That is the point.\n"
    );
    println!(
        "  Save the key below in your password manager NOW. To read this data on\n  \
         another device, copy the SAME key there — devices must share one key.\n"
    );
    println!("  Key (store securely — shown once):");
    println!("    {}", identity.secret);
    println!();
    println!("  Fingerprint: {}", identity.fingerprint);
    println!("  Device:      {device_uuid}");
    println!("  Bucket:      {bucket_url}");
    println!(
        "  Key file:    {}  (mode 0600)",
        state_dir.join("sync-identity").display()
    );
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_hint_is_two_lines_with_the_references_indentation() {
        assert_eq!(
            SYNC_INSTALL_HINT,
            "  This command needs optional dependencies that aren't installed.\n  \
             Install them with:  pip install 'stackunderflow[sync]'"
        );
    }

    #[test]
    fn init_never_asks_about_the_bucket_dependency() {
        // `_sync_missing_deps(need_bucket=False)` — `init` validates the URL
        // itself and does not need a client to do it.
        assert!(missing_deps(false, None).is_empty());
        assert!(missing_deps(false, Some("s3://b")).is_empty());
    }

    #[test]
    fn ssh_destinations_need_no_bucket_client_and_s3_ones_do() {
        assert!(missing_deps(true, Some("ssh://host/srv")).is_empty());
        assert_eq!(missing_deps(true, Some("s3://bucket")), vec!["boto3"]);
        // An unknown scheme reports the missing package rather than a bare
        // import error — the reference's deliberate ordering.
        assert_eq!(missing_deps(true, Some("gs://bucket")), vec!["boto3"]);
        // A store with no identity row passes `None`, which leaves the caller's
        // flag alone: `push` asks with `True`, so it reports.
        assert_eq!(missing_deps(true, None), vec!["boto3"]);
    }

    #[test]
    fn a_none_field_prints_pythons_none() {
        assert_eq!(show(None), "None");
        assert_eq!(show(Some("x")), "x");
    }
}
