"""Canonical task classifier — one source of truth for intent / size / language.

Move 0 of the comparative benchmark engine (issue #99, spec 26 §8). Before
this module three surfaces each classified a session's *task* their own way:

* ``services/tag_service.py`` — regex patterns, **6** intent labels
  (``build/fix/explore/refactor/test/ops``), multi-label (a session can carry
  several intents), emitted as ``intent:<label>`` auto-tags.
* ``services/mode_recommender.py`` — substring keywords, **5** labels (no
  ``ops``), single best label, feeding the cost-only recommender.
* the benchmark engine (new) — needs the *same* stratum key every other
  surface uses, or its verdicts silently disagree with the tags/recommender.

Two classifiers that disagree on what a "fix" is are a latent bug: the
recommender can route on one taxonomy while the tags show another. This module
collapses them into one deterministic, dependency-free classifier and resolves
the 5-vs-6 divergence by **adopting the 6-label set** (``+ops``) as canonical.

Design contract
---------------
* **Pure + deterministic.** No I/O, no clock, no store. ``classify_task`` is a
  function of its input string only, so it is trivially cache-safe and
  reproducible across runs (the benchmark's read-through cache relies on this).
* **One matcher.** Intent detection is the regex table :data:`INTENT_PATTERNS`
  — the richer, word-boundary-correct, 6-label set lifted verbatim from the
  tag service. ``tag_service`` now delegates here (byte-identical behaviour);
  ``mode_recommender`` delegates its single-label pick here too.
* **Multi vs single.** :func:`classify_intents` returns the *set* of matching
  labels (what the tag service wants); :func:`classify_intent` collapses that
  set to one label via a fixed :data:`_INTENT_PRIORITY` (what the recommender
  and the benchmark stratum key want). ``explore`` is the no-match default —
  a bare read-only question is the most common keyword-free case.
* **Size band from token volume.** :data:`TOKEN_BANDS` are the same thresholds
  the recommender always used; :func:`band_for_token_count` applies them to an
  actual session token count (the benchmark's use), while :func:`token_band`
  applies them to a ``len(text)//4`` estimate (the recommender's use).
"""

from __future__ import annotations

import re
from typing import Any

__all__ = [
    "INTENT_LABELS",
    "INTENT_PATTERNS",
    "TOKEN_BANDS",
    "classify_intent",
    "classify_intents",
    "classify_task",
    "token_band",
    "band_for_token_count",
    "detect_languages",
    "dominant_language",
]


# ── intent taxonomy (canonical: 6 labels incl. ``ops``) ──────────────────────

# The canonical intent labels. Ordering here is declaration order only; the
# *single-pick* precedence lives in ``_INTENT_PRIORITY`` below.
INTENT_LABELS: tuple[str, ...] = (
    "build",
    "fix",
    "explore",
    "refactor",
    "test",
    "ops",
)

# Intent detection patterns: ``(regex, bare_label)``. Lifted verbatim from the
# tag service's ``INTENT_PATTERNS`` (the 6-label source of truth) with the
# ``intent:`` prefix stripped — the tag service re-applies it. A session can
# match several patterns; :func:`classify_intents` keeps all matches.
#
# NOTE on ``ops``: ``.env`` is matched as a separate alternative with
# lookarounds because a word-boundary (\b) can't anchor a pattern starting with
# a non-word char (the leading dot).
INTENT_PATTERNS: tuple[tuple[str, str], ...] = (
    # build — adding something new
    (r"\b(add|adding|added|implement|implementing|implemented|create|creating|created|build|building|built|new feature|scaffold|scaffolding|set up|setup)\b", "build"),  # noqa: E501
    # fix — bug or error
    (r"\b(fix|fixing|fixed|bug|bugs|broken|breaks|breaking|crash|crashes|crashing|error|errors|traceback|stack trace|exception|regression|doesn't work|not working|failing|failed)\b", "fix"),  # noqa: E501
    # explore — reading / understanding
    (r"\b(explain|explaining|explained|understand|understanding|walk me through|how does|how do|what does|what is|where is|show me|why is|why does|read|reading|review|reviewing|reviewed|look at|trace)\b", "explore"),  # noqa: E501
    # refactor — restructuring without behavior change
    (r"\b(refactor|refactoring|refactored|clean up|cleanup|cleaning up|simplify|simplifying|simplified|restructure|restructuring|reorganize|reorganizing|rename|renaming|extract|extracting|inline|consolidate|dedup|deduplicate)\b", "refactor"),  # noqa: E501
    # test — writing or running tests
    (r"\b(test|tests|testing|tested|unit test|integration test|pytest|jest|vitest|mocha|jasmine|rspec|assert|asserts|asserting|mock|mocking|mocked|spec|specs|coverage|tdd)\b", "test"),  # noqa: E501
    # ops — deployment, config, infra
    (r"(?:\b(?:deploy|deploying|deployed|deployment|ci/cd|ci\b|cd\b|github actions|gitlab ci|jenkins|docker|dockerfile|kubernetes|k8s|terraform|ansible|helm|env var|environment variable|nginx|caddy|systemd|pm2)\b|(?<!\w)\.env(?!\w))", "ops"),  # noqa: E501
)

# Single-pick precedence when several intents match. Specific, high-signal
# intents beat the generic ``build``; ``explore`` is the fallback. This order
# preserves ``mode_recommender``'s historical single-label decisions on its
# fixtures (e.g. "fix the failing test" resolves to ``fix``, not ``test``)
# while slotting the new ``ops`` label ahead of the catch-all ``build``.
_INTENT_PRIORITY: tuple[str, ...] = (
    "fix",
    "refactor",
    "test",
    "ops",
    "build",
    "explore",
)

