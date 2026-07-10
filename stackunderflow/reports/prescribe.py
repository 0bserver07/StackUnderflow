"""Prescriptive cost — turn descriptive waste findings into concrete actions.

Campaign #7. Two generators live here, both **advisory and read-only**:

1. :func:`generate_claudemd_preview` — takes the current CLAUDE.md *text*
   (plus the optimize waste findings that flagged it) and produces a slimmer
   version as a **preview**: a unified diff, a per-rule rationale list, and a
   dollar-savings estimate. It is a pure function — text in, dict out. It
   **never writes any file**: there is no path parameter, no ``open()``, no
   filesystem import anywhere in this module (locked by a source-scan test in
   ``tests/stackunderflow/reports/test_prescribe.py``). Applying the preview
   is the *user's* action, via copy/download in the UI.

2. :func:`build_routing_recommendations` — derives "route work-type X to
   model Y" recommendations from the per-model spend history in
   ``usage_events`` (the same facts ``model_day_mart`` is built from, plus
   the v026 ``reasoning_tokens`` attribution column and — when populated —
   ``session_quality_metrics``). Each recommendation reprices the model's
   *actual* token shape on a candidate model and reports the window and
   estimated-monthly dollar delta.

Design contract (mirrors :mod:`stackunderflow.reports.forks`):

* **Advisory, never raises.** Missing tables, a pre-v026 store, an empty
  window — all return an empty-but-well-formed result.
* **No fabricated numbers.** Every dollar figure traces to (a) token counts
  present in the input text / store rows and (b) ``compute_cost`` — used
  strictly as a black box (this module never encodes a rate, never touches
  ``models.toml`` or ``infra/costs.py`` internals). Where a signal is absent
  (e.g. ``session_quality_metrics`` empty, reasoning not attributed for a
  provider) the corresponding field is ``None`` and no rule that needs it
  fires.
* **Own query helpers.** All SQL lives here behind ``sqlite_master`` guards;
  nothing is added to ``store/queries.py`` or the marts.
* **Explicit extrapolation.** Monthly figures are the window figure scaled
  by ``30 / observed_days``; the factor is included in the payload so the
  arithmetic is auditable.
"""

from __future__ import annotations

import difflib
import re
import sqlite3
from dataclasses import dataclass, field
from typing import Any

from stackunderflow.reports.optimize import (
    CLAUDE_MD_TOKEN_THRESHOLD,
    WASTE_PRICING_MODEL,
    approx_tokens,
    tokens_to_usd,
)
from stackunderflow.infra.model_catalog import routing_candidates
from stackunderflow.reports.scope import Scope
from stackunderflow.services.context_budget import DEFAULT_SESSIONS_PER_MONTH

__all__ = [
    "generate_claudemd_preview",
    "build_routing_recommendations",
    "ROUTING_CANDIDATES",
    "SECTION_EXTRACT_TOKEN_THRESHOLD",
    "LOW_REASONING_SHARE",
    "HIGH_REASONING_SHARE",
    "SHORT_OUTPUT_TOKENS_PER_EVENT",
    "MIN_SAVINGS_SHARE",
    "MIN_WINDOW_SAVINGS_USD",
    "QUALITY_SCORE_FLOOR",
    "BASELINE_MONTH_DAYS",
]


# ── tunables — CLAUDE.md slimmer ─────────────────────────────────────────────

# A section whose body estimates above this is proposed for extraction into a
# side doc (the classic CLAUDE.md slim: keep the heading + a pointer inline).
SECTION_EXTRACT_TOKEN_THRESHOLD = 600

# Blank-line runs of this length or more collapse to a single blank line.
BLANK_RUN_COLLAPSE_AT = 3

# Duplicate paragraphs shorter than this (normalised chars) are left alone —
# short repeated fragments ("---", one-word list items) are usually deliberate.
DEDUPE_MIN_CHARS = 60

# Sections whose heading matches are never proposed for extraction — hard
# rules / safety text belongs inline no matter how long it is.
_KEEP_HEADING_RE = re.compile(r"\b(rules?|never|must|safety|critical|important)\b", re.I)

# ── tunables — model routing ─────────────────────────────────────────────────

# Reasoning share (reasoning_tokens / output_tokens) below which an
# *attributed* workload counts as "low-reasoning" — a downshift candidate.
LOW_REASONING_SHARE = 0.05
# ...and above which it counts as "reasoning-heavy".
HIGH_REASONING_SHARE = 0.25

