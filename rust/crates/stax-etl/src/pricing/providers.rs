//! The pluggable pricers — port of `stackunderflow/infra/providers/`.
//!
//! Python discovers these by walking the package and registering one singleton
//! per concrete `ProviderPricer` subclass; the class attribute `provider_name` IS
//! the registry key and `provider_aliases` maps extra strings onto the *same
//! singleton*. Two behaviours of that design are load-bearing and reproduced
//! here rather than approximated:
//!
//! * `get_pricer` on an unknown name returns the Anthropic pricer, so a record
//!   with a missing provider prices conservatively instead of raising
//!   mid-aggregation;
//! * `resolve_pricing_provider` compares `shell is upstream` — *singleton
//!   identity*, not name equality. `claude` and `anthropic` are the same object,
//!   so the recorded provider is kept; `kilocode` and `cline` are different
//!   objects even though `KiloCodePricer` subclasses `ClinePricer`. [`Pricer`] is
//!   a `Copy` enum whose variants are exactly those singletons, so `==` on it is
//!   `is` on them.
//!
//! Only Anthropic and OpenAI carry rates of their own; every other pricer is
//! either a shell that delegates by model-id prefix or a table lookup. The rate
//! tables that are still *in code* on the Python side (OpenAI's non-manifest
//! families, Gemini, Qwen, Cursor) are transcribed here because they are code
//! there too — `data/models.toml` is the only file that is data, and it is read,
//! not copied.

use super::manifest::{Manifest, Rates};
use super::{RawTokens, Tokens};

/// One registered pricer singleton.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pricer {
    /// `AnthropicPricer` — manifest-backed; also the registry fallback.
    Anthropic,
    /// `AntigravityPricer` — delegates to Gemini's table.
    Antigravity,
    /// `ClinePricer` — vendor-prefix delegation plus the `cline-auto` peg.
    Cline,
    /// `CodeiumPricer` — a stub with no rate card.
    Codeium,
    /// `ContinuePricer` — vendor-prefix delegation.
    Continue,
    /// `CopilotPricer` — vendor-prefix delegation.
    Copilot,
    /// `CursorPricer` — cursor-native table, then vendor delegation, then a
    /// Sonnet-tier estimate rather than `$0`.
    Cursor,
    /// `CursorAgentPricer` — bare-prefix delegation only.
    CursorAgent,
    /// `DroidPricer` — vendor-prefix delegation plus the `droid-auto` peg.
    Droid,
    /// `GeminiPricer` — static table.
    Gemini,
    /// `HermesPricer` — OpenAI on `gpt`/`codex`, Anthropic otherwise.
    Hermes,
    /// `KiloCodePricer` — a distinct singleton with `ClinePricer`'s behaviour.
    KiloCode,
    /// `KiroPricer` — dotted ids normalised, `kiro-auto` unpriced.
    Kiro,
    /// `OpenAIPricer` — manifest-first, then the in-code family ladder.
    OpenAi,
    /// `OpenClawPricer` — OpenAI on `gpt`/`codex`, Anthropic otherwise.
    OpenClaw,
    /// `OpenCodePricer` — bare-prefix delegation only.
    OpenCode,
    /// `PiPricer` — everything routes through OpenAI, default `gpt-5`.
    Pi,
    /// `QwenPricer` — static table.
    Qwen,
    /// `RooCodePricer` — a distinct singleton with `ClinePricer`'s behaviour.
    RooCode,
}

/// Every pricer, in the order Python's package walk registers them (modules
/// sorted by name). Order is not behaviourally load-bearing — the routing rules
/// are sorted by a total key — but keeping it makes the two registries
/// diff-comparable.
pub const ALL: [Pricer; 19] = [
    Pricer::Anthropic,
    Pricer::Antigravity,
    Pricer::Cline,
    Pricer::Codeium,
    Pricer::Continue,
    Pricer::Copilot,
    Pricer::Cursor,
    Pricer::CursorAgent,
    Pricer::Droid,
    Pricer::Gemini,
    Pricer::Hermes,
    Pricer::KiloCode,
    Pricer::Kiro,
    Pricer::OpenAi,
    Pricer::OpenClaw,
    Pricer::OpenCode,
    Pricer::Pi,
    Pricer::Qwen,
    Pricer::RooCode,
];

// ── in-code rate tables (these are code on the Python side too) ──────────────

