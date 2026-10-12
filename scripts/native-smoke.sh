#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [[ $# -lt 2 || $# -gt 3 ]]; then printf '%s\n' 'Usage: bash scripts/native-smoke.sh /absolute/debug/Fleet.app /absolute/isolated-profile [/absolute/scenario.js]' >&2; exit 2; fi
APP="$1"
PROFILE="$2"
SCRIPT="${3:-$ROOT/scripts/native-smoke.js}"
[[ "$APP" == /* && -x "$APP/Contents/MacOS/fleet" && "$PROFILE" == /* && "$SCRIPT" == /* && -f "$SCRIPT" ]] || { printf '%s\n' 'Provide absolute app, isolated profile, and scenario paths.' >&2; exit 2; }
[[ ! -L "$PROFILE" ]] || { printf '%s\n' 'The profile must not be a symlink.' >&2; exit 2; }
mkdir -p "$PROFILE"
chmod 700 "$PROFILE"
[[ ! -e "$PROFILE/smoke-proof.json" ]] || { printf '%s\n' 'Archive the previous smoke-proof.json before repeating a run; stale proof is never accepted.' >&2; exit 2; }
printf '%s\n' 'Launching the real packaged WKWebView. This requires a debug bundle and an interactive macOS desktop.'
printf '%s\n' 'The dialogs scenario requires actual native-panel choices. Do not supply credentials to a smoke script.'
FLEET_PROFILE_DIR="$PROFILE" FLEET_SMOKE_SCRIPT="$SCRIPT" "$APP/Contents/MacOS/fleet" > "$PROFILE/native-smoke.log" 2>&1 &
PID=$!
trap 'kill -TERM "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true' EXIT
STATUS=0
if ! xcrun swift "$ROOT/scripts/native-capture.swift" --pid "$PID" --activate-only; then
  printf '%s\n' 'The owned app could not be activated. Inspect native-smoke.log.' >&2
  STATUS=1
fi
DEADLINE=$((SECONDS + ${FLEET_SMOKE_TIMEOUT:-300}))
while [[ "$STATUS" -eq 0 && ! -f "$PROFILE/smoke-proof.json" ]]; do
  if ! kill -0 "$PID" 2>/dev/null; then
    printf '%s\n' 'The app exited before emitting native proof. Inspect native-smoke.log.' >&2
    STATUS=1
    break
  fi
  if [[ $SECONDS -ge $DEADLINE ]]; then
    printf '%s\n' 'Native smoke timed out. A release bundle intentionally has no smoke hook.' >&2
    STATUS=1
    break
  fi
  sleep 0.2
done
if [[ -f "$PROFILE/smoke-proof.json" ]]; then
  python3 - "$PROFILE/smoke-proof.json" <<'PY' || STATUS=1
import json, sys
with open(sys.argv[1]) as f:
    result = json.load(f)
print(json.dumps(result, indent=2))
if result.get('status') != 'passed' or result.get('native') is not True:
    raise SystemExit('The native scenario did not pass.')
PY
  printf 'Proof: %s\n' "$PROFILE/smoke-proof.json"
else
  printf '%s\n' 'No native scenario proof was produced.' >&2
  STATUS=1
fi
if ! xcrun swift "$ROOT/scripts/native-capture.swift" --pid "$PID" --output "$PROFILE/native-screenshot.png"; then
  printf '%s\n' 'Owned-window image capture failed. Inspect the capture error and Screen Recording permission; visual proof remains missing.' >&2
  STATUS=1
elif [[ ! -s "$PROFILE/native-screenshot.png" ]]; then
  printf '%s\n' 'Native screenshot was not produced.' >&2
  STATUS=1
else
  printf 'Native image: %s\n' "$PROFILE/native-screenshot.png"
fi
printf '%s\n' 'Only scenarios recorded in the proof ran. This does not prove public distribution or process survival.'
exit "$STATUS"
