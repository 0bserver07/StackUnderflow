//! `sync/bucket.py` — the narrow `ObjectStore` interface and its dispatch.
//!
//! The sync logic depends only on `put/get/list/delete`, so S3 quirks stay out
//! of the protocol and the test suite is hermetic. That four-method surface is
//! the whole reason `push` and `pull` are testable without a network, and the
//! port keeps it exactly that narrow.
//!
//! # What is here and what is not
//!
//! [`ObjectStore`], [`InMemoryObjectStore`], [`parse_bucket_url`],
//! [`scheme_of`], [`requires_boto3`] and [`store_from_url`]'s dispatch are all
//! ported. [`S3ObjectStore`] is **not**: it is a `boto3` client, and `boto3` is
//! not installed on the parity host, so a Rust S3 client could be diffed
//! against nothing at all. DIV-213 records that with its reasoning; the *key
//! layout* it would use (`_full`, the `list` prefix strip) is ported and
//! unit-pinned here so the part that decides object names is not deferred with
//! the part that moves bytes.

use std::collections::BTreeMap;

/// `ObjectNotFound` — `get` was handed a key that is not there.
///
/// A `KeyError` subclass in the reference, and `pull` branches on it: "peer has
/// no manifest yet" is normal. Distinguishing it from a transport failure is
/// the entire reason `ssh_store` uses a sentinel exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectNotFound(pub String);

impl std::fmt::Display for ObjectNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `KeyError`'s `str()` is `repr(args[0])` — a *quoted* key. It reaches
        // the user through `pull`'s warning strings (`object unreadable
        // ('stackunderflow/v1/…')`), so the quotes are part of the diff.
        write!(f, "{}", stax_core::queries::paths::py_repr(&self.0))
    }
}

impl std::error::Error for ObjectNotFound {}

