#!/usr/bin/env bash
# Done checks (blackboard C1-C4). Usage: scripts/check.sh [--ignored]
# Lanes: do NOT run this whole script; run targeted `cargo test -p <crate> <filter>` instead.
set -uo pipefail
cd "$(dirname "$0")/.."
# The GUI's headless view tests render with tiny-skia (no GPU).
export ICED_TEST_BACKEND="${ICED_TEST_BACKEND:-tiny-skia}"
fail=0
run() { echo "== $*"; "$@" || { echo "!! FAILED: $*"; fail=1; }; }
run cargo fmt --all --check
run cargo clippy --workspace --all-targets -- -D warnings
run cargo test --workspace
if [[ "${1:-}" == "--ignored" ]]; then
  # Real-system tests: systemd --user transient units, real sockets. Run serially.
  # Set LANTERN_TEST_CADDY to a verified upstream Caddy (>= 2.11) to include the real-Caddy e2e.
  skip=(--skip single_instance_and_tray_on_private_bus)
  if [[ -z "${LANTERN_TEST_CADDY:-}" ]]; then
    echo "!! LANTERN_TEST_CADDY unset: skipping the real-Caddy e2e"
    skip+=(--skip app_caddy_real_system_https_reload_recovery_and_removal)
  fi
  run cargo test --workspace -- --ignored --test-threads=1 "${skip[@]}"
  # The GUI's D-Bus test needs a private session bus, never the desktop's.
  run dbus-run-session -- env LANTERN_GUI_TEST_PRIVATE_BUS=1 \
    cargo test -p lantern-gui -- --ignored single_instance_and_tray_on_private_bus
fi
exit $fail