# For providers with no reasoning attribution (column stays 0 — e.g. Claude,
# Grok) the only honest lightness signal is output size per event.
SHORT_OUTPUT_TOKENS_PER_EVENT = 300

# A downshift is only worth surfacing when the candidate saves at least this
# share of the window spend AND at least this many dollars.
MIN_SAVINGS_SHARE = 0.25
MIN_WINDOW_SAVINGS_USD = 0.01

# Upshift recommendations need evidence the current model is struggling:
# an average graded session quality below this floor (0–5 scale, v020).
QUALITY_SCORE_FLOOR = 3.5

# Monthly extrapolation baseline: window_delta * (30 / observed_days).
BASELINE_MONTH_DAYS = 30

# Candidate models a workload can be routed to. Loaded from
# ``infra/model_candidates.json`` (routing_candidate entries only) —
# the same catalog ``services/whatif.py`` reads, so the sets can no
# longer drift. Names models only — never rates; ``compute_cost`` is
# the single source of every dollar figure.
ROUTING_CANDIDATES: tuple[tuple[str, str, str], ...] = routing_candidates()

_ROUTING_CAVEATS: tuple[str, ...] = (
    "Candidate costs are a rate-card swap of the observed token shape, not a "
    "re-run — a different model may tokenize differently or need more/fewer "
    "output tokens for the same task.",
    "Monthly figures extrapolate the window spend by 30 / observed active "
    "days; they assume the window is representative.",
)


# ══════════════════════════════════════════════════════════════════════════
# (a) CLAUDE.md slimmer — pure text → preview
# ══════════════════════════════════════════════════════════════════════════


@dataclass
class _Block:
    """One structural block of the markdown document.

    ``kind`` ∈ {heading, fence, comment, blank, para}. ``lines`` holds the
    original lines verbatim so an untouched document round-trips exactly
    (``render(parse(text)) == text``).
    """

    kind: str
    lines: list[str] = field(default_factory=list)

    @property
    def text(self) -> str:
        return "\n".join(self.lines)


_HEADING_RE = re.compile(r"^#{1,6}\s")
_FENCE_RE = re.compile(r"^(```|~~~)")


def _parse_blocks(text: str) -> list[_Block]:
    """Split markdown into heading / fence / comment / blank / para blocks.

    Line-exact: every input line lands in exactly one block, in order.
    Fenced code (``` or ~~~) is opaque — nothing inside a fence is ever
    classified as a comment/heading/blank run, so no transform can touch
    code examples. An unterminated fence or comment runs to EOF.
    """
    lines = text.split("\n")
    blocks: list[_Block] = []
    i = 0
    n = len(lines)
    while i < n:
        line = lines[i]
        stripped = line.strip()
        fence = _FENCE_RE.match(stripped)
        if fence:
            marker = fence.group(1)
            block = _Block("fence", [line])
            i += 1
            while i < n:
                block.lines.append(lines[i])
                closed = lines[i].strip().startswith(marker)
                i += 1
                if closed:
                    break
            blocks.append(block)
            continue
        if stripped.startswith("<!--"):
            block = _Block("comment", [line])
            closed = "-->" in line
            i += 1
            while not closed and i < n:
                block.lines.append(lines[i])
                closed = "-->" in lines[i]
                i += 1
            blocks.append(block)
            continue
        if stripped == "":
            block = _Block("blank", [line])
            i += 1
            while i < n and lines[i].strip() == "":
                block.lines.append(lines[i])
                i += 1
            blocks.append(block)
            continue
        if _HEADING_RE.match(line):
            blocks.append(_Block("heading", [line]))
            i += 1
            continue
        block = _Block("para", [line])
        i += 1
        while i < n:
            nxt = lines[i]
            s = nxt.strip()
            if s == "" or _HEADING_RE.match(nxt) or _FENCE_RE.match(s) or s.startswith("<!--"):
                break
            block.lines.append(nxt)
            i += 1
        blocks.append(block)
    return blocks


def _render(blocks: list[_Block]) -> str:
    return "\n".join(line for b in blocks for line in b.lines)


