//! The Rust half of the wave-6 sync differ.
//!
//! One process per case. `argv[1]` is the op, the rest are its arguments, and
//! the answer goes to stdout as ONE line of
//! [`stax_memory::pyjson::dumps_compact`] — which is `json.dumps(obj,
//! separators=(",", ":"))`. `rust/parity/sync_parity.py` is the other half:
//! same ops, same arguments, same writer, so `diff` over the two lines is the
//! whole comparison.
//!
//! Nothing here reaches the network. `ssh_store` is exercised as **argv**: the
//! exact list that would reach `execve`, compared string for string against
//! what CPython's `subprocess.run` would have been handed. The
//! `ssh_store::LocalShellTransport` round trip lives in the crate's unit tests,
//! where a scratch directory can stand in for a host.
//!
//! Two pieces of scaffolding are the driver's, not the product's, and both are
//! mirrored line-for-line in the Python half:
//!
//! * `FileObjectStore` — a directory-backed store, so a `push` case and a
//!   `pull` case can share one bucket across two processes.
//! * `identity_encryptor` — `lambda b: b`. age ciphertext is randomised per
//!   blob and can never be byte-compared; the PLAINTEXT is what push
//!   idempotency hashes, and that is what these cases compare. Real age is
//!   proven by the `crypto/*` interop rows, which round-trip a blob through the
//!   *other* implementation.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusqlite::Connection;
use serde_json::{Map, Value};
use stax_memory::pyjson::dumps_compact;
use stax_sync::bucket::{ObjectNotFound, ObjectStore, StoreError};
use stax_sync::pyvalue::PyValue;
use stax_sync::{egress, keys, merge, replicate, runner, serialize, ssh_store};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let Some(op) = argv.get(1) else {
        eprintln!("usage: stax-sync-parity <op> [args…]");
        return ExitCode::from(2);
    };
    let args: Vec<&str> = argv[2..].iter().map(String::as_str).collect();
    match dispatch(op, &args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("stax-sync-parity: {message}");
            ExitCode::from(2)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch(op: &str, args: &[&str]) -> Result<(), String> {
    match op {
        "keys-fingerprint" => keys_fingerprint(arg(args, 0)?),
        "keys-identity-path" => keys_identity_path(arg(args, 0)?),
        "keys-resolve" => keys_resolve(arg(args, 0)?, arg(args, 1)?, arg(args, 2)?, arg(args, 3)?),
        "keys-store-file" => keys_store_file(arg(args, 0)?, arg(args, 1)?),
        "url-bucket" => url_bucket(arg(args, 0)?),
        "url-scheme" => url_scheme(arg(args, 0)?),
        "url-store-from" => url_store_from(arg(args, 0)?, arg(args, 1)?),
        "ssh-parse" => ssh_parse(arg(args, 0)?),
        "ssh-put" => ssh_invocation(arg(args, 0)?, &Op::Put(arg(args, 1)?)),
        "ssh-get" => ssh_invocation(arg(args, 0)?, &Op::Get(arg(args, 1)?)),
        "ssh-list" => ssh_invocation(arg(args, 0)?, &Op::List),
        "ssh-delete" => ssh_invocation(arg(args, 0)?, &Op::Delete(arg(args, 1)?)),
        "ssh-find" => ssh_find(arg(args, 0)?, arg(args, 1)?, arg(args, 2)?),
        "shlex-quote" => shlex_quote_op(arg(args, 0)?),
        "object-keys" => object_keys(arg(args, 0)?, arg(args, 1)?),
        "rsync-plan" => rsync_plan(arg(args, 0)?, arg(args, 1)?, arg(args, 2)?, arg(args, 3)?),
        "rsync-outcome" => rsync_outcome(arg(args, 0)?, arg(args, 1)?, arg(args, 2)?),
        "shards" => shards(arg(args, 0)?),
        "shard-roundtrip" => shard_roundtrip(arg(args, 0)?),
        "month-of" => month_of(arg(args, 0)?),
        "json-loads" => json_loads(arg(args, 0)?),
        "py-int" => py_int_op(arg(args, 0)?),
        "push" => push_op(
            arg(args, 0)?,
            arg(args, 1)?,
            arg(args, 2)?,
            arg(args, 3)?,
            arg(args, 4)?,
        ),
        "pull" => pull_op(
            arg(args, 0)?,
            arg(args, 1)?,
            arg(args, 2)?,
            arg(args, 3)?,
            arg(args, 4)?,
            arg(args, 5)?,
            arg(args, 6)?,
        ),
        "status" => status_op(arg(args, 0)?),
        "merge-overview" => merge_overview(arg(args, 0)?),
        "merge-parts" => merge_parts(arg(args, 0)?),
        "egress-guard" => egress_guard(arg(args, 0)?, arg(args, 1)?, arg(args, 2)?),
        "egress-serialize" => egress_serialize(arg(args, 0)?),
        "egress-scan" => egress_scan(arg(args, 0)?, arg(args, 1)?),
        "cipher-encrypt" => cipher_encrypt(arg(args, 0)?, arg(args, 1)?),
        "cipher-decrypt" => cipher_decrypt(arg(args, 0)?, arg(args, 1)?),
        "cipher-genkey" => cipher_genkey(),
        "cipher-recipient" => cipher_recipient(arg(args, 0)?),
        other => Err(format!("unknown op '{other}'")),
    }
}

fn arg<'a>(args: &[&'a str], index: usize) -> Result<&'a str, String> {
    args.get(index)
        .copied()
        .ok_or_else(|| format!("missing argument {index}"))
}

