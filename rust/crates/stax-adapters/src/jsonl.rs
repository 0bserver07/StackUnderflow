//! Byte-offset JSONL streaming — the port of `adapters/_streaming.py`.
//!
//! Two invariants come from that module and every JSONL adapter depends on
//! both:
//!
//! * **`seq` is the byte position where a line starts.** It is the same number
//!   the ingest watermark stores, so a resumed read is a `seek` and a
//!   strictly-greater-than comparison — not a re-parse.
//! * **A file that cannot be read yields nothing and never raises.** Oversize,
//!   missing, unreadable, unseekable: every one of them is "no records", because
//!   an exception here aborts the whole batch's ingest.
//!
//! The one behavior that did *not* port is the `logging.warning` on each skip:
//! this crate has no logger to call (the workspace has not chosen a facade yet).
//! Every silent-skip site below is marked `LOG:` so the wave that adds one can
//! grep for them.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// Files larger than this are skipped entirely (`_streaming.py:37`).
///
/// 128 MB is ~two orders of magnitude over the largest real session log; a file
/// that big is a runaway logger, and skipping beats OOMing the ingest worker.
pub const MAX_SESSION_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// A soft hint for adapters that read single-document formats
/// (`_streaming.py:45`). Line iteration streams either way.
pub const STREAM_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// The deepest nesting [`parse_json`] accepts — orjson's ceiling, to the level.
///
/// `claude.py` parses with `orjson.loads` and catches
/// `(orjson.JSONDecodeError, ValueError)`, so *its* limit is the one this
/// constant matches. Measured on the reference interpreter (orjson 3.11.9)
/// rather than read off a doc page: depth 1024 parses, depth 1025 raises
/// `JSONDecodeError: array and object recursion depth exceeded` — which is a
/// `ValueError`, so the adapter skips that line. Arrays and objects have the
/// same ceiling.
///
/// ```sh
/// .venv/bin/python -c 'import orjson
/// d = lambda n: "["*n + "1" + "]"*n
/// orjson.loads(d(1024)); orjson.loads(d(1025))'
/// # orjson.JSONDecodeError: array and object recursion depth exceeded: line 1 column 1026
/// ```
///
/// **CORRECTION (wave 2c, measured 2026-07-31).** An earlier revision of this
/// comment said *every* Python adapter parses with `orjson`. It does not:
/// `claude.py` is the **only** `orjson` caller in `stackunderflow/adapters/`,
/// and the other 18 use the stdlib `json`, whose ceiling is 9997 (it raises
/// `RecursionError` — not a `ValueError` — at 9998, which escapes every
/// adapter's `except` clause and kills that file's ingest). So for 19 of the 20
/// providers this constant is ~9× *stricter* than the original: a line nested
/// 1025–9997 deep is a record Python ingests and this port refuses. It is
/// counted rather than swallowed ([`deep_json_skips`]) and no corpus measured so
/// far comes within an order of magnitude of it, so the constant is left where
/// it is — moving it would change behaviour for 19 landed providers and their
/// pinned tests, which is a decision for the wave that decides to make it, not a
/// side effect of a doc fix. The stdlib also accepts `NaN` / `Infinity` /
/// `-Infinity` / `1e999`, all of which `serde_json` refuses; that class is
/// recorded on [`crate::hermes`] with the same measurement.
///
/// ```sh
/// .venv/bin/python -c 'import json
/// d = lambda n: "["*n + "1" + "]"*n
/// json.loads(d(9997)); json.loads(d(9998))'
/// # RecursionError: maximum recursion depth exceeded
/// ```
pub const MAX_JSON_DEPTH: usize = 1024;

/// How many lines [`parse_json`] has refused for exceeding [`MAX_JSON_DEPTH`].
///
/// A process-wide counter rather than a return value on purpose: the adapters
/// are deliberately free of storage, logging and diagnostics plumbing (that is
/// what let 20 of them be written in parallel), and threading a diagnostics sink
/// through every `read()` signature to report an event that has never yet
/// happened on real data would be the tail wagging the dog. It is a counter and
/// not a boolean because "how many" is the question a caller actually asks.
///
/// [`deep_json_skips`] reads it; `stax-adapter-parity` prints it to stderr when
/// it is non-zero, which is what keeps this from being another silent drop.
static DEEP_JSON_SKIPS: AtomicU64 = AtomicU64::new(0);

