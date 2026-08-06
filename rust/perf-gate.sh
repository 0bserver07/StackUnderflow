#!/usr/bin/env bash
# DIV-349 — the gate that lets Python's perf-budget tests retire.
# Budgets are contractual in --release; the tests self-skip in debug.
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")"
cargo build --release -p stax-cli >/dev/null
# Serial, like the reference suite: concurrent gates fight for CPU and the
# budgets are per-measurement, not per-machine-load.
cargo test --release -p stax-server --test perf_gates -- --nocapture --test-threads=1 "$@"