/// `emit` — one compact line, exactly what the Python half writes.
fn emit(value: &Value) -> Result<(), String> {
    println!("{}", dumps_compact(value));
    Ok(())
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.to_owned(), value);
    }
    Value::Object(map)
}

/// The corpus spells "absent" as a bare `-`; argv cannot carry `None`.
fn dash(value: &str) -> Option<&str> {
    (value != "-").then_some(value)
}

// ── base64 (RFC 4648, standard alphabet) ─────────────────────────────────────
//
// Hand-rolled rather than a dependency: it is 30 lines, the crate already
// carries a 115-entry lock addition, and a differ that needs a new dependency
// to read its own corpus has more surface than the thing it measures.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len().div_ceil(3) * 4);
    for chunk in raw.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let packed = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        out.push(B64[((packed >> 18) & 63) as usize] as char);
        out.push(B64[((packed >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((packed >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(packed & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut bits = 0_u32;
    let mut count = 0_u32;
    let mut out = Vec::new();
    for ch in text
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '=')
    {
        let value = B64
            .iter()
            .position(|c| *c as char == ch)
            .ok_or_else(|| format!("bad base64 character {ch:?}"))?;
        bits = (bits << 6) | u32::try_from(value).unwrap_or_default();
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(u8::try_from((bits >> count) & 0xff).unwrap_or_default());
        }
    }
    Ok(out)
}

fn sha_hex(raw: &[u8]) -> String {
    serialize::hex_digest(raw)
}

// ── scaffolding ──────────────────────────────────────────────────────────────

/// A directory-backed `ObjectStore` so two processes can share one bucket.
struct FileObjectStore {
    root: PathBuf,
    put_calls: u64,
    get_calls: u64,
    list_calls: u64,
    delete_calls: u64,
}

impl FileObjectStore {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            put_calls: 0,
            get_calls: 0,
            list_calls: 0,
            delete_calls: 0,
        }
    }

    fn counters(&self) -> Value {
        obj(vec![
            ("put", Value::from(self.put_calls)),
            ("get", Value::from(self.get_calls)),
            ("list", Value::from(self.list_calls)),
            ("delete", Value::from(self.delete_calls)),
        ])
    }
}

impl ObjectStore for FileObjectStore {
    fn put(&mut self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        self.put_calls += 1;
        let path = self.root.join(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Transport(e.to_string()))?;
        }
        std::fs::write(&path, data).map_err(|e| StoreError::Transport(e.to_string()))
    }

    fn get(&mut self, key: &str) -> Result<Vec<u8>, StoreError> {
        self.get_calls += 1;
        let path = self.root.join(key);
        if !path.is_file() {
            return Err(StoreError::NotFound(ObjectNotFound(key.to_owned())));
        }
        std::fs::read(&path).map_err(|e| StoreError::Transport(e.to_string()))
    }

    fn list(&mut self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.list_calls += 1;
        let mut keys: Vec<String> = walk(&self.root)
            .into_iter()
            .filter(|key| key.starts_with(prefix))
            .collect();
        keys.sort_unstable();
        Ok(keys)
    }

    fn delete(&mut self, key: &str) -> Result<(), StoreError> {
        self.delete_calls += 1;
        let path = self.root.join(key);
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|e| StoreError::Transport(e.to_string()))?;
        }
        Ok(())
    }
}