def _strip_comments(blocks: list[_Block]) -> tuple[list[_Block], dict[str, Any] | None]:
    """Drop HTML comment blocks (author notes still cost tokens every session)."""
    kept = [b for b in blocks if b.kind != "comment"]
    removed = [b for b in blocks if b.kind == "comment"]
    if not removed:
        return blocks, None
    detail = {
        "removed_comments": len(removed),
        "samples": [b.lines[0].strip()[:80] for b in removed[:3]],
    }
    return kept, detail


def _normalise_para(b: _Block) -> str:
    return " ".join(" ".join(b.lines).split())


def _dedupe_paragraphs(blocks: list[_Block]) -> tuple[list[_Block], dict[str, Any] | None]:
    """Drop exact duplicate paragraphs after the first occurrence."""
    seen: set[str] = set()
    out: list[_Block] = []
    dropped: list[str] = []
    for b in blocks:
        if b.kind == "para":
            norm = _normalise_para(b)
            if len(norm) >= DEDUPE_MIN_CHARS:
                if norm in seen:
                    dropped.append(norm[:80])
                    continue
                seen.add(norm)
        out.append(b)
    if not dropped:
        return blocks, None
    return out, {"removed_duplicates": len(dropped), "samples": dropped[:3]}


def _heading_slug(heading_line: str) -> str:
    title = heading_line.lstrip("#").strip()
    slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")
    return slug or "section"


def _extract_sections(
    blocks: list[_Block],
    *,
    section_token_threshold: int,
) -> tuple[list[_Block], dict[str, Any] | None]:
    """Replace oversized section bodies with a one-line pointer.

    A section is a heading plus everything up to the next heading (any
    level). Bodies estimating over ``section_token_threshold`` are swapped
    for a pointer paragraph naming a suggested side-doc path — moving the
    text there is the user's action; the preview only shows the shape.
    Headings matching :data:`_KEEP_HEADING_RE` (rules/safety text) are
    never extracted.
    """
    # Section boundaries: indexes of heading blocks.
    out: list[_Block] = []
    extracted: list[dict[str, Any]] = []
    i = 0
    n = len(blocks)
    while i < n:
        b = blocks[i]
        if b.kind != "heading":
            out.append(b)
            i += 1
            continue
        # Collect the body (blocks until the next heading).
        j = i + 1
        body = []
        while j < n and blocks[j].kind != "heading":
            body.append(blocks[j])
            j += 1
        body_text = "\n".join(x.text for x in body)
        body_tokens = approx_tokens(body_text)
        heading_line = b.lines[0]
        if body_tokens >= section_token_threshold and not _KEEP_HEADING_RE.search(heading_line):
            slug = _heading_slug(heading_line)
            pointer = (
                f"> Body moved to docs/claude-md/{slug}.md (~{body_tokens} tokens) — "
                "slimmed by StackUnderflow; move the original text there before "
                "adopting this file."
            )
            out.append(b)
            out.append(_Block("blank", [""]))
            out.append(_Block("para", [pointer]))
            out.append(_Block("blank", [""]))
            extracted.append(
                {
                    "heading": heading_line.lstrip("#").strip(),
                    "suggested_path": f"docs/claude-md/{slug}.md",
                    "body_tokens": body_tokens,
                }
            )
            i = j
            continue
        out.append(b)
        i += 1
    if not extracted:
        return blocks, None
    return out, {"extracted_sections": extracted}


def _collapse_blank_runs(blocks: list[_Block]) -> tuple[list[_Block], dict[str, Any] | None]:
    """Collapse runs of ``BLANK_RUN_COLLAPSE_AT``+ blank lines to one."""
    removed = 0
    out: list[_Block] = []
    for b in blocks:
        if b.kind == "blank" and len(b.lines) >= BLANK_RUN_COLLAPSE_AT:
            removed += len(b.lines) - 1
            out.append(_Block("blank", [""]))
        else:
            out.append(b)
    if removed == 0:
        return blocks, None
    return out, {"blank_lines_removed": removed}


# rule id → human summary template (formatted with the rule's detail dict)
_RULE_SUMMARIES = {
    "strip_html_comments": (
        "Removed {removed_comments} HTML comment block(s) — author notes "
        "the model pays to read every session."
    ),
    "dedupe_paragraphs": (
        "Removed {removed_duplicates} duplicate paragraph(s) — repeated "
        "text costs tokens twice."
    ),
    "extract_oversized_sections": (
        "Proposed moving {count} oversized section(s) to side docs, "
        "keeping a one-line pointer inline."
    ),
    "collapse_blank_runs": (
        "Collapsed runs of blank lines ({blank_lines_removed} line(s) removed)."
    ),
}


