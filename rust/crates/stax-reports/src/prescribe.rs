//! `reports/prescribe.py` — descriptive waste findings turned into actions.
//!
//! | Item | Python | Reached from |
//! |---|---|---|
//! | [`generate_claudemd_preview`] | same | `POST /api/optimize/claudemd-preview`, `GET …/prescriptions` |
//! | [`build_routing_recommendations`] | same | `GET /api/optimize/prescriptions` |
//! | [`unified_diff`] | `difflib.unified_diff` | the preview's `preview_diff` |
//!
//! Both generators are advisory and read-only, and the CLAUDE.md one is a
//! **pure function**: text in, dict out. There is no path parameter, no
//! `open()`, and no filesystem import anywhere in the Python module — a
//! source-scan test in `tests/stackunderflow/reports/test_prescribe.py` locks
//! that down. The port keeps the property: nothing in this file touches
//! `std::fs` except the model-catalog read, which is package DATA and is
//! injected as a path.
//!
//! # `difflib` had to come with it
//!
//! `preview_diff` is `"".join(difflib.unified_diff(...))`, and a unified diff is
//! not a format you can approximate — the hunk headers are derived from the
//! opcode groups, so a matcher that finds a *different but equally valid*
//! alignment produces different bytes. [`SequenceMatcher`] is therefore a
//! transcription of CPython's `Lib/difflib.py`, not a re-derivation, down to:
//!
//! * **`autojunk`.** For `len(b) >= 200`, elements appearing in more than
//!   `len(b)//100 + 1` positions are dropped from the `b2j` index — the
//!   "popular element" heuristic. It changes which blocks match on any document
//!   over 200 lines, which every bloated CLAUDE.md is.
//! * **Popular elements are pruned from the index but are NOT junk,** so
//!   `find_longest_match`'s two "extend by non-junk" loops still walk over them.
//!   Dropping that distinction silently shortens every match.
//! * **`get_matching_blocks` uses a LIFO queue and sorts at the end**, so the
//!   recursion order does not matter but the adjacency collapse that follows
//!   does.
//! * **`get_grouped_opcodes` rewrites the first and last opcode in place**
//!   before it starts grouping, and the trailing `if group and not (len(group)
//!   == 1 and group[0][0] == 'equal')` suppresses a final all-equal group.
//!
//! # The other things a careless port gets wrong
//!
//! * **`str.splitlines(keepends=True)` splits on eleven boundaries**, not just
//!   `\n` — `\r`, `\v`, `\f`, `\x1c`–`\x1e`, `\x85`, ` `, ` ` all
//!   count, and `\r\n` is one boundary. Meanwhile `_parse_blocks` uses
//!   `text.split("\n")`, which splits on `\n` alone. Two different line notions
//!   in one function, and both are reproduced.
//! * **`_render(parse(text)) == text` for an untouched document.** The block
//!   parser keeps every line verbatim and `_render` joins with `\n`, so a
//!   trailing newline becomes a final empty "blank" block and survives.
//! * **A no-op transform returns the ORIGINAL block list**, not the rebuilt
//!   one — `if not dropped: return blocks, None`. So a rule that changed
//!   nothing cannot perturb the next rule's input.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value};
use stax_etl::pricing::RawTokens;
use stax_etl::pricing::costs::PricingEngine;

use crate::optimize::{
    CLAUDE_MD_TOKEN_THRESHOLD, WASTE_PRICING_MODEL, WasteKind, approx_tokens, round_half_even,
    tokens_to_usd,
};
use crate::pyops::char_prefix;
use crate::scope::Scope;

// ── tunables — CLAUDE.md slimmer ─────────────────────────────────────────────

/// `SECTION_EXTRACT_TOKEN_THRESHOLD = 600`.
const SECTION_EXTRACT_TOKEN_THRESHOLD: i64 = 600;
/// `BLANK_RUN_COLLAPSE_AT = 3`.
const BLANK_RUN_COLLAPSE_AT: usize = 3;
/// `DEDUPE_MIN_CHARS = 60` — shorter repeats are usually deliberate.
const DEDUPE_MIN_CHARS: usize = 60;

/// `from stackunderflow.services.context_budget import DEFAULT_SESSIONS_PER_MONTH`.
///
/// Re-exported rather than re-declared: Python imports it here AND in
/// `routes/optimize.py` (as the POST body's field default), and two copies of a
/// magic 100 is exactly how the two would drift.
pub use crate::context_budget::DEFAULT_SESSIONS_PER_MONTH;

// ── tunables — model routing ─────────────────────────────────────────────────

/// `LOW_REASONING_SHARE = 0.05`.
const LOW_REASONING_SHARE: f64 = 0.05;
/// `HIGH_REASONING_SHARE = 0.25`.
const HIGH_REASONING_SHARE: f64 = 0.25;
/// `SHORT_OUTPUT_TOKENS_PER_EVENT = 300`.
const SHORT_OUTPUT_TOKENS_PER_EVENT: f64 = 300.0;
/// `MIN_SAVINGS_SHARE = 0.25`.
const MIN_SAVINGS_SHARE: f64 = 0.25;
/// `MIN_WINDOW_SAVINGS_USD = 0.01`.
const MIN_WINDOW_SAVINGS_USD: f64 = 0.01;
/// `QUALITY_SCORE_FLOOR = 3.5` — on the 0–5 scale `session_quality_metrics` uses.
const QUALITY_SCORE_FLOOR: f64 = 3.5;
/// `BASELINE_MONTH_DAYS = 30`.
const BASELINE_MONTH_DAYS: f64 = 30.0;

/// `_ROUTING_CAVEATS`.
const ROUTING_CAVEATS: [&str; 2] = [
    "Candidate costs are a rate-card swap of the observed token shape, not a \
     re-run — a different model may tokenize differently or need more/fewer \
     output tokens for the same task.",
    "Monthly figures extrapolate the window spend by 30 / observed active \
     days; they assume the window is representative.",
];

// ══════════════════════════════════════════════════════════════════════════
// (a) the CLAUDE.md slimmer
// ══════════════════════════════════════════════════════════════════════════

/// `_Block.kind` ∈ {heading, fence, comment, blank, para}.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Heading,
    Fence,
    Comment,
    Blank,
    Para,
}

/// `@dataclass class _Block` — the original lines, verbatim.
#[derive(Debug, Clone)]
struct Block {
    kind: Kind,
    lines: Vec<String>,
}

impl Block {
    /// `_Block.text` — `"\n".join(self.lines)`.
    fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// `_HEADING_RE = ^#{1,6}\s` — matched against the RAW line, not the stripped one.
fn is_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return false;
    }
    // `\s` is a single whitespace character, and it must be present.
    line.chars().nth(hashes).is_some_and(char::is_whitespace)
}

/// `_FENCE_RE = ^(```|~~~)` — matched against the STRIPPED line. Returns the marker.
fn fence_marker(stripped: &str) -> Option<&'static str> {
    if stripped.starts_with("```") {
        Some("```")
    } else if stripped.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// `_parse_blocks(text)` — line-exact; every input line lands in one block.
///
/// Fenced code is opaque: nothing inside a fence is classified as a comment,
/// heading or blank run, so no transform can touch a code example. An
/// unterminated fence or comment runs to EOF.
fn parse_blocks(text: &str) -> Vec<Block> {
    // `text.split("\n")` — `\n` only, unlike `splitlines()`.
    let lines: Vec<&str> = text.split('\n').collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut i = 0;
    let n = lines.len();
    while i < n {
        let line = lines[i];
        let stripped = line.trim();
        if let Some(marker) = fence_marker(stripped) {
            let mut block = Block {
                kind: Kind::Fence,
                lines: vec![line.to_owned()],
            };
            i += 1;
            while i < n {
                block.lines.push(lines[i].to_owned());
                let closed = lines[i].trim().starts_with(marker);
                i += 1;
                if closed {
                    break;
                }
            }
            blocks.push(block);
            continue;
        }
        if stripped.starts_with("<!--") {
            let mut block = Block {
                kind: Kind::Comment,
                lines: vec![line.to_owned()],
            };
            let mut closed = line.contains("-->");
            i += 1;
            while !closed && i < n {
                block.lines.push(lines[i].to_owned());
                closed = lines[i].contains("-->");
                i += 1;
            }
            blocks.push(block);
            continue;
        }
        if stripped.is_empty() {
            let mut block = Block {
                kind: Kind::Blank,
                lines: vec![line.to_owned()],
            };
            i += 1;
            while i < n && lines[i].trim().is_empty() {
                block.lines.push(lines[i].to_owned());
                i += 1;
            }
            blocks.push(block);
            continue;
        }
        if is_heading(line) {
            blocks.push(Block {
                kind: Kind::Heading,
                lines: vec![line.to_owned()],
            });
            i += 1;
            continue;
        }
        let mut block = Block {
            kind: Kind::Para,
            lines: vec![line.to_owned()],
        };
        i += 1;
        while i < n {
            let next = lines[i];
            let s = next.trim();
            if s.is_empty()
                || is_heading(next)
                || fence_marker(s).is_some()
                || s.starts_with("<!--")
            {
                break;
            }
            block.lines.push(next.to_owned());
            i += 1;
        }
        blocks.push(block);
    }
    blocks
}

/// `_render(blocks)` — `"\n".join(line for b in blocks for line in b.lines)`.
fn render(blocks: &[Block]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for block in blocks {
        for line in &block.lines {
            out.push(line);
        }
    }
    out.join("\n")
}