/// The number of lines refused for nesting deeper than [`MAX_JSON_DEPTH`] since
/// this process started.
///
/// Zero on every real corpus measured so far (the deepest line in 55,647 live
/// Claude records is nowhere near it); a non-zero value means records were
/// dropped and should be reported, not swallowed.
#[must_use]
pub fn deep_json_skips() -> u64 {
    DEEP_JSON_SKIPS.load(Ordering::Relaxed)
}

/// Parse one JSONL line the way the Python adapters do — `orjson.loads` with
/// its errors swallowed to "skip this line".
///
/// **This exists because `serde_json::from_slice` is not that function.** Its
/// default nesting limit is 128 containers, orjson's is
/// [`MAX_JSON_DEPTH`]=1024, and every adapter's parse site is
/// `let Ok(obj) = … else { continue }` — so each of the ~900 depths in between
/// is a *valid* record that Python ingests and this port dropped, with exit 0
/// and not a word on stderr. Measured on a five-line file nested
/// 100/128/129/200/900 deep: `stax-adapter-parity records claude` emitted 1
/// record where `parity/python_reference.py` emitted 5.
///
/// The fast path is unchanged — the overwhelmingly common line is a handful of
/// levels deep and parses on the first attempt, at the same speed as before.
/// Only a line that *fails* is re-parsed with
/// `Deserializer::disable_recursion_limit`, and only after a byte scan has
/// bounded its depth, so a runaway document (a malformed log with a million
/// open brackets) can neither exhaust the stack nor be parsed into memory. The
/// retry is deliberately triggered by *any* first-attempt failure rather than
/// by recognising serde_json's "recursion limit exceeded" message: matching an
/// upstream error string is a silent regression waiting for a point release,
/// and a genuinely malformed line — which Python also skips — merely costs a
/// second parse it was never going to survive.
///
/// Returns `None` for exactly what orjson refuses: malformed JSON, trailing
/// garbage after the value, and nesting past [`MAX_JSON_DEPTH`].
#[must_use]
pub fn parse_json(bytes: &[u8]) -> Option<Value> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) => Some(value),
        Err(_) => parse_json_deep(bytes),
    }
}

/// Stack for the deep parse, sized from the measured cost of a level.
///
/// `serde_json` recurses once per nested container, and an unoptimised build's
/// frame is the expensive one: 900 levels overflowed a stock 2 MB test thread
/// (~2.3 KB per level, against a few hundred bytes with optimisation). At
/// [`MAX_JSON_DEPTH`] that is ~2.4 MB, so 32 MB is an order of magnitude of
/// headroom on the *worst* build. It costs nothing to be generous: a thread
/// stack is reserved address space, and only the pages actually touched are
/// ever committed.
const DEEP_PARSE_STACK_BYTES: usize = 32 * 1024 * 1024;

/// The slow path of [`parse_json`]: depth-bounded, recursion-limit-free, and
/// run on a stack that can hold [`MAX_JSON_DEPTH`] frames.
///
/// The thread is the second half of the stack guarantee. Bounding the depth
/// caps how *many* frames the parser can push, but not how big they are, and
/// the caller's stack is not ours to spend — an ingest worker, a hook, or a
/// test harness each hands us a different one (libtest's is 2 MB, and that is
/// where the overflow was first seen). A stack overflow aborts the process
/// where the bug being fixed merely dropped a record, so the deep path gets a
/// stack it is known to fit in. It is spawned per deep line, which is free at
/// the frequency this path actually runs: zero times on every corpus measured.
fn parse_json_deep(bytes: &[u8]) -> Option<Value> {
    if exceeds_depth(bytes, MAX_JSON_DEPTH) {
        DEEP_JSON_SKIPS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("stax-deep-json".to_string())
            .stack_size(DEEP_PARSE_STACK_BYTES)
            .spawn_scoped(scope, || parse_json_unbounded(bytes))
            .ok()
            // A spawn failure (or a panic in the parser, which would be a
            // serde_json bug) reads as "this line did not parse" — the same
            // answer the caller already handles.
            .and_then(|worker| worker.join().ok())
            .flatten()
    })
}

/// The parse itself, on the deep path's stack.
fn parse_json_unbounded(bytes: &[u8]) -> Option<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    deserializer.disable_recursion_limit();
    // `into_iter` rather than `Value::deserialize`, which would need `serde`
    // itself as a dependency of this crate for the trait to be in scope.
    let mut values = deserializer.into_iter::<Value>();
    let value = values.next()?.ok()?;
    // `from_slice` rejects trailing content after the value and so must this:
    // the stream deserializer would happily read `{"a":1} {"b":2}` as two
    // documents, where orjson calls it "trailing garbage" and raises.
    let rest = bytes.get(values.byte_offset()..)?;
    rest.iter()
        .all(|byte| byte.is_ascii_whitespace())
        .then_some(value)
}

