#!/usr/bin/env python3
"""The Python side of the endpoint differ, booted with its writers disarmed.

`rust/endpoint-parity.sh` runs `uvicorn parity.pyserver:app`. This module is
`stackunderflow.server.app` with exactly three interventions, and each one is
here because leaving it out makes the harness measure something other than the
endpoints.

1. **`run_ingest` becomes a no-op.** `server._lifespan` starts a background
   thread that walks every provider's on-disk transcripts and writes them into
   the store. Against the maintainer's real `~/.claude` and a 3.9 GB store copy
   that is minutes of work *and* a live writer mutating the very rows both
   servers are being diffed on. The differ would be racing an ingest.

2. **`backfill_price_book` becomes a no-op.** Same reason, narrower blast
   radius: it is an idempotent UPSERT, but it is still a write, and the Rust
   side never writes rates. Both implementations therefore read whatever the
   `price_book` table already holds — `use_price_book_store` and
   `prime_price_book_cache` still run, so the seam stays ON, which is what a
   running server does (`rust/crates/stax-server/src/pricing.rs` pins the Rust
   half to the same source).

3. **`_maybe_clean_cold_cache` becomes a no-op.** It `rmtree`s
   `$STACKUNDERFLOW_HOME/cache`, and the harness home is a state directory the
   CLI gate also uses.

The watcher and its lock are disabled through the environment variables the
`--no-watcher` / `--no-lock` flags already set, so that part is not a patch at
all.

Everything else — the five service constructors, `schema.apply`, the routers,
the middleware, the SPA routes — runs exactly as it does in production. The
patches are applied to the modules the lifespan imports *inside* the function
body, which is why importing them here is enough.

NOT patched, on purpose: anything that changes a RESPONSE. If this file ever
grows a fourth intervention that touches a payload, the harness has stopped
being evidence.
"""

from __future__ import annotations

import os

# The watcher writes marts and the lock fences a singleton; both are lifecycle,
# not endpoints. `server._watcher_disabled` / `_lock_disabled` read these.
os.environ.setdefault("STACKUNDERFLOW_DISABLE_WATCHER", "1")
os.environ.setdefault("STACKUNDERFLOW_DISABLE_LOCK", "1")

import stackunderflow.ingest as _ingest  # noqa: E402
import stackunderflow.infra.costs as _costs  # noqa: E402


def _no_ingest(conn, adapters, *args, **kwargs):  # noqa: ANN001, ANN002, ANN003, ARG001
    """`run_ingest` with the store left alone. Returns its counts shape."""
    return {}


def _no_backfill(conn, *args, **kwargs):  # noqa: ANN001, ANN002, ANN003, ARG001
    """`backfill_price_book`, disarmed. Reads still see the existing rows."""
    return None


_ingest.run_ingest = _no_ingest
_costs.backfill_price_book = _no_backfill

import stackunderflow.server as _server  # noqa: E402

_server._maybe_clean_cold_cache = lambda: None  # noqa: SLF001

app = _server.app

__all__ = ["app"]