/// `_strip_comments(blocks)` — drop HTML comment blocks.
fn strip_comments(blocks: &[Block]) -> (Vec<Block>, Option<Map<String, Value>>) {
    let removed: Vec<&Block> = blocks.iter().filter(|b| b.kind == Kind::Comment).collect();
    if removed.is_empty() {
        // `return blocks, None` — the ORIGINAL list, not a copy.
        return (blocks.to_vec(), None);
    }
    let kept: Vec<Block> = blocks
        .iter()
        .filter(|b| b.kind != Kind::Comment)
        .cloned()
        .collect();
    let mut detail = Map::new();
    detail.insert(
        "removed_comments".to_owned(),
        Value::from(i64::try_from(removed.len()).unwrap_or(i64::MAX)),
    );
    detail.insert(
        "samples".to_owned(),
        Value::Array(
            removed
                .iter()
                .take(3)
                .map(|b| Value::from(char_prefix(b.lines[0].trim(), 80)))
                .collect(),
        ),
    );
    (kept, Some(detail))
}

/// `_normalise_para(b)` — `" ".join(" ".join(b.lines).split())`.
fn normalise_para(block: &Block) -> String {
    block
        .lines
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `_dedupe_paragraphs(blocks)` — drop exact duplicate paragraphs after the first.
fn dedupe_paragraphs(blocks: &[Block]) -> (Vec<Block>, Option<Map<String, Value>>) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Block> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for block in blocks {
        if block.kind == Kind::Para {
            let norm = normalise_para(block);
            // `len(norm)` is CODE POINTS.
            if norm.chars().count() >= DEDUPE_MIN_CHARS {
                if seen.contains(&norm) {
                    dropped.push(char_prefix(&norm, 80));
                    continue;
                }
                seen.insert(norm);
            }
        }
        out.push(block.clone());
    }
    if dropped.is_empty() {
        return (blocks.to_vec(), None);
    }
    let mut detail = Map::new();
    detail.insert(
        "removed_duplicates".to_owned(),
        Value::from(i64::try_from(dropped.len()).unwrap_or(i64::MAX)),
    );
    detail.insert(
        "samples".to_owned(),
        Value::Array(
            dropped
                .iter()
                .take(3)
                .map(|s| Value::from(s.clone()))
                .collect(),
        ),
    );
    (out, Some(detail))
}

/// `_KEEP_HEADING_RE = \b(rules?|never|must|safety|critical|important)\b`, `re.I`.
///
/// Hand-rolled rather than pulling in a regex engine. `\b` is a transition
/// between a word character and a non-word one; Python's `\w` is Unicode by
/// default for `str` patterns, so the boundary test here is
/// `is_alphanumeric() || '_'`, which is the same class for every input a
/// markdown heading can carry.
fn keep_heading(heading_line: &str) -> bool {
    // "rules" before "rule" mirrors the greedy `s?`; for a boolean answer the
    // order is immaterial, and it is written this way so it reads like the regex.
    const KEYWORDS: [&str; 7] = [
        "rules",
        "rule",
        "never",
        "must",
        "safety",
        "critical",
        "important",
    ];
    let chars: Vec<char> = heading_line.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    for start in 0..chars.len() {
        if start > 0 && is_word(chars[start - 1]) {
            continue; // no `\b` before this position
        }
        for keyword in KEYWORDS {
            let kw: Vec<char> = keyword.chars().collect();
            if start + kw.len() > chars.len() {
                continue;
            }
            let matches = kw
                .iter()
                .enumerate()
                .all(|(k, wanted)| chars[start + k].to_ascii_lowercase() == *wanted);
            if !matches {
                continue;
            }
            let after = start + kw.len();
            if after < chars.len() && is_word(chars[after]) {
                continue; // no `\b` after this position
            }
            return true;
        }
    }
    false
}

/// `_heading_slug(heading_line)`.
///
/// `re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")`, then `or "section"`.
/// The character class is ASCII-only *after* a Unicode lowercase, so an
/// all-Cyrillic heading slugs to the empty string and becomes `"section"`.
fn heading_slug(heading_line: &str) -> String {
    let title: String = heading_line.trim_start_matches('#').trim().to_lowercase();
    let mut out = String::new();
    let mut in_run = false;
    for ch in title.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            out.push(ch);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    let slug = out.trim_matches('-').to_owned();
    if slug.is_empty() {
        "section".to_owned()
    } else {
        slug
    }
}

/// `_extract_sections(blocks, section_token_threshold=…)`.
fn extract_sections(
    blocks: &[Block],
    section_token_threshold: i64,
) -> (Vec<Block>, Option<Map<String, Value>>) {
    let mut out: Vec<Block> = Vec::new();
    let mut extracted: Vec<Value> = Vec::new();
    let mut i = 0;
    let n = blocks.len();
    while i < n {
        let block = &blocks[i];
        if block.kind != Kind::Heading {
            out.push(block.clone());
            i += 1;
            continue;
        }
        // The section body is every block up to the next heading, any level.
        let mut j = i + 1;
        let mut body: Vec<&Block> = Vec::new();
        while j < n && blocks[j].kind != Kind::Heading {
            body.push(&blocks[j]);
            j += 1;
        }
        let body_text = body.iter().map(|b| b.text()).collect::<Vec<_>>().join("\n");
        let body_tokens = approx_tokens(&body_text);
        let heading_line = block.lines[0].clone();
        if body_tokens >= section_token_threshold && !keep_heading(&heading_line) {
            let slug = heading_slug(&heading_line);
            let pointer = format!(
                "> Body moved to docs/claude-md/{slug}.md (~{body_tokens} tokens) — \
                 slimmed by StackUnderflow; move the original text there before \
                 adopting this file."
            );
            out.push(block.clone());
            out.push(Block {
                kind: Kind::Blank,
                lines: vec![String::new()],
            });
            out.push(Block {
                kind: Kind::Para,
                lines: vec![pointer],
            });
            out.push(Block {
                kind: Kind::Blank,
                lines: vec![String::new()],
            });
            let mut entry = Map::new();
            entry.insert(
                "heading".to_owned(),
                Value::from(heading_line.trim_start_matches('#').trim().to_owned()),
            );
            entry.insert(
                "suggested_path".to_owned(),
                Value::from(format!("docs/claude-md/{slug}.md")),
            );
            entry.insert("body_tokens".to_owned(), Value::from(body_tokens));
            extracted.push(Value::Object(entry));
            i = j;
            continue;
        }
        out.push(block.clone());
        i += 1;
    }
    if extracted.is_empty() {
        return (blocks.to_vec(), None);
    }
    let mut detail = Map::new();
    detail.insert("extracted_sections".to_owned(), Value::Array(extracted));
    (out, Some(detail))
}

/// `_collapse_blank_runs(blocks)` — 3+ blank lines become one.
fn collapse_blank_runs(blocks: &[Block]) -> (Vec<Block>, Option<Map<String, Value>>) {
    let mut removed: usize = 0;
    let mut out: Vec<Block> = Vec::new();
    for block in blocks {
        if block.kind == Kind::Blank && block.lines.len() >= BLANK_RUN_COLLAPSE_AT {
            removed += block.lines.len() - 1;
            out.push(Block {
                kind: Kind::Blank,
                lines: vec![String::new()],
            });
        } else {
            out.push(block.clone());
        }
    }
    if removed == 0 {
        return (blocks.to_vec(), None);
    }
    let mut detail = Map::new();
    detail.insert(
        "blank_lines_removed".to_owned(),
        Value::from(i64::try_from(removed).unwrap_or(i64::MAX)),
    );
    (out, Some(detail))
}

/// `_RULE_SUMMARIES[rule_id].format(**{k: v for k, v in detail.items() if not
/// isinstance(v, list)})` — the LIST-valued keys are excluded from the format
/// arguments, which is why every template names only scalars.
fn rule_summary(rule_id: &str, detail: &Map<String, Value>) -> String {
    let scalar = |key: &str| -> i64 { detail.get(key).and_then(Value::as_i64).unwrap_or(0) };
    match rule_id {
        "strip_html_comments" => format!(
            "Removed {} HTML comment block(s) — author notes the model pays to \
             read every session.",
            scalar("removed_comments")
        ),
        "dedupe_paragraphs" => format!(
            "Removed {} duplicate paragraph(s) — repeated text costs tokens twice.",
            scalar("removed_duplicates")
        ),
        "extract_oversized_sections" => format!(
            "Proposed moving {} oversized section(s) to side docs, keeping a \
             one-line pointer inline.",
            scalar("count")
        ),
        "collapse_blank_runs" => format!(
            "Collapsed runs of blank lines ({} line(s) removed).",
            scalar("blank_lines_removed")
        ),
        // Unreachable — the stage list is closed.
        _ => String::new(),
    }
}

/// `_bloat_flagged(findings)` — does the caller's finding list flag context bloat?
fn bloat_flagged(findings: Option<&[Value]>) -> bool {
    for finding in findings.unwrap_or_default() {
        // `f.get("pattern_id") or f.get("kind")` — falsy falls through.
        let pid = finding
            .get("pattern_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                finding
                    .get("kind")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            });
        if matches!(pid, Some("bloated_claude_md" | "context_budget_bloat")) {
            return true;
        }
    }
    false
}

