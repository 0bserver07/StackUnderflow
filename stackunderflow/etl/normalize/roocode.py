"""Roo Code normalizer (Cline-family).

Roo Code is another Cline fork distributed under the VS Code extension
id ``rooveterinaryinc.roo-cline``. Same on-disk shape as Cline (and
KiloCode); same transform. Only ``provider_name`` differs so the
registry can route ``provider='roocode'`` rows here.

The pricer-side provider map already routes ``roocode`` to Anthropic.
"""

from __future__ import annotations

from .cline import ClineNormalizer


class RooCodeNormalizer(ClineNormalizer):
    provider_name = "roocode"
