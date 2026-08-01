//! `sync/ssh_store.py` — an `ObjectStore` backed by the system `ssh` binary.
//!
//! Objects are files under a remote directory. Deliberately dependency-free on
//! both sides: everything goes through `ssh(1)`, so keys, agents,
//! `~/.ssh/config` aliases, ProxyJump and port forwarding behave exactly as they
//! do in a shell. The payload is already `age` ciphertext by the time it gets
//! here — this is a transport, not a security boundary.
//!
//! # The port's shape: construction is separated from execution
//!
//! Every method here builds an argv and a remote command *string*, then hands
//! both to a [`Transport`]. That split is not decoration: the brief for this
//! wave forbids running real `ssh` against a remote host, so the differ compares
//! **constructed argv** against Python's — the exact bytes that would reach
//! `execve` — and a [`LocalShellTransport`] runs the same remote commands
//! against a scratch directory with `sh -c`. The remote command strings are
//! therefore proven twice: as text against the reference, and as behaviour
//! against a filesystem.
//!
//! # Three details that are answers, not style
//!
//! * **`shlex.quote` is ported, not approximated.** The remote command is a
//!   shell string; a quoting function that differs by one character is a remote
//!   code execution difference, not a formatting one. [`shlex_quote`] reproduces
//!   CPython's `_find_unsafe` character class exactly.
//! * **Sentinel exit codes, never stderr.** 42 = no such object, 43 = no such
//!   root. The reference's comment says why: "sshd banners and warnings (e.g.
//!   OpenSSH's post-quantum advisory) write to stderr on every single
//!   connection, which would make every miss look like an error."
//! * **`put` is write-temp-then-rename.** A reader sees the previous object or
//!   the complete new one, never a half-written shard — which is what makes the
//!   manifest commit in `runner` a commit at all.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::bucket::{ObjectNotFound, ObjectStore, StoreError};

/// `_SSH_BASE_OPTS` — ssh with a password prompt would hang a scripted push.
pub const SSH_BASE_OPTS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ConnectTimeout=10",
];

/// `_DEFAULT_TIMEOUT` — seconds.
pub const DEFAULT_TIMEOUT: u64 = 120;

/// `_RC_NO_SUCH_OBJECT` — the remote shell's "that key isn't here".
pub const RC_NO_SUCH_OBJECT: i32 = 42;

/// `_RC_NO_SUCH_ROOT` — the remote shell's "the sync root doesn't exist yet".
pub const RC_NO_SUCH_ROOT: i32 = 43;

/// `SSHStoreError` — an ssh invocation failed for a non-missing-object reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SSHStoreError(pub String);

impl std::fmt::Display for SSHStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SSHStoreError {}

/// A parsed `ssh://` destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SSHTarget {
    /// `user@host` or just `host`.
    pub host: String,
    /// Absolute remote directory holding the shards.
    pub root: String,
    /// `-p`, when the URL carried one.
    pub port: Option<u16>,
}

impl SSHTarget {
    /// `ssh_argv()` — `["ssh", *_SSH_BASE_OPTS, ("-p", port)?, host]`.
    ///
    /// The order is load-bearing for the differ: options first, the port pair
    /// second, the host last, and the remote command appended by the caller.
    #[must_use]
    pub fn ssh_argv(&self) -> Vec<String> {
        let mut argv = vec!["ssh".to_owned()];
        argv.extend(SSH_BASE_OPTS.iter().map(|opt| (*opt).to_owned()));
        if let Some(port) = self.port {
            argv.push("-p".to_owned());
            argv.push(port.to_string());
        }
        argv.push(self.host.clone());
        argv
    }
}