/// `infra/providers/cursor.py::_SONNET_TIER` — ESTIMATED, Anthropic Sonnet 4.x.
const SONNET_TIER: Rates = (3.0, 15.0, 3.75, 0.30);
/// `infra/providers/cursor.py::_COMPOSER_1_TIER` — Cursor-published.
const COMPOSER_1_TIER: Rates = (1.25, 10.00, 1.5625, 0.125);

/// `infra/providers/cursor.py::_CURSOR_RATES`.
const CURSOR_RATES: [(&str, Rates); 6] = [
    ("composer-1", COMPOSER_1_TIER),
    ("composer-2", SONNET_TIER),
    ("cursor-auto", SONNET_TIER),
    ("cursor-fast", SONNET_TIER),
    ("auto", SONNET_TIER),
    ("fast", SONNET_TIER),
];

/// `infra/providers/gemini.py::_RATES`.
const GEMINI_RATES: [(&str, Rates); 11] = [
    ("gemini-2.5-pro", (1.25, 10.00, 0.0, 0.31)),
    ("gemini-2.5-flash", (0.30, 2.50, 0.0, 0.075)),
    ("gemini-2.5-flash-lite", (0.10, 0.40, 0.0, 0.025)),
    ("gemini-1.5-pro", (1.25, 5.00, 0.0, 0.3125)),
    ("gemini-1.5-flash", (0.075, 0.30, 0.0, 0.018_75)),
    ("gemini-3.1-pro", (2.00, 12.00, 0.0, 0.50)),
    ("gemini-3.0-pro", (2.00, 12.00, 0.0, 0.50)),
    ("gemini-3-pro-preview", (2.00, 12.00, 0.0, 0.50)),
    ("gemini-3.1-pro-preview", (2.00, 12.00, 0.0, 0.50)),
    ("gemini-3-flash-preview", (0.30, 2.50, 0.0, 0.075)),
    ("gemini-auto", (1.25, 10.00, 0.0, 0.31)),
];

/// `infra/providers/qwen.py::_RATES`.
const QWEN_RATES: [(&str, Rates); 8] = [
    ("qwen-max", (3.00, 12.00, 0.0, 0.30)),
    ("qwen-max-longcontext", (3.00, 12.00, 0.0, 0.30)),
    ("qwen-plus", (1.20, 3.60, 0.0, 0.12)),
    ("qwen-turbo", (0.30, 0.60, 0.0, 0.03)),
    ("qwen-coder", (1.20, 3.60, 0.0, 0.12)),
    ("qwen-coder-plus", (1.20, 3.60, 0.0, 0.12)),
    ("qwen3-coder", (1.20, 3.60, 0.0, 0.12)),
    ("qwen-auto", (1.20, 3.60, 0.0, 0.12)),
];

/// `infra/providers/openai.py::_RATES`, keyed by `_Family` member NAME because
/// `rates_for` looks the family up by name (`_Family[canonical]`).
const OPENAI_RATES: [(&str, Rates); 9] = [
    ("GPT_5_CODEX", (1.25, 10.0, 0.0, 0.125)),
    ("GPT_52_CODEX", (1.25, 10.0, 0.0, 0.125)),
    ("GPT_53_CODEX", (1.25, 10.0, 0.0, 0.125)),
    ("GPT_54", (2.50, 20.0, 0.0, 0.25)),
    ("GPT_5", (2.50, 20.0, 0.0, 0.25)),
    ("GPT_5_MINI", (0.25, 2.00, 0.0, 0.025)),
    ("GPT_4O", (2.50, 10.0, 0.0, 1.25)),
    ("GPT_4O_MINI", (0.15, 0.60, 0.0, 0.075)),
    ("GPT_41", (2.50, 10.0, 0.0, 0.625)),
];

/// `infra/providers/openai.py::_FALLBACK`.
const OPENAI_FALLBACK: &str = "GPT_5_CODEX";

fn table_lookup(table: &[(&str, Rates)], key: &str) -> Option<Rates> {
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, rates)| *rates)
}

/// `_RATES[_FALLBACK]` — the rate every unrecognised OpenAI id lands on.
fn openai_fallback_rates() -> Rates {
    table_lookup(&OPENAI_RATES, OPENAI_FALLBACK)
        .expect("the OpenAI fallback family is always in the in-code table")
}

