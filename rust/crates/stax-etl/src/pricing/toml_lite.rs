//! A strict, dependency-free reader for the TOML subset `data/models.toml` uses.
//!
//! Why not the `toml` crate: the manifest is read once at startup, the subset it
//! uses is small and stable (arrays of tables, one level of nesting, scalars and
//! string arrays), and pulling `toml` + `serde` + `winnow` into the workspace
//! would churn the shared `Cargo.lock` that every agent in the fleet builds
//! against. The correctness argument that replaces "we trust the crate" is
//! empirical: `tests/pricing_parity.rs` re-parses the real manifest with
//! CPython's `tomllib` and asserts this reader produced the same values, and the
//! cost sweep would surface any misread number as a divergence.
//!
//! Fidelity rule: syntax this reader rejects is syntax Python's `tomllib` also
//! rejects (both raise, `model_manifest._models()` does not catch), and values it
//! parses as [`Value::Other`] — TOML date/time literals — are values Python
//! parses successfully but `_valid_price_row` then rejects as non-numeric, so the
//! entry is dropped on both sides rather than failing the load.

use std::fmt;

/// A parsed TOML value, restricted to the shapes the manifest can hold.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A basic or literal string.
    String(String),
    /// An integer.
    Integer(i64),
    /// A float.
    Float(f64),
    /// A boolean.
    Boolean(bool),
    /// A homogeneous or heterogeneous array.
    Array(Vec<Value>),
    /// A bare token this reader recognises as valid TOML but does not model —
    /// date and date-time literals. Kept (rather than rejected) so a manifest
    /// carrying one is *dropped by validation* exactly as Python drops it,
    /// instead of failing the whole load.
    Other(String),
}

impl Value {
    /// The string body, or `None` for any other shape.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The numeric value as `f64`, mirroring Python's
    /// `isinstance(v, int | float) and not isinstance(v, bool)` test: booleans
    /// are *not* numbers here even though Python's `bool` subclasses `int`.
    #[must_use]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Integer(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// The boolean body, or `None`.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// The array body, or `None`.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// A TOML table: ordered key/value pairs plus its child tables and
/// arrays-of-tables. Order is preserved because the manifest's order is
/// load-bearing (`canonicalize` returns the first matching entry).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pairs: Vec<(String, Value)>,
    arrays: Vec<(String, Vec<Table>)>,
    tables: Vec<(String, Table)>,
}

impl Table {
    /// The value stored under `key`, or `None`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The array-of-tables stored under `key`, or `None`.
    #[must_use]
    pub fn array(&self, key: &str) -> Option<&[Table]> {
        self.arrays
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
    }

    /// The sub-table stored under `key`, or `None`.
    #[must_use]
    pub fn table(&self, key: &str) -> Option<&Table> {
        self.tables.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Every key/value pair, in file order.
    #[must_use]
    pub fn pairs(&self) -> &[(String, Value)] {
        &self.pairs
    }

    fn insert_pair(&mut self, key: String, value: Value, line: usize) -> Result<(), TomlError> {
        if self.pairs.iter().any(|(k, _)| *k == key) {
            return Err(TomlError::new(line, format!("duplicate key `{key}`")));
        }
        self.pairs.push((key, value));
        Ok(())
    }
}

/// A parse failure, with the 1-based line it happened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlError {
    /// 1-based line number.
    pub line: usize,
    /// What went wrong.
    pub message: String,
}

impl TomlError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for TomlError {}

/// Parse `src` into a root [`Table`].
///
/// # Errors
/// On any syntax this reader does not accept — which is, by design, syntax the
/// manifest does not use.
pub fn parse(src: &str) -> Result<Table, TomlError> {
    Parser {
        src,
        pos: 0,
        line: 1,
    }
    .document()
}