/// `parse_ssh_url(url)` — `ssh://[user@]host[:port]/absolute/path`.
///
/// The path is required and must be absolute: a relative remote path would
/// resolve against whatever the login shell cd's into.
///
/// # Errors
/// The reference's three `ValueError` messages, verbatim — they reach the user
/// through `sync init`'s "Invalid ssh destination: {exc}" and through
/// `backup create --to`'s "Invalid --to destination: {exc}".
pub fn parse_ssh_url(url: &str) -> Result<SSHTarget, String> {
    let parsed = ParsedUrl::parse(url);
    if parsed.scheme != "ssh" {
        return Err(format!(
            "not an ssh:// URL: {}",
            stax_core::queries::paths::py_repr(url)
        ));
    }
    let Some(hostname) = parsed.hostname.clone().filter(|host| !host.is_empty()) else {
        return Err(format!(
            "ssh URL has no host: {}",
            stax_core::queries::paths::py_repr(url)
        ));
    };
    if parsed.path.is_empty() || parsed.path == "/" {
        return Err(format!(
            "ssh URL needs an absolute remote directory, e.g. \
             ssh://host/srv/stackunderflow-sync (got {})",
            stax_core::queries::paths::py_repr(url)
        ));
    }
    let host = match &parsed.username {
        Some(user) => format!("{user}@{hostname}"),
        None => hostname,
    };
    Ok(SSHTarget {
        host,
        // `parsed.path.rstrip("/")` — every trailing slash, not just one.
        root: parsed.path.trim_end_matches('/').to_owned(),
        port: parsed.port,
    })
}

/// The slice of `urllib.parse.urlparse` this module actually reads.
///
/// A full `urlparse` port would be a module of its own; four fields are used
/// here and each has a documented rule. Written out rather than pulled from a
/// URL crate because the crates normalise (lowercasing paths, percent-decoding,
/// rejecting empty hosts) and every one of those normalisations would be a
/// silent divergence in a destination string a user typed.
#[derive(Debug, Default)]
struct ParsedUrl {
    scheme: String,
    username: Option<String>,
    hostname: Option<String>,
    port: Option<u16>,
    path: String,
}

impl ParsedUrl {
    fn parse(url: &str) -> Self {
        // `urlparse` splits the scheme at the first `:` when what follows is a
        // valid scheme; for our inputs that is the `://` form.
        let Some((scheme, rest)) = url.split_once("://") else {
            return Self {
                scheme: String::new(),
                path: url.to_owned(),
                ..Self::default()
            };
        };
        // `scheme` is lowercased by `urlparse`.
        let scheme = scheme.to_lowercase();
        // netloc runs to the first `/`, `?` or `#`.
        let netloc_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (netloc, tail) = rest.split_at(netloc_end);
        let path = tail.split(['?', '#']).next().unwrap_or_default().to_owned();

        // `userinfo@hostport` — `rpartition("@")`, so a password containing `@`
        // still leaves the host on the right.
        let (userinfo, hostport) = match netloc.rfind('@') {
            Some(index) => (Some(&netloc[..index]), &netloc[index + 1..]),
            None => (None, netloc),
        };
        // `username` is everything before the first `:` of the userinfo.
        let username = userinfo.map(|info| {
            info.split_once(':')
                .map_or(info, |(user, _password)| user)
                .to_owned()
        });

        // `hostname` is lowercased by `urlparse`; the port is the tail after
        // the LAST `:` outside brackets. IPv6 literals are `[::1]:22`.
        let (host, port) = if let Some(close) = hostport.rfind(']') {
            let host = hostport[..=close].to_owned();
            let port = hostport[close + 1..]
                .strip_prefix(':')
                .and_then(|text| text.parse().ok());
            (host, port)
        } else {
            match hostport.rsplit_once(':') {
                Some((host, port_text)) => (host.to_owned(), port_text.parse().ok()),
                None => (hostport.to_owned(), None),
            }
        };
        let hostname = if host.is_empty() {
            None
        } else {
            // `urlparse` strips the brackets off an IPv6 literal.
            Some(
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_lowercase(),
            )
        };

        Self {
            scheme,
            username,
            hostname,
            port,
            path,
        }
    }
}