/// A float as JSON — `0.0` stays a float, non-finite becomes `null`.
fn json_float(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// `generate_claudemd_preview(text, findings=…, file_label=…, sessions_per_month=…)`.
///
/// Pure: text in, the twelve-key preview object out. Never touches the
/// filesystem — "apply" is the caller copying `slimmed_text`.
///
/// The four rules run in order and each contributes one `rationale` entry when
/// it changed something. `extract_oversized_sections` is conditional: it only
/// runs when the document is bloat-flagged, i.e. a `bloated_claude_md` /
/// `context_budget_bloat` finding was passed OR the text itself estimates over
/// `CLAUDE_MD_TOKEN_THRESHOLD`.
///
/// `tokens_saved` per rule is measured against the text as it stood *before*
/// that rule, so the per-rule figures sum to the total only when no rule
/// re-expands the document — which none of them can.
#[must_use]
pub fn generate_claudemd_preview(
    engine: &PricingEngine,
    claude_md_text: &str,
    findings: Option<&[Value]>,
    file_label: &str,
    sessions_per_month: i64,
) -> Value {
    let original = claude_md_text;
    let mut rationale: Vec<Value> = Vec::new();

    let mut blocks = parse_blocks(original);
    let mut current_text = original.to_owned();

    let extraction_enabled =
        approx_tokens(original) > CLAUDE_MD_TOKEN_THRESHOLD || bloat_flagged(findings);

    // The stage list, in order. `extract_oversized_sections` is inserted third
    // only when enabled — so `collapse_blank_runs` is always last.
    let mut stages: Vec<&str> = vec!["strip_html_comments", "dedupe_paragraphs"];
    if extraction_enabled {
        stages.push("extract_oversized_sections");
    }
    stages.push("collapse_blank_runs");

    for rule_id in stages {
        let (new_blocks, detail) = match rule_id {
            "strip_html_comments" => strip_comments(&blocks),
            "dedupe_paragraphs" => dedupe_paragraphs(&blocks),
            "extract_oversized_sections" => {
                extract_sections(&blocks, SECTION_EXTRACT_TOKEN_THRESHOLD)
            }
            _ => collapse_blank_runs(&blocks),
        };
        let Some(mut detail) = detail else {
            continue;
        };
        let new_text = render(&new_blocks);
        let saved = approx_tokens(&current_text) - approx_tokens(&new_text);
        if rule_id == "extract_oversized_sections" {
            // `detail = dict(detail); detail["count"] = len(...)` — appended
            // AFTER `extracted_sections`, so the key order is (sections, count).
            let count = detail
                .get("extracted_sections")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            detail.insert(
                "count".to_owned(),
                Value::from(i64::try_from(count).unwrap_or(i64::MAX)),
            );
        }
        let summary = rule_summary(rule_id, &detail);
        // `tokens_to_usd(saved) or 0.0` — None AND 0.0 both become 0.0.
        let per_session = tokens_to_usd(engine, Some(saved), WasteKind::Input).unwrap_or(0.0);
        let mut entry = Map::new();
        entry.insert("rule".to_owned(), Value::from(rule_id));
        entry.insert("summary".to_owned(), Value::from(summary));
        entry.insert("tokens_saved".to_owned(), Value::from(saved));
        entry.insert(
            "estimated_savings_usd_per_session".to_owned(),
            json_float(per_session),
        );
        #[allow(
            clippy::cast_precision_loss,
            reason = "sessions_per_month is bounded to 1..=100_000 by the request model"
        )]
        entry.insert(
            "estimated_savings_usd_monthly".to_owned(),
            json_float(round_half_even(per_session * sessions_per_month as f64, 4)),
        );
        entry.insert("detail".to_owned(), Value::Object(detail));
        rationale.push(Value::Object(entry));

        blocks = new_blocks;
        current_text = new_text;
    }

    let slimmed = current_text;
    let changed = slimmed != original;

    let original_tokens = approx_tokens(original);
    let slimmed_tokens = approx_tokens(&slimmed);
    let tokens_saved = original_tokens - slimmed_tokens;
    let per_session = tokens_to_usd(engine, Some(tokens_saved), WasteKind::Input).unwrap_or(0.0);

    let preview_diff = if changed {
        unified_diff(
            &splitlines_keepends(original),
            &splitlines_keepends(&slimmed),
            file_label,
            &format!("{file_label} (slim preview)"),
        )
    } else {
        String::new()
    };

    let mut out = Map::new();
    out.insert("file_label".to_owned(), Value::from(file_label));
    out.insert("changed".to_owned(), Value::Bool(changed));
    out.insert("preview_diff".to_owned(), Value::from(preview_diff));
    // `slimmed if changed else ""` — an unchanged document sends no body back.
    out.insert(
        "slimmed_text".to_owned(),
        Value::from(if changed { slimmed } else { String::new() }),
    );
    out.insert("rationale".to_owned(), Value::Array(rationale));
    out.insert("original_tokens".to_owned(), Value::from(original_tokens));
    out.insert("slimmed_tokens".to_owned(), Value::from(slimmed_tokens));
    out.insert("tokens_saved".to_owned(), Value::from(tokens_saved));
    out.insert(
        "estimated_savings_usd_per_session".to_owned(),
        json_float(per_session),
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "sessions_per_month is bounded to 1..=100_000 by the request model"
    )]
    out.insert(
        "estimated_savings_usd_monthly".to_owned(),
        json_float(round_half_even(per_session * sessions_per_month as f64, 4)),
    );
    out.insert(
        "sessions_per_month".to_owned(),
        Value::from(sessions_per_month),
    );
    out.insert(
        "heuristic".to_owned(),
        Value::from(format!(
            "tokens ≈ len(text)//4; savings priced as input tokens at \
             {WASTE_PRICING_MODEL} via compute_cost; monthly = per-session × \
             {sessions_per_month} sessions"
        )),
    );
    Value::Object(out)
}

// ── difflib ──────────────────────────────────────────────────────────────────

/// `str.splitlines(keepends=True)`.
///
/// Eleven boundaries, not one: `\n`, `\r`, `\r\n` (as a single boundary),
/// `\v` (U+000B), `\f` (U+000C), `\x1c`, `\x1d`, `\x1e`, `\x85`, ` `,
/// ` `. `"".splitlines()` is `[]`, and a trailing terminator does NOT
/// produce a final empty element.
#[must_use]
pub fn splitlines_keepends(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        current.push(ch);
        i += 1;
        let is_break = matches!(
            ch,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if is_break {
            // `\r\n` is ONE boundary.
            if ch == '\r' && i < chars.len() && chars[i] == '\n' {
                current.push('\n');
                i += 1;
            }
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// One `difflib.Match(a, b, size)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MatchBlock {
    a: usize,
    b: usize,
    size: usize,
}

/// One `difflib` opcode: `(tag, i1, i2, j1, j2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Opcode {
    tag: Tag,
    i1: usize,
    i2: usize,
    j1: usize,
    j2: usize,
}

/// The four opcode tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Equal,
    Replace,
    Delete,
    Insert,
}

/// `difflib.SequenceMatcher(None, a, b)` over lines, `autojunk=True`.
struct SequenceMatcher<'a> {
    a: &'a [String],
    b: &'a [String],
    /// `b2j` — element → the indices it occupies in `b`, popular ones purged.
    b2j: HashMap<&'a str, Vec<usize>>,
}

