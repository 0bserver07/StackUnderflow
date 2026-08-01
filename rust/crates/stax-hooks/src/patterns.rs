//! The two `reports/patterns.py` normalisers, reused verbatim.
//!
//! `proactive.py` imports `_normalise_command` and `_normalise_signature` from
//! the mining report and calls them with a comment that says why: *"VERBATIM
//! reuse — cluster-key parity"*. These functions produce the **cache keys**. The
//! miner writes `proactive_signals.json` keyed by their output and the hook looks
//! a live command / error up by re-deriving the same key, so a normaliser that
//! differs by one character is not a rendering difference — it is a permanent,
//! silent cache miss that looks exactly like "no pattern found".
//!
//! `reports/patterns.py` is 1,097 lines and is not otherwise ported (it is one of
//! the eight deferred large services). Rather than pull that whole module in, the
//! two functions the hook path reaches live here, byte-identical, with the same
//! constants and the same regexes. When `patterns` lands they collapse into it.

use std::sync::LazyLock;

use regex::Regex;

/// `patterns._basename` — *not* `os.path.basename`. It normalises Windows
/// separators first and strips trailing ones, so `a\b\c\` → `c`.
#[must_use]
pub fn basename(path: &str) -> String {
    let normalised = path.replace('\\', "/");
    let trimmed = normalised.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(index) => trimmed[index + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

static HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{8,}\b").expect("literal"));
static NUM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("literal"));
static PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:[A-Za-z]:)?(?:/[^\s'":,)\]]+){2,}"#).expect("literal"));
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("literal"));

/// `patterns._normalise_signature` — collapse an error body into a stable
/// cross-session signature.
///
/// First meaningful line only; absolute paths → their basename; long hex runs and
/// numbers → placeholders; whitespace collapsed; truncated at 160. Two
/// occurrences of `File /a/b/foo.py:212 not found` and `File /x/foo.py:7 not
/// found` normalise identically.
#[must_use]
pub fn normalise_signature(text: &str) -> String {
    let mut line = "";
    // `str.splitlines()` splits on more than `\n`, but every caller here feeds
    // captured tool output; `\n` and `\r\n` are what appear, and `\r` is left on
    // the front of the next line by neither implementation because `.strip()`
    // removes it.
    for candidate in text.split('\n') {
        let candidate = candidate.trim();
        if !candidate.is_empty() {
            line = candidate;
            break;
        }
    }
    let line = PATH_RE.replace_all(line, |caps: &regex::Captures<'_>| basename(&caps[0]));
    let line = HEX_RE.replace_all(&line, "<hex>");
    let line = NUM_RE.replace_all(&line, "<n>");
    let line = WS_RE.replace_all(&line, " ");
    let line = line.trim();
    if line.is_empty() {
        "<empty error body>".to_string()
    } else {
        crate::pystr::head(line, 160)
    }
}

static ENV_ASSIGN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=\S*\s+").expect("literal"));
static CD_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^cd\s+\S+\s*&&\s*").expect("literal"));

/// `patterns._SUBCOMMAND_HEADS` / `_SCRIPT_HEADS` — the multi-command CLIs whose
/// first non-flag token is part of the cluster key.
///
/// Pulled from `reports/patterns.py`; `_SCRIPT_HEADS` differ in that the
/// subcommand is basenamed (`python /a/b/manage.py` → `python manage.py`).
const SUBCOMMAND_HEADS: [&str; 23] = [
    "apt",
    "brew",
    "bundle",
    "cargo",
    "composer",
    "docker",
    "dotnet",
    "gh",
    "git",
    "go",
    "gradle",
    "kubectl",
    "make",
    "mvn",
    "npm",
    "pip",
    "pip3",
    "pnpm",
    "poetry",
    "stackunderflow",
    "terraform",
    "uv",
    "yarn",
];
const SCRIPT_HEADS: [&str; 7] = ["bash", "node", "npx", "python", "python3", "ruby", "sh"];

/// `patterns._normalise_command` — reduce a Bash command line to its cluster key
/// (`"npm install"`, `"pytest"`).
///
/// Strips leading `cd X &&` hops and env assignments (bounded to three rounds —
/// malformed input cannot loop), then keys on the executable basename plus, for
/// the known multi-command CLIs, the first non-flag subcommand token.
#[must_use]
pub fn normalise_command(cmd: &str) -> String {
    let mut s = cmd.trim().to_string();
    for _ in 0..3 {
        let stripped = ENV_ASSIGN_RE.replace(&s, "").into_owned();
        let stripped = CD_PREFIX_RE.replace(&stripped, "").trim().to_string();
        if stripped == s {
            break;
        }
        s = stripped;
    }
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let Some(first) = tokens.first() else {
        return "<empty>".to_string();
    };
    let head = basename(first);
    let mut sub = String::new();
    let is_subcommand_head = SUBCOMMAND_HEADS.contains(&head.as_str());
    let is_script_head = SCRIPT_HEADS.contains(&head.as_str());
    if is_subcommand_head || is_script_head {
        for token in &tokens[1..] {
            if token.starts_with('-') {
                continue;
            }
            sub = if is_script_head {
                basename(token)
            } else {
                (*token).to_string()
            };
            break;
        }
    }
    crate::pystr::head(format!("{head} {sub}").trim(), 80)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signature_erases_the_variable_parts() {
        assert_eq!(
            normalise_signature("File /a/b/foo.py:212 not found"),
            normalise_signature("File /x/y/foo.py:7 not found")
        );
        assert_eq!(
            normalise_signature("File /a/b/foo.py:212 not found"),
            "File foo.py:<n> not found"
        );
        assert_eq!(
            normalise_signature("commit deadbeefcafe0123 failed"),
            "commit <hex> failed"
        );
        assert_eq!(normalise_signature(""), "<empty error body>");
        assert_eq!(normalise_signature("   \n  \n"), "<empty error body>");
        // The FIRST meaningful line only.
        assert_eq!(normalise_signature("\n\n  boom  \nsecond"), "boom");
    }

    #[test]
    fn the_signature_is_bounded() {
        let long = format!("E{}", "x".repeat(400));
        assert_eq!(crate::pystr::len_chars(&normalise_signature(&long)), 160);
    }

    #[test]
    fn the_cluster_key_strips_prefixes_and_keeps_subcommands() {
        assert_eq!(
            normalise_command("npm install --save-dev foo"),
            "npm install"
        );
        assert_eq!(normalise_command("cd /repo && git status"), "git status");
        assert_eq!(normalise_command("FOO=1 BAR=2 cargo test"), "cargo test");
        assert_eq!(normalise_command("/usr/local/bin/pytest -q"), "pytest");
        assert_eq!(
            normalise_command("python /a/b/manage.py migrate"),
            "python manage.py"
        );
        assert_eq!(normalise_command("   "), "<empty>");
        // A flag before the subcommand is skipped, not keyed on.
        assert_eq!(normalise_command("git -C /repo status"), "git /repo");
    }

    #[test]
    fn basename_normalises_windows_separators() {
        assert_eq!(basename(r"a\b\c"), "c");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("plain"), "plain");
    }
}
