"""Roo Code pricer.

Roo Code is a Cline-family VS Code extension that
delegates inference to a real upstream provider (Anthropic, OpenAI, etc.)
and records the model used as a vendor-prefixed string on the first user
message — same on-disk format as Cline.

Pricing strategy is identical to :class:`ClinePricer`: parse the vendor
prefix and delegate ``rates_for`` to the matching real pricer, returning
``None`` for unknown vendors so the cost layer surfaces a missing rate
rather than mispricing against an arbitrary table.

Spec §3.2.
"""

from __future__ import annotations

from .cline import ClinePricer


class RooCodePricer(ClinePricer):
    provider_name = "roocode"
