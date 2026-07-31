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

/// Files larger than this are skipped entirely (`_streaming.py:37`).
///
/// 128 MB is ~two orders of magnitude over the largest real session log; a file
/// that big is a runaway logger, and skipping beats OOMing the ingest worker.
pub const MAX_SESSION_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// A soft hint for adapters that read single-document formats
/// (`_streaming.py:45`). Line iteration streams either way.
pub const STREAM_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

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
    fn strip_matches_python_bytes_strip() {
        assert_eq!(py_bytes_strip(b"  a b \n"), b"a b");
        assert_eq!(py_bytes_strip(b"\x0b\x0c"), b"");
        assert_eq!(py_bytes_strip(b""), b"");
    }
}