/// `shlex.quote(s)` — CPython's, character class included.
///
/// `_find_unsafe = re.compile(r'[^\w@%+=:,./-]', re.ASCII).search`. With
/// `re.ASCII`, `\w` is `[a-zA-Z0-9_]` — NOT Unicode word characters, so a
/// non-ASCII path is quoted. Empty string is `''`. A single quote inside is
/// closed, escaped and reopened: `'` → `'"'"'`.
#[must_use]
pub fn shlex_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_owned();
    }
    let safe = |ch: char| {
        ch.is_ascii_alphanumeric()
            || ch == '_'
            || matches!(ch, '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    };
    if text.chars().all(safe) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// One fully-constructed remote invocation: the argv, and whether it takes stdin.
///
/// This is the differ's unit. `argv` is what would reach `execve`; `argv.last()`
/// is the remote shell command; `stdin` records whether the object body is
/// piped in (only `put` does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInvocation {
    /// The full argv, remote command included as the last element.
    pub argv: Vec<String>,
    /// Whether the caller pipes the object body to stdin.
    pub stdin: bool,
}

/// How a [`RemoteInvocation`] actually runs.
///
/// `SshTransport` is the real one. `LocalShellTransport` runs the *same remote
/// command string* under `sh -c` in a scratch directory, which is how the
/// wave-6 brief's "local-target runs on scratch dirs are fine" is honoured
/// without an ssh daemon.
pub trait Transport {
    /// Run `invocation`, optionally with `stdin`, and return `(rc, stdout, stderr)`.
    ///
    /// # Errors
    /// Only for failures that are not the remote command's own exit code —
    /// a spawn failure or a timeout, which the caller turns into an
    /// [`SSHStoreError`].
    fn run(
        &self,
        invocation: &RemoteInvocation,
        stdin: Option<&[u8]>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), TransportFailure>;
}

/// A spawn failure or a timeout — never a non-zero remote exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFailure {
    /// `subprocess.TimeoutExpired`.
    Timeout,
    /// `OSError` — the reference's `could not run ssh: {exc}`.
    Spawn(String),
}

/// The real transport: spawn `ssh`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SshTransport;

impl Transport for SshTransport {
    fn run(
        &self,
        invocation: &RemoteInvocation,
        stdin: Option<&[u8]>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), TransportFailure> {
        spawn(&invocation.argv, stdin)
    }
}

/// Run the remote command locally under `sh -c` — the scratch-dir transport.
///
/// The argv is still *constructed* as ssh's; only the execution differs, so a
/// case that passes here has exercised the same remote command string the
/// differ compared against Python's.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalShellTransport;

impl Transport for LocalShellTransport {
    fn run(
        &self,
        invocation: &RemoteInvocation,
        stdin: Option<&[u8]>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), TransportFailure> {
        let remote = invocation.argv.last().cloned().unwrap_or_default();
        spawn(&["sh".to_owned(), "-c".to_owned(), remote], stdin)
    }
}

fn spawn(
    argv: &[String],
    stdin: Option<&[u8]>,
) -> Result<(i32, Vec<u8>, Vec<u8>), TransportFailure> {
    use std::io::Write as _;

    let (program, args) = argv
        .split_first()
        .ok_or_else(|| TransportFailure::Spawn("empty argv".to_owned()))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| TransportFailure::Spawn(err.to_string()))?;
    if let Some(body) = stdin {
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| TransportFailure::Spawn("stdin pipe unavailable".to_owned()))?;
        pipe.write_all(body)
            .map_err(|err| TransportFailure::Spawn(err.to_string()))?;
        drop(pipe);
    }
    let output = child
        .wait_with_output()
        .map_err(|err| TransportFailure::Spawn(err.to_string()))?;
    // `CompletedProcess.returncode` is negative for a signal; `code()` is
    // `None` there and the reference would see the negative number. Neither
    // side branches on it beyond `!= 0`, so -1 stands in.
    Ok((
        output.status.code().unwrap_or(-1),
        output.stdout,
        output.stderr,
    ))
}

/// `SSHObjectStore` — key/value objects as files under a remote directory.
pub struct SSHObjectStore<T: Transport> {
    /// The parsed destination.
    pub target: SSHTarget,
    /// `timeout` — seconds. Carried for the error message; the transport owns
    /// enforcement.
    pub timeout: u64,
    transport: T,
}

