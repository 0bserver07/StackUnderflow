"""Python side of the pricing parity sweep — the oracle, not a reimplementation.

Run by `tests/pricing_parity.rs` as `python -c "<this file>" <args>`. It imports
the reference implementation from the worktree under test and prints one
tab-separated record per case; the Rust harness recomputes each case with
`stax_etl::pricing` and compares IEEE-754 bit patterns, not rounded dollars.

argv (after `-c`):
  1  repo root (the worktree holding `stackunderflow/`)
  2  a scratch directory to use as $STACKUNDERFLOW_HOME
  3  the store to take the distinct (provider, model, speed) sweep from

Two deliberate pins, both matching the default state of a freshly imported
`stackunderflow` process — and both required for the run to stay read-only:

* `$STACKUNDERFLOW_HOME` points at a scratch dir, so `Settings()`,
  `model_manifest._STORE_PATH` and the pricing cache never touch the live
  dataset. The alias map is therefore empty, as it is on this machine.
* the upstream pricing overlay is pinned empty by injecting a stub
  `PricingService`. Left alone, `costs._load_overlay()` runs at import time (via
  `RATE_CARD`), hits the network, and writes a `cache/pricing.json` into the data
  dir. The overlay is empty on this machine anyway (no such cache file exists),
  so pinning it changes nothing except determinism.

Output sections, one record per line, `\t`-separated:

  COMBO   provider  model  speed                    -- distinct rows in the store
  PRICER  key       singleton provider_name  supports_per_message_tokens
  CASE    provider  model  speed  at_ts  vec  <5 x float bits>  <repr(total)>
  OACASE  provider  model  speed  at_ts  <5 x float bits>       -- OpenAI raw shape
  VENDOR  model     vendor|-
  RESOLVE provider|-  model  resolved
  FMT     <bits>    formatted
  COUNT   <section> <n>
"""

import os
import sqlite3
import struct
import sys
import types

REPO, SCRATCH_HOME, STORE = sys.argv[1], sys.argv[2], sys.argv[3]

os.environ["STACKUNDERFLOW_HOME"] = SCRATCH_HOME

# Pin the overlay empty BEFORE `infra.costs` is imported: its module-level
# RATE_CARD build calls _load_overlay(), which would otherwise fetch from the
# network and write a cache file.
_stub = types.ModuleType("stackunderflow.services.pricing_service")


class PricingService:  # noqa: D101 - stub
    def get_pricing(self):  # noqa: D102 - stub
        return {"pricing": {}}


_stub.PricingService = PricingService
sys.modules["stackunderflow.services.pricing_service"] = _stub

sys.path.insert(0, REPO)

from stackunderflow.infra import costs  # noqa: E402
from stackunderflow.infra import model_manifest  # noqa: E402
from stackunderflow.infra.providers import registered_pricers  # noqa: E402

assert costs.__file__.startswith(REPO), f"imported the wrong tree: {costs.__file__}"
assert costs._load_overlay() == {}, "overlay must be pinned empty"
assert model_manifest._use_store is False, "the price-book seam must be off"

out = []


def bits(value):
    return struct.pack(">d", float(value)).hex()


# ── the sweep universe ──────────────────────────────────────────────────────

conn = sqlite3.connect(f"file:{STORE}?mode=ro", uri=True)
try:
    events = conn.execute(
        "SELECT DISTINCT provider, model, speed FROM usage_events"
    ).fetchall()
    raw = conn.execute(
        "SELECT DISTINCT p.provider, COALESCE(m.model, ''), m.speed "
        "FROM messages m "
        "JOIN sessions s ON s.id = m.session_fk "
        "JOIN projects p ON p.id = s.project_id"
    ).fetchall()
finally:
    conn.close()

combos = sorted({(p or "", m or "", s or "standard") for p, m, s in events + raw})
for provider, model, speed in combos:
    out.append(f"COMBO\t{provider}\t{model}\t{speed}")
out.append(f"COUNT\tCOMBO\t{len(combos)}")

canonical_ids = list(model_manifest.canonical_ids())
registry = registered_pricers()
provider_keys = sorted(registry)

# The registry itself, not just its outputs: for every key, which SINGLETON it
# resolves to (`provider_name` is that singleton's identity — the thing
# `resolve_pricing_provider`'s `shell is upstream` test compares) and what the
# pricer says about per-message tokens.
for key in provider_keys:
    pricer = registry[key]
    out.append(
        "PRICER\t{}\t{}\t{}".format(
            key, pricer.provider_name, int(pricer.supports_per_message_tokens())
        )
    )
out.append(f"COUNT\tPRICER\t{len(provider_keys)}")

# Ids that exercise the routing chain's corners: autoselectors, vendor prefixes,
# ids nothing claims, casing/whitespace, and the models the store carries that
# are absent from `[canonical_ids]`.
edge_models = [
    "",
    "<synthetic>",
    "claude-opus-5",
    "claude-sonnet-5",
    "glm-5.2",
    "grok-4.5",
    "grok-build",
    "big-pickle",
    "deepseek-v4-flash-free",
    "opencode-auto",
    "codex-1",
    "cursor-auto",
    "cursor-fast",
    "kiro-auto",
    "copilot-auto",
    "continue-auto",
    "auto",
    "fast",
    "composer-2",
    "claude-4.5-sonnet-thinking",
    "gemini-2.5-pro-preview-05-06",
    "gemini-3-pro",
    "gemini-2.5-flash-experimental",
    "anthropic/claude-3-5-sonnet",
    "openai/gpt-4o",
    "ollama/llama-3",
    "CLAUDE-OPUS-4-8",
    "  claude-opus-4-8  ",
    "claude.3.5.sonnet",
    "qwen3-coder",
    "gpt-4.1",
    "gpt-4o-mini",
    "unheard-of-model",
]