/// Anything an object store can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The key is absent.
    NotFound(ObjectNotFound),
    /// Everything else — `SSHStoreError`, a botocore error, a broken pipe.
    Transport(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(err) => write!(f, "{err}"),
            Self::Transport(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for StoreError {}

/// A minimal object store: bytes in, bytes out, list by prefix, delete.
///
/// `@runtime_checkable Protocol` in the reference — structural typing there, a
/// trait object here. Same four methods, same order.
pub trait ObjectStore {
    /// Write `data` at `key`, overwriting.
    ///
    /// # Errors
    /// Transport failures.
    fn put(&mut self, key: &str, data: &[u8]) -> Result<(), StoreError>;

    /// Read `key`.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when absent, [`StoreError::Transport`] otherwise.
    fn get(&mut self, key: &str) -> Result<Vec<u8>, StoreError>;

    /// Every key under `prefix`, sorted.
    ///
    /// # Errors
    /// Transport failures. A missing root is an EMPTY list, not an error.
    fn list(&mut self, prefix: &str) -> Result<Vec<String>, StoreError>;

    /// Remove `key`. Absent is a no-op, not an error.
    ///
    /// # Errors
    /// Transport failures.
    fn delete(&mut self, key: &str) -> Result<(), StoreError>;
}

/// `InMemoryObjectStore` — dict-backed, for tests. Counts calls for assertions.
///
/// The counters are not decoration: the reference's idempotency tests assert
/// `put_calls == 0` on a no-op push, which is a stronger claim than "the result
/// says uploaded=0". The differ compares them on both sides for the same reason.
#[derive(Debug, Default, Clone)]
pub struct InMemoryObjectStore {
    data: BTreeMap<String, Vec<u8>>,
    /// `put_calls`.
    pub put_calls: u64,
    /// `get_calls`.
    pub get_calls: u64,
    /// `list_calls`.
    pub list_calls: u64,
    /// `delete_calls`.
    pub delete_calls: u64,
}

impl InMemoryObjectStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `key in store`.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// `len(store)`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the store holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Every `(key, body)` pair, key-sorted — the differ's dump surface.
    #[must_use]
    pub fn entries(&self) -> Vec<(&str, &[u8])> {
        self.data
            .iter()
            .map(|(key, body)| (key.as_str(), body.as_slice()))
            .collect()
    }

    /// Overwrite a stored body without counting a `put` — failure injection.
    ///
    /// Not in the reference (its tests reach into `_data` directly, which is
    /// the same thing through a different door). Used to corrupt a blob so
    /// `pull`'s decrypt-failure warning has something to fire on.
    pub fn poke(&mut self, key: &str, data: &[u8]) {
        self.data.insert(key.to_owned(), data.to_vec());
    }

    /// Remove a stored body without counting a `delete` — failure injection.
    pub fn drop_key(&mut self, key: &str) {
        self.data.remove(key);
    }
}

impl ObjectStore for InMemoryObjectStore {
    fn put(&mut self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        self.put_calls += 1;
        self.data.insert(key.to_owned(), data.to_vec());
        Ok(())
    }

    fn get(&mut self, key: &str) -> Result<Vec<u8>, StoreError> {
        self.get_calls += 1;
        self.data
            .get(key)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(ObjectNotFound(key.to_owned())))
    }

    fn list(&mut self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.list_calls += 1;
        // `sorted(k for k in self._data if k.startswith(prefix))` — a BTreeMap
        // is already in that order, and Python sorts `str` by code point, which
        // for these keys (hex uuids, ASCII family names) is byte order.
        Ok(self
            .data
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn delete(&mut self, key: &str) -> Result<(), StoreError> {
        self.delete_calls += 1;
        // `.pop(key, None)` — absent is fine.
        self.data.remove(key);
        Ok(())
    }
}

/// `parse_bucket_url(url)` — split `s3://bucket[/prefix]` into `(bucket, prefix)`.
///
/// # Errors
/// The reference's two `ValueError` messages, verbatim (they reach a user
/// through `sync init`'s failure path).
pub fn parse_bucket_url(url: &str) -> Result<(String, String), String> {
    if !url.starts_with("s3://") {
        return Err(format!(
            "bucket URL must start with s3:// — got {}",
            stax_core::queries::paths::py_repr(url)
        ));
    }
    let rest = &url["s3://".len()..];
    // `rest.split("/", 1)` — at most one split, so the remainder keeps its
    // slashes.
    let (bucket, prefix) = match rest.split_once('/') {
        Some((bucket, tail)) => (bucket, Some(tail)),
        None => (rest, None),
    };
    if bucket.is_empty() {
        return Err(format!(
            "bucket URL has no bucket name — got {}",
            stax_core::queries::paths::py_repr(url)
        ));
    }
    // `parts[1].strip("/")` strips BOTH ends, not just the leading slash.
    let prefix = prefix.map_or_else(String::new, |tail| tail.trim_matches('/').to_owned());
    Ok((bucket.to_owned(), prefix))
}

/// The key layout [`S3ObjectStore`] would use — `_full(key)`.
///
/// Ported and pinned even though the transport is deferred (DIV-213), because
/// this is the function that decides what an object is *called*, and a name
/// scheme that drifts makes two implementations invisible to each other.
#[must_use]
pub fn s3_full_key(key_prefix: &str, key: &str) -> String {
    let prefix = key_prefix.trim_matches('/');
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}/{key}")
    }
}

/// The inverse `S3ObjectStore.list` applies — `obj["Key"][strip:]`.
///
/// `strip = len(self._prefix) + 1 if self._prefix else 0`, i.e. the prefix and
/// its slash. Note it is a blind slice, not a `removeprefix`: a paginator that
/// returned a key outside the prefix would be silently truncated. Bug-for-bug.
#[must_use]
pub fn s3_strip_prefix(key_prefix: &str, full_key: &str) -> String {
    let prefix = key_prefix.trim_matches('/');
    if prefix.is_empty() {
        return full_key.to_owned();
    }
    let strip = prefix.len() + 1;
    full_key.chars().skip(strip).collect()
}