impl SSHObjectStore<SshTransport> {
    /// `ssh_store_from_url(url, timeout=…)`.
    ///
    /// # Errors
    /// [`parse_ssh_url`]'s.
    pub fn from_url(url: &str, timeout: u64) -> Result<Self, String> {
        Ok(Self {
            target: parse_ssh_url(url)?,
            timeout,
            transport: SshTransport,
        })
    }
}

impl<T: Transport> SSHObjectStore<T> {
    /// Build a store over an explicit transport.
    pub const fn with_transport(target: SSHTarget, timeout: u64, transport: T) -> Self {
        Self {
            target,
            timeout,
            transport,
        }
    }

    /// `_remote_path(key)` — absolute remote path, refusing escapes.
    ///
    /// Keys are generated internally, but a traversal would write outside the
    /// sync root, so it is checked rather than trusted. The check is exactly
    /// the reference's: a leading `/`, or `..` as a whole path *segment*. A key
    /// containing `..` inside a segment (`a..b`) is allowed by both.
    ///
    /// # Errors
    /// `unsafe object key: {key!r}`.
    pub fn remote_path(&self, key: &str) -> Result<String, String> {
        if key.starts_with('/') || key.split('/').any(|segment| segment == "..") {
            return Err(format!(
                "unsafe object key: {}",
                stax_core::queries::paths::py_repr(key)
            ));
        }
        Ok(format!("{}/{}", self.target.root, key))
    }

    /// The argv `put` would run.
    ///
    /// `mkdir -p <parent> && cat > <tmp> && mv -f <tmp> <path>` — write to a
    /// temp file and rename, so a reader never sees a half-written shard.
    ///
    /// Note the parent is computed by `rsplit("/", 1)[0]` on the *unquoted*
    /// path, and the temp name is the path plus `.part`.
    ///
    /// # Errors
    /// [`Self::remote_path`]'s.
    pub fn put_invocation(&self, key: &str) -> Result<RemoteInvocation, String> {
        let path = self.remote_path(key)?;
        let parent = path
            .rsplit_once('/')
            .map_or(path.as_str(), |(head, _)| head);
        let tmp = format!("{path}.part");
        let remote = format!(
            "mkdir -p {} && cat > {} && mv -f {} {}",
            shlex_quote(parent),
            shlex_quote(&tmp),
            shlex_quote(&tmp),
            shlex_quote(&path)
        );
        Ok(self.invocation(remote, true))
    }

    /// The argv `get` would run.
    ///
    /// # Errors
    /// [`Self::remote_path`]'s.
    pub fn get_invocation(&self, key: &str) -> Result<RemoteInvocation, String> {
        let path = shlex_quote(&self.remote_path(key)?);
        Ok(self.invocation(
            format!("if test -f {path}; then cat {path}; else exit {RC_NO_SUCH_OBJECT}; fi"),
            false,
        ))
    }

    /// The argv `list` would run. Note it ignores the prefix — the filtering
    /// happens locally, on the `find` output.
    #[must_use]
    pub fn list_invocation(&self) -> RemoteInvocation {
        let root = shlex_quote(&self.target.root);
        self.invocation(
            format!(
                "if test -d {root}; then find {root} -type f -print; \
                 else exit {RC_NO_SUCH_ROOT}; fi"
            ),
            false,
        )
    }

    /// The argv `delete` would run — `rm -f`, so an absent object is a no-op.
    ///
    /// # Errors
    /// [`Self::remote_path`]'s.
    pub fn delete_invocation(&self, key: &str) -> Result<RemoteInvocation, String> {
        let path = shlex_quote(&self.remote_path(key)?);
        Ok(self.invocation(format!("rm -f {path}"), false))
    }

    fn invocation(&self, remote: String, stdin: bool) -> RemoteInvocation {
        let mut argv = self.target.ssh_argv();
        argv.push(remote);
        RemoteInvocation { argv, stdin }
    }