/// Every file under `root`, as `/`-joined relative keys.
fn walk(root: &Path) -> Vec<String> {
    fn inner(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                inner(base, &path, out);
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    if root.is_dir() {
        inner(root, root, &mut out);
    }
    out
}

fn identity_encryptor(payload: &[u8]) -> Result<Vec<u8>, String> {
    Ok(payload.to_vec())
}

fn connect(path: &str) -> Result<Connection, String> {
    Connection::open(path).map_err(|err| err.to_string())
}

// ── store dumps ──────────────────────────────────────────────────────────────

fn dump_table(conn: &Connection, table: &str, order: &str) -> Result<Value, String> {
    let sql = format!("SELECT * FROM {table} ORDER BY {order}");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut map = Map::new();
        for (index, name) in names.iter().enumerate() {
            let cell = PyValue::from_sqlite(row.get_ref(index).map_err(|e| e.to_string())?);
            map.insert(name.clone(), cell.to_json());
        }
        out.push(Value::Object(map));
    }
    Ok(Value::Array(out))
}

fn dump_bucket(root: &Path) -> Value {
    let mut keys = walk(root);
    keys.sort_unstable();
    let mut map = Map::new();
    for key in keys {
        let body = std::fs::read(root.join(&key)).unwrap_or_default();
        map.insert(
            key,
            obj(vec![
                ("len", Value::from(body.len())),
                ("sha256", Value::from(sha_hex(&body))),
            ]),
        );
    }
    Value::Object(map)
}

fn decode_manifest(root: &Path, device: &str) -> Value {
    let path = root.join(runner::manifest_key(device, runner::DEFAULT_PREFIX));
    let Ok(body) = std::fs::read(&path) else {
        return Value::Null;
    };
    stax_sync::pyerr::loads(&body)
        .unwrap_or_else(|err| obj(vec![("_undecodable", Value::from(err.to_string()))]))
}

fn dump_remote_tables(conn: &Connection) -> Result<Value, String> {
    Ok(obj(vec![
        (
            "daily_mart_remote",
            dump_table(
                conn,
                "daily_mart_remote",
                "device_uuid, day, provider, slug, model, speed",
            )?,
        ),
        (
            "provider_day_mart_remote",
            dump_table(
                conn,
                "provider_day_mart_remote",
                "device_uuid, day, provider",
            )?,
        ),
        (
            "model_day_mart_remote",
            dump_table(
                conn,
                "model_day_mart_remote",
                "device_uuid, day, model, speed",
            )?,
        ),
        (
            "project_mart_remote",
            dump_table(conn, "project_mart_remote", "device_uuid, provider, slug")?,
        ),
        (
            "session_mart_remote",
            dump_table(conn, "session_mart_remote", "device_uuid, session_id")?,
        ),
    ]))
}

// ── ops ──────────────────────────────────────────────────────────────────────

fn keys_fingerprint(recipient: &str) -> Result<(), String> {
    emit(&obj(vec![(
        "fingerprint",
        Value::from(keys::fingerprint(recipient)),
    )]))
}

fn keys_identity_path(state_dir: &str) -> Result<(), String> {
    emit(&obj(vec![(
        "path",
        Value::from(
            keys::identity_path(Path::new(state_dir))
                .to_string_lossy()
                .into_owned(),
        ),
    )]))
}

fn keys_resolve(
    state_dir: &str,
    env_value: &str,
    keychain: &str,
    file_value: &str,
) -> Result<(), String> {
    let dir = Path::new(state_dir);
    let mut env = BTreeMap::new();
    if let Some(value) = dash(env_value) {
        env.insert(keys::ENV_KEY.to_owned(), value.to_owned());
    }
    let chain = dash(keychain).map(str::to_owned);
    let reader = move || chain.clone();
    if let Some(value) = dash(file_value) {
        keys::store_secret_file(value, dir).map_err(|e| e.to_string())?;
    }
    let sources = keys::SecretSources {
        env,
        keychain_reader: &reader,
    };
    emit(&obj(vec![(
        "secret",
        keys::resolve_secret(dir, &sources).map_or(Value::Null, Value::from),
    )]))
}

fn keys_store_file(state_dir: &str, secret: &str) -> Result<(), String> {
    let path = keys::store_secret_file(secret, Path::new(state_dir)).map_err(|e| e.to_string())?;
    let mode = file_mode(&path);
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    emit(&obj(vec![
        (
            "name",
            Value::from(
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        ),
        ("mode", Value::from(mode)),
        ("content", Value::from(content)),
    ]))
}

#[cfg(unix)]
fn file_mode(path: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;

    let bits = std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or_default();
    // `oct(0o600)` is the string `'0o600'`.
    format!("0o{bits:o}")
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> String {
    "0o600".to_owned()
}

fn url_bucket(url: &str) -> Result<(), String> {
    match stax_sync::bucket::parse_bucket_url(url) {
        Ok((name, prefix)) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("bucket", Value::from(name)),
            ("prefix", Value::from(prefix)),
        ])),
        Err(message) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(message)),
        ])),
    }
}

