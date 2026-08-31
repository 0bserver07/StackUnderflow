//! Artifact readers for D1 — JSON via the workspace serde_json, plus two
//! hand-rolled readers (toml-lite, dotenv) so the manifest grows no parser.
//! Anything a reader cannot understand is an `Err`, and the scanner turns
//! that into posture `unknown` — degraded never means silent (§8.3).

use anyhow::{Result, bail};
use serde_json::{Map, Value};

/// Read an artifact into a JSON value tree according to its declared format.
pub fn read_artifact(text: &str, format: crate::Format) -> Result<Value> {
    match format {
        crate::Format::Json => Ok(serde_json::from_str(text)?),
        crate::Format::TomlLite => toml_lite(text),
        crate::Format::Env => dotenv(text),
    }
}

/// Walk a dotted key path (`telemetry.trace_upload`) through objects.
pub fn lookup<'v>(root: &'v Value, dotted: &str) -> Option<&'v Value> {
    let mut node = root;
    for part in dotted.split('.') {
        node = node.as_object()?.get(part)?;
    }
    Some(node)
}

/// The minimal TOML subset agent configs actually use: `[section]` headers
/// (dotted sections nest), `key = value` scalars, `#` comments, blank lines.
/// Arrays, tables-in-arrays, multi-line strings: out of scope → `Err`.
fn toml_lite(text: &str) -> Result<Value> {
    let mut root = Map::new();
    let mut section: Vec<String> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if name.starts_with('[') {
                bail!("line {}: array-of-tables is beyond toml-lite", idx + 1);
            }
            section = name
                .trim()
                .split('.')
                .map(|s| s.trim().to_string())
                .collect();
            ensure_path(&mut root, &section)?;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            bail!("line {}: expected `key = value`", idx + 1);
        };
        let target = descend(&mut root, &section)?;
        target.insert(key.trim().to_string(), scalar(value.trim()));
    }
    Ok(Value::Object(root))
}

/// `KEY=VALUE` per line; `export ` prefixes and `#` comments allowed; values
/// stay strings (signatures list the string forms they mean).
fn dotenv(text: &str) -> Result<Value> {
    let mut map = Map::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(&line).trim();
        let Some((key, value)) = line.split_once('=') else {
            bail!("line {}: expected KEY=VALUE", idx + 1);
        };
        map.insert(key.trim().to_string(), Value::String(unquote(value.trim())));
    }
    Ok(Value::Object(map))
}

fn strip_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    for (i, ch) in line.char_indices() {
        match (ch, in_str) {
            ('"' | '\'', None) => in_str = Some(ch),
            (q, Some(open)) if q == open => in_str = None,
            ('#', None) => return &line[..i],
            _ => {}
        }
    }
    line
}

fn scalar(s: &str) -> Value {
    match s {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(n) = s.parse::<i64>() {
        return Value::from(n);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::from(f);
    }
    Value::String(unquote(s))
}

fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn ensure_path(root: &mut Map<String, Value>, path: &[String]) -> Result<()> {
    descend(root, path).map(|_| ())
}

fn descend<'m>(
    root: &'m mut Map<String, Value>,
    path: &[String],
) -> Result<&'m mut Map<String, Value>> {
    let mut node = root;
    for part in path {
        let entry = node
            .entry(part.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        match entry {
            Value::Object(map) => node = map,
            _ => bail!("section [{part}] collides with a scalar key"),
        }
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_lite_sections_scalars_comments() {
        let v =
            toml_lite("# c\n[features]\ntelemetry = true # on\n[a.b]\nn = 3\ns = \"x\"\n").unwrap();
        assert_eq!(lookup(&v, "features.telemetry"), Some(&Value::Bool(true)));
        assert_eq!(lookup(&v, "a.b.n"), Some(&Value::from(3)));
        assert_eq!(lookup(&v, "a.b.s"), Some(&Value::String("x".into())));
    }

    #[test]
    fn toml_lite_rejects_what_it_cannot_read() {
        assert!(toml_lite("[[tables]]\n").is_err());
        assert!(toml_lite("key value\n").is_err());
    }

    #[test]
    fn dotenv_reads_exports_and_quotes() {
        let v = dotenv("# c\nexport A=1\nB=\"two\"\n").unwrap();
        assert_eq!(lookup(&v, "A"), Some(&Value::String("1".into())));
        assert_eq!(lookup(&v, "B"), Some(&Value::String("two".into())));
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let v = toml_lite("[s]\nurl = \"http://x/#y\"\n").unwrap();
        assert_eq!(
            lookup(&v, "s.url"),
            Some(&Value::String("http://x/#y".into()))
        );
    }
}