/// Where key/value pairs currently land.
enum Cursor {
    /// The root table.
    Root,
    /// A `[name]` table.
    Named(String),
    /// The last element of the `[[name]]` array-of-tables.
    Array(String),
    /// The last element of the `[[outer.inner]]` nested array-of-tables.
    Nested(String, String),
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Skip spaces and tabs only.
    fn skip_inline_space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.bump();
        }
    }

    /// Skip whitespace (including newlines) and whole-line comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r' | '\n') => {
                    self.bump();
                }
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    /// After a value: allow trailing space, an optional comment, then demand a
    /// newline or EOF.
    fn finish_line(&mut self) -> Result<(), TomlError> {
        self.skip_inline_space();
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.bump();
                Ok(())
            }
            Some('\r') => {
                self.bump();
                if self.eat('\n') {
                    Ok(())
                } else {
                    Err(TomlError::new(self.line, "stray carriage return"))
                }
            }
            Some(c) => Err(TomlError::new(
                self.line,
                format!("unexpected `{c}` after value"),
            )),
        }
    }

    fn document(&mut self) -> Result<Table, TomlError> {
        let mut root = Table::default();
        let mut cursor = Cursor::Root;
        loop {
            self.skip_trivia();
            if self.peek().is_none() {
                return Ok(root);
            }
            if self.peek() == Some('[') {
                cursor = self.header(&mut root)?;
                continue;
            }
            let line = self.line;
            let key = self.key()?;
            self.skip_inline_space();
            if !self.eat('=') {
                return Err(TomlError::new(line, format!("expected `=` after `{key}`")));
            }
            self.skip_inline_space();
            let value = self.value()?;
            self.finish_line()?;
            table_at(&mut root, &cursor, line)?.insert_pair(key, value, line)?;
        }
    }

    /// Parse a `[table]` / `[[array]]` / `[[outer.inner]]` header and move the
    /// cursor. Returns the new cursor.
    fn header(&mut self, root: &mut Table) -> Result<Cursor, TomlError> {
        let line = self.line;
        self.bump(); // '['
        let double = self.eat('[');
        self.skip_inline_space();
        let first = self.key()?;
        self.skip_inline_space();
        let second = if self.eat('.') {
            self.skip_inline_space();
            let name = self.key()?;
            self.skip_inline_space();
            Some(name)
        } else {
            None
        };
        if !self.eat(']') {
            return Err(TomlError::new(line, "unterminated header"));
        }
        if double && !self.eat(']') {
            return Err(TomlError::new(line, "unterminated `[[` header"));
        }
        self.finish_line()?;

        match (double, second) {
            (false, None) => {
                if root.tables.iter().any(|(k, _)| *k == first) {
                    return Err(TomlError::new(line, format!("duplicate table `{first}`")));
                }
                root.tables.push((first.clone(), Table::default()));
                Ok(Cursor::Named(first))
            }
            (true, None) => {
                match root.arrays.iter_mut().find(|(k, _)| *k == first) {
                    Some((_, items)) => items.push(Table::default()),
                    None => root.arrays.push((first.clone(), vec![Table::default()])),
                }
                Ok(Cursor::Array(first))
            }
            (true, Some(inner)) => {
                let Some((_, items)) = root.arrays.iter_mut().find(|(k, _)| *k == first) else {
                    return Err(TomlError::new(
                        line,
                        format!("`[[{first}.{inner}]]` before any `[[{first}]]`"),
                    ));
                };
                let Some(parent) = items.last_mut() else {
                    return Err(TomlError::new(
                        line,
                        format!("`[[{first}.{inner}]]` before any `[[{first}]]`"),
                    ));
                };
                match parent.arrays.iter_mut().find(|(k, _)| *k == inner) {
                    Some((_, nested)) => nested.push(Table::default()),
                    None => parent.arrays.push((inner.clone(), vec![Table::default()])),
                }
                Ok(Cursor::Nested(first, inner))
            }
            (false, Some(inner)) => Err(TomlError::new(
                line,
                format!("dotted table header `[{first}.{inner}]` is not supported"),
            )),
        }
    }

    fn key(&mut self) -> Result<String, TomlError> {
        if self.peek() == Some('"') {
            return match self.basic_string()? {
                Value::String(s) => Ok(s),
                _ => unreachable!("basic_string always yields a string"),
            };
        }
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            self.bump();
        }
        if start == self.pos {
            return Err(TomlError::new(self.line, "expected a key"));
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn value(&mut self) -> Result<Value, TomlError> {
        match self.peek() {
            Some('"') => self.basic_string(),
            Some('\'') => self.literal_string(),
            Some('[') => self.array(),
            Some('{') => Err(TomlError::new(
                self.line,
                "inline tables are not supported by this reader",
            )),
            Some(_) => self.bare_value(),
            None => Err(TomlError::new(self.line, "expected a value")),
        }
    }

    fn basic_string(&mut self) -> Result<Value, TomlError> {
        let line = self.line;
        self.bump(); // opening quote
        if self.peek() == Some('"') {
            // Either an empty string or a multi-line `"""` opener.
            self.bump();
            if self.peek() == Some('"') {
                return Err(TomlError::new(
                    line,
                    "multi-line strings are not supported by this reader",
                ));
            }
            return Ok(Value::String(String::new()));
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return Err(TomlError::new(line, "unterminated string")),
                Some('"') => return Ok(Value::String(out)),
                Some('\\') => {
                    let escaped = self
                        .bump()
                        .ok_or_else(|| TomlError::new(line, "unterminated escape"))?;
                    match escaped {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => out.push(self.unicode_escape(4, line)?),
                        'U' => out.push(self.unicode_escape(8, line)?),
                        other => {
                            return Err(TomlError::new(line, format!("bad escape `\\{other}`")));
                        }
                    }
                }
                Some(c) => out.push(c),
            }
        }
    }

    fn unicode_escape(&mut self, width: usize, line: usize) -> Result<char, TomlError> {
        let mut code = 0u32;
        for _ in 0..width {
            let c = self
                .bump()
                .ok_or_else(|| TomlError::new(line, "unterminated unicode escape"))?;
            let digit = c
                .to_digit(16)
                .ok_or_else(|| TomlError::new(line, "bad unicode escape"))?;
            code = code * 16 + digit;
        }
        char::from_u32(code).ok_or_else(|| TomlError::new(line, "bad unicode scalar"))
    }

    fn literal_string(&mut self) -> Result<Value, TomlError> {
        let line = self.line;
        self.bump(); // opening quote
        let start = self.pos;
        loop {
            match self.peek() {
                None | Some('\n') => return Err(TomlError::new(line, "unterminated string")),
                Some('\'') => {
                    let body = self.src[start..self.pos].to_string();
                    self.bump();
                    return Ok(Value::String(body));
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    fn array(&mut self) -> Result<Value, TomlError> {
        let line = self.line;
        self.bump(); // '['
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(TomlError::new(line, "unterminated array")),
                Some(']') => {
                    self.bump();
                    return Ok(Value::Array(items));
                }
                Some(_) => {}
            }
            items.push(self.value()?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    return Ok(Value::Array(items));
                }
                _ => return Err(TomlError::new(self.line, "expected `,` or `]` in array")),
            }
        }
    }

    /// A bare token: boolean, number, or a date/time literal we keep as
    /// [`Value::Other`].
    fn bare_value(&mut self) -> Result<Value, TomlError> {
        let line = self.line;
        let start = self.pos;
        while matches!(self.peek(), Some(c) if !matches!(c, ',' | ']' | '}' | '#' | '\n' | '\r')) {
            self.bump();
        }
        let raw = self.src[start..self.pos].trim_end();
        if raw.is_empty() {
            return Err(TomlError::new(line, "expected a value"));
        }
        match raw {
            "true" => return Ok(Value::Boolean(true)),
            "false" => return Ok(Value::Boolean(false)),
            _ => {}
        }
        let cleaned: String = raw.chars().filter(|c| *c != '_').collect();
        let looks_float = cleaned.contains('.')
            || cleaned.contains('e')
            || cleaned.contains('E')
            || cleaned.contains("inf")
            || cleaned.contains("nan");
        if looks_float
            && !cleaned.contains(':')
            && let Ok(f) = cleaned.parse::<f64>()
        {
            return Ok(Value::Float(f));
        }
        if let Ok(i) = cleaned.parse::<i64>() {
            return Ok(Value::Integer(i));
        }
        if is_date_like(raw) {
            return Ok(Value::Other(raw.to_string()));
        }
        Err(TomlError::new(line, format!("cannot parse value `{raw}`")))
    }
}