/// Whether `bytes` opens more than `limit` nested containers anywhere.
///
/// A byte scan, not a parse: it runs only on the failed-once path, and its job
/// is to bound the work the real parser is about to do. Brackets inside strings
/// do not count, which is the only subtlety — hence the escape tracking. On
/// malformed input the count can be wrong in either direction, and that is
/// harmless: the parse that follows rejects the line anyway.
fn exceeds_depth(bytes: &[u8], limit: usize) -> bool {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// The size of `path`, or `None` when it is unreadable or over the cap.
///
/// `_streaming.py:stat_or_skip` / `_file_size_or_skip`. `None` means "yield
/// nothing, do not raise".
#[must_use]
pub fn stat_or_skip(path: &Path) -> Option<u64> {
    // LOG: python warns "Cannot stat %s".
    let size = std::fs::metadata(path).ok()?.len();
    if size > MAX_SESSION_FILE_BYTES {
        // LOG: python warns "Skipping %s: size %d exceeds cap %d".
        return None;
    }
    Some(size)
}

/// `(line_offset, raw_line)` over a JSONL file, starting at `since_offset`.
///
/// The port of `_streaming.py:iter_jsonl_lines`. Lines keep their trailing
/// `\n` exactly as Python's binary-mode file iteration does (it splits on `\n`
/// only — no universal-newline translation), because `offset += len(raw_line)`
/// is what keeps `seq` aligned with the file's bytes.
pub struct JsonlLines {
    reader: Option<BufReader<File>>,
    offset: u64,
}

impl JsonlLines {
    /// Open `path` for line iteration from `since_offset`.
    ///
    /// Never fails: an unreadable, oversize, or unseekable file yields an
    /// iterator that is immediately exhausted.
    #[must_use]
    pub fn open(path: &Path, since_offset: i64) -> Self {
        let empty = Self {
            reader: None,
            offset: 0,
        };
        if stat_or_skip(path).is_none() {
            return empty;
        }
        // LOG: python warns "Cannot read %s".
        let Ok(file) = File::open(path) else {
            return empty;
        };
        let mut reader = BufReader::new(file);
        let mut offset = 0_u64;
        if since_offset > 0 {
            // LOG: python warns "Cannot seek %s to offset %d".
            #[allow(
                clippy::cast_sign_loss,
                reason = "guarded by the `since_offset > 0` branch"
            )]
            let target = since_offset as u64;
            if reader.seek(SeekFrom::Start(target)).is_err() {
                return empty;
            }
            offset = target;
        }
        Self {
            reader: Some(reader),
            offset,
        }
    }
}

impl Iterator for JsonlLines {
    type Item = (i64, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        let reader = self.reader.as_mut()?;
        let mut line = Vec::new();
        match read_until_newline(reader, &mut line) {
            Ok(0) => {
                self.reader = None;
                None
            }
            Ok(read) => {
                let line_offset = self.offset;
                self.offset += read as u64;
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "file offsets past i64::MAX are unreachable — the \
                     128 MB cap in `stat_or_skip` bounds this to 2^27"
                )]
                Some((line_offset as i64, line))
            }
            Err(_) => {
                // A mid-stream read error is the same contract as a missing
                // file: stop, do not raise.
                self.reader = None;
                None
            }
        }
    }
}

/// `BufRead::read_until` without requiring the caller to import the trait, and
/// with the byte count Python's `len(raw_line)` reports.
fn read_until_newline(reader: &mut BufReader<File>, out: &mut Vec<u8>) -> std::io::Result<usize> {
    use std::io::BufRead;
    reader.read_until(b'\n', out)
}