fn url_scheme(url: &str) -> Result<(), String> {
    emit(&obj(vec![
        ("scheme", Value::from(stax_sync::bucket::scheme_of(url))),
        (
            "requires_boto3",
            Value::Bool(stax_sync::bucket::requires_boto3(url)),
        ),
        (
            "supported",
            Value::Array(
                stax_sync::bucket::SUPPORTED_SCHEMES
                    .iter()
                    .map(|scheme| Value::from(*scheme))
                    .collect(),
            ),
        ),
    ]))
}

fn url_store_from(url: &str, endpoint: &str) -> Result<(), String> {
    match stax_sync::bucket::store_from_url(url, dash(endpoint)) {
        Ok(stax_sync::bucket::Destination::Ssh(target)) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("kind", Value::from("ssh")),
            ("host", Value::from(target.host)),
            ("root", Value::from(target.root)),
            ("port", target.port.map_or(Value::Null, Value::from)),
        ])),
        Ok(stax_sync::bucket::Destination::S3 {
            bucket,
            key_prefix,
            endpoint_url,
        }) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("kind", Value::from("s3")),
            ("bucket", Value::from(bucket)),
            ("prefix", Value::from(key_prefix)),
            (
                "endpoint_url",
                endpoint_url.map_or(Value::Null, Value::from),
            ),
        ])),
        Err(message) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(message)),
        ])),
    }
}

fn ssh_parse(url: &str) -> Result<(), String> {
    match ssh_store::parse_ssh_url(url) {
        Ok(target) => {
            let argv = target.ssh_argv();
            emit(&obj(vec![
                ("ok", Value::Bool(true)),
                ("host", Value::from(target.host)),
                ("root", Value::from(target.root)),
                ("port", target.port.map_or(Value::Null, Value::from)),
                (
                    "argv",
                    Value::Array(argv.into_iter().map(Value::from).collect()),
                ),
            ]))
        }
        Err(message) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(message)),
        ])),
    }
}

enum Op<'a> {
    Put(&'a str),
    Get(&'a str),
    List,
    Delete(&'a str),
}

fn ssh_invocation(url: &str, op: &Op<'_>) -> Result<(), String> {
    let target = match ssh_store::parse_ssh_url(url) {
        Ok(target) => target,
        Err(message) => {
            return emit(&obj(vec![
                ("ok", Value::Bool(false)),
                ("error", Value::from(message)),
            ]));
        }
    };
    let store = ssh_store::SSHObjectStore::with_transport(
        target,
        ssh_store::DEFAULT_TIMEOUT,
        ssh_store::SshTransport,
    );
    let built = match op {
        Op::Put(key) => store.put_invocation(key),
        Op::Get(key) => store.get_invocation(key),
        Op::List => Ok(store.list_invocation()),
        Op::Delete(key) => store.delete_invocation(key),
    };
    match built {
        Ok(invocation) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            (
                "argv",
                Value::Array(invocation.argv.into_iter().map(Value::from).collect()),
            ),
            ("stdin", Value::Bool(invocation.stdin)),
        ])),
        Err(message) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(message)),
        ])),
    }
}

fn ssh_find(url: &str, prefix: &str, stdout_b64: &str) -> Result<(), String> {
    let target = ssh_store::parse_ssh_url(url)?;
    let store = ssh_store::SSHObjectStore::with_transport(
        target,
        ssh_store::DEFAULT_TIMEOUT,
        ssh_store::SshTransport,
    );
    let stdout = b64_decode(stdout_b64)?;
    emit(&obj(vec![(
        "keys",
        Value::Array(
            store
                .parse_find_output(&stdout, prefix)
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    )]))
}

fn shlex_quote_op(text_b64: &str) -> Result<(), String> {
    let raw = b64_decode(text_b64)?;
    let text = String::from_utf8(raw).map_err(|err| err.to_string())?;
    emit(&obj(vec![(
        "quoted",
        Value::from(ssh_store::shlex_quote(&text)),
    )]))
}

fn object_keys(device: &str, shard_key: &str) -> Result<(), String> {
    emit(&obj(vec![
        (
            "object_key",
            Value::from(runner::object_key(
                device,
                shard_key,
                runner::DEFAULT_PREFIX,
            )),
        ),
        (
            "manifest_key",
            Value::from(runner::manifest_key(device, runner::DEFAULT_PREFIX)),
        ),
        ("prefix", Value::from(runner::DEFAULT_PREFIX)),
        ("schema", Value::from(runner::MANIFEST_SCHEMA)),
    ]))
}

fn rsync_plan(
    dest_name: &str,
    dest_path: &str,
    to_url: &str,
    previous: &str,
) -> Result<(), String> {
    match replicate::plan(dest_name, dest_path, to_url, dash(previous)) {
        Ok(plan) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("remote_dir", Value::from(plan.remote_dir)),
            (
                "mkdir_argv",
                Value::Array(plan.mkdir_argv.into_iter().map(Value::from).collect()),
            ),
            (
                "rsync_argv",
                Value::Array(plan.rsync_argv.into_iter().map(Value::from).collect()),
            ),
            ("ssh_cmd", Value::from(plan.ssh_cmd)),
        ])),
        Err(message) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(message)),
        ])),
    }
}