impl Pricer {
    /// The `provider_name` class attribute — the registry key and the singleton's
    /// identity.
    #[must_use]
    pub const fn provider_name(self) -> &'static str {
        match self {
            Pricer::Anthropic => "anthropic",
            Pricer::Antigravity => "antigravity",
            Pricer::Cline => "cline",
            Pricer::Codeium => "codeium",
            Pricer::Continue => "continue",
            Pricer::Copilot => "copilot",
            Pricer::Cursor => "cursor",
            Pricer::CursorAgent => "cursor-agent",
            Pricer::Droid => "droid",
            Pricer::Gemini => "gemini",
            Pricer::Hermes => "hermes",
            Pricer::KiloCode => "kilocode",
            Pricer::Kiro => "kiro",
            Pricer::OpenAi => "openai",
            Pricer::OpenClaw => "openclaw",
            Pricer::OpenCode => "opencode",
            Pricer::Pi => "pi",
            Pricer::Qwen => "qwen",
            Pricer::RooCode => "roocode",
        }
    }

    /// The `provider_aliases` class attribute — extra registry keys that resolve
    /// to this same singleton.
    #[must_use]
    pub const fn provider_aliases(self) -> &'static [&'static str] {
        match self {
            Pricer::Anthropic => &["claude"],
            Pricer::OpenAi => &["codex"],
            Pricer::Pi => &["omp"],
            _ => &[],
        }
    }

    /// `model_id_prefixes` — routing hints the pricer declares about itself.
    #[must_use]
    pub const fn model_id_prefixes(self) -> &'static [&'static str] {
        match self {
            Pricer::Anthropic => &["glm-"],
            Pricer::Cursor => &["composer-", "cursor-"],
            Pricer::Gemini => &["gemini"],
            Pricer::Qwen => &["qwen"],
            _ => &[],
        }
    }

    /// `model_id_substrings` — routing hints the pricer declares about itself.
    #[must_use]
    pub const fn model_id_substrings(self) -> &'static [&'static str] {
        match self {
            Pricer::Anthropic => &["claude"],
            Pricer::OpenAi => &["gpt", "codex"],
            _ => &[],
        }
    }

    /// `supports_per_message_tokens()`.
    #[must_use]
    pub const fn supports_per_message_tokens(self) -> bool {
        // False for the sources whose token counts are estimated or absent:
        // Antigravity's payloads are encrypted, Codeium is a discovery-only stub,
        // Cursor and Kiro estimate from character length, Cursor Agent has no
        // counts at all.
        !matches!(
            self,
            Pricer::Antigravity
                | Pricer::Codeium
                | Pricer::Cursor
                | Pricer::CursorAgent
                | Pricer::Kiro
        )
    }

    /// `normalize_tokens()` — reshape raw provider tokens into the canonical four.
    ///
    /// Every pricer but OpenAI's is the Anthropic no-op coercion, including the
    /// ones that *delegate rates* to OpenAI (Pi) or Gemini (Antigravity): the
    /// Python classes define their own no-op rather than inheriting the
    /// delegate's, and Pi's records already arrive canonical.
    #[must_use]
    pub fn normalize_tokens(self, raw: &RawTokens) -> Tokens {
        match self {
            Pricer::OpenAi => normalize_openai(raw),
            _ => Tokens {
                input: raw.get("input").unwrap_or(0),
                output: raw.get("output").unwrap_or(0),
                cache_creation: raw.get("cache_creation").unwrap_or(0),
                cache_read: raw.get("cache_read").unwrap_or(0),
            },
        }
    }

    /// `canonicalize()` — resolve a free-form model id to this pricer's key.
    ///
    /// `None` mirrors Python returning `None`, which only the manifest-backed
    /// Anthropic path can do (and only when the manifest declares no fallback).
    #[must_use]
    pub fn canonicalize(self, manifest: &Manifest, model_id: &str) -> Option<String> {
        match self {
            Pricer::Anthropic => manifest.canonicalize(model_id, "anthropic"),
            Pricer::OpenAi => Some(
                manifest
                    .canonicalize(model_id, "openai")
                    .unwrap_or_else(|| identify_openai(model_id).to_string()),
            ),
            // Antigravity delegates identity to Gemini.
            Pricer::Antigravity | Pricer::Gemini => Some(lower_strip(model_id)),
            Pricer::Droid | Pricer::Hermes | Pricer::OpenClaw | Pricer::Qwen => {
                Some(lower_strip(model_id))
            }
            Pricer::Cursor => Some(lower_strip(model_id)),
            // `model_id or ""` — pass through, NOT lower-cased.
            Pricer::Cline
            | Pricer::KiloCode
            | Pricer::RooCode
            | Pricer::Codeium
            | Pricer::Continue
            | Pricer::Copilot
            | Pricer::CursorAgent
            | Pricer::OpenCode => Some(model_id.to_string()),
            Pricer::Kiro => Some(lower_strip(model_id).replace('.', "-")),
            // Pi defaults to gpt-5 and otherwise defers to OpenAI's identity.
            Pricer::Pi => Pricer::OpenAi.canonicalize(
                manifest,
                if model_id.is_empty() {
                    "gpt-5"
                } else {
                    model_id
                },
            ),
        }
    }

    /// `rates_for()` — `(input, output, cache_write, cache_read)` in `$/M`, or
    /// `None` for "this canonical id is unknown to me".
    #[must_use]
    pub fn rates_for(self, manifest: &Manifest, canonical: Option<&str>) -> Option<Rates> {
        // Three pricers do NOT start with `if not canonical: return None`, and
        // the difference is observable: the Anthropic manifest lookup resolves a
        // `None` family to the provider fallback, and OpenAI's `_Family[None]`
        // raises `KeyError` straight into `_RATES[_FALLBACK]`.
        match self {
            Pricer::Anthropic => return manifest.rates_for(canonical, "anthropic", None),
            Pricer::OpenAi => {
                return Some(
                    manifest
                        .rates_for(canonical, "openai", None)
                        .or_else(|| canonical.and_then(|c| table_lookup(&OPENAI_RATES, c)))
                        .unwrap_or_else(openai_fallback_rates),
                );
            }
            Pricer::Pi => return Pricer::OpenAi.rates_for(manifest, canonical),
            _ => {}
        }
        // `if not canonical: return None` — absent and empty are both falsy.
        let canonical = canonical.filter(|c| !c.is_empty())?;
        match self {
            Pricer::Anthropic | Pricer::OpenAi | Pricer::Pi => {
                unreachable!("handled above")
            }
            Pricer::Antigravity | Pricer::Gemini => table_lookup(&GEMINI_RATES, canonical),
            Pricer::Qwen => table_lookup(&QWEN_RATES, canonical),
            Pricer::Codeium => None,
            Pricer::Cline | Pricer::KiloCode | Pricer::RooCode => {
                cline_rates(manifest, canonical, true)
            }
            Pricer::Continue | Pricer::Copilot => cline_rates(manifest, canonical, false),
            Pricer::CursorAgent => {
                let lowered = canonical.to_lowercase();
                if lowered.starts_with("claude-") {
                    return delegate(manifest, Pricer::Anthropic, &lowered);
                }
                if lowered.starts_with("gpt-") {
                    return delegate(manifest, Pricer::OpenAi, &lowered);
                }
                None
            }
            Pricer::OpenCode => {
                let lowered = canonical.to_lowercase();
                if lowered.starts_with("claude-") {
                    return delegate(manifest, Pricer::Anthropic, &lowered);
                }
                if lowered.starts_with("gpt-") || lowered.starts_with("codex-") {
                    return delegate(manifest, Pricer::OpenAi, &lowered);
                }
                None
            }
            Pricer::Droid => {
                if canonical.starts_with("claude-") {
                    return delegate(manifest, Pricer::Anthropic, canonical);
                }
                if canonical.starts_with("gpt-") || canonical.contains("codex") {
                    return delegate(manifest, Pricer::OpenAi, canonical);
                }
                if canonical == "droid-auto" {
                    return delegate(manifest, Pricer::Anthropic, "claude-sonnet-4-5");
                }
                None
            }
            Pricer::Hermes | Pricer::OpenClaw => {
                if canonical.starts_with("gpt-") || canonical.contains("codex") {
                    return delegate(manifest, Pricer::OpenAi, canonical);
                }
                delegate(manifest, Pricer::Anthropic, canonical)
            }
            Pricer::Kiro => {
                if canonical == "kiro-auto" {
                    return None;
                }
                if canonical.starts_with("claude-") || canonical.starts_with("anthropic/") {
                    let target = canonical
                        .split_once('/')
                        .map_or(canonical, |(_, rest)| rest);
                    return delegate(manifest, Pricer::Anthropic, target);
                }
                if canonical.starts_with("gpt-") || canonical.starts_with("openai/") {
                    let target = canonical
                        .split_once('/')
                        .map_or(canonical, |(_, rest)| rest);
                    return delegate(manifest, Pricer::OpenAi, target);
                }
                None
            }
            Pricer::Cursor => cursor_rates(manifest, canonical),
        }
    }

    /// `compute()` — tokens × `rates_for(canonicalize(model))`, with the two
    /// overrides Python declares.
    ///
    /// `AnthropicPricer.compute` threads `at_ts` into the manifest lookup and
    /// folds the priority/fast multiplier into input+output (never cache);
    /// `OpenAIPricer.compute` threads `at_ts` and falls back to the in-code table
    /// when the manifest does not carry the family. Every other pricer uses the
    /// base implementation, which ignores both `speed` and `at_ts`.
    #[must_use]
    pub fn compute(
        self,
        manifest: &Manifest,
        tokens: &Tokens,
        model: &str,
        speed: &str,
        at_ts: Option<&str>,
    ) -> super::CostBreakdown {
        match self {
            Pricer::Anthropic => {
                let canonical = self.canonicalize(manifest, model);
                let mut rates = manifest.rates_for(canonical.as_deref(), "anthropic", at_ts);
                if speed == "fast"
                    && let Some(r) = rates
                    && let Some(mult) = manifest.fast_multiplier(canonical.as_deref(), "anthropic")
                {
                    rates = Some((r.0 * mult, r.1 * mult, r.2, r.3));
                }
                super::apply_rates(tokens, rates)
            }
            Pricer::OpenAi => {
                let canonical = self.canonicalize(manifest, model);
                let rates = manifest
                    .rates_for(canonical.as_deref(), "openai", at_ts)
                    .or_else(|| self.rates_for(manifest, canonical.as_deref()));
                super::apply_rates(tokens, rates)
            }
            _ => {
                let canonical = self.canonicalize(manifest, model);
                let rates = self.rates_for(manifest, canonical.as_deref());
                super::apply_rates(tokens, rates)
            }
        }
    }
}

