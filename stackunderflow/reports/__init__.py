"""Terminal-facing reporting layer — consumed by the CLI."""

from stackunderflow.reports.scope import Scope, parse_period

__all__ = ["Scope", "parse_period"]
