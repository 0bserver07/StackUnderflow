//! Query strings, parsed the way starlette + pydantic parse them.
//!
//! Not `axum::extract::Query`, and not `serde_urlencoded`, for two reasons that
//! both show up in the ported signatures:
//!
//! * **Repeated keys are a list, not an overwrite.**
//!   `provider: Annotated[list[str] | None, Query()]` means
//!   `?provider=cursor&provider=cline` arrives as `["cursor", "cline"]`.
//!   `serde_urlencoded` into a struct keeps one.
//! * **Scalars take the *last* occurrence.** starlette builds
//!   `QueryParams._dict` with a comprehension over the item list, so
//!   `?offset=1&offset=2` resolves to `2`. Not the first, which is the
//!   intuitive guess and the one that would have shipped.
//!
//! Coercion follows pydantic v2's lax mode for the shapes the routes actually
//! declare: `bool`, `int`, `int | None`, `str`, `list[str]`. A value that will
//! not coerce is a `422`, and [`QueryError`] carries enough to build one.

use std::borrow::Cow;

/// A parsed query string: ordered `(key, value)` pairs, decoded.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

/// A query parameter that would not coerce — starlette answers `422`.
#[derive(Debug, Clone)]
pub struct QueryError {
    /// The parameter name, for the `loc` of a validation error.
    pub field: String,
    /// The raw value that failed.
    pub input: String,
    /// pydantic's error `type`, e.g. `int_parsing`.
    pub kind: &'static str,
}

impl Query {
    /// Parse a raw query string (no leading `?`).
    ///
    /// `+` decodes to a space and `%XX` to its byte, matching
    /// `urllib.parse.parse_qsl`. Invalid UTF-8 after decoding is replaced
    /// rather than rejected, which is `errors="replace"` — starlette's default.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut pairs = Vec::new();
        for chunk in raw.split('&') {
            if chunk.is_empty() {
                continue;
            }
            let (key, value) = match chunk.split_once('=') {
                Some((k, v)) => (k, v),
                None => (chunk, ""),
            };
            pairs.push((decode(key).into_owned(), decode(value).into_owned()));
        }
        Self { pairs }
    }

    /// The **last** value for `key`, or `None`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Every value for `key`, in order — the `list[str]` shape.
    #[must_use]
    pub fn get_all(&self, key: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// `bool = default`.
    ///
    /// pydantic v2 lax-mode string→bool: `1/true/t/yes/y/on` and
    /// `0/false/f/no/n/off`, case-insensitive. Anything else is `422`, which is
    /// worth knowing — `?include_stats=maybe` is not silently false.
    ///
    /// # Errors
    /// When the value is present and does not coerce.
    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool, QueryError> {
        let Some(raw) = self.get(key) else {
            return Ok(default);
        };
        match raw.to_ascii_lowercase().as_str() {
            "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
            "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
            _ => Err(QueryError {
                field: key.to_owned(),
                input: raw.to_owned(),
                kind: "bool_parsing",
            }),
        }
    }

    /// `int = default`.
    ///
    /// # Errors
    /// When the value is present and is not an integer.
    pub fn int_or(&self, key: &str, default: i64) -> Result<i64, QueryError> {
        Ok(self.opt_int(key)?.unwrap_or(default))
    }

    /// `int | None = None`.
    ///
    /// # Errors
    /// When the value is present and is not an integer.
    pub fn opt_int(&self, key: &str) -> Result<Option<i64>, QueryError> {
        let Some(raw) = self.get(key) else {
            return Ok(None);
        };
        // pydantic strips surrounding whitespace before parsing an int from a
        // string; `"  5 "` is 5. It does *not* accept `"5.0"` for an `int`
        // field unless the float is integral — which it is, but the query
        // shapes here never send one, so the narrow rule is the honest one.
        raw.trim().parse::<i64>().map(Some).map_err(|_| QueryError {
            field: key.to_owned(),
            input: raw.to_owned(),
            kind: "int_parsing",
        })
    }

    /// `str = default`.
    #[must_use]
    pub fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }

    /// `list[str] | None = None` — `None` when the key never appears.
    ///
    /// FastAPI gives the handler `None`, not `[]`, for an absent repeated
    /// param, and several routes branch on exactly that (`if not provider:`
    /// treats both the same, but `provider is not None` does not).
    #[must_use]
    pub fn opt_list(&self, key: &str) -> Option<Vec<String>> {
        let values = self.get_all(key);
        if values.is_empty() {
            None
        } else {
            Some(values.into_iter().map(str::to_owned).collect())
        }
    }
}

/// `urllib.parse.unquote_plus` over percent-encoding.
fn decode(raw: &str) -> Cow<'_, str> {
    if !raw.contains('%') && !raw.contains('+') {
        return Cow::Borrowed(raw);
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi << 4 | lo);
                        i += 3;
                    }
                    // A malformed escape is left literal, as `unquote` does.
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_keys_survive_as_a_list() {
        let q = Query::parse("provider=cursor&provider=cline");
        assert_eq!(
            q.opt_list("provider"),
            Some(vec!["cursor".into(), "cline".into()])
        );
    }

    #[test]
    fn a_scalar_takes_the_last_occurrence() {
        // starlette's `QueryParams._dict` is a comprehension over the pair
        // list, so the last write wins. Counter-intuitive and load-bearing.
        let q = Query::parse("offset=1&offset=2");
        assert_eq!(q.int_or("offset", 0).expect("int"), 2);
    }

    #[test]
    fn absent_repeated_param_is_none_not_empty() {
        let q = Query::parse("");
        assert!(q.opt_list("provider").is_none());
    }

    #[test]
    fn bools_take_pydantics_vocabulary() {
        for raw in ["1", "true", "TRUE", "t", "yes", "y", "on"] {
            let q = Query::parse(&format!("include_stats={raw}"));
            assert!(q.bool_or("include_stats", false).expect("bool"), "{raw}");
        }
        for raw in ["0", "false", "f", "no", "n", "off"] {
            let q = Query::parse(&format!("include_stats={raw}"));
            assert!(!q.bool_or("include_stats", true).expect("bool"), "{raw}");
        }
        let q = Query::parse("include_stats=maybe");
        assert_eq!(
            q.bool_or("include_stats", false).unwrap_err().kind,
            "bool_parsing"
        );
    }

    #[test]
    fn missing_values_take_the_declared_default() {
        let q = Query::parse("sort_by=name");
        assert_eq!(q.str_or("sort_by", "last_modified"), "name");
        assert_eq!(
            Query::parse("").str_or("sort_by", "last_modified"),
            "last_modified"
        );
        assert_eq!(Query::parse("").opt_int("limit").expect("none"), None);
    }

    #[test]
    fn percent_and_plus_decode_like_parse_qsl() {
        let q = Query::parse("q=a+b%20c&slug=%2Dhome%2Du");
        assert_eq!(q.get("q"), Some("a b c"));
        assert_eq!(q.get("slug"), Some("-home-u"));
    }

    #[test]
    fn a_bare_key_is_the_empty_string() {
        let q = Query::parse("details&limit=5");
        assert_eq!(q.get("details"), Some(""));
        assert_eq!(q.int_or("limit", 0).expect("int"), 5);
    }
}
