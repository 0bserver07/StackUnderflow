"""Go static analyzer — vet errors / cyclomatic complexity.

Shells out to ``go vet`` and ``gocyclo`` (both must be on ``PATH`` —
when absent the analyzer skips cleanly with a recorded warning, same
contract as the TS analyzer).

Metrics produced:

* ``lint_count`` — number of ``go vet`` findings on the file (a Go-vet
  finding is functionally a lint hit).
* ``complexity`` — average cyclomatic complexity from ``gocyclo``.

Coverage (``go test -coverprofile``) is **deferred** — needs test
runner sandboxing, same as the Python coverage path. Type completeness
isn't a meaningful Go metric (the language requires types) so it's
absent from this analyzer.
"""

from __future__ import annotations

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
ALL_METRICS = ("complexity", "lint_count")


def _go_available() -> bool:
    return shutil.which("go") is not None


def _gocyclo_available() -> bool:
    return shutil.which("gocyclo") is not None


def available() -> tuple[bool, str]:
    parts: list[str] = []
    have_any = False
    if _go_available():
        have_any = True
    else:
        parts.append("go not on PATH (install Go: https://go.dev/dl)")
    if _gocyclo_available():
        have_any = True
    else:
        parts.append("gocyclo not on PATH (go install github.com/fzipp/gocyclo/cmd/gocyclo@latest)")
    return have_any, "; ".join(parts)


def _lint_count_via_go_vet(
    file_path: Path,
) -> tuple[float | None, dict[str, object], str | None]:
    """Count of ``go vet`` issues on the file."""
    if not _go_available():
        return None, {}, "go not on PATH"
    cmd = ["go", "vet", str(file_path)]
    try:
        proc = subprocess.run(  # noqa: S603 — args fully controlled, file_path is tmpdir
            cmd, capture_output=True, text=True, timeout=_TIMEOUT_S, check=False,
        )
    except subprocess.TimeoutExpired:
        return None, {}, f"go vet timed out after {_TIMEOUT_S}s"
    except (OSError, FileNotFoundError) as e:
        return None, {}, f"go vet failed to start: {e}"
    # ``go vet`` writes findings to stderr, exits non-zero on findings.
    # A bare-file invocation in a tmpdir without a go.mod often errors
    # "no Go files in <dir>" before vet even runs — we treat that as
    # "no findings observable" rather than a hard failure.
    err = (proc.stderr or "").strip()
    if "no Go files" in err or "go.mod file not found" in err:
        return None, {}, "go vet requires a module context (no go.mod in tmpdir)"
    findings = [
        line for line in err.splitlines()
        if line and not line.startswith(("#", "go: "))
    ]
    return (
        float(len(findings)),
        {"vet_lines": min(5, len(findings))},
        None,
    )


def _complexity_via_gocyclo(
    file_path: Path,
) -> tuple[float | None, dict[str, object], str | None]:
    """Average cyclomatic complexity via ``gocyclo``."""
    if not _gocyclo_available():
        return None, {}, "gocyclo not on PATH"
    cmd = ["gocyclo", "-avg", str(file_path)]
    try:
        proc = subprocess.run(  # noqa: S603 — args fully controlled, file_path is tmpdir
            cmd, capture_output=True, text=True, timeout=_TIMEOUT_S, check=False,
        )
    except subprocess.TimeoutExpired:
        return None, {}, f"gocyclo timed out after {_TIMEOUT_S}s"
    except (OSError, FileNotFoundError) as e:
        return None, {}, f"gocyclo failed to start: {e}"
    if proc.returncode != 0:
        return None, {}, f"gocyclo exit {proc.returncode}: {proc.stderr.strip()[:200]}"
    # gocyclo's last line with "Average:" is what we want.
    out = (proc.stdout or "").strip()
    avg: float | None = None
    func_count = 0
    for line in out.splitlines():
        line = line.strip()
        if line.lower().startswith("average:"):
            try:
                avg = float(line.split(":", 1)[1].strip())
            except (IndexError, ValueError):
                avg = None
        else:
            # Per-function line. Format: "<complexity> <package> <func> <pos>"
            parts = line.split()
            if parts and parts[0].isdigit():
                func_count += 1
    if avg is None and func_count == 0:
        return None, {"functions": 0}, None
    return (
        round(avg, 3) if avg is not None else None,
        {"functions": func_count},
        None,
    )


def analyze(file_path: Path, content: str) -> FileMetrics:
    """Run every available metric over ``content``."""
    _ = content
    out = FileMetrics()

    cx_value, cx_details, cx_warn = _complexity_via_gocyclo(file_path)
    if cx_value is not None:
        out.metrics["complexity"] = cx_value
    if cx_details:
        out.details["complexity"] = cx_details
    if cx_warn:
        out.warnings.append(f"complexity: {cx_warn}")

    lint_value, lint_details, lint_warn = _lint_count_via_go_vet(file_path)
    if lint_value is not None:
        out.metrics["lint_count"] = lint_value
    if lint_details:
        out.details["lint_count"] = lint_details
    if lint_warn:
        out.warnings.append(f"lint_count: {lint_warn}")

    return out