impl<'a> SequenceMatcher<'a> {
    /// `__init__` + `__chain_b`.
    fn new(a: &'a [String], b: &'a [String]) -> Self {
        let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, elt) in b.iter().enumerate() {
            b2j.entry(elt.as_str()).or_default().push(i);
        }
        // `isjunk` is None here, so the junk purge is skipped entirely and
        // `bjunk` stays empty — which is why the "extend by non-junk" loops in
        // `find_longest_match` never stop early below.
        //
        // The POPULAR purge is a different thing and it does run: for
        // `len(b) >= 200`, an element in more than `n // 100 + 1` positions is
        // dropped from the index. It is NOT added to `bjunk`, so the extension
        // loops still walk over it — that asymmetry is the whole reason a
        // "simplified" matcher produces different hunks on a long file.
        let n = b.len();
        if n >= 200 {
            let ntest = n / 100 + 1;
            let popular: Vec<&str> = b2j
                .iter()
                .filter(|(_, idxs)| idxs.len() > ntest)
                .map(|(elt, _)| *elt)
                .collect();
            for elt in popular {
                b2j.remove(elt);
            }
        }
        Self { a, b, b2j }
    }

    /// `find_longest_match(alo, ahi, blo, bhi)`.
    fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> MatchBlock {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(indices) = self.b2j.get(self.a[i].as_str()) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    // `break`, not `continue` — the index list is ascending.
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|prev| j2len.get(&prev).copied())
                        .unwrap_or(0)
                        + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // Extend by equal elements on each end. With `isjunk=None` there is no
        // junk, so only this pair of loops runs and the two "tack on junk"
        // loops CPython follows them with are no-ops.
        while besti > alo && bestj > blo && self.a[besti - 1] == self.b[bestj - 1] {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a[besti + bestsize] == self.b[bestj + bestsize]
        {
            bestsize += 1;
        }
        MatchBlock {
            a: besti,
            b: bestj,
            size: bestsize,
        }
    }

    /// `get_matching_blocks()`.
    fn matching_blocks(&self) -> Vec<MatchBlock> {
        let la = self.a.len();
        let lb = self.b.len();
        let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, la, 0, lb)];
        let mut blocks: Vec<MatchBlock> = Vec::new();
        // `queue.pop()` — LIFO. The final sort makes the order immaterial, but
        // the traversal is reproduced anyway.
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.find_longest_match(alo, ahi, blo, bhi);
            if m.size > 0 {
                blocks.push(m);
                if alo < m.a && blo < m.b {
                    queue.push((alo, m.a, blo, m.b));
                }
                if m.a + m.size < ahi && m.b + m.size < bhi {
                    queue.push((m.a + m.size, ahi, m.b + m.size, bhi));
                }
            }
        }
        blocks.sort_by_key(|m| (m.a, m.b, m.size));

        // Collapse adjacent equal blocks.
        let (mut i1, mut j1, mut k1) = (0usize, 0usize, 0usize);
        let mut non_adjacent: Vec<MatchBlock> = Vec::new();
        for m in blocks {
            if i1 + k1 == m.a && j1 + k1 == m.b {
                k1 += m.size;
            } else {
                if k1 > 0 {
                    non_adjacent.push(MatchBlock {
                        a: i1,
                        b: j1,
                        size: k1,
                    });
                }
                i1 = m.a;
                j1 = m.b;
                k1 = m.size;
            }
        }
        if k1 > 0 {
            non_adjacent.push(MatchBlock {
                a: i1,
                b: j1,
                size: k1,
            });
        }
        // The zero-length sentinel at the end is load-bearing: `get_opcodes`
        // needs it to emit the trailing insert/delete.
        non_adjacent.push(MatchBlock {
            a: la,
            b: lb,
            size: 0,
        });
        non_adjacent
    }

    /// `get_opcodes()`.
    fn opcodes(&self) -> Vec<Opcode> {
        let mut i = 0usize;
        let mut j = 0usize;
        let mut answer: Vec<Opcode> = Vec::new();
        for m in self.matching_blocks() {
            let tag = if i < m.a && j < m.b {
                Some(Tag::Replace)
            } else if i < m.a {
                Some(Tag::Delete)
            } else if j < m.b {
                Some(Tag::Insert)
            } else {
                None
            };
            if let Some(tag) = tag {
                answer.push(Opcode {
                    tag,
                    i1: i,
                    i2: m.a,
                    j1: j,
                    j2: m.b,
                });
            }
            i = m.a + m.size;
            j = m.b + m.size;
            if m.size > 0 {
                answer.push(Opcode {
                    tag: Tag::Equal,
                    i1: m.a,
                    i2: i,
                    j1: m.b,
                    j2: j,
                });
            }
        }
        answer
    }

    /// `get_grouped_opcodes(n)`.
    fn grouped_opcodes(&self, n: usize) -> Vec<Vec<Opcode>> {
        let mut codes = self.opcodes();
        if codes.is_empty() {
            codes = vec![Opcode {
                tag: Tag::Equal,
                i1: 0,
                i2: 1,
                j1: 0,
                j2: 1,
            }];
        }
        // The leading and trailing all-equal runs are trimmed IN PLACE, before
        // any grouping happens.
        if codes[0].tag == Tag::Equal {
            let c = codes[0];
            codes[0] = Opcode {
                tag: c.tag,
                i1: c.i1.max(c.i2.saturating_sub(n)),
                i2: c.i2,
                j1: c.j1.max(c.j2.saturating_sub(n)),
                j2: c.j2,
            };
        }
        let last = codes.len() - 1;
        if codes[last].tag == Tag::Equal {
            let c = codes[last];
            codes[last] = Opcode {
                tag: c.tag,
                i1: c.i1,
                i2: c.i2.min(c.i1 + n),
                j1: c.j1,
                j2: c.j2.min(c.j1 + n),
            };
        }

        let nn = n + n;
        let mut groups: Vec<Vec<Opcode>> = Vec::new();
        let mut group: Vec<Opcode> = Vec::new();
        for code in codes {
            let mut c = code;
            // A long unchanged run ends the current group and starts a new one.
            if c.tag == Tag::Equal && c.i2 - c.i1 > nn {
                group.push(Opcode {
                    tag: c.tag,
                    i1: c.i1,
                    i2: c.i2.min(c.i1 + n),
                    j1: c.j1,
                    j2: c.j2.min(c.j1 + n),
                });
                groups.push(std::mem::take(&mut group));
                c.i1 = c.i1.max(c.i2.saturating_sub(n));
                c.j1 = c.j1.max(c.j2.saturating_sub(n));
            }
            group.push(c);
        }
        // A trailing group that is nothing but context is suppressed.
        if !group.is_empty() && !(group.len() == 1 && group[0].tag == Tag::Equal) {
            groups.push(group);
        }
        groups
    }
}

/// `_format_range_unified(start, stop)`.
fn format_range_unified(start: usize, stop: usize) -> String {
    let mut beginning = start + 1;
    let length = stop - start;
    if length == 1 {
        return beginning.to_string();
    }
    if length == 0 {
        // An empty range begins at the line just before the range.
        beginning -= 1;
    }
    format!("{beginning},{length}")
}

/// `"".join(difflib.unified_diff(a, b, fromfile, tofile))`, `n=3`, `lineterm="\n"`.
///
/// Returns the joined string rather than an iterator because that is the only
/// thing the one caller does with it. No `\ No newline at end of file` marker —
/// `difflib` does not emit one, so a final line without a terminator simply
/// runs into the next diff line.
#[must_use]
pub fn unified_diff(a: &[String], b: &[String], fromfile: &str, tofile: &str) -> String {
    let matcher = SequenceMatcher::new(a, b);
    let mut out = String::new();
    let mut started = false;
    for group in matcher.grouped_opcodes(3) {
        if !started {
            started = true;
            // `fromfiledate` / `tofiledate` are empty, so no tab-date suffix.
            let _ = writeln!(out, "--- {fromfile}\n+++ {tofile}");
        }
        let first = group[0];
        let last = group[group.len() - 1];
        let file1 = format_range_unified(first.i1, last.i2);
        let file2 = format_range_unified(first.j1, last.j2);
        let _ = writeln!(out, "@@ -{file1} +{file2} @@");
        for code in group {
            match code.tag {
                Tag::Equal => {
                    for line in &a[code.i1..code.i2] {
                        out.push(' ');
                        out.push_str(line);
                    }
                }
                Tag::Replace => {
                    for line in &a[code.i1..code.i2] {
                        out.push('-');
                        out.push_str(line);
                    }
                    for line in &b[code.j1..code.j2] {
                        out.push('+');
                        out.push_str(line);
                    }
                }
                Tag::Delete => {
                    for line in &a[code.i1..code.i2] {
                        out.push('-');
                        out.push_str(line);
                    }
                }
                Tag::Insert => {
                    for line in &b[code.j1..code.j2] {
                        out.push('+');
                        out.push_str(line);
                    }
                }
            }
        }
    }
    out
}

// ══════════════════════════════════════════════════════════════════════════
// (b) model-routing recommendations
// ══════════════════════════════════════════════════════════════════════════

/// `_table_exists` — accepts tables AND views, unlike `store/mart_queries.py`'s.
fn table_or_view_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ? LIMIT 1",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// One `(pricer, model_id, label)` routing candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The `compute_cost` routing key.
    pub pricer: String,
    /// The model id.
    pub model: String,
    /// The human label the rationale prints.
    pub label: String,
}

/// The model catalogue, compiled in.
///
/// The second runtime read of the Python package directory the decommission
/// report missed (DIV-400) — `routes/whatif.rs` makes the same one. Same rule as
/// the rate card: a file on disk wins, the compiled-in copy answers when there
/// is none, and it is the *same* file read at build time rather than a
/// transcription.
pub const EMBEDDED_MODEL_CANDIDATES: &str =
    include_str!("../../../../stackunderflow/infra/model_candidates.json");

/// `infra/model_candidates.json`'s text — from `package_dir` when it is there,
/// from the binary when it is not.
///
/// A file that exists but cannot be read (permissions) yields the compiled-in
/// copy too, because both callers already treat an unreadable catalogue as an
/// empty one; there is no error channel here to widen.
#[must_use]
pub fn read_model_candidates(package_dir: &Path) -> String {
    let path = package_dir.join("infra").join("model_candidates.json");
    std::fs::read_to_string(path).unwrap_or_else(|_| EMBEDDED_MODEL_CANDIDATES.to_owned())
}