/// TOML date / date-time / time literals, loosely recognised. Kept as
/// [`Value::Other`] so validation drops the entry the way Python's does.
fn is_date_like(raw: &str) -> bool {
    raw.chars().all(|c| {
        c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 't' | 'Z' | 'z' | '.' | '+' | ' ')
    }) && raw.chars().any(|c| c.is_ascii_digit())
}

/// Resolve the cursor to the table pairs currently land in.
fn table_at<'t>(
    root: &'t mut Table,
    cursor: &Cursor,
    line: usize,
) -> Result<&'t mut Table, TomlError> {
    match cursor {
        Cursor::Root => Ok(root),
        Cursor::Named(name) => root
            .tables
            .iter_mut()
            .find(|(k, _)| k == name)
            .map(|(_, t)| t)
            .ok_or_else(|| TomlError::new(line, "internal: missing named table")),
        Cursor::Array(name) => root
            .arrays
            .iter_mut()
            .find(|(k, _)| k == name)
            .and_then(|(_, items)| items.last_mut())
            .ok_or_else(|| TomlError::new(line, "internal: missing array element")),
        Cursor::Nested(outer, inner) => root
            .arrays
            .iter_mut()
            .find(|(k, _)| k == outer)
            .and_then(|(_, items)| items.last_mut())
            .and_then(|parent| {
                parent
                    .arrays
                    .iter_mut()
                    .find(|(k, _)| k == inner)
                    .and_then(|(_, nested)| nested.last_mut())
            })
            .ok_or_else(|| TomlError::new(line, "internal: missing nested array element")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_manifest_shape() {
        let src = r#"
# a comment
[[model]]
family = "OPUS_48"
provider = "anthropic"
match = ["opus", "4", "8"]
fast_multiplier = 6.0
  [[model.price]]
  input = 5.0
  output = 25.0
  cache_write = 6.25
  cache_read = 0.50

[[model]]
family = "GPT_54"
provider = "openai"
ids = ["gpt-5.4"]
  [[model.price]]
  effective_until = "2026-04-26"
  input = 2.50
  output = 20.0
  cache_write = 0.0
  cache_read = 0.25
  [[model.price]]
  effective_from = "2026-04-26"
  input = 2.50
  output = 15.0
  cache_write = 0.0
  cache_read = 0.25

[canonical_ids]
anthropic = [
  "claude-opus-4-8",   # trailing comment
  "claude-opus-4-7",
]
openai = ["gpt-5.4"]
"#;
        let doc = parse(src).expect("parses");
        let models = doc.array("model").expect("model array");
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0].get("family").and_then(Value::as_str),
            Some("OPUS_48")
        );
        assert_eq!(
            models[0].get("fast_multiplier").and_then(Value::as_number),
            Some(6.0)
        );
        let prices = models[0].array("price").expect("price rows");
        assert_eq!(prices.len(), 1);
        assert_eq!(
            prices[0].get("cache_write").and_then(Value::as_number),
            Some(6.25)
        );
        let gpt = &models[1];
        assert_eq!(gpt.array("price").map(<[Table]>::len), Some(2));
        assert_eq!(
            gpt.array("price").unwrap()[0]
                .get("effective_until")
                .and_then(Value::as_str),
            Some("2026-04-26")
        );
        let ids = doc.table("canonical_ids").expect("canonical_ids");
        let anthropic = ids
            .get("anthropic")
            .and_then(Value::as_array)
            .expect("array");
        assert_eq!(anthropic.len(), 2);
        assert_eq!(anthropic[0].as_str(), Some("claude-opus-4-8"));
    }

    #[test]
    fn rejects_unsupported_syntax_loudly() {
        assert!(parse("a = {b = 1}").is_err(), "inline table");
        assert!(parse("[a.b]\nx = 1").is_err(), "dotted table header");
        assert!(parse("a = \"\"\"x\"\"\"").is_err(), "multi-line string");
        assert!(parse("[[a.b]]\nx = 1").is_err(), "nested before outer");
        assert!(parse("a = 1\na = 2").is_err(), "duplicate key");
        assert!(parse("[t]\nx = 1\n[t]\ny = 2").is_err(), "duplicate table");
        assert!(parse("a = ").is_err(), "missing value");
        assert!(parse("a 1").is_err(), "missing equals");
    }

    #[test]
    fn keeps_date_literals_as_other_so_validation_can_drop_them() {
        let doc = parse("[[model]]\ninput = 1979-05-27\n").expect("parses");
        let entry = &doc.array("model").unwrap()[0];
        assert_eq!(
            entry.get("input"),
            Some(&Value::Other("1979-05-27".to_string()))
        );
        assert_eq!(entry.get("input").and_then(Value::as_number), None);
    }

    #[test]
    fn booleans_are_not_numbers() {
        let doc = parse("a = true\nb = 1\nc = 1.5\n").expect("parses");
        assert_eq!(doc.get("a").and_then(Value::as_number), None);
        assert_eq!(doc.get("a").and_then(Value::as_bool), Some(true));
        assert_eq!(doc.get("b").and_then(Value::as_number), Some(1.0));
        assert_eq!(doc.get("c").and_then(Value::as_number), Some(1.5));
    }

    #[test]
    fn strings_handle_escapes_and_empties() {
        let doc = parse("a = \"\"\nb = \"x\\ty\"\nc = 'raw\\n'\n").expect("parses");
        assert_eq!(doc.get("a").and_then(Value::as_str), Some(""));
        assert_eq!(doc.get("b").and_then(Value::as_str), Some("x\ty"));
        assert_eq!(doc.get("c").and_then(Value::as_str), Some("raw\\n"));
    }
}
