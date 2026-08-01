//! The five CPython string operations the hook renderers depend on.
//!
//! Every one of them is character-indexed, not byte-indexed, and every one of
//! them appears inside a *budget* — `text[:399]`, `line[:10]`, `snippet[:139]`.
//! A byte slice would produce a different string on the first non-ASCII
//! character and panic on the first multi-byte boundary, and the reference's own
//! truncation marker is `…`, so non-ASCII is not hypothetical: it is what the
//! truncator itself inserts.

/// `text[:n]` — the first `n` **characters**.
#[must_use]
pub fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// `len(text)` — Python's length, in characters.
#[must_use]
pub fn len_chars(text: &str) -> usize {
    text.chars().count()
}

/// `text.strip()` / `.rstrip()` / `.lstrip()` with no argument — Unicode
/// whitespace on the relevant end(s).
///
/// `char::is_whitespace` is the White_Space property, which is what
/// `str.strip()` uses.
#[must_use]
pub fn rstrip(text: &str) -> &str {
    text.trim_end_matches(char::is_whitespace)
}

/// `" ".join(text.split())` — collapse every run of whitespace to one space and
/// drop leading/trailing runs. `str.split()` with no argument, exactly.
#[must_use]
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The truncation both `inject._trim` and `inject._clip` perform:
/// `text[: max(1, limit - 1)].rstrip() + "…"`, applied only when over budget.
#[must_use]
pub fn clip(text: &str, limit: usize) -> String {
    if len_chars(text) <= limit {
        return text.to_string();
    }
    let keep = limit.saturating_sub(1).max(1);
    format!("{}…", rstrip(&head(text, keep)))
}

/// `inject._trim` — collapse whitespace, then clip.
#[must_use]
pub fn trim(text: &str, limit: usize) -> String {
    clip(&collapse_whitespace(text), limit)
}

/// `f"{value:.2f}"` — CPython's fixed-point formatting.
///
/// Both languages round the exact binary value to the nearest 2-decimal string
/// and break ties to even, so `{:.2}` is the same function. Spelled out because
/// "the cost column" is a number a maintainer reads.
#[must_use]
pub fn format_2f(value: f64) -> String {
    format!("{value:.2}")
}

/// `os.path.basename(p)` for POSIX — everything after the last `/`.
#[must_use]
pub fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[index + 1..],
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slicing_counts_characters() {
        // 6 characters, 12 bytes. A byte slice would cut mid-codepoint.
        let text = "日本語です。ね";
        assert_eq!(head(text, 3), "日本語");
        assert_eq!(len_chars(text), 7);
    }

    #[test]
    fn clip_only_fires_over_budget() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdefghij", 10), "abcdefghij");
        assert_eq!(clip("abcdefghijk", 10), "abcdefghi…");
        // The rstrip runs BEFORE the ellipsis is appended.
        assert_eq!(clip("abcdefgh  ijk", 10), "abcdefgh…");
        // limit 1 → max(1, 0) = 1 character kept.
        assert_eq!(clip("abc", 1), "a…");
    }

    #[test]
    fn trim_collapses_first() {
        assert_eq!(trim("  a\n\tb   c  ", 100), "a b c");
        assert_eq!(trim("a\nb", 3), "a b");
    }

    #[test]
    fn basename_is_posix() {
        assert_eq!(basename("/a/b/c.py"), "c.py");
        assert_eq!(basename("c.py"), "c.py");
        assert_eq!(basename("/a/b/"), "");
    }

    #[test]
    fn two_decimals_match_python() {
        assert_eq!(format_2f(0.0), "0.00");
        assert_eq!(format_2f(1.005), "1.00"); // the classic: 1.005 is < 1.005
        assert_eq!(format_2f(2.675), "2.67");
        assert_eq!(format_2f(12.345), "12.35");
    }
}