fn rsync_outcome(returncode: &str, stderr_b64: &str, what: &str) -> Result<(), String> {
    let code: i32 = returncode
        .parse()
        .map_err(|_| "bad returncode".to_owned())?;
    let stderr = String::from_utf8(b64_decode(stderr_b64)?).map_err(|err| err.to_string())?;
    let (ok, message) = replicate::rsync_outcome(code, &stderr, what);
    emit(&obj(vec![
        ("ok", Value::Bool(ok)),
        ("message", Value::from(message)),
        (
            "reported",
            Value::from(replicate::rsync_reported(&stderr, 6)),
        ),
    ]))
}

fn shards(store_path: &str) -> Result<(), String> {
    let conn = connect(store_path)?;
    let built = serialize::build_shards(&conn).map_err(|err| err.to_string())?;
    let rows: Vec<Value> = built
        .iter()
        .map(|shard| {
            let bytes = shard.to_bytes();
            obj(vec![
                ("shard_key", Value::from(shard.shard_key())),
                ("family", Value::from(shard.family.clone())),
                ("month", Value::from(shard.month.clone())),
                (
                    "columns",
                    Value::Array(
                        shard
                            .columns
                            .iter()
                            .map(|column| Value::from(column.as_str()))
                            .collect(),
                    ),
                ),
                ("row_count", Value::from(shard.rows.len())),
                ("bytes", Value::from(bytes.len())),
                ("content_hash", Value::from(shard.content_hash())),
                (
                    "canonical",
                    Value::from(String::from_utf8_lossy(&bytes).into_owned()),
                ),
            ])
        })
        .collect();
    emit(&obj(vec![
        ("count", Value::from(built.len())),
        (
            "families",
            Value::Array(
                serialize::mart_families()
                    .into_iter()
                    .map(Value::from)
                    .collect(),
            ),
        ),
        ("shards", Value::Array(rows)),
    ]))
}

fn shard_roundtrip(store_path: &str) -> Result<(), String> {
    let conn = connect(store_path)?;
    let built = serialize::build_shards(&conn).map_err(|err| err.to_string())?;
    let mut rows = Vec::new();
    for shard in &built {
        let restored = serialize::shard_from_bytes(&shard.to_bytes())?;
        rows.push(obj(vec![
            ("shard_key", Value::from(shard.shard_key())),
            ("hash_before", Value::from(shard.content_hash())),
            ("hash_after", Value::from(restored.content_hash())),
            (
                "stable",
                Value::Bool(restored.content_hash() == shard.content_hash()),
            ),
            (
                "columns_equal",
                Value::Bool(restored.columns == shard.columns),
            ),
        ]));
    }
    emit(&obj(vec![("shards", Value::Array(rows))]))
}

fn month_of(raw: &str) -> Result<(), String> {
    let value: Value = serde_json::from_str(raw).map_err(|err| err.to_string())?;
    emit(&obj(vec![(
        "month",
        Value::from(serialize::month_of(&PyValue::from_json(&value))),
    )]))
}

fn json_loads(payload_b64: &str) -> Result<(), String> {
    match stax_sync::pyerr::loads(&b64_decode(payload_b64)?) {
        Ok(_) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("error", Value::Null),
        ])),
        Err(err) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(err.to_string())),
        ])),
    }
}

fn py_int_op(raw: &str) -> Result<(), String> {
    let value: Value = serde_json::from_str(raw).map_err(|err| err.to_string())?;
    match stax_sync::pyerr::py_int(&value) {
        Ok(number) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("value", Value::from(number)),
        ])),
        Err(message) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(message)),
        ])),
    }
}

