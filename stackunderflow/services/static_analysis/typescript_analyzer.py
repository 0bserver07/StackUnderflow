"""TypeScript static analyzer — type errors / lint count.

Shells out to ``tsc`` (TypeScript compiler) and ``eslint``. Both are
required to be on ``PATH`` — there's no Python-side package to depend
on, so the optional-dep declaration is "we expect the user's project
toolchain to provide these". When neither is present the runner skips
the file with a warning recorded in the row's ``details_json``.

Metrics produced:

* ``type_completeness`` — derived from ``tsc --noEmit`` error count.
  Lower error count ⇒ higher completeness. We surface the *raw error
  count* as ``type_completeness`` (negated by the runner where the
  consumer wants "higher is better"); to keep the schema's enum
  meaningful across analyzers, the value is ``1.0 - min(1, errors / 10)``
  so it lives in [0, 1] like the Python analyzer's metric.
* ``lint_count`` — number of ``eslint`` problems on the file.

Complexity is **deferred** — there's no clean cross-toolchain answer
for TS/JS complexity (escomplex is unmaintained, ts-complexity-checker
is project-local, npx-package availability is unreliable). When that
gap closes we'll wire it through the same shape.
"""

from __future__ import annotations

import json
import shutil
import subprocess  # noqa: S404 — analyzer-on-tmpdir, command args fully controlled
from pathlib import Path

from stackunderflow.services.static_analysis.python_analyzer import FileMetrics

__all__ = [
    "ALL_METRICS",
    "analyze",
    "available",
]

_TIMEOUT_S = 60
ALL_METRICS = ("lint_count", "type_completeness")


def _tsc_available() -> bool:
    return shutil.which("tsc") is not None


def _eslint_available() -> bool:
    return shutil.which("eslint") is not None


def available() -> tuple[bool, str]:
    parts: list[str] = []
    have_any = False
    if _tsc_available():
        have_any = True
    else:
        parts.append("tsc not on PATH (npm install -g typescript)")
    if _eslint_available():
        have_any = True
    else:
        parts.append("eslint not on PATH (npm install -g eslint)")
    return have_any, "; ".join(parts)


def _type_completeness_via_tsc(
    file_path: Path,
) -> tuple[float | None, dict[str, object], str | None]:
    """Run ``tsc --noEmit`` and translate the error count into a [0, 1] score."""
    if not _tsc_available():
        return None, {}, "tsc not on PATH"
    cmd = [
        "tsc",
        "--noEmit",
        "--pretty",
        "false",
        # ``--target`` is best-effort; without a project tsconfig the
        # compiler defaults to ES3, which floods the baseline error
        # count with "object spread requires ES2015" noise. ES2020 is a
        # reasonable middle that doesn't introduce JSX/strict assumptions.
        "--target",
        "es2020",
        "--module",
        "esnext",
        "--moduleResolution",
        "node",
        "--allowJs",
        "--skipLibCheck",
        str(file_path),
    ]
    try:
        proc = subprocess.run(  # noqa: S603 — args fully controlled, file_path is tmpdir
            cmd, capture_output=True, text=True, timeout=_TIMEOUT_S, check=False,
        )
    except subprocess.TimeoutExpired:
        return None, {}, f"tsc timed out after {_TIMEOUT_S}s"
    except (OSError, FileNotFoundError) as e:
        return None, {}, f"tsc failed to start: {e}"
    # tsc returns non-zero when there are type errors. We don't differentiate
    # "error" from "config issue" because the cap below pegs the score at 0
    # for any 10+ findings — same outcome as a complete failure.
    err_lines = [
        line for line in (proc.stdout or "").splitlines()
        if "error TS" in line
    ]
    error_count = len(err_lines)
    # Map [0, ∞) errors to [1, 0] completeness.
    score = 1.0 - min(1.0, error_count / 10.0)
    return (
        round(score, 3),
        {"type_errors": error_count},
        None,
    )


def _lint_count(
    file_path: Path,
) -> tuple[float | None, dict[str, object], str | None]:
    """Count of ``eslint`` problems for the file."""
    if not _eslint_available():
        return None, {}, "eslint not on PATH"
    cmd = [
        "eslint",
        "--format",
        "json",
        "--no-eslintrc",
        # Minimal default config — same rationale as ruff's --isolated.
        # We don't want the host project's .eslintrc.json polluting the
        # baseline.
        str(file_path),
    ]
    try:
        proc = subprocess.run(  # noqa: S603 — args fully controlled, file_path is tmpdir
            cmd, capture_output=True, text=True, timeout=_TIMEOUT_S, check=False,
        )
    except subprocess.TimeoutExpired:
        return None, {}, f"eslint timed out after {_TIMEOUT_S}s"
    except (OSError, FileNotFoundError) as e:
        return None, {}, f"eslint failed to start: {e}"
    # eslint exits 0 (clean), 1 (problems), 2 (config error).
    if proc.returncode not in (0, 1):
        return None, {}, f"eslint exit {proc.returncode}: {proc.stderr.strip()[:200]}"
    try:
        data = json.loads(proc.stdout) if proc.stdout.strip() else []
    except json.JSONDecodeError as e:
        return None, {}, f"eslint JSON parse failed: {e}"
    if not isinstance(data, list):
        return None, {}, "eslint JSON not a list"
    # ``data`` is one entry per file; we always pass exactly one file in.
    total = 0
    rule_freq: dict[str, int] = {}
    for entry in data:
        if not isinstance(entry, dict):
            continue
        msgs = entry.get("messages")
        if not isinstance(msgs, list):
            continue
        for m in msgs:
            if not isinstance(m, dict):
                continue
            total += 1
            rule = m.get("ruleId")
            if isinstance(rule, str):
                rule_freq[rule] = rule_freq.get(rule, 0) + 1
    top_rules = sorted(rule_freq.items(), key=lambda kv: kv[1], reverse=True)[:3]
    return (
        float(total),
        {"top_rules": [{"code": code, "count": count} for code, count in top_rules]},
        None,
    )


def analyze(file_path: Path, content: str) -> FileMetrics:
    """Run every available metric over ``content``."""
    # Touch ``content`` so the file_path argument matches the on-disk
    # write the runner already did. Both tools want a real file.
    _ = content
    out = FileMetrics()

    tc_value, tc_details, tc_warn = _type_completeness_via_tsc(file_path)
    if tc_value is not None:
        out.metrics["type_completeness"] = tc_value
    if tc_details:
        out.details["type_completeness"] = tc_details
    if tc_warn:
        out.warnings.append(f"type_completeness: {tc_warn}")

    lint_value, lint_details, lint_warn = _lint_count(file_path)
    if lint_value is not None:
        out.metrics["lint_count"] = lint_value
    if lint_details:
        out.details["lint_count"] = lint_details
    if lint_warn:
        out.warnings.append(f"lint_count: {lint_warn}")

    return out
