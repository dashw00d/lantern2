#!/usr/bin/env bash
# Captures the native (iced) GUI under Xvfb with a private D-Bus session, against the demo daemon from demo.sh.
# Needs: xvfb-run, dbus-run-session, ffmpeg. Writes docs/screenshots/gui.png (cropped above the "Other listeners" list,
# which would list the host's real listeners).
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
ROOT="${LANTERN_SHOT_ROOT:-/tmp/lantern-shots}"
OUT="${SHOT_OUT:-$REPO/docs/screenshots}"
mkdir -p "$OUT"
export ROOT OUT
# Output goes to a log: D-Bus-activated helpers inherit stdout and would keep a calling pipe open.
timeout 90 xvfb-run -a -s "-screen 0 1000x700x24" dbus-run-session -- bash -c '
  source "$ROOT/env.sh"
  export XDG_RUNTIME_DIR="$ROOT/xrun"; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
  export LANTERN_GUI_BUS_NAME=dev.lantern.ScreenshotGui
  unset WAYLAND_DISPLAY
  lantern gui --foreground > "$ROOT/gui.log" 2>&1 &
  gui=$!
  sleep 8
  ffmpeg -loglevel error -y -f x11grab -draw_mouse 0 -video_size 1000x700 -i "$DISPLAY" -frames:v 1 "$ROOT/gui-full.png"
  kill $gui 2>/dev/null || true
  ffmpeg -loglevel error -y -i "$ROOT/gui-full.png" -vf "crop=1000:622:0:0" "$OUT/gui.png"
' > "$ROOT/gui-run.log" 2>&1
echo "wrote $OUT/gui.png"