/// `SUPPORTED_SCHEMES`.
pub const SUPPORTED_SCHEMES: &[&str] = &["s3", "ssh"];

/// `scheme_of(url)` — the transport scheme, `""` when absent.
///
/// `url.split("://", 1)[0].lower() if "://" in url else ""`. The `in` test is
/// on the whole string, so `"weird://x"` answers `weird` and `"nocolon"`
/// answers `""` — the two are different failures downstream.
#[must_use]
pub fn scheme_of(url: &str) -> String {
    url.find("://")
        .map(|index| url[..index].to_lowercase())
        .unwrap_or_default()
}

/// `requires_boto3(url)` — whether `url` needs the optional bucket dependency.
///
/// `ssh://` goes through the system `ssh` binary. An *unknown* scheme answers
/// `True` so the caller gets the install hint rather than a bare import error —
/// which means `stax sync push` against a typo'd URL reports a missing package
/// before it reports a bad URL, on both sides.
#[must_use]
pub fn requires_boto3(url: &str) -> bool {
    scheme_of(url) != "ssh"
}

/// What [`store_from_url`] resolved a destination to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// `ssh://…` → an [`crate::ssh_store::SSHObjectStore`] target.
    Ssh(crate::ssh_store::SSHTarget),
    /// `s3://…` → `(bucket, key_prefix, endpoint_url)`.
    S3 {
        /// The bucket name.
        bucket: String,
        /// The key prefix (`""` when the URL names only a bucket).
        key_prefix: String,
        /// `--endpoint`, or `None` for the AWS default.
        endpoint_url: Option<String>,
    },
}

