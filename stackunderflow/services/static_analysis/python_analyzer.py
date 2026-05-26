"""Python static analyzer — complexity / lint count / type completeness.

The analyzer takes the full text of a single Python file (already
written to a tmpdir by the runner) and produces a ``FileMetrics`` dict
keyed by metric name. Each metric is independently optional:

* ``complexity`` — average cyclomatic complexity across functions /
  methods in the file. Computed via ``radon.complexity.cc_visit``
  (optional dep, declared in ``[analysis]``). Missing ⇒ metric absent.
* ``lint_count`` — number of ``ruff`` findings on the file. We ship with
  ``ruff`` as a dev dep so this is the metric most likely to populate;
  shells out to ``ruff check --output-format json``. The subprocess is
  hard-capped at ``_TIMEOUT_S`` seconds — a hung linter never blocks
  the analyzer.
* ``type_completeness`` — ratio of typed function signatures (rough
  proxy: ``mypy --no-error-summary`` clean lines / total). Optional
  dep ``mypy`` (also a dev dep). When the file has no functions the
  metric is reported as ``None`` (rather than an arbitrary 1.0).

``available()`` returns ``(True, "")`` when *at least one* metric can
run (so the runner emits something). The per-metric availability is
encoded in the result: a missing tool yields the metric being absent
from the returned dict + a reason recorded in ``warnings``.

Coverage (``coverage.py``) is **deferred** to Spec 22 — needs test
runner sandboxing. ``ALL_METRICS`` here lists only the three metrics
this file implements.
"""

from __future__ import annotations

import json
import shutil
import subprocess  # noqa: S404 — analyzer-on-tmpdir, command args fully controlled
from dataclasses import dataclass, field
from pathlib import Path

__all__ = [
    "ALL_METRICS",
    "FileMetrics",
    "analyze",
    "available",
]

# Hard cap on each subprocess. Spec calls out 60s default per-file budget;
# we use the same here. A radon-pure-python pass is millisecond-fast — the
# cap is for the shell-out tools (ruff/mypy on a giant file).
_TIMEOUT_S = 60

# The closed list of metrics this analyzer can produce. Mirrors the runner's
# ``METRIC_KEYS`` minus ``coverage`` (deferred).
ALL_METRICS = ("complexity", "lint_count", "type_completeness")


@dataclass(slots=True)
class FileMetrics:
    """One analyzer pass over one file. ``metrics`` keys are metric names.

    A metric being *absent* from ``metrics`` means the analyzer couldn't
    produce a value (tool missing, file empty, or parse failure); the
    reason lives in ``warnings``. ``details`` carries per-metric extras
    (lint rule frequency, average vs. max complexity) that get persisted
    as the row's ``details_json``.
    """

    metrics: dict[str, float] = field(default_factory=dict)
    details: dict[str, object] = field(default_factory=dict)
    warnings: list[str] = field(default_factory=list)


# ── availability ──────────────────────────────────────────────────────────


def _radon_available() -> bool:
    try:
        import radon.complexity  # noqa: F401 — import is the probe
    except ImportError:
        return False
    return True


def _ruff_available() -> bool:
    return shutil.which("ruff") is not None


def _mypy_available() -> bool:
    # Importing as a module is cheaper than a subprocess version probe.
    try:
        import mypy.api  # noqa: F401
    except ImportError:
        return False
    return True


def available() -> tuple[bool, str]:
    """Return ``(any_metric_available, reason)``.

    The runner invokes the analyzer when ``any_metric_available`` is
    True. ``reason`` summarises what's missing so the CLI can warn the
    user once per language.
    """
    parts: list[str] = []
    have_any = False
    if _radon_available():
        have_any = True
    else:
        parts.append("radon not installed (pip install 'stackunderflow[analysis]')")
    if _ruff_available():
        have_any = True
    else:
        parts.append("ruff not on PATH (pip install ruff)")
    if _mypy_available():
        have_any = True
    else:
        parts.append("mypy not installed (pip install 'stackunderflow[analysis]')")
    return have_any, "; ".join(parts)


# ── per-metric implementations ────────────────────────────────────────────


def _complexity(content: str) -> tuple[float | None, dict[str, object], str | None]:
    """Average cyclomatic complexity across all functions/methods.

    Empty file or zero-function file returns ``(None, {}, None)`` — the
    metric is meaningfully absent rather than spuriously zero.
    """
    if not _radon_available():
        return None, {}, "radon not installed"
    try:
        from radon.complexity import cc_visit
    except ImportError as e:  # pragma: no cover — guarded by _radon_available
        return None, {}, f"radon import failed: {e}"
    try:
        results = cc_visit(content)
    except (SyntaxError, ValueError) as e:
        return None, {}, f"radon parse failed: {e}"
    if not results:
        return None, {"functions": 0}, None
    cmps = [float(r.complexity) for r in results]
    avg = sum(cmps) / len(cmps)
    return (
        round(avg, 3),
        {
            "functions": len(cmps),
            "max_complexity": int(max(cmps)),
            "min_complexity": int(min(cmps)),
        },
        None,
    )


