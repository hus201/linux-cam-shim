#!/usr/bin/env bash
# Soak test: repeatedly open and close the cam-shim virtual camera.
#
# Catches EINVAL, worker crashes, and stale heartbeats on reopen.
#
# Usage:
#   # serve already running (started separately with sudo):
#   ./scripts/soak.sh
#
#   # start serve for the duration of the test:
#   sudo ./scripts/soak.sh --start-serve
#
#   ./scripts/soak.sh --iterations 100 --hold-secs 3 --verbose
#
# Requires: v4l2-ctl (v4l-utils), python3, a compatible webcam plugged in.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

ITERATIONS=50
HOLD_SECS=2
IDLE_SECS=1
WAIT_SECS=45
START_SERVE=0
DEVICE=""
CAM_SHIM=""
VERBOSE=0

SERVE_PID=""
CAM_SHIM_BIN=""
LOOPBACK_DEV=""

log() {
  printf '[soak] %s\n' "$*"
}

vlog() {
  if [[ "$VERBOSE" -eq 1 ]]; then
    log "$@"
  fi
}

die() {
  log "ERROR: $*"
  exit 1
}

usage() {
  sed -n '2,16p' "$0" | sed 's/^# \?//'
  cat <<'EOF'

Options:
  --iterations N   Open/close cycles (default: 50)
  --hold-secs N    Seconds to read from the virtual camera each cycle (default: 2)
  --idle-secs N    Seconds between close and reopen (default: 1)
  --wait-secs N    Seconds to wait for loopback after starting serve (default: 45)
  --device PATH    Virtual camera node (default: auto-detect cam-shim loopback)
  --cam-shim PATH  cam-shim binary (default: target/release/cam-shim or PATH)
  --start-serve    Start `cam-shim serve` in the background for this run
  --verbose        Print v4l2-ctl output each iteration
  -h, --help       Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --iterations) ITERATIONS="$2"; shift 2 ;;
    --hold-secs) HOLD_SECS="$2"; shift 2 ;;
    --idle-secs) IDLE_SECS="$2"; shift 2 ;;
    --wait-secs) WAIT_SECS="$2"; shift 2 ;;
    --device) DEVICE="$2"; shift 2 ;;
    --cam-shim) CAM_SHIM="$2"; shift 2 ;;
    --start-serve) START_SERVE=1; shift ;;
    --verbose) VERBOSE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
done

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

find_cam_shim_bin() {
  if [[ -n "$CAM_SHIM" ]]; then
    [[ -x "$CAM_SHIM" ]] || die "cam-shim not executable: $CAM_SHIM"
    echo "$CAM_SHIM"
    return
  fi
  if [[ -x "$ROOT_DIR/target/release/cam-shim" ]]; then
    echo "$ROOT_DIR/target/release/cam-shim"
    return
  fi
  if [[ -x "$ROOT_DIR/target/debug/cam-shim" ]]; then
    echo "$ROOT_DIR/target/debug/cam-shim"
    return
  fi
  if command -v cam-shim >/dev/null 2>&1; then
    command -v cam-shim
    return
  fi
  die "cam-shim not found — build with: cargo build --release"
}

is_cam_shim_loopback_name() {
  local name="$1"
  [[ "$name" == *"Linux Std"* || "$name" == *"Standardized"* || "$name" == *" -" ]]
}

find_loopback_device() {
  if [[ -n "$DEVICE" ]]; then
    [[ -e "$DEVICE" ]] || die "device not found: $DEVICE"
    echo "$DEVICE"
    return
  fi

  local sys name target dev
  for sys in /sys/class/video4linux/video*; do
    [[ -e "$sys" ]] || continue
    target="$(readlink "$sys" 2>/dev/null || true)"
    [[ "$target" == *"devices/virtual/video4linux"* ]] || continue
    name="$(tr -d '\0' <"$sys/name" 2>/dev/null || true)"
    if is_cam_shim_loopback_name "$name"; then
      dev="/dev/$(basename "$sys")"
      [[ -e "$dev" ]] || continue
      echo "$dev"
      return
    fi
  done

  return 1
}

serve_running() {
  "$CAM_SHIM_BIN" status --json 2>/dev/null | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except json.JSONDecodeError:
    sys.exit(1)
sys.exit(0 if data.get("serve_running") else 1)
'
}

check_heartbeat_fresh() {
  local status_json
  status_json="$("$CAM_SHIM_BIN" status --json 2>/dev/null)" || return 1

  STATUS_JSON="$status_json" python3 -c '
import json, os, sys

raw = os.environ.get("STATUS_JSON", "")
try:
    data = json.loads(raw)
except json.JSONDecodeError:
    sys.exit(0)

if not data.get("serve_running"):
    sys.exit(0)

managed = data.get("managed") or []
if not managed:
    sys.exit(0)

state_age = data.get("state_age_ms")
if state_age is not None and state_age > 30_000:
    sys.exit(0)

for cam in managed:
    if cam.get("quarantined"):
        continue
    if cam.get("heartbeat_stale"):
        serial = cam.get("serial", "?")
        age = cam.get("heartbeat_age_secs", "?")
        print(f"stale heartbeat for {serial} ({age}s ago)", file=sys.stderr)
        sys.exit(1)

sys.exit(0)
'
}