/// `infra/model_catalog.routing_candidates()` — the `routing_candidate: true`
/// entries of `infra/model_candidates.json`.
///
/// Package DATA, injected as a path rather than discovered, so a test can point
/// at a fixture catalog. A missing or malformed file yields an empty list,
/// which makes every rule silently not fire — the same shape a store with no
/// `usage_events` produces.
#[must_use]
pub fn routing_candidates(package_dir: &Path) -> Vec<Candidate> {
    let text = read_model_candidates(package_dir);
    let Ok(raw) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(entries) = raw.get("candidates").and_then(Value::as_array) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|e| {
            // `e.get("routing_candidate", True)` — an ABSENT key means true.
            e.get("routing_candidate")
                .is_none_or(|v| v.as_bool().unwrap_or(false))
        })
        .filter_map(|e| {
            Some(Candidate {
                pricer: e.get("pricer")?.as_str()?.to_owned(),
                model: e.get("model")?.as_str()?.to_owned(),
                label: e.get("label")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

/// One `usage_events` rollup row.
#[derive(Debug, Clone)]
struct Rollup {
    model: String,
    provider: Option<String>,
    speed: Option<String>,
    events: i64,
    sessions: i64,
    days_active: i64,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_create_tokens: i64,
    reasoning_tokens: i64,
    cost_usd: f64,
}

/// `_scope_where(scope, project_ids)` — the WHERE fragment and its parameters.
fn scope_where(scope: Option<&Scope>, project_ids: Option<&[i64]>) -> (String, Vec<SqlValue>) {
    let mut sql = String::new();
    let mut params: Vec<SqlValue> = Vec::new();
    // `if project_ids:` — falsy on None AND on []. The empty case is handled
    // by the caller's early return, not here.
    if let Some(ids) = project_ids.filter(|ids| !ids.is_empty()) {
        let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let _ = write!(sql, " AND project_id IN ({ph})");
        params.extend(ids.iter().map(|id| SqlValue::Integer(*id)));
    }
    if let Some(since) = scope.and_then(|s| s.since.as_deref()) {
        sql.push_str(" AND ts >= ?");
        params.push(SqlValue::Text(since.to_owned()));
    }
    if let Some(until) = scope.and_then(|s| s.until.as_deref()) {
        sql.push_str(" AND ts <= ?");
        params.push(SqlValue::Text(until.to_owned()));
    }
    (sql, params)
}

/// `_load_model_rollups` — per-(model, provider, speed) aggregates + active days.
///
/// Reads `usage_events` directly rather than `model_day_mart`, because the mart
/// is global-grain (no `project_id`) and has no reasoning column. Returns
/// `([], 0)` on any schema/SQL problem — advisory, never raises. A pre-v026
/// store with no `reasoning_tokens` lands there.
fn load_model_rollups(
    conn: &Connection,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
) -> (Vec<Rollup>, i64) {
    if !table_or_view_exists(conn, "usage_events") {
        return (Vec::new(), 0);
    }
    // `if project_ids is not None and len(project_ids) == 0` — an empty filter
    // matched nothing and must NOT silently widen to the whole store.
    if project_ids.is_some_and(<[i64]>::is_empty) {
        return (Vec::new(), 0);
    }
    let (where_sql, params) = scope_where(scope, project_ids);
    let sql = format!(
        "SELECT model, provider, COALESCE(speed, 'standard') AS speed, \
                COUNT(*) AS events, \
                COUNT(DISTINCT session_id) AS sessions, \
                COUNT(DISTINCT day) AS days_active, \
                COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, \
                COALESCE(SUM(cache_create_tokens), 0) AS cache_create_tokens, \
                COALESCE(SUM(reasoning_tokens), 0) AS reasoning_tokens, \
                COALESCE(SUM(cost_usd), 0.0) AS cost_usd \
         FROM usage_events \
         WHERE model <> '' {where_sql} \
         GROUP BY model, provider, speed"
    );
    let day_sql =
        format!("SELECT COUNT(DISTINCT day) FROM usage_events WHERE model <> '' {where_sql}");

    let rows = (|| -> rusqlite::Result<Vec<Rollup>> {
        let mut stmt = conn.prepare(&sql)?;
        stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(Rollup {
                model: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                provider: row.get(1)?,
                speed: row.get(2)?,
                events: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                sessions: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                days_active: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                input_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                output_tokens: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                cache_read_tokens: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                cache_create_tokens: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                reasoning_tokens: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
                cost_usd: row.get::<_, Option<f64>>(11)?.unwrap_or(0.0),
            })
        })?
        .collect()
    })();
    let observed = conn
        .query_row(&day_sql, rusqlite::params_from_iter(params.iter()), |row| {
            row.get::<_, Option<i64>>(0)
        })
        .ok()
        .flatten()
        .unwrap_or(0);
    // `except sqlite3.Error: return [], 0` — the pair fails together.
    match rows {
        Ok(rows) => (rows, observed),
        Err(_) => (Vec::new(), 0),
    }
}

/// `_load_quality_by_model` — `{model: (avg_overall_score, graded_sessions)}`.
///
/// `session_quality_metrics` (v020) is frequently empty; an empty table or a
/// store without it contributes nothing and no rule that needs a quality signal
/// fires.
fn load_quality_by_model(
    conn: &Connection,
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
) -> HashMap<String, (f64, i64)> {
    if !table_or_view_exists(conn, "session_quality_metrics") {
        return HashMap::new();
    }
    let (where_sql, params) = scope_where(scope, project_ids);
    let sql = format!(
        "SELECT ue.model AS model, \
                AVG(sq.overall_score) AS avg_score, \
                COUNT(DISTINCT sq.session_id) AS graded \
         FROM session_quality_metrics sq \
         JOIN usage_events ue ON ue.session_id = sq.session_id \
         WHERE ue.model <> '' {where_sql} \
         GROUP BY ue.model"
    );
    let mut out = HashMap::new();
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<f64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        let (model, avg, graded) = row;
        // `if r["avg_score"] is None: continue`.
        let Some(avg) = avg else { continue };
        out.insert(model.unwrap_or_default(), (avg, graded.unwrap_or(0)));
    }
    out
}

/// `_DATE_SUFFIX_RE = -\d{8}$` — strip a trailing `-YYYYMMDD`.
fn strip_date_suffix(model: &str) -> &str {
    let bytes = model.as_bytes();
    if bytes.len() >= 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
    {
        return &model[..model.len() - 9];
    }
    model
}

/// `_same_model(model, candidate_id)` — date-suffix aware.
fn same_model(model: &str, candidate_id: &str) -> bool {
    let a = strip_date_suffix(model);
    let b = strip_date_suffix(candidate_id);
    a == b || a.starts_with(&format!("{b}-")) || b.starts_with(&format!("{a}-"))
}

/// `_classify_work_type(...)` — `(work_type, reasoning_share)`.
///
/// `reasoning_tokens == 0` means **unattributed** (v026: providers with no
/// measurable reasoning stay 0), never "does no reasoning" — which is why the
/// zero branch produces its own two labels rather than falling into
/// "low-reasoning".
fn classify_work_type(
    reasoning_tokens: i64,
    output_tokens: i64,
    events: i64,
) -> (&'static str, f64) {
    #[allow(
        clippy::cast_precision_loss,
        reason = "token and event counts are far below 2^53"
    )]
    let share = if output_tokens > 0 {
        reasoning_tokens as f64 / output_tokens as f64
    } else {
        0.0
    };
    if reasoning_tokens > 0 {
        if share >= HIGH_REASONING_SHARE {
            return ("reasoning-heavy", share);
        }
        if share < LOW_REASONING_SHARE {
            return ("low-reasoning", share);
        }
        return ("mixed", share);
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "token and event counts are far below 2^53"
    )]
    let avg_out = if events > 0 {
        output_tokens as f64 / events as f64
    } else {
        0.0
    };
    if avg_out < SHORT_OUTPUT_TOKENS_PER_EVENT {
        return ("short-output (reasoning unattributed)", share);
    }
    ("unattributed", share)
}

/// `format(value, ",.Nf")` — fixed decimals, then thousands separators.
fn grouped_fixed(value: f64, digits: usize) -> String {
    let fixed = format!("{value:.digits$}");
    let (sign, body) = match fixed.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", fixed.as_str()),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let mut grouped = String::with_capacity(int_part.len() + int_part.len() / 3);
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (int_part.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if frac_part.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{frac_part}")
    }
}

/// `format(value, ".N%")` — multiply by 100, fix the decimals, append `%`.
fn percent(value: f64, digits: usize) -> String {
    format!("{:.*}%", digits, value * 100.0)
}

/// `_make_recommendation(...)` — one recommendation row, shared by every rule.
#[allow(
    clippy::too_many_arguments,
    reason = "one row assembler mirroring a keyword-only Python signature"
)]
fn make_recommendation(
    rec_id: &str,
    candidate: &(f64, String, String),
    rationale: String,
    caveats: Vec<&str>,
    rollup: &Rollup,
    work_type: &str,
    reasoning_share: f64,
    actual_cost: f64,
    monthly_factor: Option<f64>,
    quality: Option<(f64, i64)>,
) -> Value {
    let (cand_cost, cand_id, cand_label) = candidate;
    let window_delta = cand_cost - actual_cost;
    let events = rollup.events;
    let output_tokens = rollup.output_tokens;

    let mut evidence = Map::new();
    evidence.insert("events".to_owned(), Value::from(events));
    evidence.insert("sessions".to_owned(), Value::from(rollup.sessions));
    evidence.insert("days_active".to_owned(), Value::from(rollup.days_active));
    evidence.insert("output_tokens".to_owned(), Value::from(output_tokens));
    evidence.insert(
        "reasoning_tokens".to_owned(),
        Value::from(rollup.reasoning_tokens),
    );
    evidence.insert(
        "reasoning_share".to_owned(),
        json_float(round_half_even(reasoning_share, 4)),
    );
    #[allow(
        clippy::cast_precision_loss,
        reason = "token and event counts are far below 2^53"
    )]
    evidence.insert(
        "avg_output_tokens_per_event".to_owned(),
        // `round(output/events, 1) if events else 0.0` — the else is a FLOAT.
        json_float(if events > 0 {
            round_half_even(output_tokens as f64 / events as f64, 1)
        } else {
            0.0
        }),
    );
    evidence.insert(
        "avg_quality_score".to_owned(),
        quality.map_or(Value::Null, |(score, _)| {
            json_float(round_half_even(score, 3))
        }),
    );
    evidence.insert(
        "graded_sessions".to_owned(),
        Value::from(quality.map_or(0, |(_, graded)| graded)),
    );

    let mut obj = Map::new();
    obj.insert("rec_id".to_owned(), Value::from(rec_id));
    obj.insert("work_type".to_owned(), Value::from(work_type));
    obj.insert("from_model".to_owned(), Value::from(rollup.model.clone()));
    // `rollup["provider"] or "anthropic"` — NULL and "" both default.
    obj.insert(
        "provider".to_owned(),
        Value::from(non_empty_or(rollup.provider.as_deref(), "anthropic")),
    );
    obj.insert(
        "speed".to_owned(),
        Value::from(non_empty_or(rollup.speed.as_deref(), "standard")),
    );
    obj.insert("to_model".to_owned(), Value::from(cand_id.clone()));
    obj.insert("to_label".to_owned(), Value::from(cand_label.clone()));
    obj.insert(
        "window_cost_usd".to_owned(),
        json_float(round_half_even(actual_cost, 4)),
    );
    obj.insert(
        "candidate_window_cost_usd".to_owned(),
        json_float(round_half_even(*cand_cost, 4)),
    );
    obj.insert(
        "window_delta_usd".to_owned(),
        json_float(round_half_even(window_delta, 4)),
    );
    obj.insert(
        "estimated_monthly_delta_usd".to_owned(),
        // `round(delta * factor, 4) if monthly_factor else None` — a factor of
        // 0.0 would be falsy too, but `observed_days > 0` guarantees it is not.
        monthly_factor.map_or(Value::Null, |factor| {
            json_float(round_half_even(window_delta * factor, 4))
        }),
    );
    obj.insert("evidence".to_owned(), Value::Object(evidence));
    obj.insert("rationale".to_owned(), Value::from(rationale));
    obj.insert(
        "caveats".to_owned(),
        Value::Array(caveats.into_iter().map(Value::from).collect()),
    );
    Value::Object(obj)
}