_COMPILED_INTENT: tuple[tuple[re.Pattern[str], str], ...] = tuple(
    (re.compile(pattern, re.IGNORECASE), label) for pattern, label in INTENT_PATTERNS
)


# ── size bands ───────────────────────────────────────────────────────────────

# Token bands for the "same shape" filter. Edges are token-count thresholds;
# a count below the upper bound of a band falls in it. Identical to the
# thresholds ``mode_recommender`` has always used — kept here as the single
# definition both it and the benchmark import.
TOKEN_BANDS: tuple[tuple[str, int], ...] = (
    ("tiny", 200),
    ("small", 800),
    ("med", 3000),
    ("large", 10**9),  # catch-all
)


# ── language hints ───────────────────────────────────────────────────────────

# Lowercased substring match. Intentionally short + high-signal. Ported from
# ``mode_recommender._LANGUAGE_HINTS`` so text-derived language detection is
# consistent across the recommender and the benchmark.
_LANGUAGE_HINTS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("python",     ("python", ".py", "pytest", "django", "flask", "fastapi")),
    ("typescript", ("typescript", ".ts", ".tsx", "react ", "vite", "next.js")),
    ("javascript", ("javascript", ".js ", " js ", "node.js", "nodejs", "npm ")),
    ("rust",       ("rust", ".rs", "cargo ")),
    ("go",         (" go ", ".go", "golang", "go mod ")),
    ("sql",        ("sql", "sqlite", "postgres", "select ", "create table")),
    ("shell",      ("bash", "zsh", "shell", ".sh ", "#!/bin/")),
    ("html",       ("html", ".html", "<div", "<span")),
    ("css",        ("css", ".css", "tailwind")),
)


# ── intent classification ────────────────────────────────────────────────────


def classify_intents(text: str) -> set[str]:
    """Return the set of intent labels whose pattern matches ``text``.

    Multi-label: a session can legitimately be several things (a build that
    ends in a fix). This is what the tag service consumes — it re-applies the
    ``intent:`` prefix to each label. Empty string / no match → empty set.
    """
    if not text:
        return set()
    return {label for rx, label in _COMPILED_INTENT if rx.search(text)}


def classify_intent(text: str) -> str:
    """Collapse the matching-intent set to a single canonical label.

    Precedence follows :data:`_INTENT_PRIORITY`; ``explore`` is returned when
    nothing matches (a keyword-free prompt is almost always a read-only
    question). This is the recommender's and the benchmark's single-label view.
    """
    matches = classify_intents(text)
    if not matches:
        return "explore"
    for label in _INTENT_PRIORITY:
        if label in matches:
            return label
    # Every matched label is, by construction, in _INTENT_PRIORITY; this is a
    # defensive fallback that keeps the function total.
    return "explore"


# ── size band ────────────────────────────────────────────────────────────────


def band_for_token_count(n_tokens: int) -> str:
    """Return the band label for an *actual* token count.

    The benchmark uses this against a session's real token volume
    (``input + output``); the recommender uses :func:`token_band` against a
    char/4 estimate of the prompt. Both share :data:`TOKEN_BANDS`.
    """
    n = max(0, int(n_tokens))
    for label, upper in TOKEN_BANDS:
        if n < upper:
            return label
    return TOKEN_BANDS[-1][0]


def token_band(text: str) -> str:
    """Return the band for the rough token count of ``text`` (``len//4``)."""
    return band_for_token_count(max(0, len(text or "")) // 4)


# ── language ─────────────────────────────────────────────────────────────────


def detect_languages(text: str) -> list[str]:
    """Return the sorted list of language labels mentioned in ``text``."""
    if not text:
        return []
    lowered = text.lower()
    out: set[str] = set()
    for label, hints in _LANGUAGE_HINTS:
        for hint in hints:
            if hint in lowered:
                out.add(label)
                break
    return sorted(out)


def dominant_language(text: str) -> str | None:
    """Return the single most-mentioned language in ``text``, or ``None``.

    "Dominant" = the language whose hint substrings occur most often; ties
    break alphabetically for determinism. ``None`` when no language is hinted.
    """
    if not text:
        return None
    lowered = text.lower()
    counts: dict[str, int] = {}
    for label, hints in _LANGUAGE_HINTS:
        total = sum(lowered.count(hint) for hint in hints)
        if total > 0:
            counts[label] = total
    if not counts:
        return None
    # Highest count wins; ties break alphabetically (earlier label first).
    best = min(counts.items(), key=lambda kv: (-kv[1], kv[0]))
    return best[0]


# ── canonical entry point ────────────────────────────────────────────────────


def classify_task(session_text: str) -> dict[str, Any]:
    """Classify a task from its text into ``{intent, size_band, language}``.

    The one canonical classifier the tag service, the mode recommender, and the
    benchmark engine all share. ``intent`` is the single canonical label
    (6-label set incl. ``ops``); ``size_band`` is the char/4 band of the text;
    ``language`` is the dominant hinted language or ``None``.

    Callers with a real session token count (the benchmark) should prefer
    :func:`band_for_token_count` over the text-derived ``size_band`` here, per
    spec 26 §3.2 ("applied to session token volume, not just the prompt").
    """
    text = session_text or ""
    return {
        "intent": classify_intent(text),
        "size_band": token_band(text),
        "language": dominant_language(text),
    }