/// `canonicalize` for the pricers that do `model_id.strip().lower()`.
///
/// Python guards these with `isinstance(model_id, str)`; a Rust `&str` is always
/// one, so only the strip+lower survives. The empty-string result is preserved
/// because several `rates_for` bodies branch on it.
fn lower_strip(model_id: &str) -> String {
    model_id.trim().to_lowercase()
}

/// The shared body of `ClinePricer` / `ContinuePricer` / `CopilotPricer`.
///
/// `cline_auto_peg` is the one difference: Cline (and its `kilocode` / `roocode`
/// subclasses) pegs the `cline-auto` autoselector to Sonnet 4.5 rather than
/// leaving it at `$0`. Note the Python compares the *original* `canonical` there,
/// not the lower-cased copy — so `CLINE-AUTO` is not pegged.
fn cline_rates(manifest: &Manifest, canonical: &str, cline_auto_peg: bool) -> Option<Rates> {
    if canonical.is_empty() {
        return None;
    }
    let lowered = canonical.to_lowercase();
    let (vendor, suffix) = match lowered.split_once('/') {
        Some((v, s)) => (v.to_string(), s.to_string()),
        None => (String::new(), lowered.clone()),
    };
    if vendor == "anthropic" || lowered.starts_with("claude-") {
        let target = if vendor == "anthropic" {
            &suffix
        } else {
            &lowered
        };
        return delegate(manifest, Pricer::Anthropic, target);
    }
    if vendor == "openai" || lowered.starts_with("gpt-") {
        let target = if vendor == "openai" {
            &suffix
        } else {
            &lowered
        };
        return delegate(manifest, Pricer::OpenAi, target);
    }
    if cline_auto_peg && canonical == "cline-auto" {
        return delegate(manifest, Pricer::Anthropic, "claude-sonnet-4-5");
    }
    None
}

