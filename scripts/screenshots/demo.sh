#!/usr/bin/env bash
# Start (or stop) an isolated Lantern daemon seeded with generic demo projects, for README screenshots.
#
#   scripts/screenshots/demo.sh start   # build first: (cd ui && npm ci && npm run build) && cargo build --release -p lantern
#   scripts/screenshots/demo.sh stop
#
# Everything lives under $LANTERN_SHOT_ROOT (default /tmp/lantern-shots) with its own HOME, config/state
# dirs, daemon port (4790), proxy ports and DNS port. Demo projects are created in $LANTERN_DEMO_DIR
# (default /tmp/demo). The daemon uses Lantern's default systemd unit names (lantern-inst-*, lantern-cmd-*),
# so `start` refuses to run if any such user units already exist (i.e. a real Lantern is installed and active).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
ROOT="${LANTERN_SHOT_ROOT:-/tmp/lantern-shots}"
DEMO="${LANTERN_DEMO_DIR:-/tmp/demo}"
BROKEN="${LANTERN_BROKEN_DIR:-/tmp/demo-broken}"
BIN="${LANTERN_BIN:-$REPO/target/release/lantern}"
PORT="${LANTERN_SHOT_PORT:-4790}"

write_env() {
  cat > "$ROOT/env.sh" <<EOF
export HOME="$ROOT/home"
export LANTERN_CONFIG_DIR="$ROOT/config"
export LANTERN_STATE_DIR="$ROOT/state"
export LANTERN_DATA_DIR="$ROOT/data"
export LANTERN_CACHE_DIR="$ROOT/cache"
export LANTERN_RUNTIME_DIR="$ROOT/run"
export LANTERN_DAEMON_URL="http://127.0.0.1:$PORT"
export PATH="$(dirname "$BIN"):\$PATH"
EOF
}

mkproj() { # id name description tags accent
  mkdir -p "$DEMO/$1"
  cp "$REPO/scripts/screenshots/demo-server.py" "$DEMO/$1/serve.py"
  cat > "$DEMO/$1/lantern.yaml" <<EOF
id: $1
name: $2
description: $3
kind: project
tags: [$4]
type: proxy
run:
  cmd: python3 -u serve.py --port \${PORT} --title "$2" --blurb "$3" --accent "$5"
health_endpoint: /health
EOF
}

post() { curl -fsS -X POST "http://127.0.0.1:$PORT$1" -H 'content-type: application/json' ${2:+-d "$2"} > /dev/null; }