fn push_op(
    store_path: &str,
    bucket_root: &str,
    device: &str,
    now: &str,
    repeat: &str,
) -> Result<(), String> {
    let root = PathBuf::from(bucket_root);
    let mut store = FileObjectStore::new(&root);
    let conn = connect(store_path)?;
    let rounds: usize = repeat.parse().map_err(|_| "bad repeat".to_owned())?;
    let mut results = Vec::new();
    for _ in 0..rounds {
        let result = runner::push(
            &conn,
            &mut store,
            device,
            "fp0123456789abcd",
            &identity_encryptor,
            now,
            runner::DEFAULT_PREFIX,
        )
        .map_err(|err| err.to_string())?;
        results.push(result.to_json());
    }
    let outbox = dump_table(&conn, "sync_outbox", "shard_key")?;
    emit(&obj(vec![
        ("results", Value::Array(results)),
        ("counters", store.counters()),
        ("bucket", dump_bucket(&root)),
        ("manifest", decode_manifest(&root, device)),
        ("outbox", outbox),
    ]))
}

fn pull_op(
    target_store: &str,
    peer_store: &str,
    bucket_root: &str,
    peer_uuid: &str,
    self_uuid: &str,
    now: &str,
    mode: &str,
) -> Result<(), String> {
    let root = PathBuf::from(bucket_root);
    let mut seeded = FileObjectStore::new(&root);
    {
        let peer_conn = connect(peer_store)?;
        let seed_device = if mode == "self-only" {
            self_uuid
        } else {
            peer_uuid
        };
        runner::push(
            &peer_conn,
            &mut seeded,
            seed_device,
            "fp0123456789abcd",
            &identity_encryptor,
            "2026-07-01T00:00:00+00:00",
            runner::DEFAULT_PREFIX,
        )
        .map_err(|err| err.to_string())?;
    }

    let conn = connect(target_store)?;
    apply_mode(mode, &root, peer_uuid, &conn)?;
    let mut store = FileObjectStore::new(&root);
    let rounds = if mode == "twice" { 2 } else { 1 };
    let mut results = Vec::new();
    for _ in 0..rounds {
        match runner::pull(
            &conn,
            &mut store,
            self_uuid,
            &identity_encryptor,
            now,
            runner::DEFAULT_PREFIX,
        ) {
            Ok(result) => results.push(result.to_json()),
            Err(err) => {
                // The reference RAISES out of `pull` for a bad `generation`; an
                // exception is a comparable answer, so it is reported as one.
                return emit(&obj(vec![(
                    "raised",
                    Value::from(format!("{}: {err}", python_exception_name(&err))),
                )]));
            }
        }
    }
    emit(&obj(vec![
        ("results", Value::Array(results)),
        ("counters", store.counters()),
        (
            "cursors",
            dump_table(&conn, "sync_cursors", "remote_device_uuid, shard_key")?,
        ),
        (
            "devices",
            dump_table(&conn, "sync_remote_devices", "remote_device_uuid")?,
        ),
        ("remote", dump_remote_tables(&conn)?),
        (
            "remote_rows",
            Value::from(merge::remote_row_count(&conn).map_err(|err| err.to_string())?),
        ),
    ]))
}

/// Which CPython exception class the reference would have raised.
///
/// `int()` raises `ValueError` for a bad string and `TypeError` for a bad type;
/// the driver prints `f"{type(exc).__name__}: {exc}"`, so the class name is part
/// of the compared line.
fn python_exception_name(err: &runner::SyncError) -> &'static str {
    let text = err.to_string();
    if text.starts_with("invalid literal for int()") {
        "ValueError"
    } else if text.starts_with("int() argument must be") {
        "TypeError"
    } else {
        "RuntimeError"
    }
}