/// `CursorPricer.rates_for` — table, then vendor delegation (with the Gemini
/// suffix retry), then the Sonnet-tier estimate rather than `$0`.
fn cursor_rates(manifest: &Manifest, canonical: &str) -> Option<Rates> {
    if canonical.is_empty() {
        return None;
    }
    if let Some(rates) = table_lookup(&CURSOR_RATES, canonical) {
        return Some(rates);
    }
    if canonical.starts_with("claude-") {
        return delegate(manifest, Pricer::Anthropic, canonical);
    }
    if canonical.starts_with("gpt-") || canonical.starts_with("codex") {
        return delegate(manifest, Pricer::OpenAi, canonical);
    }
    if canonical.starts_with("gemini-") {
        if let Some(rates) = delegate(manifest, Pricer::Gemini, canonical) {
            return Some(rates);
        }
        let base = strip_gemini_suffix(canonical);
        if base != canonical
            && let Some(rates) = delegate(manifest, Pricer::Gemini, base)
        {
            return Some(rates);
        }
        return Some(SONNET_TIER);
    }
    Some(SONNET_TIER)
}

/// `cursor._strip_gemini_suffix` — trim a trailing `-preview-…` / `-experimental`.
fn strip_gemini_suffix(model_id: &str) -> &str {
    for marker in ["-preview-", "-experimental"] {
        if let Some(idx) = model_id.find(marker) {
            return &model_id[..idx];
        }
    }
    model_id
}