def _bloat_flagged(findings: list[Any] | None) -> bool:
    """True when the caller's findings include a context-bloat pattern."""
    for f in findings or []:
        pid = None
        if isinstance(f, dict):
            pid = f.get("pattern_id") or f.get("kind")
        else:
            pid = getattr(f, "pattern_id", None)
        if pid in ("bloated_claude_md", "context_budget_bloat"):
            return True
    return False


def generate_claudemd_preview(
    claude_md_text: str,
    *,
    findings: list[Any] | None = None,
    file_label: str = "CLAUDE.md",
    sessions_per_month: int = DEFAULT_SESSIONS_PER_MONTH,
    section_token_threshold: int = SECTION_EXTRACT_TOKEN_THRESHOLD,
    bloat_token_threshold: int = CLAUDE_MD_TOKEN_THRESHOLD,
) -> dict[str, Any]:
    """Generate a slimmer-CLAUDE.md **preview** from the current text.

    Pure function: ``findings`` (the optimize waste findings that flagged the
    file — dicts or :class:`~stackunderflow.reports.optimize.Finding`) plus
    the current text in; ``{preview_diff, rationale, estimated_savings...}``
    out. **Never touches the filesystem** — no path is accepted and no file
    is written; "apply" is the caller copying/downloading ``slimmed_text``.

    Rules applied, in order (each contributes one rationale entry when it
    changed something):

    1. ``strip_html_comments`` — drop ``<!-- … -->`` blocks (outside code
       fences).
    2. ``dedupe_paragraphs`` — drop exact duplicate paragraphs (≥
       ``DEDUPE_MIN_CHARS`` normalised chars) after the first occurrence.
    3. ``extract_oversized_sections`` — only when the document is
       bloat-flagged (a ``bloated_claude_md`` / ``context_budget_bloat``
       finding was passed, or the text estimates over
       ``bloat_token_threshold``): section bodies over
       ``section_token_threshold`` are replaced by a pointer line naming a
       suggested side-doc path. Rule/safety-titled sections are never
       extracted.
    4. ``collapse_blank_runs`` — 3+ consecutive blank lines become one.

    Savings math (all auditable): ``tokens_saved`` uses the same ~4-chars/
    token heuristic the optimize detectors use; the per-session dollar figure
    prices those tokens as *input* via ``compute_cost`` at
    :data:`~stackunderflow.reports.optimize.WASTE_PRICING_MODEL`; monthly =
    per-session × ``sessions_per_month``. Empty/clean input returns
    ``changed=False`` with zero savings — no invented numbers.
    """
    original = claude_md_text or ""
    rationale: list[dict[str, Any]] = []

    blocks = _parse_blocks(original)
    current_text = original

    extraction_enabled = (
        approx_tokens(original) > bloat_token_threshold or _bloat_flagged(findings)
    )

    stages: list[tuple[str, Any]] = [
        ("strip_html_comments", _strip_comments),
        ("dedupe_paragraphs", _dedupe_paragraphs),
    ]
    if extraction_enabled:
        stages.append(
            (
                "extract_oversized_sections",
                lambda bl: _extract_sections(bl, section_token_threshold=section_token_threshold),
            )
        )
    stages.append(("collapse_blank_runs", _collapse_blank_runs))

    for rule_id, transform in stages:
        new_blocks, detail = transform(blocks)
        if detail is None:
            continue
        new_text = _render(new_blocks)
        saved = approx_tokens(current_text) - approx_tokens(new_text)
        if rule_id == "extract_oversized_sections":
            detail = dict(detail)
            detail["count"] = len(detail.get("extracted_sections", []))
        summary = _RULE_SUMMARIES[rule_id].format(**{k: v for k, v in detail.items() if not isinstance(v, list)})
        per_session = tokens_to_usd(saved) or 0.0
        rationale.append(
            {
                "rule": rule_id,
                "summary": summary,
                "tokens_saved": saved,
                "estimated_savings_usd_per_session": per_session,
                "estimated_savings_usd_monthly": round(per_session * sessions_per_month, 4),
                "detail": detail,
            }
        )
        blocks = new_blocks
        current_text = new_text

    slimmed = current_text
    changed = slimmed != original

    original_tokens = approx_tokens(original)
    slimmed_tokens = approx_tokens(slimmed)
    tokens_saved = original_tokens - slimmed_tokens
    per_session = tokens_to_usd(tokens_saved) or 0.0

    preview_diff = ""
    if changed:
        preview_diff = "".join(
            difflib.unified_diff(
                original.splitlines(keepends=True),
                slimmed.splitlines(keepends=True),
                fromfile=file_label,
                tofile=f"{file_label} (slim preview)",
            )
        )

    return {
        "file_label": file_label,
        "changed": changed,
        "preview_diff": preview_diff,
        "slimmed_text": slimmed if changed else "",
        "rationale": rationale,
        "original_tokens": original_tokens,
        "slimmed_tokens": slimmed_tokens,
        "tokens_saved": tokens_saved,
        "estimated_savings_usd_per_session": per_session,
        "estimated_savings_usd_monthly": round(per_session * sessions_per_month, 4),
        "sessions_per_month": sessions_per_month,
        "heuristic": (
            "tokens ≈ len(text)//4; savings priced as input tokens at "
            f"{WASTE_PRICING_MODEL} via compute_cost; monthly = per-session × "
            f"{sessions_per_month} sessions"
        ),
    }