/// `x or default` for a nullable TEXT column.
fn non_empty_or(value: Option<&str>, default: &str) -> String {
    value
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
        .to_owned()
}

/// `build_routing_recommendations(conn, scope=…, project_ids=…)`.
///
/// `project_ids`: `None` = whole store; `Some([])` = a filter that matched
/// nothing, which returns empty and **never silently widens**.
///
/// Three rules, each of which simply does not fire when its evidence is
/// missing: `downshift_low_reasoning`, `downshift_short_output`,
/// `upshift_reasoning_quality`. The delta sign matches `/api/whatif` —
/// `candidate − actual`, so negative is cheaper — and the recommendation list
/// is sorted by it ascending, i.e. biggest saving first and upshifts trailing.
///
/// # Panics
/// Never: the `expect` on the candidate comparison is over costs that were
/// filtered to finite positives before the comparison.
#[must_use]
pub fn build_routing_recommendations(
    conn: &Connection,
    engine: &PricingEngine,
    candidates: &[Candidate],
    scope: Option<&Scope>,
    project_ids: Option<&[i64]>,
) -> Value {
    let (rollups, observed_days) = load_model_rollups(conn, scope, project_ids);
    if rollups.is_empty() {
        let mut empty = Map::new();
        empty.insert("recommendations".to_owned(), Value::Array(Vec::new()));
        empty.insert("models".to_owned(), Value::Array(Vec::new()));
        empty.insert("observed_days".to_owned(), Value::from(0));
        empty.insert("monthly_factor".to_owned(), Value::Null);
        empty.insert("caveats".to_owned(), Value::Array(Vec::new()));
        return Value::Object(empty);
    }

    let quality = load_quality_by_model(conn, scope, project_ids);
    #[allow(clippy::cast_precision_loss, reason = "a day count is far below 2^53")]
    let monthly_factor = (observed_days > 0).then(|| BASELINE_MONTH_DAYS / observed_days as f64);

    let mut model_rows: Vec<(f64, Value)> = Vec::new();
    let mut recs: Vec<Value> = Vec::new();

    for r in &rollups {
        let model = r.model.clone();
        let provider = non_empty_or(r.provider.as_deref(), "anthropic");
        let speed = non_empty_or(r.speed.as_deref(), "standard");
        let (work_type, reasoning_share) =
            classify_work_type(r.reasoning_tokens, r.output_tokens, r.events);
        let q = quality.get(&model).copied();

        let mut row = Map::new();
        row.insert("model".to_owned(), Value::from(model.clone()));
        row.insert("provider".to_owned(), Value::from(provider.clone()));
        row.insert("speed".to_owned(), Value::from(speed.clone()));
        row.insert("events".to_owned(), Value::from(r.events));
        row.insert("sessions".to_owned(), Value::from(r.sessions));
        row.insert("days_active".to_owned(), Value::from(r.days_active));
        let window_cost = round_half_even(r.cost_usd, 4);
        row.insert("window_cost_usd".to_owned(), json_float(window_cost));
        row.insert("output_tokens".to_owned(), Value::from(r.output_tokens));
        row.insert(
            "reasoning_tokens".to_owned(),
            Value::from(r.reasoning_tokens),
        );
        row.insert(
            "reasoning_share".to_owned(),
            json_float(round_half_even(reasoning_share, 4)),
        );
        row.insert(
            "reasoning_attributed".to_owned(),
            Value::Bool(r.reasoning_tokens > 0),
        );
        row.insert("work_type".to_owned(), Value::from(work_type));
        row.insert(
            "avg_quality_score".to_owned(),
            q.map_or(Value::Null, |(score, _)| {
                json_float(round_half_even(score, 3))
            }),
        );
        row.insert(
            "graded_sessions".to_owned(),
            Value::from(q.map_or(0, |(_, graded)| graded)),
        );
        model_rows.push((window_cost, Value::Object(row)));

        let actual_cost = r.cost_usd;
        if actual_cost <= 0.0 {
            continue; // nothing priced → no dollar claim to make
        }

        let shape = RawTokens::canonical(
            r.input_tokens,
            r.output_tokens,
            r.cache_create_tokens,
            r.cache_read_tokens,
        );

        // Reprice the observed shape on every SAME-PROVIDER candidate. A
        // candidate that prices at or below zero is skipped outright — a $0
        // candidate would fabricate a 100% saving.
        let mut priced: Vec<(f64, String, String)> = Vec::new();
        for cand in candidates {
            if cand.pricer != provider || same_model(&model, &cand.model) {
                continue;
            }
            let cost = engine
                .compute_cost(&shape, &cand.model, &cand.pricer, &speed, None)
                .total_cost;
            if cost <= 0.0 {
                continue;
            }
            priced.push((cost, cand.model.clone(), cand.label.clone()));
        }
        if priced.is_empty() {
            continue;
        }

        // `min(cheaper, key=lambda p: p[0])` — the FIRST minimum wins a tie,
        // in catalog order.
        let cheaper: Vec<&(f64, String, String)> =
            priced.iter().filter(|p| p.0 < actual_cost).collect();
        if let Some(best) = cheaper
            .iter()
            .copied()
            .min_by(|a, b| a.0.total_cmp(&b.0).then(std::cmp::Ordering::Greater))
        {
            let savings = actual_cost - best.0;
            let savings_share = savings / actual_cost;
            if savings >= MIN_WINDOW_SAVINGS_USD && savings_share >= MIN_SAVINGS_SHARE {
                if work_type == "low-reasoning" {
                    recs.push(make_recommendation(
                        "downshift_low_reasoning",
                        best,
                        format!(
                            "Only {} of {model}'s output tokens were reasoning in this \
                             window — light work its rate card overprices. Routing it to {} \
                             would have cost ${} instead of ${} ({} less).",
                            percent(reasoning_share, 1),
                            best.2,
                            grouped_fixed(best.0, 2),
                            grouped_fixed(actual_cost, 2),
                            percent(savings_share, 0),
                        ),
                        Vec::new(),
                        r,
                        work_type,
                        reasoning_share,
                        actual_cost,
                        monthly_factor,
                        q,
                    ));
                } else if work_type == "short-output (reasoning unattributed)" {
                    #[allow(
                        clippy::cast_precision_loss,
                        reason = "token and event counts are far below 2^53"
                    )]
                    let avg_out = r.output_tokens as f64 / r.events as f64;
                    recs.push(make_recommendation(
                        "downshift_short_output",
                        best,
                        format!(
                            "{model} averaged {} output tokens per event — short \
                             completions. Routing them to {} would have cost ${} instead \
                             of ${} ({} less).",
                            grouped_fixed(avg_out, 0),
                            best.2,
                            grouped_fixed(best.0, 2),
                            grouped_fixed(actual_cost, 2),
                            percent(savings_share, 0),
                        ),
                        vec![
                            "Reasoning attribution is unavailable for this provider — \
                             this recommendation is based on output size alone.",
                        ],
                        r,
                        work_type,
                        reasoning_share,
                        actual_cost,
                        monthly_factor,
                        q,
                    ));
                }
            }
        }

        // The upshift rule needs BOTH a reasoning-heavy signal and graded
        // evidence the current model is underperforming. No grade → no rec.
        if work_type == "reasoning-heavy"
            && let Some((score, graded)) = q
            && score < QUALITY_SCORE_FLOOR
        {
            let dearer: Vec<&(f64, String, String)> =
                priced.iter().filter(|p| p.0 > actual_cost).collect();
            if let Some(step_up) = dearer
                .iter()
                .copied()
                .min_by(|a, b| a.0.total_cmp(&b.0).then(std::cmp::Ordering::Greater))
            {
                recs.push(make_recommendation(
                    "upshift_reasoning_quality",
                    step_up,
                    format!(
                        "{model} spends {} of its output on reasoning but its graded \
                         sessions average {:.1}/5 (n={graded}). Routing this \
                         reasoning-heavy work to {} costs ${} more over the window — \
                         an investment in quality, not a saving.",
                        percent(reasoning_share, 0),
                        score,
                        step_up.2,
                        grouped_fixed(step_up.0 - actual_cost, 2),
                    ),
                    Vec::new(),
                    r,
                    work_type,
                    reasoning_share,
                    actual_cost,
                    monthly_factor,
                    q,
                ));
            }
        }
    }

    // `recs.sort(key=lambda rec: rec["window_delta_usd"])` — ascending, stable.
    recs.sort_by(|a, b| {
        let ka = a
            .get("window_delta_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let kb = b
            .get("window_delta_usd")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        ka.total_cmp(&kb)
    });
    // `sorted(model_rows, key=lambda m: -m["window_cost_usd"])` — stable, so
    // ties keep the GROUP BY order.
    model_rows.sort_by(|a, b| (-a.0).total_cmp(&(-b.0)));

    let mut out = Map::new();
    out.insert("recommendations".to_owned(), Value::Array(recs));
    out.insert(
        "models".to_owned(),
        Value::Array(model_rows.into_iter().map(|(_, row)| row).collect()),
    );
    out.insert("observed_days".to_owned(), Value::from(observed_days));
    out.insert(
        "monthly_factor".to_owned(),
        monthly_factor.map_or(Value::Null, |factor| json_float(round_half_even(factor, 4))),
    );
    out.insert(
        "caveats".to_owned(),
        Value::Array(ROUTING_CAVEATS.iter().map(|c| Value::from(*c)).collect()),
    );
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> PricingEngine {
        let package =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow");
        PricingEngine::from_manifest_path(&crate::pricing::manifest_path(&package))
            .expect("the shipped manifest")
    }

    fn preview(text: &str) -> Value {
        generate_claudemd_preview(&engine(), text, None, "CLAUDE.md", 100)
    }

    #[test]
    fn splitlines_keepends_splits_on_more_than_a_newline() {
        assert_eq!(splitlines_keepends(""), Vec::<String>::new());
        assert_eq!(splitlines_keepends("a\nb"), vec!["a\n", "b"]);
        // A trailing terminator does NOT produce a final empty element.
        assert_eq!(splitlines_keepends("a\n"), vec!["a\n"]);
        // `\r\n` is ONE boundary, `\r` alone is one too.
        assert_eq!(splitlines_keepends("a\r\nb\rc"), vec!["a\r\n", "b\r", "c"]);
        // Form feed and the file/group separators count as boundaries.
        assert_eq!(splitlines_keepends("a\x0cb"), vec!["a\u{0c}", "b"]);
        assert_eq!(splitlines_keepends("a\u{2028}b"), vec!["a\u{2028}", "b"]);
    }

    #[test]
    fn parse_then_render_round_trips_an_untouched_document_exactly() {
        for text in [
            "",
            "\n",
            "# H\n\nbody\n",
            "```rust\nlet x = 1;\n```\n\ntail\n",
            "<!-- unterminated comment\nkeeps going",
            "a\n\n\n\n\nb\n",
        ] {
            assert_eq!(render(&parse_blocks(text)), text, "round trip of {text:?}");
        }
    }

    #[test]
    fn a_fence_is_opaque_so_nothing_inside_it_is_a_heading_or_a_blank_run() {
        let blocks = parse_blocks("```\n# not a heading\n\n\n\n<!-- not a comment -->\n```\n");
        assert_eq!(blocks[0].kind, Kind::Fence);
        // One fence block swallowing everything up to the closing marker, then
        // the trailing empty line the final "\n" leaves behind.
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1].kind, Kind::Blank);
    }

    #[test]
    fn the_keep_heading_regex_is_word_bounded_and_case_insensitive() {
        assert!(keep_heading("## Hard RULES"));
        assert!(keep_heading("# The rule"));
        assert!(keep_heading("# Never do this"));
        assert!(keep_heading("### IMPORTANT"));
        // `rules?` cannot match inside a longer word.
        assert!(!keep_heading("# Ruleset overview"));
        assert!(!keep_heading("# Architecture"));
        // `must` inside `mustard` has no trailing boundary.
        assert!(!keep_heading("# Mustard"));
    }

    #[test]
    fn the_heading_slug_is_ascii_only_and_falls_back_to_section() {
        assert_eq!(heading_slug("## Hello, World!"), "hello-world");
        assert_eq!(heading_slug("#   Spaced   Out  "), "spaced-out");
        // Lowercased first, then filtered — so non-ASCII disappears entirely.
        assert_eq!(heading_slug("## ЗАГОЛОВОК"), "section");
        assert_eq!(heading_slug("###"), "section");
    }

    #[test]
    fn a_clean_document_reports_changed_false_and_an_empty_diff() {
        // Byte-compared against the reference on the same input.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&preview(
                "# Title\n\nA short paragraph that is here once.\n"
            )),
            concat!(
                r#"{"file_label":"CLAUDE.md","changed":false,"preview_diff":"","slimmed_text":"","#,
                r#""rationale":[],"original_tokens":11,"slimmed_tokens":11,"tokens_saved":0,"#,
                r#""estimated_savings_usd_per_session":0.0,"estimated_savings_usd_monthly":0.0,"#,
                r#""sessions_per_month":100,"heuristic":"tokens ≈ len(text)//4; savings priced as "#,
                r#"input tokens at claude-sonnet-4-6 via compute_cost; monthly = per-session × 100 sessions"}"#,
            )
        );
    }

    #[test]
    fn comments_and_blank_runs_produce_the_reference_diff_byte_for_byte() {
        // Expected value produced by running the reference
        // `generate_claudemd_preview` on this exact input.
        let text = "# Title\n\n<!-- author note\nspanning lines -->\n\nBody text goes here \
                    and is long enough to matter for the dedupe rule check.\n\n\n\n\nTail.\n";
        let got = preview(text);
        assert_eq!(
            got["preview_diff"].as_str().expect("string"),
            "--- CLAUDE.md\n+++ CLAUDE.md (slim preview)\n@@ -1,11 +1,6 @@\n # Title\n \n\
             -<!-- author note\n-spanning lines -->\n \n Body text goes here and is long \
             enough to matter for the dedupe rule check.\n \n-\n-\n-\n Tail.\n"
        );
        assert_eq!(
            got["slimmed_text"].as_str().expect("string"),
            "# Title\n\n\nBody text goes here and is long enough to matter for the dedupe \
             rule check.\n\nTail.\n"
        );
        assert_eq!(got["original_tokens"], Value::from(33));
        assert_eq!(got["slimmed_tokens"], Value::from(23));
        assert_eq!(got["tokens_saved"], Value::from(10));
        let rationale = got["rationale"].as_array().expect("array");
        assert_eq!(rationale.len(), 2);
        assert_eq!(rationale[0]["rule"], Value::from("strip_html_comments"));
        assert_eq!(rationale[0]["tokens_saved"], Value::from(9));
        assert_eq!(
            rationale[0]["summary"],
            Value::from(
                "Removed 1 HTML comment block(s) — author notes the model pays to read \
                 every session."
            )
        );
        assert_eq!(rationale[1]["rule"], Value::from("collapse_blank_runs"));
        assert_eq!(
            rationale[1]["detail"]["blank_lines_removed"],
            Value::from(3)
        );
    }

    #[test]
    fn a_duplicate_paragraph_is_dropped_and_priced() {
        let para =
            "A duplicated paragraph long enough to pass the sixty character floor easily.\n\n";
        let text = format!("# T\n\n{para}{para}End.\n");
        let got = preview(&text);
        assert_eq!(
            got["preview_diff"].as_str().expect("string"),
            concat!(
                "--- CLAUDE.md\n+++ CLAUDE.md (slim preview)\n@@ -2,6 +2,5 @@\n \n",
                " A duplicated paragraph long enough to pass the sixty character floor easily.\n",
                " \n",
                "-A duplicated paragraph long enough to pass the sixty character floor easily.\n",
                " \n End.\n",
            )
        );
        assert_eq!(got["tokens_saved"], Value::from(19));
        // 19 input tokens at sonnet-4-6 → $0.0001 per session, ×100 → $0.01.
        assert_eq!(got["estimated_savings_usd_per_session"], json_float(0.0001));
        assert_eq!(got["estimated_savings_usd_monthly"], json_float(0.01));
    }

    #[test]
    fn a_paragraph_under_the_dedupe_floor_is_left_alone() {
        // `len(norm) >= DEDUPE_MIN_CHARS` — 60 normalised characters.
        let text = "# T\n\nshort line\n\nshort line\n";
        let got = preview(text);
        assert_eq!(got["changed"], Value::Bool(false));
    }

    #[test]
    fn section_extraction_only_runs_when_the_document_is_bloat_flagged() {
        // A section body over 600 tokens, in a document under the 5_000-token
        // bloat threshold: no extraction, because the rule is gated.
        let body = "word ".repeat(600); // 3_000 chars ≈ 750 tokens
        let text = format!("# Architecture\n\n{body}\n");
        assert_eq!(preview(&text)["changed"], Value::Bool(false));

        // Same text, but the caller passes the bloat finding → the rule fires.
        let finding = serde_json::json!({"pattern_id": "bloated_claude_md"});
        let flagged = generate_claudemd_preview(
            &engine(),
            &text,
            Some(std::slice::from_ref(&finding)),
            "CLAUDE.md",
            100,
        );
        assert_eq!(flagged["changed"], Value::Bool(true));
        let rationale = flagged["rationale"].as_array().expect("array");
        assert_eq!(
            rationale[0]["rule"],
            Value::from("extract_oversized_sections")
        );
        // The `count` key is appended AFTER `extracted_sections`.
        assert!(stax_memory::pyjson::dumps_http(&rationale[0]["detail"]).contains("\"count\""));
        assert_eq!(rationale[0]["detail"]["count"], Value::from(1));
        assert_eq!(
            rationale[0]["detail"]["extracted_sections"][0]["suggested_path"],
            Value::from("docs/claude-md/architecture.md")
        );
    }

    #[test]
    fn a_rules_titled_section_is_never_extracted_however_long() {
        let body = "word ".repeat(600);
        let text = format!("# Hard rules\n\n{body}\n");
        let finding = serde_json::json!({"pattern_id": "bloated_claude_md"});
        let got = generate_claudemd_preview(
            &engine(),
            &text,
            Some(std::slice::from_ref(&finding)),
            "CLAUDE.md",
            100,
        );
        assert_eq!(got["changed"], Value::Bool(false));
    }

    #[test]
    fn the_bloat_flag_also_accepts_the_context_budget_shape() {
        let budget = serde_json::json!({"kind": "context_budget_bloat"});
        assert!(bloat_flagged(Some(std::slice::from_ref(&budget))));
        let other = serde_json::json!({"pattern_id": "junk_reads"});
        assert!(!bloat_flagged(Some(std::slice::from_ref(&other))));
        assert!(!bloat_flagged(None));
    }

    #[test]
    fn the_unified_range_collapses_a_single_line_and_backs_up_an_empty_one() {
        assert_eq!(format_range_unified(0, 1), "1");
        assert_eq!(format_range_unified(0, 3), "1,3");
        // An EMPTY range begins at the line just before it — `0,0`, not `1,0`.
        assert_eq!(format_range_unified(0, 0), "0,0");
        assert_eq!(format_range_unified(5, 5), "5,0");
    }

    #[test]
    fn a_pure_insertion_at_the_head_is_the_diff_python_writes() {
        let a: Vec<String> = vec!["b\n".into(), "c\n".into()];
        let b: Vec<String> = vec!["a\n".into(), "b\n".into(), "c\n".into()];
        assert_eq!(
            unified_diff(&a, &b, "old", "new"),
            "--- old\n+++ new\n@@ -1,2 +1,3 @@\n+a\n b\n c\n"
        );
    }

    #[test]
    fn two_distant_edits_become_two_hunks_not_one() {
        let a: Vec<String> = (0..30).map(|i| format!("line {i}\n")).collect();
        let mut b = a.clone();
        b[1] = "CHANGED 1\n".into();
        b[28] = "CHANGED 28\n".into();
        let diff = unified_diff(&a, &b, "old", "new");
        assert_eq!(diff.matches("@@ -").count(), 2, "{diff}");
        assert!(
            diff.starts_with("--- old\n+++ new\n@@ -1,5 +1,5 @@\n line 0\n-line 1\n+CHANGED 1\n")
        );
        assert!(
            diff.ends_with("-line 28\n+CHANGED 28\n line 29\n"),
            "{diff}"
        );
    }

    #[test]
    fn the_date_suffix_rule_treats_a_dated_id_as_the_same_model() {
        assert!(same_model(
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-5"
        ));
        assert!(same_model(
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929"
        ));
        // The prefix leg: one id is the other plus a `-`-separated suffix.
        assert!(same_model("claude-opus-4", "claude-opus-4-8"));
        assert!(!same_model("claude-opus-4-8", "claude-haiku-4-5"));
    }

    #[test]
    fn zero_reasoning_tokens_means_unattributed_not_low_reasoning() {
        // The whole point of the v026 attribution column: 0 is "we could not
        // measure it", never "it did no reasoning".
        assert_eq!(
            classify_work_type(0, 100, 1).0,
            "short-output (reasoning unattributed)"
        );
        assert_eq!(classify_work_type(0, 100_000, 1).0, "unattributed");
        assert_eq!(classify_work_type(1, 1_000, 1).0, "low-reasoning");
        assert_eq!(classify_work_type(300, 1_000, 1).0, "reasoning-heavy");
        assert_eq!(classify_work_type(100, 1_000, 1).0, "mixed");
        // No output at all → share 0.0 and the events leg decides.
        assert_eq!(
            classify_work_type(0, 0, 0).0,
            "short-output (reasoning unattributed)"
        );
    }

    #[test]
    fn the_percent_and_money_formats_match_pythons() {
        assert_eq!(percent(0.0321, 1), "3.2%");
        assert_eq!(percent(0.456, 0), "46%");
        assert_eq!(grouped_fixed(1234.5, 2), "1,234.50");
        assert_eq!(grouped_fixed(1_234_567.0, 0), "1,234,567");
        assert_eq!(grouped_fixed(-1234.5, 2), "-1,234.50");
    }

    #[test]
    fn a_store_with_no_usage_events_is_an_empty_but_well_formed_payload() {
        let conn = Connection::open_in_memory().expect("in-memory");
        assert_eq!(
            stax_memory::pyjson::dumps_http(&build_routing_recommendations(
                &conn,
                &engine(),
                &[],
                None,
                None
            )),
            r#"{"recommendations":[],"models":[],"observed_days":0,"monthly_factor":null,"caveats":[]}"#
        );
    }

    #[test]
    fn an_empty_project_id_filter_returns_empty_and_never_widens_to_the_store() {
        let conn = seeded();
        let all = build_routing_recommendations(&conn, &engine(), &[], None, None);
        assert_eq!(all["models"].as_array().expect("array").len(), 1);
        // `Some([])` is "a filter that matched nothing", NOT "no filter".
        let none = build_routing_recommendations(&conn, &engine(), &[], None, Some(&[]));
        assert_eq!(none["models"].as_array().expect("array").len(), 0);
        assert_eq!(none["observed_days"], Value::from(0));
    }

    #[test]
    fn a_downshift_fires_on_short_output_with_the_attribution_caveat() {
        let conn = seeded();
        let candidates = routing_candidates(
            &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../stackunderflow"),
        );
        assert!(!candidates.is_empty(), "the shipped catalog loads");
        let payload = build_routing_recommendations(&conn, &engine(), &candidates, None, None);
        // Byte-compared against the reference run on the identical store. The
        // rationale sentence, the four-place rounding and the key order are all
        // in here on purpose — every one of them is a contract.
        assert_eq!(
            stax_memory::pyjson::dumps_http(&payload),
            concat!(
                r#"{"recommendations":[{"rec_id":"downshift_short_output","#,
                r#""work_type":"short-output (reasoning unattributed)","#,
                r#""from_model":"claude-opus-4-8","provider":"anthropic","speed":"standard","#,
                r#""to_model":"claude-haiku-4-5-20251001","to_label":"Claude Haiku 4.5","#,
                r#""window_cost_usd":15.0,"candidate_window_cost_usd":1.0005,"#,
                r#""window_delta_usd":-13.9995,"estimated_monthly_delta_usd":-419.985,"#,
                r#""evidence":{"events":1,"sessions":1,"days_active":1,"output_tokens":100,"#,
                r#""reasoning_tokens":0,"reasoning_share":0.0,"#,
                r#""avg_output_tokens_per_event":100.0,"avg_quality_score":null,"#,
                r#""graded_sessions":0},"#,
                r#""rationale":"claude-opus-4-8 averaged 100 output tokens per event — "#,
                r#"short completions. Routing them to Claude Haiku 4.5 would have cost "#,
                r#"$1.00 instead of $15.00 (93% less).","#,
                r#""caveats":["Reasoning attribution is unavailable for this provider — "#,
                r#"this recommendation is based on output size alone."]}],"#,
                r#""models":[{"model":"claude-opus-4-8","provider":"anthropic","#,
                r#""speed":"standard","events":1,"sessions":1,"days_active":1,"#,
                r#""window_cost_usd":15.0,"output_tokens":100,"reasoning_tokens":0,"#,
                r#""reasoning_share":0.0,"reasoning_attributed":false,"#,
                r#""work_type":"short-output (reasoning unattributed)","#,
                r#""avg_quality_score":null,"graded_sessions":0}],"#,
                r#""observed_days":1,"monthly_factor":30.0,"#,
                r#""caveats":["Candidate costs are a rate-card swap of the observed token "#,
                r#"shape, not a re-run — a different model may tokenize differently or need "#,
                r#"more/fewer output tokens for the same task.","#,
                r#""Monthly figures extrapolate the window spend by 30 / observed active "#,
                r#"days; they assume the window is representative."]}"#,
            )
        );
    }

    /// A store with one expensive, short-output Opus rollup — the shape the
    /// `downshift_short_output` rule is built for.
    fn seeded() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory");
        conn.execute_batch(
            "CREATE TABLE usage_events (
                 project_id INTEGER, session_id TEXT, ts TEXT, day TEXT,
                 model TEXT, provider TEXT, speed TEXT,
                 input_tokens INTEGER, output_tokens INTEGER,
                 cache_read_tokens INTEGER, cache_create_tokens INTEGER,
                 reasoning_tokens INTEGER, cost_usd REAL);
             INSERT INTO usage_events VALUES
                 (1, 's1', '2026-07-01T00:00:00+00:00', '2026-07-01',
                  'claude-opus-4-8', 'anthropic', 'standard',
                  1000000, 100, 0, 0, 0, 15.0);",
        )
        .expect("schema");
        conn
    }
}