start() {
  [[ -x "$BIN" ]] || { echo "missing $BIN (cargo build --release -p lantern)" >&2; exit 1; }
  if systemctl --user list-units 'lantern-*' --all --no-legend | grep -qE '^\S*\s*lantern-(inst|cmd|op|caddy)'; then
    echo "refusing: lantern-* user units already exist; stop them first" >&2; exit 1
  fi
  ss -ltn | grep -q ":$PORT " && { echo "port $PORT is busy" >&2; exit 1; }
  rm -rf "$ROOT" "$DEMO" "$BROKEN"
  mkdir -p "$ROOT"/{home,config,state,data,cache,run} "$DEMO" "$BROKEN"
  write_env
  cat > "$ROOT/config/config.toml" <<EOF
[daemon]
port = $PORT

[proxy]
http_port = 18080
https_port = 18443

[dns]
listen = "127.0.0.1:15354"

[workspaces]
roots = ["$DEMO"]
depth = 1

[ports]
range_start = 38000
range_end = 38099
EOF

  mkproj storefront "Storefront" "Customer-facing shop front end" "web, frontend" "#6366f1"
  mkproj admin-dashboard "Admin Dashboard" "Internal back office for orders and inventory" "web, internal" "#10b981"
  mkproj docs-site "Docs Site" "Product documentation site" "docs, static" "#f59e0b"
  mkproj orders-api "Orders API" "REST API for orders, carts and payments" "api, backend" "#ef4444"
  mkproj mailer-worker "Mailer Worker" "Queue worker that sends transactional email" "worker, backend" "#8b5cf6"
  # The worker fails on purpose so the screenshots can show a crash reason and its logs.
  cat > "$DEMO/mailer-worker/worker.py" <<'EOF'
import sys
print("mailer-worker: starting queue consumer", flush=True)
print("mailer-worker: error: cannot connect to Redis at 127.0.0.1:6379 (connection refused)", file=sys.stderr, flush=True)
sys.exit(1)
EOF
  rm "$DEMO/mailer-worker/serve.py"
  sed -i 's|^  cmd: .*|  cmd: python3 -u worker.py|; /health_endpoint/d' "$DEMO/mailer-worker/lantern.yaml"
  cat > "$DEMO/orders-api/run_tests.py" <<'EOF'
import time
for name in ["test_create_order", "test_cart_totals", "test_refund_flow", "test_inventory_sync"]:
    time.sleep(0.2); print(f"PASSED  tests/test_orders.py::{name}", flush=True)
print("4 passed in 0.9s", flush=True)
EOF
  cat > "$DEMO/orders-api/watch_jobs.py" <<'EOF'
import time
n = 0
while True:
    n += 1; print(f"job {1000+n} processed (order.created)", flush=True); time.sleep(2)
EOF
  for d in storefront orders-api; do
    (cd "$DEMO/$d" && git init -q -b main && git add -A \
      && git -c user.name=Demo -c user.email=demo@example.com commit -q -m "Initial commit" && git branch feature-checkout)
  done
  # A folder with no lantern.yaml, so the "Add tool" dialog has a suggestion to show.
  mkdir -p "$DEMO/marketing-site"
  echo '{ "name": "marketing-site", "private": true, "scripts": { "dev": "vite" }, "devDependencies": { "vite": "^6.0.0" } }' > "$DEMO/marketing-site/package.json"
  echo '<html></html>' > "$DEMO/marketing-site/index.html"
  # A manifest with a deliberate mistake for `lantern manifest check`.
  printf 'id: demo\nrun:\n  cmd: [npm, run, dev]\n' > "$BROKEN/lantern.yaml"

  # shellcheck disable=SC1091
  source "$ROOT/env.sh"
  (setsid nohup "$BIN" daemon > "$ROOT/daemon.log" 2>&1 & echo $! > "$ROOT/daemon.pid")
  for _ in $(seq 40); do curl -fsS "http://127.0.0.1:$PORT/api/projects" > /dev/null 2>&1 && break; sleep 0.5; done
  "$BIN" scan "$DEMO" > /dev/null
  "$BIN" --no-color on storefront orders-api docs-site admin-dashboard
  "$BIN" --no-color run storefront@feature-checkout
  "$BIN" on mailer-worker > /dev/null 2>&1 || true   # fails by design
  post /api/launchers '{"name":"Run API tests","cmd":"python3 run_tests.py","cwd":"'"$DEMO"'/orders-api"}'
  post /api/launchers '{"name":"Watch job queue","cmd":"python3 -u watch_jobs.py","cwd":"'"$DEMO"'/orders-api"}'
  post /api/launchers '{"name":"Seed demo data","cmd":"echo seeding 120 products && sleep 1 && echo done","cwd":"'"$DEMO"'/storefront"}'
  post /api/launchers/watch-job-queue/start
  post /api/launchers/run-api-tests/start
  sleep 3
  echo "demo daemon ready: http://127.0.0.1:$PORT  (env: $ROOT/env.sh)"
}

stop() {
  [[ -f "$ROOT/env.sh" ]] || { echo "nothing to stop"; return 0; }
  # shellcheck disable=SC1091
  source "$ROOT/env.sh"
  "$BIN" off --all -y > /dev/null 2>&1 || true
  for id in watch-job-queue run-api-tests seed-demo-data; do post "/api/launchers/$id/stop" 2>/dev/null || true; done
  [[ -f "$ROOT/daemon.pid" ]] && kill "$(cat "$ROOT/daemon.pid")" 2>/dev/null || true
  sleep 1
  systemctl --user list-units 'lantern-*' --all --no-legend | grep -vE 'lantern-dev' || true
  rm -rf "$ROOT" "$DEMO" "$BROKEN"
  echo "stopped and removed $ROOT $DEMO $BROKEN"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  *) echo "usage: $0 start|stop" >&2; exit 2 ;;
esac