# ══════════════════════════════════════════════════════════════════════════
# (b) model-routing recommendations
# ══════════════════════════════════════════════════════════════════════════


def _table_exists(conn: sqlite3.Connection, name: str) -> bool:
    """sqlite_master guard (accepts tables and views, like reports/forks.py)."""
    try:
        row = conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ? LIMIT 1",
            (name,),
        ).fetchone()
    except sqlite3.Error:
        return False
    return row is not None


def _scope_where(
    scope: Scope | None,
    project_ids: list[int] | None,
) -> tuple[str, list[Any]]:
    """WHERE fragment + params for the usage_events reads below."""
    sql = ""
    params: list[Any] = []
    if project_ids:
        placeholders = ",".join("?" for _ in project_ids)
        sql += f" AND project_id IN ({placeholders})"
        params.extend(project_ids)
    if scope is not None and scope.since is not None:
        sql += " AND ts >= ?"
        params.append(scope.since)
    if scope is not None and scope.until is not None:
        sql += " AND ts <= ?"
        params.append(scope.until)
    return sql, params


def _load_model_rollups(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
    project_ids: list[int] | None,
) -> tuple[list[dict[str, Any]], int]:
    """Per-(model, provider, speed) aggregates + distinct active days in window.

    Same facts ``model_day_mart`` materialises, read from ``usage_events``
    directly because the mart is global-grain (no ``project_id``) and has no
    reasoning column. Returns ``([], 0)`` on any schema/SQL problem (e.g. a
    pre-v026 store without ``reasoning_tokens``) — advisory, never raises.
    """
    if not _table_exists(conn, "usage_events"):
        return [], 0
    if project_ids is not None and len(project_ids) == 0:
        return [], 0
    where, params = _scope_where(scope, project_ids)
    # The f-strings below only splice ``where`` — a fixed skeleton of
    # ``AND col >= ?`` fragments plus ``?`` placeholders built by
    # ``_scope_where``; every value is parameter-bound. Same pattern (and
    # justification) as the qa/search services' S608 suppressions.
    sql = (
        "SELECT model, provider, COALESCE(speed, 'standard') AS speed, "  # noqa: S608
        "       COUNT(*) AS events, "
        "       COUNT(DISTINCT session_id) AS sessions, "
        "       COUNT(DISTINCT day) AS days_active, "
        "       COALESCE(SUM(input_tokens), 0) AS input_tokens, "
        "       COALESCE(SUM(output_tokens), 0) AS output_tokens, "
        "       COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, "
        "       COALESCE(SUM(cache_create_tokens), 0) AS cache_create_tokens, "
        "       COALESCE(SUM(reasoning_tokens), 0) AS reasoning_tokens, "
        "       COALESCE(SUM(cost_usd), 0.0) AS cost_usd "
        "FROM usage_events "
        f"WHERE model <> '' {where} "
        "GROUP BY model, provider, speed"
    )
    day_sql = (
        f"SELECT COUNT(DISTINCT day) FROM usage_events WHERE model <> '' {where}"  # noqa: S608
    )
    try:
        rows = conn.execute(sql, params).fetchall()
        day_row = conn.execute(day_sql, params).fetchone()
    except sqlite3.Error:
        return [], 0
    observed_days = int(day_row[0] or 0) if day_row else 0
    return [dict(r) for r in rows], observed_days


