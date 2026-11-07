#!/usr/bin/env bash
# Done checks (blackboard C1-C4). Usage: scripts/check.sh [--ignored]
# Lanes: do NOT run this whole script; run targeted `cargo test -p <crate> <filter>` instead.
set -uo pipefail
cd "$(dirname "$0")/.."
fail=0
run() { echo "== $*"; "$@" || { echo "!! FAILED: $*"; fail=1; }; }
run cargo fmt --all --check
run cargo clippy --workspace --all-targets -- -D warnings
run cargo test --workspace
if [[ "${1:-}" == "--ignored" ]]; then
  # Real-system tests: systemd --user transient units, real sockets. Run serially.
  run cargo test --workspace -- --ignored --test-threads=1
fi
exit $fail