/// `store_from_url(url, endpoint_url)` — dispatch on the scheme.
///
/// The reference *builds* a store here; this returns the resolved destination
/// so the caller decides whether it can construct a transport for it. The
/// dispatch, the order of the two branches and the `ValueError` message are the
/// reference's.
///
/// # Errors
/// The reference's "unsupported sync destination" message, with the scheme list
/// rendered the same way (`", ".join(f"{s}://" …)`).
pub fn store_from_url(url: &str, endpoint_url: Option<&str>) -> Result<Destination, String> {
    let scheme = scheme_of(url);
    if scheme == "ssh" {
        return crate::ssh_store::parse_ssh_url(url).map(Destination::Ssh);
    }
    if scheme == "s3" {
        let (bucket, key_prefix) = parse_bucket_url(url)?;
        return Ok(Destination::S3 {
            bucket,
            key_prefix,
            endpoint_url: endpoint_url.map(str::to_owned),
        });
    }
    Err(format!(
        "unsupported sync destination {} — expected one of: {}",
        stax_core::queries::paths::py_repr(url),
        SUPPORTED_SCHEMES
            .iter()
            .map(|scheme| format!("{scheme}://"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_url_splits_at_the_first_slash_only() {
        assert_eq!(
            parse_bucket_url("s3://my-bucket/a/b/c").expect("parse"),
            ("my-bucket".to_owned(), "a/b/c".to_owned())
        );
        assert_eq!(
            parse_bucket_url("s3://my-bucket").expect("parse"),
            ("my-bucket".to_owned(), String::new())
        );
        assert_eq!(
            parse_bucket_url("s3://my-bucket/").expect("parse"),
            ("my-bucket".to_owned(), String::new())
        );
        // `.strip("/")` takes BOTH ends.
        assert_eq!(
            parse_bucket_url("s3://my-bucket//deep//").expect("parse"),
            ("my-bucket".to_owned(), "deep".to_owned())
        );
    }

    #[test]
    fn the_bucket_url_errors_are_the_references_words() {
        assert_eq!(
            parse_bucket_url("gs://x").expect_err("scheme"),
            "bucket URL must start with s3:// — got 'gs://x'"
        );
        assert_eq!(
            parse_bucket_url("s3:///prefix").expect_err("no bucket"),
            "bucket URL has no bucket name — got 's3:///prefix'"
        );
    }

    #[test]
    fn scheme_of_lowercases_and_answers_empty_without_a_separator() {
        assert_eq!(scheme_of("S3://b"), "s3");
        assert_eq!(scheme_of("SSH://h/p"), "ssh");
        assert_eq!(scheme_of("nocolon"), "");
        assert_eq!(scheme_of("weird://x"), "weird");
        // `split("://", 1)[0]` — a second `://` is left in the remainder.
        assert_eq!(scheme_of("s3://a://b"), "s3");
    }

    #[test]
    fn only_ssh_escapes_the_boto3_requirement() {
        assert!(!requires_boto3("ssh://host/srv"));
        assert!(requires_boto3("s3://bucket"));
        // An unknown scheme reports the missing package first — deliberately.
        assert!(requires_boto3("gs://bucket"));
        assert!(requires_boto3("nocolon"));
    }

    #[test]
    fn the_unsupported_destination_message_lists_both_schemes() {
        assert_eq!(
            store_from_url("gs://bucket", None).expect_err("unsupported"),
            "unsupported sync destination 'gs://bucket' — expected one of: s3://, ssh://"
        );
    }

    #[test]
    fn the_dispatch_prefers_ssh_then_s3() {
        assert!(matches!(
            store_from_url("ssh://host/srv/sync", None).expect("ssh"),
            Destination::Ssh(_)
        ));
        assert_eq!(
            store_from_url("s3://b/p", Some("https://r2.example")).expect("s3"),
            Destination::S3 {
                bucket: "b".to_owned(),
                key_prefix: "p".to_owned(),
                endpoint_url: Some("https://r2.example".to_owned()),
            }
        );
    }

    #[test]
    fn an_s3_url_that_will_not_parse_reports_the_parse_error_not_the_dispatch_one() {
        assert_eq!(
            store_from_url("s3:///nope", None).expect_err("bad"),
            "bucket URL has no bucket name — got 's3:///nope'"
        );
    }

    #[test]
    fn the_in_memory_store_counts_every_call_including_the_misses() {
        let mut store = InMemoryObjectStore::new();
        assert!(store.is_empty());
        store.put("b/2", b"two").expect("put");
        store.put("a/1", b"one").expect("put");
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("a/1").expect("get"), b"one");
        assert!(matches!(store.get("missing"), Err(StoreError::NotFound(_))));
        assert_eq!(store.list("a/").expect("list"), vec!["a/1".to_owned()]);
        assert_eq!(
            store.list("").expect("list all"),
            vec!["a/1".to_owned(), "b/2".to_owned()]
        );
        // Deleting an absent key is a no-op that still counts.
        store.delete("missing").expect("delete");
        assert_eq!(store.put_calls, 2);
        assert_eq!(store.get_calls, 2);
        assert_eq!(store.list_calls, 2);
        assert_eq!(store.delete_calls, 1);
    }

    #[test]
    fn object_not_found_stringifies_like_a_keyerror() {
        // `str(KeyError("k"))` is `"'k'"` — the quotes are in the warning text
        // `pull` writes, so they are a diff surface.
        assert_eq!(ObjectNotFound("k".to_owned()).to_string(), "'k'");
    }

    #[test]
    fn the_s3_key_layout_is_pinned_even_though_the_transport_is_not() {
        assert_eq!(s3_full_key("", "a/b"), "a/b");
        assert_eq!(s3_full_key("pre", "a/b"), "pre/a/b");
        assert_eq!(s3_full_key("/pre/", "a/b"), "pre/a/b");
        assert_eq!(s3_strip_prefix("pre", "pre/a/b"), "a/b");
        assert_eq!(s3_strip_prefix("", "a/b"), "a/b");
        // The blind slice, reproduced: a key outside the prefix loses
        // `len(prefix)+1` characters instead of being skipped.
        assert_eq!(s3_strip_prefix("pre", "xxxxyz"), "yz");
    }
}