    fn run(
        &self,
        invocation: &RemoteInvocation,
        stdin: Option<&[u8]>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), SSHStoreError> {
        self.transport
            .run(invocation, stdin)
            .map_err(|failure| match failure {
                TransportFailure::Timeout => SSHStoreError(format!(
                    "ssh timed out after {}s against {}",
                    self.timeout, self.target.host
                )),
                TransportFailure::Spawn(message) => {
                    SSHStoreError(format!("could not run ssh: {message}"))
                }
            })
    }

    /// `parse_find_output` — the local half of `list`.
    ///
    /// Split out because it is pure and every branch of it is a decision: lines
    /// outside the root are skipped, `.part` files are "an in-flight put, not an
    /// object", and only then does the prefix filter apply. `sorted()` last.
    #[must_use]
    pub fn parse_find_output(&self, stdout: &[u8], prefix: &str) -> Vec<String> {
        let text = String::from_utf8_lossy(stdout);
        let root_slash = format!("{}/", self.target.root);
        let mut keys: Vec<String> = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || !line.starts_with(&root_slash) {
                continue;
            }
            let key = &line[root_slash.len()..];
            if key.ends_with(".part") {
                continue;
            }
            if key.starts_with(prefix) {
                keys.push(key.to_owned());
            }
        }
        keys.sort_unstable();
        keys
    }
}

impl<T: Transport> ObjectStore for SSHObjectStore<T> {
    fn put(&mut self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let invocation = self.put_invocation(key).map_err(StoreError::Transport)?;
        let (rc, _out, err) = self
            .run(&invocation, Some(data))
            .map_err(|e| StoreError::Transport(e.0))?;
        if rc != 0 {
            return Err(StoreError::Transport(format!(
                "put {} failed (rc={rc}): {}",
                stax_core::queries::paths::py_repr(key),
                String::from_utf8_lossy(&err).trim()
            )));
        }
        Ok(())
    }

    fn get(&mut self, key: &str) -> Result<Vec<u8>, StoreError> {
        let invocation = self.get_invocation(key).map_err(StoreError::Transport)?;
        let (rc, out, err) = self
            .run(&invocation, None)
            .map_err(|e| StoreError::Transport(e.0))?;
        if rc == RC_NO_SUCH_OBJECT {
            return Err(StoreError::NotFound(ObjectNotFound(key.to_owned())));
        }
        if rc != 0 {
            return Err(StoreError::Transport(format!(
                "get {} failed (rc={rc}): {}",
                stax_core::queries::paths::py_repr(key),
                String::from_utf8_lossy(&err).trim()
            )));
        }
        Ok(out)
    }

    fn list(&mut self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let invocation = self.list_invocation();
        let (rc, out, err) = self
            .run(&invocation, None)
            .map_err(|e| StoreError::Transport(e.0))?;
        if rc == RC_NO_SUCH_ROOT {
            // A missing root is an EMPTY store, not an error: `sync push` to a
            // fresh destination must work without pre-creating anything.
            return Ok(Vec::new());
        }
        if rc != 0 {
            return Err(StoreError::Transport(format!(
                "list failed (rc={rc}): {}",
                String::from_utf8_lossy(&err).trim()
            )));
        }
        Ok(self.parse_find_output(&out, prefix))
    }

    fn delete(&mut self, key: &str) -> Result<(), StoreError> {
        let invocation = self.delete_invocation(key).map_err(StoreError::Transport)?;
        let (rc, _out, err) = self
            .run(&invocation, None)
            .map_err(|e| StoreError::Transport(e.0))?;
        if rc != 0 {
            return Err(StoreError::Transport(format!(
                "delete {} failed (rc={rc}): {}",
                stax_core::queries::paths::py_repr(key),
                String::from_utf8_lossy(&err).trim()
            )));
        }
        Ok(())
    }
}