/// The failure-injection mutations, mirrored line-for-line in the Python half.
#[allow(clippy::too_many_lines)]
fn apply_mode(mode: &str, root: &Path, peer: &str, target: &Connection) -> Result<(), String> {
    let manifest_path = root.join(runner::manifest_key(peer, runner::DEFAULT_PREFIX));
    match mode {
        "normal" | "twice" | "self-only" => {}
        "corrupt-shard" => {
            if let Some(key) = first_shard_key(root, peer) {
                std::fs::write(root.join(key), [0x00_u8, 0x9F, 0x12, 0xFF])
                    .map_err(|err| err.to_string())?;
            }
        }
        "missing-object" => {
            if let Some(key) = first_shard_key(root, peer) {
                std::fs::remove_file(root.join(key)).map_err(|err| err.to_string())?;
            }
        }
        "no-manifest" => std::fs::remove_file(&manifest_path).map_err(|err| err.to_string())?,
        "not-json-manifest" => {
            std::fs::write(&manifest_path, b"not json").map_err(|err| err.to_string())?;
        }
        "empty-manifest" => {
            std::fs::write(&manifest_path, b"").map_err(|err| err.to_string())?;
        }
        "bad-schema" => rewrite_manifest(root, peer, &|manifest| {
            manifest.insert("schema".into(), Value::from("bogus/9"));
        })?,
        "no-schema" => rewrite_manifest(root, peer, &|manifest| {
            manifest.shift_remove("schema");
        })?,
        "stale-gen" => {
            target
                .execute(
                    "INSERT INTO sync_remote_devices \
                     (remote_device_uuid, alias, key_fingerprint, first_seen, last_seen, last_generation) \
                     VALUES (?, NULL, 'seed', 'seed-ts', 'seed-ts', 99)",
                    [peer],
                )
                .map_err(|err| err.to_string())?;
        }
        "gen-string" => rewrite_manifest(root, peer, &|manifest| {
            manifest.insert("generation".into(), Value::from("7"));
        })?,
        "gen-float" => rewrite_manifest(root, peer, &|manifest| {
            manifest.insert("generation".into(), Value::from(2.9));
        })?,
        "gen-missing" => rewrite_manifest(root, peer, &|manifest| {
            manifest.shift_remove("generation");
        })?,
        "unknown-family" => mutate_shard(root, peer, &|payload| {
            payload.insert("family".into(), Value::from("bogus_mart"));
        })?,
        "column-mismatch" => mutate_shard(root, peer, &|payload| {
            if let Some(Value::Array(columns)) = payload.get_mut("columns") {
                columns.pop();
            }
        })?,
        "no-shards" => rewrite_manifest(root, peer, &|manifest| {
            manifest.insert("shards".into(), Value::Object(Map::new()));
        })?,
        other => return Err(format!("unknown pull mode '{other}'")),
    }
    Ok(())
}

fn first_shard_key(root: &Path, device: &str) -> Option<String> {
    let dir = format!("{}/{device}/shards", runner::DEFAULT_PREFIX);
    let mut keys: Vec<String> = walk(&root.join(&dir))
        .into_iter()
        .filter(|name| name.ends_with(".age"))
        .map(|name| format!("{dir}/{name}"))
        .collect();
    keys.sort_unstable();
    keys.into_iter().next()
}

fn rewrite_manifest(
    root: &Path,
    device: &str,
    mutate: &dyn Fn(&mut Map<String, Value>),
) -> Result<(), String> {
    let path = root.join(runner::manifest_key(device, runner::DEFAULT_PREFIX));
    let body = std::fs::read(&path).map_err(|err| err.to_string())?;
    let mut value = stax_sync::pyerr::loads(&body).map_err(|err| err.to_string())?;
    let map = value
        .as_object_mut()
        .ok_or_else(|| "manifest is not an object".to_owned())?;
    mutate(map);
    // `json.dumps(manifest, sort_keys=True, separators=(",", ":"))`. The
    // manifest was written sorted and mutations only replace or remove existing
    // keys, so insertion order is still sorted order.
    std::fs::write(&path, dumps_compact(&value)).map_err(|err| err.to_string())
}

fn mutate_shard(
    root: &Path,
    device: &str,
    mutate: &dyn Fn(&mut Map<String, Value>),
) -> Result<(), String> {
    let Some(key) = first_shard_key(root, device) else {
        return Ok(());
    };
    let path = root.join(&key);
    let body = std::fs::read(&path).map_err(|err| err.to_string())?;
    let mut value = stax_sync::pyerr::loads(&body).map_err(|err| err.to_string())?;
    let map = value
        .as_object_mut()
        .ok_or_else(|| "shard is not an object".to_owned())?;
    mutate(map);
    let rewritten = stax_memory::pyjson::dumps_http(&value).into_bytes();
    std::fs::write(&path, &rewritten).map_err(|err| err.to_string())?;

    let shard_key = key
        .rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix(".age"))
        .unwrap_or_default()
        .to_owned();
    let hash = sha_hex(&rewritten);
    let len = rewritten.len();
    rewrite_manifest(root, device, &move |manifest| {
        if let Some(Value::Object(entries)) = manifest.get_mut("shards")
            && let Some(Value::Object(entry)) = entries.get_mut(&shard_key)
        {
            entry.insert("content_hash".into(), Value::from(hash.clone()));
            entry.insert("bytes".into(), Value::from(len));
        }
    })
}

fn status_op(store_path: &str) -> Result<(), String> {
    let conn = connect(store_path)?;
    let state = runner::status(&conn).map_err(|err| err.to_string())?;
    emit(&state.to_json())
}

fn merge_overview(store_path: &str) -> Result<(), String> {
    let conn = connect(store_path)?;
    let payload = merge::merged_overview(&conn).map_err(|err| err.to_string())?;
    emit(&Value::Object(payload))
}

