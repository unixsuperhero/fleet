#!/usr/bin/env bash
# Drives the native macOS dialogs of scripts/native-setup-smoke.js through Accessibility.
# Run inside the fixture so FLEET_SMOKE_FIXTURE_MANIFEST and FLEET_PROFILE_DIR exist:
#   FLEET_SMOKE_SETUP=1 bash scripts/smoke-ssh-fixture.sh -- \
#     bash scripts/native-setup-dialogs.sh /abs/debug/Fleet.app [/abs/tuios-inbox]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="${1:-}"
CHECKOUT="${2:-$HOME/proj/tuios-inbox}"
MANIFEST="${FLEET_SMOKE_FIXTURE_MANIFEST:-}"
PROFILE="${FLEET_PROFILE_DIR:-}"
die() { printf '%s\n' "$*" >&2; exit 1; }
[[ "$APP" == /* && -x "$APP/Contents/MacOS/fleet" && "$CHECKOUT" == /* && "$MANIFEST" == /* && -f "$MANIFEST" && "$PROFILE" == /* ]] \
  || die 'Usage: FLEET_SMOKE_SETUP=1 bash scripts/smoke-ssh-fixture.sh -- bash scripts/native-setup-dialogs.sh /abs/Fleet.app [/abs/tuios-inbox]'
[[ -f "$CHECKOUT/server.mjs" && -d "$CHECKOUT/public" ]] || die "Checkout needs server.mjs and public/: $CHECKOUT"
[[ "$(osascript -e 'tell application "System Events" to UI elements enabled' 2>/dev/null)" == true ]] || die 'macOS Accessibility is not enabled for this terminal. This script changes no permissions.'

WORK="$(mktemp -d /tmp/fleet-setup-driver.XXXXXX)"
SCENARIO="$WORK/scenario.js"
LOG="$PROFILE/native-setup-driver.log"
python3 -I - "$MANIFEST" "$CHECKOUT" "$ROOT/scripts/native-setup-smoke.js" "$SCENARIO" <<'PY'
import json, os, sys
manifest, checkout, source, out = sys.argv[1:]
with open(manifest) as f:
    fixture = json.load(f)
if 'setup' not in fixture:
    raise SystemExit('Manifest has no setup section; start the fixture with FLEET_SMOKE_SETUP=1.')
fixture.update(checkout=os.path.realpath(checkout), uid=os.getuid())
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as f:
    f.write('window.fleetSetupFixture = ' + json.dumps(fixture) + ';\n')
    with open(source) as s:
        f.write(s.read())
PY

swiftc "$ROOT/scripts/native-setup-ax.swift" -o "$WORK/native-ax"

FLEET_SMOKE_TIMEOUT="${FLEET_SMOKE_TIMEOUT:-900}" bash "$ROOT/scripts/native-smoke.sh" "$APP" "$PROFILE" "$SCENARIO" &
RUNNER=$!
trap 'kill -TERM "$RUNNER" 2>/dev/null || true; rm -rf "$WORK"' EXIT
gui() {
  local t=$SECONDS rc=0 out
  out="$("$WORK/native-ax" "$PID" "$@" 2>&1)" || rc=$?
  [[ $((SECONDS - t)) -lt 5 ]] || printf '%s slow gui %s: %ss rc=%s\n' "$(date -u +%FT%TZ)" "$1" "$((SECONDS - t))" "$rc" >> "$LOG"
  printf '%s' "$out"; return "$rc"
}
note() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" | tee -a "$LOG" >&2; }

PID=""
for _ in $(seq 1 150); do
  PID="$(pgrep -P "$RUNNER" -f "$APP/Contents/MacOS/fleet" | head -n 1 || true)"
  [[ -n "$PID" ]] && break
  kill -0 "$RUNNER" 2>/dev/null || die 'native-smoke.sh exited before the app started.'
  sleep 0.2
done
[[ -n "$PID" ]] || die 'Owned Fleet pid not found.'
note "driving owned Fleet pid $PID"
PROOF="${FLEET_PROOF_DIR:-/tmp/fleet-native-setup-proof-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$PROOF"
# Foreground the owned app and keep a screenshot as evidence before any dialog is answered.
front=""
for _ in $(seq 1 20); do
  front="$("$WORK/native-ax" "$PID" front 2>&1 || true)"
  grep -q 'frontmost=true' <<<"$front" && break
  sleep 0.5
done
note "$front"
grep -q 'frontmost=true' <<<"$front" || die "Owned Fleet is not frontmost: $front"
printf '%s\n' "$front" > "$PROOF/frontmost.txt"
screencapture -x "$PROOF/fleet-frontmost.png" || die 'screencapture failed (Screen Recording permission?)'
note "screenshot: $PROOF/fleet-frontmost.png"

# kind|expected-heading-regex|button. Confirms are approved only when the actual heading matches.
STEPS=(
  'picker||Cancel'
  'picker||Choose'
  'confirm|^Confirm setup: Inspect |Cancel'
  'confirm|^Confirm setup: Inspect |Run setup'
  'confirm|^Confirm setup: Start TUIOS daemon on |Run setup'
  'confirm|^Confirm setup: Start TUIOS daemon on |Run setup'
  'confirm|^Confirm setup: Preview capture for |Run setup'
)
index=0
DEADLINE=$((SECONDS + ${FLEET_SMOKE_TIMEOUT:-900}))
while [[ $index -lt ${#STEPS[@]} ]]; do
  kill -0 "$RUNNER" 2>/dev/null || die "Scenario ended before native step $((index + 1)) of ${#STEPS[@]}; see $PROFILE/native-smoke.log."
  [[ $SECONDS -lt $DEADLINE ]] || die 'Timed out waiting for native dialogs.'
  t0=$SECONDS
  info="$(gui info)" || die "Native GUI observation failed: $info"
  [[ $((SECONDS - t0)) -lt 5 ]] || note "slow Accessibility read: $((SECONDS - t0))s"
  if [[ -z "$info" ]]; then sleep 0.5; continue; fi
  IFS='|' read -r kind heading button <<<"${STEPS[$index]}"
  if [[ "$kind" == picker ]]; then
    grep -qx 'button:Open' <<<"$info" || die "Expected a native Open panel at step $((index + 1)); saw: $(tr '\n' '|' <<<"$info")"
    if [[ "$button" == Cancel ]]; then
      note "step $((index + 1)): canceling native folder picker"
      out="$(gui click Cancel)" || die "Could not click Cancel: $out"
    else
      note "step $((index + 1)): choosing $CHECKOUT"
      out="$(gui goto "$CHECKOUT")" || die "Go-to-folder failed: $out"
      for _ in $(seq 1 10); do sleep 1; out="$(gui click Open)" && break; done
      [[ "$out" == clicked ]] || die "Could not click Open: $out"
      note "step $((index + 1)): clicked Open"
    fi
  else
    grep -qx 'button:Run setup' <<<"$info" || die "Expected a native setup confirmation at step $((index + 1)); saw: $(tr '\n' '|' <<<"$info")"
    if ! grep -Eq "^text:${heading#^}" <<<"$info"; then
      gui click Cancel >/dev/null 2>&1 || true
      die "Refusing to approve a confirmation without heading /${heading}/: $(tr '\n' '|' <<<"$info")"
    fi
    note "step $((index + 1)): $button on '$(grep -E "^text:${heading#^}" <<<"$info" | head -n 1 | cut -c1-120)'"
    out="$(gui click "$button")" || die "Could not click $button: $out"
  fi
  # Wait for the sheet to close before looking for the next one.
  for _ in $(seq 1 60); do [[ -z "$(gui info || true)" ]] && break; sleep 0.5; done
  index=$((index + 1))
done
note 'all native steps answered; waiting for the scenario proof'
wait "$RUNNER"
note "running real SSH transfer proof"
bun "$ROOT/scripts/native-setup-transfer.mjs" "$MANIFEST" "$CHECKOUT" 2>&1 | tee "$PROOF/transfer.log"
[[ "${PIPESTATUS[0]}" -eq 0 ]] || die "Transfer proof failed; see $PROOF/transfer.log"
cp "$LOG" "$PROOF/native-setup-driver.log" 2>/dev/null || true
cp "$PROFILE/native-smoke.log" "$PROOF/" 2>/dev/null || true
note "proof dir: $PROOF"