AT_TS = [
    None,
    "2026-01-15",
    "2026-04-25T23:59:59+00:00",
    "2026-04-26",
    "2026-04-26T00:00:00+00:00",
    "2026-07-30T12:34:56.789012+00:00",
]
SPEEDS = ["standard", "fast"]
VECTORS = [
    (0, 0, 0, 0),
    (1, 1, 1, 1),
    (1234567, 98765, 4321, 7654321),
]


def emit_case(provider, model, speed, at_ts, vec_index, vec):
    tokens = {
        "input": vec[0],
        "output": vec[1],
        "cache_creation": vec[2],
        "cache_read": vec[3],
    }
    got = costs.compute_cost(tokens, model, provider, speed=speed, at_ts=at_ts)
    out.append(
        "CASE\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{!r}".format(
            provider,
            model,
            speed,
            "-" if at_ts is None else at_ts,
            vec_index,
            bits(got["input_cost"]),
            bits(got["output_cost"]),
            bits(got["cache_creation_cost"]),
            bits(got["cache_read_cost"]),
            bits(got["total_cost"]),
            got["total_cost"],
        )
    )


cases = 0

# 1. Every (provider, model, speed) in the store, at every at_ts, for 3 vectors.
for provider, model, _stored_speed in combos:
    for speed in SPEEDS:
        for at_ts in AT_TS:
            for i, vec in enumerate(VECTORS):
                emit_case(provider, model, speed, at_ts, i, vec)
                cases += 1

# 2. Every models.toml canonical id crossed with every registered pricer key.
for model in canonical_ids:
    for provider in provider_keys:
        for speed in SPEEDS:
            for at_ts in AT_TS:
                emit_case(provider, model, speed, at_ts, 2, VECTORS[2])
                cases += 1

# 3. The corner ids, crossed with every registered pricer key.
for model in edge_models:
    for provider in provider_keys:
        for speed in SPEEDS:
            for at_ts in (None, "2026-01-15"):
                emit_case(provider, model, speed, at_ts, 2, VECTORS[2])
                cases += 1

out.append(f"COUNT\tCASE\t{cases}")

# 4. The OpenAI raw wire shape, which only OpenAIPricer.normalize_tokens reshapes.
oa_cases = 0
for provider in provider_keys:
    for model in ("gpt-5.5", "gpt-5.4", "gpt-5-codex", "claude-opus-4-8", ""):
        for at_ts in (None, "2026-01-15"):
            tokens = {
                "input_tokens": 1000000,
                "output_tokens": 250000,
                "cached_input_tokens": 400000,
                "reasoning_output_tokens": 90000,
            }
            got = costs.compute_cost(tokens, model, provider, at_ts=at_ts)
            out.append(
                "OACASE\t{}\t{}\tstandard\t{}\t{}\t{}\t{}\t{}\t{}".format(
                    provider,
                    model,
                    "-" if at_ts is None else at_ts,
                    bits(got["input_cost"]),
                    bits(got["output_cost"]),
                    bits(got["cache_creation_cost"]),
                    bits(got["cache_read_cost"]),
                    bits(got["total_cost"]),
                )
            )
            oa_cases += 1
out.append(f"COUNT\tOACASE\t{oa_cases}")

# 5. The provider-resolution chain, standalone.
resolve_models = canonical_ids + edge_models + [m for _, m, _ in combos]
seen = set()
vendor_n = resolve_n = 0
for model in resolve_models:
    if model not in seen:
        seen.add(model)
        vendor = costs.vendor_for_model(model)
        out.append(f"VENDOR\t{model}\t{'-' if vendor is None else vendor}")
        vendor_n += 1
    for provider in [None, ""] + provider_keys + ["grok", "totally-unknown"]:
        resolved = costs.resolve_pricing_provider(provider, model)
        out.append(f"RESOLVE\t{'-' if provider is None else provider}\t{model}\t{resolved}")
        resolve_n += 1
out.append(f"COUNT\tVENDOR\t{vendor_n}")
out.append(f"COUNT\tRESOLVE\t{resolve_n}")

# 6. format_dollars across every band boundary and the awkward roundings.
amounts = [
    0.0, 1e-9, 0.0001, 0.00005, 0.005, 0.0099999, 0.01, 0.015, 0.0125, 0.0135,
    0.1, 0.5, 0.9999, 1.0, 1.005, 1.015, 1.025, 2.675, 9.995, 99.994, 99.995,
    99.999, 100.0, 100.5, 101.5, 1234.5, 1235.5, 999999.5, 1234567.891,
    -0.001, -1.0, -1234.5, -99.995, float("inf"), float("-inf"),
]
for amount in amounts:
    out.append(f"FMT\t{bits(amount)}\t{costs.format_dollars(amount)}")
out.append(f"COUNT\tFMT\t{len(amounts)}")

sys.stdout.write("\n".join(out))
sys.stdout.write("\n")