/// `self._x.rates_for(self._x.canonicalize(target))` — the delegation idiom every
/// shell pricer uses.
fn delegate(manifest: &Manifest, to: Pricer, target: &str) -> Option<Rates> {
    let canonical = to.canonicalize(manifest, target);
    to.rates_for(manifest, canonical.as_deref())
}

/// `OpenAIPricer.normalize_tokens` — the one non-trivial reshape.
fn normalize_openai(raw: &RawTokens) -> Tokens {
    if raw.contains("input_tokens") || raw.contains("cached_input_tokens") {
        let raw_input = safe_int(raw.get("input_tokens"));
        let cached = safe_int(raw.get("cached_input_tokens"));
        let raw_output = safe_int(raw.get("output_tokens"));
        let reasoning = safe_int(raw.get("reasoning_output_tokens"));
        return Tokens {
            input: (raw_input - cached).max(0),
            output: raw_output + reasoning,
            cache_creation: 0,
            cache_read: cached,
        };
    }
    Tokens {
        input: safe_int(raw.get("input")),
        output: safe_int(raw.get("output")),
        cache_creation: safe_int(raw.get("cache_creation")),
        cache_read: safe_int(raw.get("cache_read")),
    }
}

/// `openai._safe_int` — coerce to a non-negative int; garbage degrades to 0.
/// Only the clamp survives the port: a Rust `i64` cannot be a string or `inf`.
fn safe_int(value: Option<i64>) -> i64 {
    value.unwrap_or(0).max(0)
}

/// `OpenAIPricer._identify` — the in-code family ladder, returning the `_Family`
/// member NAME because that is what `canonicalize` returns and `rates_for` keys on.
fn identify_openai(model_id: &str) -> &'static str {
    if model_id.is_empty() {
        return OPENAI_FALLBACK;
    }
    let normalized = model_id.to_lowercase().replace('.', "-");
    let parts: std::collections::HashSet<&str> = normalized.split('-').collect();

    if parts.contains("codex") {
        if parts.contains("5") && parts.contains("3") {
            return "GPT_53_CODEX";
        }
        if parts.contains("5") && parts.contains("2") {
            return "GPT_52_CODEX";
        }
        return "GPT_5_CODEX";
    }

    if parts.contains("gpt") {
        let has_mini = parts.contains("mini");
        if parts.contains("5") && parts.contains("4") {
            return "GPT_54";
        }
        if parts.contains("5") {
            return if has_mini { "GPT_5_MINI" } else { "GPT_5" };
        }
        if parts.contains("4o") || (parts.contains("4") && parts.contains("o")) {
            return if has_mini { "GPT_4O_MINI" } else { "GPT_4O" };
        }
        if (parts.contains("4") && parts.contains("1")) || parts.contains("4-1") {
            return "GPT_41";
        }
    }

    OPENAI_FALLBACK
}

/// `get_pricer(provider)` — registry lookup, case-insensitive, Anthropic on miss.
///
/// The fallback is deliberate: pricing an unknown provider against Anthropic's
/// card produces a conservative-ish number instead of raising mid-aggregation.
#[must_use]
pub fn get_pricer(provider: &str) -> Pricer {
    let key = provider.to_lowercase();
    for pricer in ALL {
        if pricer.provider_name() == key {
            return pricer;
        }
    }
    for pricer in ALL {
        if pricer.provider_aliases().contains(&key.as_str()) {
            return pricer;
        }
    }
    Pricer::Anthropic
}