def _load_quality_by_model(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None,
    project_ids: list[int] | None,
) -> dict[str, tuple[float, int]]:
    """``{model: (avg_overall_score, graded_session_count)}`` — empty-safe.

    ``session_quality_metrics`` (v020) is frequently empty; an empty table
    (or a store without it) contributes nothing and no rule that needs a
    quality signal fires. Sessions are attributed to the model(s) they ran.
    """
    if not _table_exists(conn, "session_quality_metrics"):
        return {}
    where, params = _scope_where(scope, project_ids)
    # ``where`` is the same fixed, fully-parametrised fragment as above —
    # column names in ``_scope_where`` are unqualified, and every column it
    # names (``project_id``, ``ts``) resolves to ``usage_events`` (``ue``)
    # in this join, matching the rollup query's scoping.
    sql = (
        "SELECT ue.model AS model, "  # noqa: S608 — fixed skeleton, bound values
        "       AVG(sq.overall_score) AS avg_score, "
        "       COUNT(DISTINCT sq.session_id) AS graded "
        "FROM session_quality_metrics sq "
        "JOIN usage_events ue ON ue.session_id = sq.session_id "
        f"WHERE ue.model <> '' {where} "
        "GROUP BY ue.model"
    )
    try:
        rows = conn.execute(sql, params).fetchall()
    except sqlite3.Error:
        return {}
    out: dict[str, tuple[float, int]] = {}
    for r in rows:
        if r["avg_score"] is None:
            continue
        out[r["model"]] = (float(r["avg_score"]), int(r["graded"] or 0))
    return out


_DATE_SUFFIX_RE = re.compile(r"-\d{8}$")


def _same_model(model: str, candidate_id: str) -> bool:
    """True when the candidate is the model already in use (date-suffix aware)."""
    a = _DATE_SUFFIX_RE.sub("", model)
    b = _DATE_SUFFIX_RE.sub("", candidate_id)
    return a == b or a.startswith(b + "-") or b.startswith(a + "-")


def _classify_work_type(
    *,
    reasoning_tokens: int,
    output_tokens: int,
    events: int,
) -> tuple[str, float]:
    """(work_type, reasoning_share) for one model's window aggregate.

    ``reasoning_tokens == 0`` means *unattributed* (v026: providers with no
    measurable reasoning stay 0) — never "does no reasoning".
    """
    share = (reasoning_tokens / output_tokens) if output_tokens > 0 else 0.0
    if reasoning_tokens > 0:
        if share >= HIGH_REASONING_SHARE:
            return "reasoning-heavy", share
        if share < LOW_REASONING_SHARE:
            return "low-reasoning", share
        return "mixed", share
    avg_out = (output_tokens / events) if events > 0 else 0.0
    if avg_out < SHORT_OUTPUT_TOKENS_PER_EVENT:
        return "short-output (reasoning unattributed)", share
    return "unattributed", share


def _make_recommendation(
    *,
    rec_id: str,
    candidate: tuple[float, str, str],
    rationale: str,
    caveats: list[str],
    rollup: dict[str, Any],
    work_type: str,
    reasoning_share: float,
    actual_cost: float,
    monthly_factor: float | None,
    quality: tuple[float, int] | None,
) -> dict[str, Any]:
    """Assemble one recommendation row (shared by every rule)."""
    cand_cost, cand_id, cand_label = candidate
    window_delta = cand_cost - actual_cost
    events = int(rollup["events"] or 0)
    output_tokens = int(rollup["output_tokens"] or 0)
    return {
        "rec_id": rec_id,
        "work_type": work_type,
        "from_model": rollup["model"],
        "provider": rollup["provider"] or "anthropic",
        "speed": rollup["speed"] or "standard",
        "to_model": cand_id,
        "to_label": cand_label,
        "window_cost_usd": round(actual_cost, 4),
        "candidate_window_cost_usd": round(cand_cost, 4),
        "window_delta_usd": round(window_delta, 4),
        "estimated_monthly_delta_usd": (
            round(window_delta * monthly_factor, 4) if monthly_factor else None
        ),
        "evidence": {
            "events": events,
            "sessions": int(rollup["sessions"] or 0),
            "days_active": int(rollup["days_active"] or 0),
            "output_tokens": output_tokens,
            "reasoning_tokens": int(rollup["reasoning_tokens"] or 0),
            "reasoning_share": round(reasoning_share, 4),
            "avg_output_tokens_per_event": round(output_tokens / events, 1) if events else 0.0,
            "avg_quality_score": round(quality[0], 3) if quality else None,
            "graded_sessions": quality[1] if quality else 0,
        },
        "rationale": rationale,
        "caveats": caveats,
    }