fn merge_parts(store_path: &str) -> Result<(), String> {
    let conn = connect(store_path)?;
    let (sessions, warnings) = merge::unioned_sessions(&conn).map_err(|err| err.to_string())?;
    let rows = |sql: &str| -> Result<Value, String> {
        Ok(Value::Array(
            merge::query_rows(&conn, sql)
                .map_err(|err| err.to_string())?
                .into_iter()
                .map(Value::Object)
                .collect(),
        ))
    };
    emit(&obj(vec![
        ("daily", rows(merge::UNIONED_DAILY)?),
        ("provider_day", rows(merge::UNIONED_PROVIDER_DAY)?),
        ("model_day", rows(merge::UNIONED_MODEL_DAY)?),
        ("projects", rows(merge::UNIONED_PROJECTS)?),
        (
            "sessions",
            Value::Array(sessions.into_iter().map(Value::Object).collect()),
        ),
        ("merge_warnings", Value::from(warnings)),
        (
            "devices",
            Value::Array(merge::device_breakdown(&conn).map_err(|err| err.to_string())?),
        ),
        (
            "remote_rows",
            Value::from(merge::remote_row_count(&conn).map_err(|err| err.to_string())?),
        ),
    ]))
}

fn allowlist(name: &str) -> Result<&'static [&'static str], String> {
    match name {
        "embed" => Ok(egress::OLLAMA_EMBED_KEYS),
        "chat" => Ok(egress::OLLAMA_CHAT_KEYS),
        other => Err(format!("unknown allowlist '{other}'")),
    }
}

fn egress_guard(kind: &str, allow: &str, body_b64: &str) -> Result<(), String> {
    let raw = b64_decode(body_b64)?;
    let value: Value = serde_json::from_slice(&raw).map_err(|err| err.to_string())?;
    let body = value
        .as_object()
        .ok_or_else(|| "body is not an object".to_owned())?;
    match egress::guard_json_body(body, allowlist(allow)?, kind) {
        Ok(result) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("body", Value::Object(result)),
        ])),
        Err(violation) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(violation.0)),
        ])),
    }
}

fn egress_serialize(body_b64: &str) -> Result<(), String> {
    let raw = b64_decode(body_b64)?;
    let value: Value = serde_json::from_slice(&raw).map_err(|err| err.to_string())?;
    emit(&obj(vec![("text", Value::from(egress::serialize(&value)))]))
}

fn egress_scan(body_b64: &str, needles_b64: &str) -> Result<(), String> {
    let raw = b64_decode(body_b64)?;
    let value: Value = serde_json::from_slice(&raw).map_err(|err| err.to_string())?;
    let needles_raw = b64_decode(needles_b64)?;
    let needles: Vec<String> =
        serde_json::from_slice(&needles_raw).map_err(|err| err.to_string())?;
    let text = egress::serialize(&value);
    emit(&obj(vec![(
        "hits",
        Value::Array(
            egress::scan(&text, &needles)
                .into_iter()
                .map(Value::from)
                .collect(),
        ),
    )]))
}

fn cipher_encrypt(recipient: &str, plaintext_b64: &str) -> Result<(), String> {
    let plain = b64_decode(plaintext_b64)?;
    let ciphertext =
        stax_sync::cipher::encrypt(&plain, recipient).map_err(|err| err.to_string())?;
    emit(&obj(vec![(
        "ciphertext",
        Value::from(b64_encode(&ciphertext)),
    )]))
}

fn cipher_decrypt(secret: &str, ciphertext_b64: &str) -> Result<(), String> {
    let ciphertext = b64_decode(ciphertext_b64)?;
    match stax_sync::cipher::decrypt(&ciphertext, secret) {
        Ok(plain) => emit(&obj(vec![
            ("ok", Value::Bool(true)),
            ("plaintext", Value::from(b64_encode(&plain))),
        ])),
        Err(err) => emit(&obj(vec![
            ("ok", Value::Bool(false)),
            ("error", Value::from(err.to_string())),
        ])),
    }
}

fn cipher_genkey() -> Result<(), String> {
    let ident = keys::generate_identity();
    emit(&obj(vec![
        ("secret", Value::from(ident.secret)),
        ("recipient", Value::from(ident.recipient)),
        ("fingerprint", Value::from(ident.fingerprint)),
    ]))
}

fn cipher_recipient(secret: &str) -> Result<(), String> {
    let recipient = keys::recipient_for(secret)?;
    emit(&obj(vec![
        ("recipient", Value::from(recipient.clone())),
        ("fingerprint", Value::from(keys::fingerprint(&recipient))),
    ]))
}