/// Every registry key — `provider_name`s and aliases — in registration order.
#[must_use]
pub fn registry_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for pricer in ALL {
        keys.push(pricer.provider_name().to_string());
        for alias in pricer.provider_aliases() {
            keys.push((*alias).to_string());
        }
    }
    keys
}

/// `(hint, pricer key, is_prefix)` rules, sorted longest-hint-first with prefix
/// outranking substring at equal length. Port of `costs._hint_routing`.
///
/// The Python sort key is `(-len(hint), not is_prefix, hint, key)`, which is
/// total — so the registration order the rules are gathered in cannot affect the
/// result, and neither can the de-duplication by singleton identity.
///
/// Built once behind a `OnceLock`, which is what `@lru_cache(maxsize=1)` buys on
/// the Python side: `vendor_for_model` runs on every priced row when the price
/// book is wired, and re-sorting eight rules per row is exactly the kind of
/// avoidable constant the port exists to remove.
#[must_use]
pub fn hint_routing() -> &'static [(&'static str, &'static str, bool)] {
    static RULES: std::sync::OnceLock<Vec<(&'static str, &'static str, bool)>> =
        std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        let mut rules: Vec<(&'static str, &'static str, bool)> = Vec::new();
        for pricer in ALL {
            let key = pricer.provider_name();
            for hint in pricer.model_id_prefixes() {
                rules.push((hint, key, true));
            }
            for hint in pricer.model_id_substrings() {
                rules.push((hint, key, false));
            }
        }
        rules.sort_by_key(|r| (std::cmp::Reverse(r.0.len()), !r.2, r.0, r.1));
        rules
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::test_support::sample_manifest;

    #[test]
    fn registry_is_case_insensitive_and_falls_back_to_anthropic() {
        assert_eq!(get_pricer("anthropic"), Pricer::Anthropic);
        assert_eq!(get_pricer("CLAUDE"), Pricer::Anthropic);
        assert_eq!(get_pricer("codex"), Pricer::OpenAi);
        assert_eq!(get_pricer("omp"), Pricer::Pi);
        assert_eq!(get_pricer("cursor-agent"), Pricer::CursorAgent);
        // Unknown names, including the ones the store actually carries.
        assert_eq!(get_pricer("grok"), Pricer::Anthropic);
        assert_eq!(get_pricer(""), Pricer::Anthropic);
    }

    #[test]
    fn aliases_share_the_singleton_but_subclasses_do_not() {
        assert_eq!(get_pricer("claude"), get_pricer("anthropic"));
        assert_ne!(get_pricer("kilocode"), get_pricer("cline"));
        assert_ne!(get_pricer("roocode"), get_pricer("cline"));
    }

    #[test]
    fn hint_rules_sort_longest_first_prefix_before_substring() {
        assert_eq!(
            hint_routing().to_vec(),
            vec![
                ("composer-", "cursor", true),
                ("cursor-", "cursor", true),
                ("gemini", "gemini", true),
                ("claude", "anthropic", false),
                ("codex", "openai", false),
                ("glm-", "anthropic", true),
                ("qwen", "qwen", true),
                ("gpt", "openai", false),
            ]
        );
    }

    #[test]
    fn openai_identify_ladder() {
        assert_eq!(identify_openai("gpt-5.3-codex"), "GPT_53_CODEX");
        assert_eq!(identify_openai("gpt-5.2-codex"), "GPT_52_CODEX");
        assert_eq!(identify_openai("gpt-5-codex"), "GPT_5_CODEX");
        assert_eq!(identify_openai("codex-mini"), "GPT_5_CODEX");
        assert_eq!(identify_openai("gpt-5.4"), "GPT_54");
        assert_eq!(identify_openai("gpt-5"), "GPT_5");
        assert_eq!(identify_openai("gpt-5-mini"), "GPT_5_MINI");
        assert_eq!(identify_openai("gpt-4o"), "GPT_4O");
        assert_eq!(identify_openai("gpt-4o-mini"), "GPT_4O_MINI");
        assert_eq!(identify_openai("gpt-4.1"), "GPT_41");
        assert_eq!(identify_openai(""), "GPT_5_CODEX");
        assert_eq!(identify_openai("llama-3"), "GPT_5_CODEX");
    }

    #[test]
    fn cline_family_pegs_only_the_exact_lowercase_auto_label() {
        let m = sample_manifest();
        assert_eq!(
            Pricer::Cline.rates_for(&m, Some("cline-auto")),
            Some((3.0, 15.0, 3.75, 0.30))
        );
        // The peg compares the ORIGINAL canonical, so the upper-cased form misses.
        assert_eq!(Pricer::Cline.rates_for(&m, Some("CLINE-AUTO")), None);
        // Continue/Copilot share the body without the peg.
        assert_eq!(Pricer::Continue.rates_for(&m, Some("cline-auto")), None);
        assert_eq!(Pricer::Copilot.rates_for(&m, Some("copilot-auto")), None);
        // Subclasses behave as Cline.
        assert_eq!(
            Pricer::KiloCode.rates_for(&m, Some("anthropic/claude-opus-4-8")),
            Some((5.0, 25.0, 6.25, 0.50))
        );
        assert_eq!(Pricer::RooCode.rates_for(&m, Some("ollama/llama-3")), None);
    }

    #[test]
    fn cursor_never_returns_zero_for_a_nonempty_id() {
        let m = sample_manifest();
        assert_eq!(
            Pricer::Cursor.rates_for(&m, Some("composer-1")),
            Some(COMPOSER_1_TIER)
        );
        assert_eq!(
            Pricer::Cursor.rates_for(&m, Some("gemini-2.5-pro-preview-05-06")),
            Some((1.25, 10.0, 0.0, 0.31))
        );
        assert_eq!(
            Pricer::Cursor.rates_for(&m, Some("totally-unknown")),
            Some(SONNET_TIER)
        );
        assert_eq!(Pricer::Cursor.rates_for(&m, Some("")), None);
    }

    #[test]
    fn pi_defaults_to_gpt_5_and_prices_foreign_ids_as_codex() {
        let m = sample_manifest();
        assert_eq!(Pricer::Pi.canonicalize(&m, "").as_deref(), Some("GPT_5"));
        assert_eq!(
            Pricer::Pi.canonicalize(&m, "claude-opus-4-7").as_deref(),
            Some("GPT_5_CODEX")
        );
        assert_eq!(
            Pricer::Pi.rates_for(&m, Some("GPT_5_CODEX")),
            Some((1.25, 10.0, 0.0, 0.125))
        );
    }

    #[test]
    fn openai_normalize_subtracts_cached_input_and_folds_reasoning() {
        let raw = RawTokens::openai_shape(1000, 400, 250, 90);
        let tokens = Pricer::OpenAi.normalize_tokens(&raw);
        assert_eq!(tokens.input, 750);
        assert_eq!(tokens.output, 490);
        assert_eq!(tokens.cache_creation, 0);
        assert_eq!(tokens.cache_read, 250);
        // Canonical-shape input is a pass-through with the non-negative clamp.
        let canonical = RawTokens::canonical(5, 6, 7, 8);
        assert_eq!(
            Pricer::OpenAi.normalize_tokens(&canonical),
            Tokens {
                input: 5,
                output: 6,
                cache_creation: 7,
                cache_read: 8
            }
        );
    }

    #[test]
    fn anthropic_normalize_does_not_clamp_negatives() {
        let raw = RawTokens::canonical(-5, 0, 0, 0);
        assert_eq!(Pricer::Anthropic.normalize_tokens(&raw).input, -5);
        // …while OpenAI's _safe_int does.
        assert_eq!(Pricer::OpenAi.normalize_tokens(&raw).input, 0);
    }

    #[test]
    fn kiro_normalises_dots_and_refuses_its_autoselector() {
        let m = sample_manifest();
        assert_eq!(
            Pricer::Kiro
                .canonicalize(&m, "claude.3.5.sonnet")
                .as_deref(),
            Some("claude-3-5-sonnet")
        );
        assert_eq!(Pricer::Kiro.rates_for(&m, Some("kiro-auto")), None);
        assert_eq!(
            Pricer::Kiro.rates_for(&m, Some("anthropic/claude-opus-4-8")),
            Some((5.0, 25.0, 6.25, 0.50))
        );
    }

    #[test]
    fn hermes_and_openclaw_default_to_anthropic_not_none() {
        let m = sample_manifest();
        // An id no vendor claims still prices, at the Anthropic fallback family.
        assert_eq!(
            Pricer::Hermes.rates_for(&m, Some("mistral-large")),
            Some((3.0, 15.0, 3.75, 0.30))
        );
        assert_eq!(
            Pricer::OpenClaw.rates_for(&m, Some("mistral-large")),
            Some((3.0, 15.0, 3.75, 0.30))
        );
    }
}