def build_routing_recommendations(
    conn: sqlite3.Connection,
    *,
    scope: Scope | None = None,
    project_ids: list[int] | None = None,
    candidates: tuple[tuple[str, str, str], ...] = ROUTING_CANDIDATES,
    compute_cost: Any | None = None,
) -> dict[str, Any]:
    """Derive model-routing recommendations from per-model spend history.

    Args:
        conn: Open store connection (read-only use; guarded).
        scope: Optional timestamp window; ``None`` = all time.
        project_ids: ``None`` = whole store; ``[]`` = a filter that matched
            nothing (returns empty, never silently widens).
        candidates: ``(provider, model_id, label)`` routing targets;
            injectable for tests. Only same-provider candidates are
            considered — provider switches are the what-if tab's job.
        compute_cost: Injectable pricer (defaults to
            ``stackunderflow.infra.costs.compute_cost``); used as a black box.

    Rules (a rule with missing evidence simply doesn't fire):

    * ``downshift_low_reasoning`` — reasoning attribution present and share
      < :data:`LOW_REASONING_SHARE`: route to the cheapest same-provider
      candidate saving ≥ :data:`MIN_SAVINGS_SHARE` of window spend.
    * ``downshift_short_output`` — reasoning unattributed but mean output/
      event < :data:`SHORT_OUTPUT_TOKENS_PER_EVENT`: same repricing, with an
      explicit attribution caveat.
    * ``upshift_reasoning_quality`` — reasoning-heavy AND graded quality
      below :data:`QUALITY_SCORE_FLOOR` (needs ``session_quality_metrics``
      rows): route up to the next-more-expensive candidate; the delta is
      positive (an investment, not a saving).

    Delta sign matches ``/api/whatif``: ``candidate − actual`` (negative =
    cheaper). Returns an empty-but-well-formed payload for an empty store.
    """
    if compute_cost is None:  # deferred import keeps module import cheap
        from stackunderflow.infra.costs import compute_cost as _cc

        compute_cost = _cc

    rollups, observed_days = _load_model_rollups(conn, scope=scope, project_ids=project_ids)
    if not rollups:
        return {
            "recommendations": [],
            "models": [],
            "observed_days": 0,
            "monthly_factor": None,
            "caveats": [],
        }

    quality = _load_quality_by_model(conn, scope=scope, project_ids=project_ids)
    monthly_factor = (BASELINE_MONTH_DAYS / observed_days) if observed_days > 0 else None

    model_rows: list[dict[str, Any]] = []
    recs: list[dict[str, Any]] = []

    for r in rollups:
        model = r["model"]
        provider = r["provider"] or "anthropic"
        speed = r["speed"] or "standard"
        events = int(r["events"] or 0)
        output_tokens = int(r["output_tokens"] or 0)
        reasoning_tokens = int(r["reasoning_tokens"] or 0)
        actual_cost = float(r["cost_usd"] or 0.0)
        work_type, reasoning_share = _classify_work_type(
            reasoning_tokens=reasoning_tokens,
            output_tokens=output_tokens,
            events=events,
        )
        q = quality.get(model)
        shape = {
            "input": int(r["input_tokens"] or 0),
            "output": output_tokens,
            "cache_creation": int(r["cache_create_tokens"] or 0),
            "cache_read": int(r["cache_read_tokens"] or 0),
        }

        model_rows.append(
            {
                "model": model,
                "provider": provider,
                "speed": speed,
                "events": events,
                "sessions": int(r["sessions"] or 0),
                "days_active": int(r["days_active"] or 0),
                "window_cost_usd": round(actual_cost, 4),
                "output_tokens": output_tokens,
                "reasoning_tokens": reasoning_tokens,
                "reasoning_share": round(reasoning_share, 4),
                "reasoning_attributed": reasoning_tokens > 0,
                "work_type": work_type,
                "avg_quality_score": round(q[0], 3) if q else None,
                "graded_sessions": q[1] if q else 0,
            }
        )

        if actual_cost <= 0:
            continue  # nothing priced → no dollar claim to make

        # Reprice the model's actual token shape on every same-provider
        # candidate. Unpriceable candidates (pricer raised, or returned a
        # non-positive cost) are skipped outright — a $0 candidate would
        # fabricate a 100% saving.
        priced: list[tuple[float, str, str]] = []  # (candidate_cost, id, label)
        for cand_provider, cand_id, cand_label in candidates:
            if cand_provider != provider or _same_model(model, cand_id):
                continue
            try:
                cost = float(
                    compute_cost(shape, cand_id, provider=cand_provider, speed=speed)["total_cost"]
                )
            except Exception:  # noqa: BLE001, S112 — one bad candidate can't sink the report
                continue
            if cost <= 0:
                continue
            priced.append((cost, cand_id, cand_label))

        if not priced:
            continue

        # Downshift rules — cheapest candidate, savings-gated.
        cheaper = [p for p in priced if p[0] < actual_cost]
        if cheaper:
            best = min(cheaper, key=lambda p: p[0])
            savings = actual_cost - best[0]
            savings_share = savings / actual_cost
            if savings >= MIN_WINDOW_SAVINGS_USD and savings_share >= MIN_SAVINGS_SHARE:
                if work_type == "low-reasoning":
                    recs.append(
                        _make_recommendation(
                            rec_id="downshift_low_reasoning",
                            candidate=best,
                            rationale=(
                                f"Only {reasoning_share:.1%} of {model}'s output tokens were "
                                f"reasoning in this window — light work its rate card overprices. "
                                f"Routing it to {best[2]} would have cost "
                                f"${best[0]:,.2f} instead of ${actual_cost:,.2f} "
                                f"({savings_share:.0%} less)."
                            ),
                            caveats=[],
                            rollup=r,
                            work_type=work_type,
                            reasoning_share=reasoning_share,
                            actual_cost=actual_cost,
                            monthly_factor=monthly_factor,
                            quality=q,
                        )
                    )
                elif work_type == "short-output (reasoning unattributed)":
                    recs.append(
                        _make_recommendation(
                            rec_id="downshift_short_output",
                            candidate=best,
                            rationale=(
                                f"{model} averaged {output_tokens / events:,.0f} output tokens per "
                                f"event — short completions. Routing them to {best[2]} would have "
                                f"cost ${best[0]:,.2f} instead of ${actual_cost:,.2f} "
                                f"({savings_share:.0%} less)."
                            ),
                            caveats=[
                                "Reasoning attribution is unavailable for this provider — "
                                "this recommendation is based on output size alone."
                            ],
                            rollup=r,
                            work_type=work_type,
                            reasoning_share=reasoning_share,
                            actual_cost=actual_cost,
                            monthly_factor=monthly_factor,
                            quality=q,
                        )
                    )

        # Upshift rule — needs BOTH a reasoning-heavy signal and graded
        # evidence the current model is underperforming. No grade → no rec.
        if work_type == "reasoning-heavy" and q is not None and q[0] < QUALITY_SCORE_FLOOR:
            dearer = [p for p in priced if p[0] > actual_cost]
            if dearer:
                step_up = min(dearer, key=lambda p: p[0])
                recs.append(
                    _make_recommendation(
                        rec_id="upshift_reasoning_quality",
                        candidate=step_up,
                        rationale=(
                            f"{model} spends {reasoning_share:.0%} of its output on reasoning "
                            f"but its graded sessions average {q[0]:.1f}/5 "
                            f"(n={q[1]}). Routing this reasoning-heavy work to {step_up[2]} "
                            f"costs ${step_up[0] - actual_cost:,.2f} more over the window — "
                            "an investment in quality, not a saving."
                        ),
                        caveats=[],
                        rollup=r,
                        work_type=work_type,
                        reasoning_share=reasoning_share,
                        actual_cost=actual_cost,
                        monthly_factor=monthly_factor,
                        quality=q,
                    )
                )

    # Biggest saving first (most negative window delta); upshifts trail.
    recs.sort(key=lambda rec: rec["window_delta_usd"])

    return {
        "recommendations": recs,
        "models": sorted(model_rows, key=lambda m: -m["window_cost_usd"]),
        "observed_days": observed_days,
        "monthly_factor": round(monthly_factor, 4) if monthly_factor else None,
        "caveats": list(_ROUTING_CAVEATS),
    }