cleanup() {
  if [[ -n "$SERVE_PID" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
    vlog "stopping serve (pid $SERVE_PID)"
    kill "$SERVE_PID" 2>/dev/null || true
    wait "$SERVE_PID" 2>/dev/null || true
  fi
}

trap cleanup EXIT

start_serve_if_requested() {
  if [[ "$START_SERVE" -eq 0 ]]; then
    return
  fi

  if serve_running; then
    log "serve already running — not starting a second instance"
    return
  fi

  if [[ "$(id -u)" -ne 0 ]]; then
    die "--start-serve requires root (run: sudo $0 ...)"
  fi

  log "starting cam-shim serve"
  "$CAM_SHIM_BIN" serve >/tmp/cam-shim-soak-serve.log 2>&1 &
  SERVE_PID=$!
  sleep 1
  kill -0 "$SERVE_PID" 2>/dev/null || die "serve exited immediately — see /tmp/cam-shim-soak-serve.log"
}

wait_for_loopback() {
  local deadline=$((SECONDS + WAIT_SECS))
  while (( SECONDS < deadline )); do
    if LOOPBACK_DEV="$(find_loopback_device)"; then
      log "using loopback device: $LOOPBACK_DEV"
      return
    fi
    sleep 1
  done
  die "no cam-shim loopback appeared within ${WAIT_SECS}s (is a compatible webcam plugged in?)"
}

verify_serve_alive() {
  if [[ "$START_SERVE" -eq 1 && -n "$SERVE_PID" ]]; then
    kill -0 "$SERVE_PID" 2>/dev/null || die "serve process died (see /tmp/cam-shim-soak-serve.log)"
  elif ! serve_running; then
    die "cam-shim serve is not running"
  fi
}

open_virtual_camera() {
  local dev="$1"
  local output
  local rc=0
  local deadline=$((SECONDS + 5))

  # Stream directly — avoid a separate `v4l2-ctl --all` probe that opens/closes
  # the device before stream-mmap runs.
  while (( SECONDS < deadline )); do
    set +e
    output="$(timeout --signal=INT "${HOLD_SECS}s" \
      v4l2-ctl -d "$dev" --stream-mmap --stream-count=10000 2>&1)"
    rc=$?
    set -e

    if [[ "$VERBOSE" -eq 1 && -n "$output" ]]; then
      printf '%s\n' "$output"
    fi

    if printf '%s' "$output" | grep -qiE 'unsupported stream type|cannot open device|failed to open'; then
      sleep 0.1
      continue
    fi

    case "$rc" in
      0|124|130) return 0 ;;
      *)
        log "v4l2-ctl failed (exit $rc): $output"
        return "$rc"
        ;;
    esac
  done

  die "timed out waiting for Video Capture on $dev"
}

run_iteration() {
  local i="$1"

  if ! LOOPBACK_DEV="$(find_loopback_device)"; then
    die "no cam-shim loopback device found (serve may have restarted the worker)"
  fi

  [[ -e "$LOOPBACK_DEV" ]] || die "loopback device disappeared: $LOOPBACK_DEV"

  vlog "iteration $i: opening $LOOPBACK_DEV for ${HOLD_SECS}s"
  open_virtual_camera "$LOOPBACK_DEV" || die "virtual camera open failed on iteration $i"

  vlog "iteration $i: idle ${IDLE_SECS}s"
  sleep "$IDLE_SECS"

  verify_serve_alive
  if ! check_heartbeat_fresh; then
    if [[ "$VERBOSE" -eq 1 ]]; then
      log "status snapshot:"
      "$CAM_SHIM_BIN" status --json 2>/dev/null | sed 's/^/[soak]   /' || true
    fi
    die "heartbeat check failed after iteration $i"
  fi
}

main() {
  require_cmd v4l2-ctl
  require_cmd python3
  require_cmd timeout

  CAM_SHIM_BIN="$(find_cam_shim_bin)"
  log "cam-shim: $CAM_SHIM_BIN"

  start_serve_if_requested

  if ! serve_running && [[ "$START_SERVE" -eq 0 ]]; then
    die "cam-shim serve is not running — start it first or use --start-serve"
  fi

  wait_for_loopback

  log "running $ITERATIONS cycles (hold ${HOLD_SECS}s, idle ${IDLE_SECS}s)"
  local i
  for (( i = 1; i <= ITERATIONS; i++ )); do
    if (( i == 1 || i % 10 == 0 || i == ITERATIONS )); then
      log "iteration $i/$ITERATIONS"
    fi
    run_iteration "$i"
  done

  log "PASS — $ITERATIONS cycles completed on $LOOPBACK_DEV"
}

main "$@"