/// Build a scratch-dir store: the same argv construction, `sh -c` execution.
///
/// The `root` is a local absolute path; every remote command in this module is
/// POSIX shell that behaves identically against it.
#[must_use]
pub fn local_scratch_store(root: &Path) -> SSHObjectStore<LocalShellTransport> {
    SSHObjectStore::with_transport(
        SSHTarget {
            host: "scratch".to_owned(),
            root: root.to_string_lossy().trim_end_matches('/').to_owned(),
            port: None,
        },
        DEFAULT_TIMEOUT,
        LocalShellTransport,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SSHTarget {
        SSHTarget {
            host: "yad@box".to_owned(),
            root: "/srv/sync".to_owned(),
            port: None,
        }
    }

    fn store() -> SSHObjectStore<SshTransport> {
        SSHObjectStore::with_transport(target(), DEFAULT_TIMEOUT, SshTransport)
    }

    #[test]
    fn the_base_argv_is_options_then_optional_port_then_host() {
        assert_eq!(
            target().ssh_argv(),
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10",
                "yad@box"
            ]
        );
        let ported = SSHTarget {
            port: Some(2222),
            ..target()
        };
        let argv = ported.ssh_argv();
        assert_eq!(&argv[argv.len() - 3..], ["-p", "2222", "yad@box"]);
    }

    #[test]
    fn parse_ssh_url_builds_host_root_and_port() {
        assert_eq!(
            parse_ssh_url("ssh://yad@box:2222/srv/sync/").expect("parse"),
            SSHTarget {
                host: "yad@box".to_owned(),
                root: "/srv/sync".to_owned(),
                port: Some(2222),
            }
        );
        assert_eq!(
            parse_ssh_url("ssh://box/srv").expect("parse"),
            SSHTarget {
                host: "box".to_owned(),
                root: "/srv".to_owned(),
                port: None,
            }
        );
        // Every trailing slash goes, not just one.
        assert_eq!(
            parse_ssh_url("ssh://box/srv///").expect("parse").root,
            "/srv"
        );
    }

    #[test]
    fn the_three_parse_errors_are_the_references_words() {
        assert_eq!(
            parse_ssh_url("s3://bucket/x").expect_err("scheme"),
            "not an ssh:// URL: 's3://bucket/x'"
        );
        assert_eq!(
            parse_ssh_url("ssh:///srv").expect_err("host"),
            "ssh URL has no host: 'ssh:///srv'"
        );
        assert_eq!(
            parse_ssh_url("ssh://box").expect_err("path"),
            "ssh URL needs an absolute remote directory, e.g. \
             ssh://host/srv/stackunderflow-sync (got 'ssh://box')"
        );
        assert_eq!(
            parse_ssh_url("ssh://box/").expect_err("root path"),
            "ssh URL needs an absolute remote directory, e.g. \
             ssh://host/srv/stackunderflow-sync (got 'ssh://box/')"
        );
    }

    #[test]
    fn shlex_quote_matches_cpythons_character_class() {
        // Safe: `\w` (ASCII) plus @%+=:,./-
        assert_eq!(shlex_quote("/srv/sync/a-b.c_d"), "/srv/sync/a-b.c_d");
        assert_eq!(shlex_quote("a@b%c+d=e:f,g"), "a@b%c+d=e:f,g");
        // Unsafe: space, `$`, `;`, `*`, and — because of `re.ASCII` — any
        // non-ASCII character.
        assert_eq!(shlex_quote("a b"), "'a b'");
        assert_eq!(shlex_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shlex_quote("café"), "'café'");
        assert_eq!(shlex_quote(""), "''");
        // The close-escape-reopen dance.
        assert_eq!(shlex_quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn put_writes_a_part_file_and_renames_it() {
        let invocation = store()
            .put_invocation("stackunderflow/v1/dev/shards/daily_mart.2026-07.age")
            .expect("build");
        assert!(invocation.stdin, "the body is piped");
        assert_eq!(
            invocation.argv.last().expect("remote command"),
            "mkdir -p /srv/sync/stackunderflow/v1/dev/shards \
             && cat > /srv/sync/stackunderflow/v1/dev/shards/daily_mart.2026-07.age.part \
             && mv -f /srv/sync/stackunderflow/v1/dev/shards/daily_mart.2026-07.age.part \
             /srv/sync/stackunderflow/v1/dev/shards/daily_mart.2026-07.age"
        );
    }

    #[test]
    fn get_uses_the_sentinel_exit_code_not_stderr() {
        let invocation = store().get_invocation("a/b").expect("build");
        assert!(!invocation.stdin);
        assert_eq!(
            invocation.argv.last().expect("remote"),
            "if test -f /srv/sync/a/b; then cat /srv/sync/a/b; else exit 42; fi"
        );
    }

    #[test]
    fn list_ignores_its_prefix_argument_on_the_wire() {
        // The remote command is prefix-free — filtering is local, which is why
        // `parse_find_output` is a separate pinned function.
        assert_eq!(
            store().list_invocation().argv.last().expect("remote"),
            "if test -d /srv/sync; then find /srv/sync -type f -print; else exit 43; fi"
        );
    }

    #[test]
    fn delete_is_rm_dash_f() {
        assert_eq!(
            store()
                .delete_invocation("a/b")
                .expect("build")
                .argv
                .last()
                .expect("remote"),
            "rm -f /srv/sync/a/b"
        );
    }

    #[test]
    fn traversal_keys_are_refused_but_dots_inside_a_segment_are_not() {
        let store = store();
        assert_eq!(
            store.remote_path("/etc/passwd").expect_err("absolute"),
            "unsafe object key: '/etc/passwd'"
        );
        assert_eq!(
            store.remote_path("a/../../etc").expect_err("traversal"),
            "unsafe object key: 'a/../../etc'"
        );
        // `".." in key.split("/")` is segment-wise: `a..b` is a legal name.
        assert_eq!(
            store.remote_path("a..b/c").expect("legal"),
            "/srv/sync/a..b/c"
        );
    }

    #[test]
    fn find_output_drops_part_files_foreign_lines_and_applies_the_prefix() {
        let store = store();
        let stdout = b"/srv/sync/stackunderflow/v1/dev/manifest.age\n\
                       /srv/sync/stackunderflow/v1/dev/shards/x.age.part\n\
                       /srv/sync/stackunderflow/v1/dev/shards/x.age\n\
                       /elsewhere/nope\n\
                       \n";
        assert_eq!(
            store.parse_find_output(stdout, "stackunderflow/v1/"),
            vec![
                "stackunderflow/v1/dev/manifest.age".to_owned(),
                "stackunderflow/v1/dev/shards/x.age".to_owned(),
            ]
        );
        assert_eq!(
            store.parse_find_output(stdout, "nothing/"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_scratch_store_round_trips_through_the_same_remote_commands() {
        let root = std::env::temp_dir().join(format!(
            "stax-sync-scratch-{}-{}",
            std::process::id(),
            crate::runner::new_device_uuid()
        ));
        std::fs::create_dir_all(&root).expect("mkdir");
        let mut store = local_scratch_store(&root);

        // A missing root is an empty store, and this one exists but is bare.
        assert_eq!(
            store.list("stackunderflow/").expect("list"),
            Vec::<String>::new()
        );
        store
            .put("stackunderflow/v1/dev/shards/a.age", b"ciphertext")
            .expect("put");
        assert_eq!(
            store
                .get("stackunderflow/v1/dev/shards/a.age")
                .expect("get"),
            b"ciphertext"
        );
        assert_eq!(
            store.list("stackunderflow/").expect("list"),
            vec!["stackunderflow/v1/dev/shards/a.age".to_owned()]
        );
        assert!(matches!(
            store.get("stackunderflow/v1/dev/shards/missing.age"),
            Err(StoreError::NotFound(_))
        ));
        store
            .delete("stackunderflow/v1/dev/shards/a.age")
            .expect("delete");
        // Deleting again is a no-op, matching S3 and the in-memory fake.
        store
            .delete("stackunderflow/v1/dev/shards/a.age")
            .expect("delete twice");
        assert_eq!(store.list("").expect("list"), Vec::<String>::new());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_root_lists_empty_rather_than_erroring() {
        let root = std::env::temp_dir().join(format!(
            "stax-sync-absent-{}-{}",
            std::process::id(),
            crate::runner::new_device_uuid()
        ));
        let mut store = local_scratch_store(&root);
        assert_eq!(store.list("").expect("list"), Vec::<String>::new());
    }
}