/// Read at most `limit` bytes from the head of `path` (`codex.py:236` —
/// `fh.read(max(int(upto), 0))`), or `None` when the file cannot be opened.
#[must_use]
pub fn read_prefix(path: &Path, limit: i64) -> Option<Vec<u8>> {
    let limit = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    let mut handle = file.take(limit as u64);
    handle.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// `bytes.splitlines()` — splits on `\n`, `\r`, and `\r\n`, dropping the
/// terminator (`codex.py:240`).
///
/// Python's *str* `splitlines` also breaks on `\v\f\x1c\x1d\x1e\x85` and the
/// Unicode separators; the `bytes` flavour used here does not, and neither does
/// this.
#[must_use]
pub fn splitlines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                out.push(&data[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&data[start..i]);
                i += if data.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// `bytes.splitlines(keepends=True)` — the same split as [`splitlines`], with
/// each terminator left on the end of its line (`cursor_agent.py:366`).
///
/// The terminator is what makes this a different function rather than a flag on
/// the other one: `cursor_agent._read_text` computes `offset += len(line_bytes)`
/// as it walks, so `seq` is only a byte offset if the `\n` (or `\r\n`) is still
/// part of the line's length.
#[must_use]
pub fn splitlines_keepends(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                out.push(&data[start..=i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                let end = if data.get(i + 1) == Some(&b'\n') {
                    i + 2
                } else {
                    i + 1
                };
                out.push(&data[start..end]);
                i = end;
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// `bytes.strip()` — ASCII whitespace only, both ends.
///
/// Python strips `b' \t\n\r\x0b\x0c'`; `u8::is_ascii_whitespace` is
/// `b' \t\n\r\x0c'` (no vertical tab), so `\x0b` is added explicitly.
#[must_use]
pub fn py_bytes_strip(line: &[u8]) -> &[u8] {
    let is_space = |b: u8| b.is_ascii_whitespace() || b == 0x0b;
    let start = line.iter().position(|b| !is_space(*b));
    let Some(start) = start else { return &[] };
    let end = line
        .iter()
        .rposition(|b| !is_space(*b))
        .unwrap_or(line.len() - 1);
    &line[start..=end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("stax-jsonl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join(name)
    }

    #[test]
    fn line_offsets_are_byte_positions() {
        let path = scratch("offsets.jsonl");
        let mut fh = File::create(&path).expect("create");
        fh.write_all(b"aa\nbbbb\nc\n").expect("write");
        drop(fh);

        let lines: Vec<_> = JsonlLines::open(&path, 0).collect();
        assert_eq!(
            lines,
            vec![
                (0, b"aa\n".to_vec()),
                (3, b"bbbb\n".to_vec()),
                (8, b"c\n".to_vec()),
            ]
        );
    }

    #[test]
    fn since_offset_seeks_and_keeps_absolute_offsets() {
        let path = scratch("seek.jsonl");
        let mut fh = File::create(&path).expect("create");
        fh.write_all(b"aa\nbbbb\nc\n").expect("write");
        drop(fh);

        let lines: Vec<_> = JsonlLines::open(&path, 3).collect();
        assert_eq!(lines, vec![(3, b"bbbb\n".to_vec()), (8, b"c\n".to_vec())]);
    }

    #[test]
    fn a_missing_file_yields_nothing_rather_than_failing() {
        let path = scratch("does-not-exist.jsonl");
        let _ = std::fs::remove_file(&path);
        assert_eq!(JsonlLines::open(&path, 0).count(), 0);
        assert_eq!(stat_or_skip(&path), None);
    }

    #[test]
    fn splitlines_matches_bytes_splitlines() {
        assert_eq!(
            splitlines(b"a\nb\r\nc\rd"),
            vec![&b"a"[..], b"b", b"c", b"d"]
        );
        assert_eq!(splitlines(b"a\n"), vec![&b"a"[..]]);
        assert_eq!(splitlines(b""), Vec::<&[u8]>::new());
    }

    #[test]
    fn splitlines_keepends_leaves_the_terminator_on() {
        assert_eq!(
            splitlines_keepends(b"a\nb\r\nc\rd"),
            vec![&b"a\n"[..], b"b\r\n", b"c\r", b"d"]
        );
        assert_eq!(splitlines_keepends(b"a\n"), vec![&b"a\n"[..]]);
        assert_eq!(splitlines_keepends(b""), Vec::<&[u8]>::new());
        // The property `cursor_agent._read_text` depends on: the pieces
        // reassemble the file, so summing their lengths walks byte offsets.
        let data = b"user: hi\r\nA: there\n\n";
        assert_eq!(
            splitlines_keepends(data)
                .iter()
                .map(|line| line.len())
                .sum::<usize>(),
            data.len()
        );
    }

    #[test]
    fn strip_matches_python_bytes_strip() {
        assert_eq!(py_bytes_strip(b"  a b \n"), b"a b");
        assert_eq!(py_bytes_strip(b"\x0b\x0c"), b"");
        assert_eq!(py_bytes_strip(b""), b"");
    }

    fn nested(depth: usize) -> String {
        format!("{}1{}", "[".repeat(depth), "]".repeat(depth))
    }

    #[test]
    fn deep_records_parse_instead_of_being_dropped() {
        // The finding, in one assertion: serde_json's default limit is 128, so
        // everything from 129 up used to vanish from the record stream with no
        // error and no diagnostic. These are the depths the repro file used.
        for depth in [1, 2, 100, 127, 128, 129, 200, 900, MAX_JSON_DEPTH] {
            let text = nested(depth);
            let value = parse_json(text.as_bytes())
                .unwrap_or_else(|| panic!("depth {depth} is valid JSON orjson parses"));
            assert_eq!(serde_json::to_string(&value).expect("re-render"), text);
        }
    }

    #[test]
    fn objects_nest_as_deeply_as_arrays() {
        let text = format!("{}1{}", r#"{"a":"#.repeat(900), "}".repeat(900));
        assert!(parse_json(text.as_bytes()).is_some());
    }

    #[test]
    fn nesting_past_orjsons_ceiling_is_refused_and_counted() {
        // Bug-for-bug: orjson raises at 1025, the Python adapter catches it and
        // skips the line, so this port skips it too — but it counts the skip
        // rather than swallowing it. The counter is process-wide and the test
        // binary is threaded, so the assertion is on the increase, not on the
        // absolute value.
        let before = deep_json_skips();
        assert_eq!(parse_json(nested(MAX_JSON_DEPTH + 1).as_bytes()), None);
        assert_eq!(parse_json(nested(20_000).as_bytes()), None);
        assert!(
            deep_json_skips() >= before + 2,
            "both skips must be counted"
        );
    }

    #[test]
    fn a_million_open_brackets_neither_parse_nor_crash() {
        // The reason the depth scan runs before the unlimited parse: this input
        // would exhaust any stack, and a stack overflow is an abort, not a skip.
        let before = deep_json_skips();
        assert_eq!(parse_json("[".repeat(1_000_000).as_bytes()), None);
        assert!(deep_json_skips() > before);
    }

    #[test]
    fn the_ordinary_rejections_still_reject() {
        // Everything orjson refuses must still be refused — the deep path is a
        // second chance at parsing, not a second chance at validity.
        for text in [
            "",
            "   ",
            "{",
            "{\"a\": }",
            "not json",
            r#"{"a":1} trailing"#,
            r#"{"a":1} {"b":2}"#,
            "[1,2,",
            "\"unterminated",
        ] {
            assert_eq!(parse_json(text.as_bytes()), None, "input {text:?}");
        }
    }

    #[test]
    fn a_deep_line_keeps_its_key_order_like_the_shallow_path() {
        // `preserve_order` is the byte-parity contract, and the deep path is a
        // different deserializer call — it must honour it too.
        let mut text = String::new();
        for _ in 0..200 {
            text.push_str(r#"{"zeta":1,"alpha":2,"mid":"#);
        }
        text.push('3');
        text.push_str(&"}".repeat(200));
        let value = parse_json(text.as_bytes()).expect("200 deep parses");
        assert_eq!(serde_json::to_string(&value).expect("re-render"), text);
    }

    #[test]
    fn brackets_inside_strings_are_not_nesting() {
        let text = format!(r#"{{"s":"{}"}}"#, "[".repeat(4_000));
        assert!(
            parse_json(text.as_bytes()).is_some(),
            "a string full of brackets is one level deep"
        );
        // …including when the quote before them is escaped.
        let text = format!(r#"{{"s":"\"{}"}}"#, "{".repeat(4_000));
        assert!(parse_json(text.as_bytes()).is_some());
    }

    #[test]
    fn the_depth_scan_counts_containers_the_way_orjson_does() {
        assert!(!exceeds_depth(b"[1]", 1));
        assert!(exceeds_depth(b"[[1]]", 1));
        assert!(!exceeds_depth(b"[[1]]", 2));
        assert!(!exceeds_depth(br#"{"a": [1, {"b": 2}]}"#, 3));
        assert!(exceeds_depth(br#"{"a": [1, {"b": 2}]}"#, 2));
        // Siblings are not cumulative.
        assert!(!exceeds_depth(b"[[1],[2],[3]]", 2));
        assert!(!exceeds_depth(br#""[[[[""#, 1));
    }
}
