#!/usr/bin/env bash
#
# Regenerate the golden fixtures for the `stackunderflow.memory/1` contract
# straight from real CLI output. One fixture per `memory` subcommand that emits
# the envelope (decisions/file/worked/sessions/ask) x {success, empty, error}.
#
#   bash contracts/stackunderflow-memory-v1/fixtures/regenerate.sh
#
# The queries below are machine-specific (they target a project + file that have
# recorded history on the maintainer's store). Override with env vars if the
# store differs:
#
#   SU_SLUG=<project-slug>  SU_FILE=<abs-path-with-history>  bash regenerate.sh
#
# NOTE: fixture *values* (session ids, costs, snippets, row internals) are a
# point-in-time snapshot of a live, growing store, so re-running will not
# reproduce byte-identical files -- that is expected. The contract is validated
# at the ENVELOPE level (scripts/check_memory_contract.py), so snapshot drift in
# the row internals does not break conformance. Re-run this after any change to
# the envelope or to the discovery result rows, then re-run the checker.
set -u

SLUG="${SU_SLUG:--Users-yadkonrad-dev-dev-year26-jan26-StackUnderflow}"
FILE="${SU_FILE:-/Users/yadkonrad/dev_dev/year26/jan26/StackUnderflow/python-legacy: cli.py}"
EMPTY_PATH="${SU_EMPTY_PATH:-/opt/stackunderflow-contract-empty-zzz}"
NOMATCH="zqxjklmnopurpleelephantxyzzy"  # a query no real session contains
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# capture <fixture-name> <memory subcommand and args...>
# Writes stdout to <fixture-name>.json. Error fixtures exit non-zero by design
# (a non-zero exit means stdout is the {"error": ...} envelope), so the exit
# code is reported but never aborts the run.
capture() {
  local name="$1"; shift
  stackunderflow memory "$@" --json >"$DIR/$name.json"
  echo "  $name.json  (exit $?)"
}

echo "Regenerating fixtures into $DIR"

capture decisions.success decisions "retry"    --project "$SLUG" --limit 2
capture decisions.empty   decisions "$NOMATCH" --project "$SLUG" --limit 2
capture decisions.error   decisions "retry"    --project "$SLUG" --since notadate

capture file.success file "$FILE"                                   --limit 2
capture file.empty   file "/tmp/stackunderflow-contract-nonexistent-zzz.py" --limit 2
capture file.error   file "$FILE"                                   --since notadate

capture worked.success worked "npm run build" --project "$SLUG" --limit 2
capture worked.empty   worked "$NOMATCH"       --project "$SLUG" --limit 2
capture worked.error   worked "npm run build" --project "$SLUG" --since notadate

capture sessions.success sessions              --project "$SLUG" --limit 2
capture sessions.empty   sessions "$EMPTY_PATH"                --limit 2
capture sessions.error   sessions              --project "$SLUG" --since notadate

capture ask.success ask "how does caching work" --project "$SLUG" --limit 2
capture ask.empty   ask "$NOMATCH"              --project "$SLUG" --limit 2
capture ask.error   ask "how does caching work" --project "$SLUG" --since notadate

echo "Done. Validate with: python scripts/check_memory_contract.py"
