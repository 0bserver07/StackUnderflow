#!/usr/bin/env bash
#
# Regenerate the golden fixtures for the `staxtrace.anchor/1` contract.
#
#   bash contracts/staxtrace-anchor-v1/fixtures/regenerate.sh
#
# Unlike contracts/staxtrace-memory-v1/fixtures/regenerate.sh, this one is
# DETERMINISTIC and machine-independent: re-running it on any box, in any year,
# reproduces byte-identical files. Nothing here reads the live store, the live
# clock or the environment. The scenario (three keys, five appends, covering
# multi-word bodies, non-ASCII, embedded quotes/tabs/backslashes and a
# multi-line markdown body) lives in
#
#   rust/crates/stax-core/tests/anchor_contract.rs
#
# and is replayed through the real storage and rendering code against a scratch
# sidecar, with an injected clock (rust/ARCHITECT-STATE.md finding 5: config and
# time enter as arguments, never as ambient state) and a fixed rendered `db`
# path. Which is why a diff here is always a real contract change, never drift.
#
# Fixtures produced (set -> get -> log, the flow an agent actually runs):
#
#   set.receipts.txt   the five `anchor set` receipt lines, in order
#   get.all.json/.txt  `anchor get` -- newest entry per key, key-sorted
#   get.one.json/.txt  `anchor get architect-state`
#   get.empty.json     `anchor get never-anchored` -- an empty result is a
#                      SUCCESS envelope, not an error
#   log.json/.txt      `anchor log architect-state` -- oldest to newest
#
# After regenerating, review the diff and re-run the checks:
#
#   rust/ci.sh
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../../rust"

# Prefer the rustup toolchain over a distro cargo. rust/ci.sh only sources
# ~/.cargo/env when `cargo` is missing entirely, which is not enough on a box
# that also has /usr/bin/cargo: an older system cargo shadows rustup, ignores
# rust-toolchain.toml, and dies on `feature edition2024 is required`.
if [ -x "$HOME/.cargo/bin/cargo" ]; then
    PATH="$HOME/.cargo/bin:$PATH"
    export PATH
fi

cargo test -p stax-core --test anchor_contract -- --ignored --nocapture regenerate

echo
echo "Regenerated. Verify with: cargo test -p stax-core --test anchor_contract"