def _lint_count(
    file_path: Path,
) -> tuple[float | None, dict[str, object], str | None]:
    """Count of ``ruff`` findings on the file.

    Returns the integer count as a float (the schema's ``REAL`` carries
    everything) plus the top-3 rule ids in details.
    """
    if not _ruff_available():
        return None, {}, "ruff not on PATH"
    cmd = [
        "ruff",
        "check",
        "--output-format=json",
        "--no-cache",
        # Don't follow project config — we want a *baseline* lint count
        # that's comparable across pre/post snapshots, not "what the host
        # project's pyproject.toml says". A custom config file would
        # make pre vs. post incomparable when only the .ruff.toml moved.
        "--isolated",
        str(file_path),
    ]
    try:
        proc = subprocess.run(  # noqa: S603 — args fully controlled, file_path is from tmpdir
            cmd, capture_output=True, text=True, timeout=_TIMEOUT_S, check=False,
        )
    except subprocess.TimeoutExpired:
        return None, {}, f"ruff timed out after {_TIMEOUT_S}s"
    except (OSError, FileNotFoundError) as e:
        return None, {}, f"ruff failed to start: {e}"
    # ruff exit code: 0 = clean, 1 = findings, 2 = error.
    if proc.returncode not in (0, 1):
        return None, {}, f"ruff exit {proc.returncode}: {proc.stderr.strip()[:200]}"
    try:
        findings = json.loads(proc.stdout) if proc.stdout.strip() else []
    except json.JSONDecodeError as e:
        return None, {}, f"ruff JSON parse failed: {e}"
    if not isinstance(findings, list):
        return None, {}, "ruff JSON not a list"
    rule_freq: dict[str, int] = {}
    for f in findings:
        if not isinstance(f, dict):
            continue
        code = f.get("code")
        if isinstance(code, str):
            rule_freq[code] = rule_freq.get(code, 0) + 1
    top_rules = sorted(rule_freq.items(), key=lambda kv: kv[1], reverse=True)[:3]
    return (
        float(len(findings)),
        {"top_rules": [{"code": code, "count": count} for code, count in top_rules]},
        None,
    )


def _type_completeness(
    file_path: Path, content: str,
) -> tuple[float | None, dict[str, object], str | None]:
    """Ratio of fully typed function signatures in the file.

    Heuristic: parse the file's AST, count ``def`` / ``async def`` nodes,
    classify each as "fully typed" (all args and return have annotations,
    excluding ``self`` / ``cls``). Returns the ratio in [0, 1] with the
    raw counts in details.

    We use the AST instead of mypy here because mypy's "type completeness"
    output requires a full project context (imports resolved); the AST
    pass is per-file and gives a portable, fast metric. ``mypy``
    availability is still surfaced in ``available()`` so a future v2
    metric (e.g. ``type_errors``) can be added against the same gate.
    """
    if not content.strip():
        return None, {}, None
    try:
        import ast
    except ImportError:  # pragma: no cover — stdlib
        return None, {}, "ast unavailable"
    try:
        tree = ast.parse(content, str(file_path))
    except SyntaxError as e:
        return None, {}, f"AST parse failed: {e}"
    total = 0
    typed = 0
    for node in ast.walk(tree):
        if not isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
            continue
        total += 1
        args = node.args
        # Args we consider for typing: positional, keyword-only, varargs,
        # kwargs. ``self`` / ``cls`` (first positional in a method) are
        # exempted — those are conventionally untyped.
        positional = list(args.args)
        if positional and positional[0].arg in ("self", "cls"):
            positional = positional[1:]
        all_params: list[ast.arg] = (
            positional
            + list(args.kwonlyargs)
            + ([args.vararg] if args.vararg else [])
            + ([args.kwarg] if args.kwarg else [])
        )
        all_typed = all(p.annotation is not None for p in all_params)
        return_typed = node.returns is not None
        if all_typed and return_typed:
            typed += 1
    if total == 0:
        return None, {"functions": 0}, None
    ratio = typed / total
    return (
        round(ratio, 3),
        {"functions": total, "typed_functions": typed},
        None,
    )


# ── orchestration ─────────────────────────────────────────────────────────


def analyze(file_path: Path, content: str) -> FileMetrics:
    """Run every available metric over ``content``.

    ``file_path`` is the on-disk location of the file (the runner has
    already written ``content`` there); some analyzers (``ruff``) need a
    real path. The returned ``FileMetrics.metrics`` dict only carries
    keys whose pass succeeded; everything else is reported in
    ``warnings``.
    """
    out = FileMetrics()
    cx_value, cx_details, cx_warn = _complexity(content)
    if cx_value is not None:
        out.metrics["complexity"] = cx_value
    if cx_details:
        out.details["complexity"] = cx_details
    if cx_warn:
        out.warnings.append(f"complexity: {cx_warn}")

    lint_value, lint_details, lint_warn = _lint_count(file_path)
    if lint_value is not None:
        out.metrics["lint_count"] = lint_value
    if lint_details:
        out.details["lint_count"] = lint_details
    if lint_warn:
        out.warnings.append(f"lint_count: {lint_warn}")

    tc_value, tc_details, tc_warn = _type_completeness(file_path, content)
    if tc_value is not None:
        out.metrics["type_completeness"] = tc_value
    if tc_details:
        out.details["type_completeness"] = tc_details
    if tc_warn:
        out.warnings.append(f"type_completeness: {tc_warn}")

    return out
