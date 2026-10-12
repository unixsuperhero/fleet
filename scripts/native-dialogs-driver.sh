#!/usr/bin/env bash
# Drives real Fleet file panels. Never substitutes picker results or alters the scenario.
set -euo pipefail
umask 077
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
die() { printf '%s\n' "$*" >&2; exit 1; }
[[ $# -ge 2 && $# -le 3 ]] || die 'Usage: bash scripts/native-dialogs-driver.sh /absolute/debug/Fleet.app /absolute/isolated-profile [/absolute/isolated-profile/native-dialog-sandbox/backup.sqlite3]'
APP="$1"
PROFILE="$2"
OUTPUT="${3:-$PROFILE/native-dialog-sandbox/round-trip.sqlite3}"
TIMEOUT="${FLEET_SMOKE_TIMEOUT:-300}"
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ ]] || die 'FLEET_SMOKE_TIMEOUT must be a positive number of seconds.'
[[ "$APP" == /* && -x "$APP/Contents/MacOS/fleet" && "$PROFILE" == /* && "$OUTPUT" == /* ]] || die 'App, isolated profile, and output must be absolute paths.'
[[ ! -L "$PROFILE" ]] || die 'The isolated profile must not be a symlink.'
[[ -f "$ROOT/scripts/native-dialog-smoke.js" ]] || die 'The existing dialog scenario is missing.'
codesign --verify --deep --strict "$APP" >/dev/null 2>&1 || die 'The selected app must have a valid signature.'
[[ "$(osascript -e 'tell application "System Events" to UI elements enabled' 2>/dev/null)" == true ]] || die 'Accessibility is unavailable to this caller. No permissions were changed.'

# Keep all native file selections inside one deterministic isolated-profile sandbox.
python3 -I - "$PROFILE" "$OUTPUT" <<'PY'
import os, pathlib, sys
profile, output = map(pathlib.Path, sys.argv[1:])
profile = profile.resolve()
if profile == pathlib.Path('/') or profile == pathlib.Path.home():
    raise SystemExit('Refusing a root/home profile.')
if profile == pathlib.Path.home() / 'Library' or pathlib.Path.home() / 'Library' in profile.parents:
    raise SystemExit('Select an isolated profile outside the user Library.')
if (profile / 'smoke-proof.json').exists():
    raise SystemExit('Archive the previous smoke-proof.json before rerunning.')
sandbox = profile / 'native-dialog-sandbox'
if sandbox.is_symlink():
    raise SystemExit('The native dialog sandbox must not be a symlink.')
if output.parent.resolve() != sandbox or output.suffix != '.sqlite3':
    raise SystemExit('Output must be a .sqlite3 file directly inside the isolated profile native-dialog-sandbox.')
if output.exists() or output.is_symlink():
    raise SystemExit('Output already exists; refusing an overwrite confirmation.')
profile.mkdir(parents=True, exist_ok=True, mode=0o700)
os.chmod(profile, 0o700)
sandbox.mkdir(mode=0o700, exist_ok=True)
os.chmod(sandbox, 0o700)
PY
APP="$(python3 -I -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$APP")"
PROFILE="$(python3 -I -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$PROFILE")"
OUTPUT="$(python3 -I -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$OUTPUT")"
FOLDER="${OUTPUT%/*}"
FILENAME="${OUTPUT##*/}"
WORK="$(mktemp -d /tmp/fleet-dialog-driver.XXXXXX)"
LOG="$PROFILE/native-dialog-driver.log"
RUNNER=''
PID=''
IDENTITY=''
RUNNER_DONE=0
note() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*" | tee -a "$LOG" >&2; }
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  # An unreaped direct child cannot have its PID reused. The app may have been
  # reaped by the runner, so compare its captured executable/start identity.
  if [[ -n "$PID" && -n "$IDENTITY" ]]; then
    current="$(ps -p "$PID" -o lstart=,comm= 2>/dev/null || true)"
    if [[ "$current" == "$IDENTITY" ]]; then kill -TERM "$PID" 2>/dev/null || true; fi
  fi
  if [[ -n "$RUNNER" && "$RUNNER_DONE" -eq 0 ]]; then
    kill -TERM "$RUNNER" 2>/dev/null || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$RUNNER" 2>/dev/null || break
      sleep 0.2
    done
    kill -KILL "$RUNNER" 2>/dev/null || true
    wait "$RUNNER" 2>/dev/null || true
  fi
  if [[ -n "$PID" && -n "$IDENTITY" ]]; then
    current="$(ps -p "$PID" -o lstart=,comm= 2>/dev/null || true)"
    if [[ "$current" == "$IDENTITY" ]]; then kill -KILL "$PID" 2>/dev/null || true; fi
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cat > "$WORK/gui.applescript" <<'AS'
property remaining : 0

on skipped(r, nativeOnly)
  if r is in {"AXOutline", "AXTable", "AXList", "AXBrowser", "AXRow", "AXColumn", "AXCell", "AXMenu", "AXMenuBar", "AXImage"} then return true
  if nativeOnly and r is in {"AXWebArea", "AXScrollArea"} then return true
  return false
end skipped

on walk(node, depth, wanted, mode, nativeOnly)
  set remaining to remaining - 1
  if remaining < 0 then error "AX search exceeded its bounded node count"
  tell application "System Events"
    try
      set r to role of node
    on error
      return missing value
    end try
    if mode is "button" and r is "AXButton" then
      try
        if name of node is wanted then return node
      end try
    else if mode is "text" then
      if r is in {"AXStaticText", "AXWindow", "AXSheet", "AXHeading"} then
        try
          if (value of node as text) contains wanted then return node
        end try
        try
          if (name of node as text) contains wanted then return node
        end try
      end if
    else if mode is "filename" and r is "AXTextField" then
      set labelText to ""
      try
        set labelText to description of node as text
      end try
      if labelText is in {"Save As:", "Save As", "Name:", "Name", "File name:", "File name"} then return node
      try
        set v to value of node as text
        -- The Rust export handler explicitly initializes this native name field.
        if v starts with "fleet-" and v ends with ".sqlite3" and v does not contain "/" then return node
      end try
    end if
    if depth >= 20 or my skipped(r, nativeOnly) then return missing value
    try
      set kids to UI elements of node
    on error
      return missing value
    end try
    repeat with child in kids
      set found to my walk(child, depth + 1, wanted, mode, nativeOnly)
      if found is not missing value then return found
    end repeat
  end tell
  return missing value
end walk

on find(node, wanted, mode, nativeOnly)
  set remaining to 4000
  return my walk(node, 0, wanted, mode, nativeOnly)
end find

on matchesPanel(node, heading, primary)
  if my find(node, heading, "text", true) is missing value then return false
  if my find(node, primary, "button", true) is missing value then return false
  if my find(node, "Cancel", "button", true) is missing value then return false
  return true
end matchesPanel

on panelBelow(node, depth, heading, primary)
  tell application "System Events"
    try
      set r to role of node
    on error
      return missing value
    end try
    if depth < 12 and not my skipped(r, true) then
      try
        set kids to UI elements of node
      on error
        set kids to {}
      end try
      -- Descendants first: use the actual sheet instead of its main-window ancestor.
      repeat with child in kids
        set found to my panelBelow(child, depth + 1, heading, primary)
        if found is not missing value then return found
      end repeat
    end if
    if r is in {"AXSheet", "AXWindow"} then
      if my matchesPanel(node, heading, primary) then return node
    end if
  end tell
  return missing value
end panelBelow

on owned(ownedPid)
  tell application "System Events"
    set candidates to every application process whose unix id is ownedPid
    if (count of candidates) is not 1 then error "Owned Fleet process is not available"
    set p to item 1 of candidates
    if unix id of p is not ownedPid then error "AX process identity mismatch"
    return p
  end tell
end owned

on foreground(p, ownedPid)
  tell application "System Events"
    if unix id of p is not ownedPid then error "AX process identity mismatch"
    set frontmost of p to true
    delay 0.1
    set actualFront to first application process whose frontmost is true
    if unix id of actualFront is not ownedPid then error "Owned Fleet is not frontmost; refusing keys"
  end tell
end foreground

on ready(p, noticeText, heading, primary)
  if my find(p, noticeText, "text", false) is missing value then return missing value
  tell application "System Events"
    repeat with w in windows of p
      set panel to my panelBelow(w, 0, heading, primary)
      if panel is not missing value then return panel
    end repeat
  end tell
  return missing value
end ready

on focusedText(p)
  tell application "System Events"
    set focusedElement to value of attribute "AXFocusedUIElement" of p
    if role of focusedElement is not in {"AXTextField", "AXComboBox"} then error "Native Go-to-folder text control is not focused"
    return focusedElement
  end tell
end focusedText

on goToFolder(p, ownedPid, folder)
  my foreground(p, ownedPid)
  tell application "System Events" to keystroke "g" using {command down, shift down}
  set untilTime to (current date) + 10
  repeat
    if my find(p, "Go", "button", true) is not missing value then exit repeat
    if (current date) > untilTime then error "Native Go-to-folder control did not appear"
    delay 0.1
  end repeat
  my focusedText(p)
  my foreground(p, ownedPid)
  tell application "System Events" to keystroke "a" using {command down}
  my focusedText(p)
  my foreground(p, ownedPid)
  tell application "System Events" to keystroke folder
  delay 0.2
  if my find(p, "Go", "button", true) is missing value then error "Native Go-to-folder control disappeared before navigation"
  my focusedText(p)
  my foreground(p, ownedPid)
  tell application "System Events" to key code 36
  set untilTime to (current date) + 10
  repeat
    if my find(p, "Go", "button", true) is missing value then exit repeat
    if (current date) > untilTime then error "Native Go-to-folder navigation did not finish"
    delay 0.1
  end repeat
end goToFolder

on run argv
  set ownedPid to item 1 of argv as integer
  set noticeText to item 2 of argv
  set heading to item 3 of argv
  set primary to item 4 of argv
  set actionName to item 5 of argv
  set folder to item 6 of argv
  set filename to item 7 of argv
  set untilTime to (current date) + (item 8 of argv as integer)
  repeat
    set p to my owned(ownedPid)
    set panel to my ready(p, noticeText, heading, primary)
    if panel is not missing value then exit repeat
    if (current date) > untilTime then error "Expected phase text and native heading/buttons were not found"
    delay 0.2
  end repeat
  my foreground(p, ownedPid)
  if actionName is "save" or actionName is "open" then
    -- The real panel and phase were validated before the first key event.
    my goToFolder(p, ownedPid, folder)
    set panel to my ready(p, noticeText, heading, primary)
    if panel is missing value then error "Expected native panel missing after folder navigation"
    tell application "System Events"
      if actionName is "save" then
        set field to my find(panel, "", "filename", true)
        if field is missing value then error "Native Save As field was not identified"
        set value of field to filename
        if (value of field as text) is not filename then error "Native Save As filename did not match"
      else
        -- Select the exact file through the actual Open panel. Command-shift-G
        -- accepts an existing full file path and leaves that file selected.
        my goToFolder(p, ownedPid, folder & "/" & filename)
      end if
    end tell
  end if
  set untilTime to (current date) + 10
  repeat
    set p to my owned(ownedPid)
    set panel to my ready(p, noticeText, heading, primary)
    if panel is missing value then error "Expected phase/native panel changed before action"
    set buttonName to primary
    if actionName is "cancel" then set buttonName to "Cancel"
    set b to my find(panel, buttonName, "button", true)
    tell application "System Events"
      if b is not missing value then
        if enabled of b then
          my foreground(p, ownedPid)
          if unix id of p is not ownedPid then error "AX process identity mismatch"
          click b
          return "answered"
        end if
      end if
    end tell
    if (current date) > untilTime then error "Expected native button remained disabled"
    delay 0.2
  end repeat
end run
AS

# Keep the runner's full output private; print only stage names and evidence paths.
FLEET_SMOKE_TIMEOUT="$TIMEOUT" bash "$ROOT/scripts/native-smoke.sh" \
  "$APP" "$PROFILE" "$ROOT/scripts/native-dialog-smoke.js" \
  > "$PROFILE/native-dialog-runner.log" 2>&1 &
RUNNER=$!
DEADLINE=$((SECONDS + TIMEOUT))
while [[ -z "$PID" ]]; do
  kill -0 "$RUNNER" 2>/dev/null || die 'The native runner exited before Fleet could be identified.'
  [[ $SECONDS -lt $DEADLINE ]] || die 'Timed out identifying owned Fleet.'
  for candidate in $(pgrep -P "$RUNNER" 2>/dev/null || true); do
    executable="$(ps -p "$candidate" -o comm= 2>/dev/null || true)"
    executable="${executable#"${executable%%[![:space:]]*}"}"
    if [[ "$executable" == "$APP/Contents/MacOS/fleet" ]]; then
      PID="$candidate"
      IDENTITY="$(ps -p "$PID" -o lstart=,comm=)"
      break
    fi
  done
  [[ -n "$PID" ]] || sleep 0.1
 done
note "Driving owned Fleet PID $PID"

# phase notice fragment | native heading | primary button | action
STEPS=(
  'Cancellation proof: press Cancel in the actual native panel for choose_backup_directory.|Choose Fleet backup directory|Open|cancel'
  'Cancellation proof: press Cancel in the actual native panel for export_backup.|Export Fleet metadata backup|Save|cancel'
  'Cancellation proof: press Cancel in the actual native panel for choose_restore.|Select Fleet metadata backup to preview|Open|cancel'
  'Cancellation proof: press Cancel in the actual native panel for export_diagnostics.|Export Fleet diagnostics|Save|cancel'
  'Save a real Fleet SQLite backup|Export Fleet metadata backup|Save|save'
  'Select the exported backup in the native Open panel:|Select Fleet metadata backup to preview|Open|open'
  'Then press Cancel in the native Replace metadata confirmation.|Replace Fleet metadata?|Replace metadata|cancel'
  'This preview will be canceled without a database write.|Select Fleet metadata backup to preview|Open|open'
  'Press Replace metadata in the native confirmation.|Select Fleet metadata backup to preview|Open|open'
  'Press Replace metadata in the native confirmation.|Replace Fleet metadata?|Replace metadata|approve'
)
index=0
for step in "${STEPS[@]}"; do
  index=$((index + 1))
  kill -0 "$RUNNER" 2>/dev/null || die "Runner exited before native step $index. See private runner log."
  left=$((DEADLINE - SECONDS))
  [[ "$left" -gt 0 ]] || die "Timed out before native step $index."
  IFS='|' read -r notice_text heading primary action <<< "$step"
  note "Step $index: $action; expected native heading '$heading'"
  if ! osascript "$WORK/gui.applescript" "$PID" "$notice_text" "$heading" \
    "$primary" "$action" "$FOLDER" "$FILENAME" "$left" \
    > "$PROFILE/native-dialog-ax-result.log" 2> "$PROFILE/native-dialog-ax-error.log"; then
    die "Native step $index failed. See private AX error log; no subsequent consent was sent."
  fi
 done
note 'All ten real native actions answered; waiting for scenario assertions and owned-window capture.'
status=0
wait "$RUNNER" || status=$?
RUNNER_DONE=1
[[ "$status" -eq 0 ]] || die 'Native proof or owned-window screenshot failed. See private runner log.'
python3 -I - "$PROFILE/smoke-proof.json" "$OUTPUT" <<'PY'
import json, pathlib, sys
proof_path, output = sys.argv[1:]
with open(proof_path) as f:
    proof = json.load(f)
if proof.get('native') is not True or proof.get('status') != 'passed' or proof.get('scenario') != 'native-dialogs-and-staged-restore':
    raise SystemExit('The actual native dialog scenario did not pass.')
rows = [r for r in proof.get('observations', []) if r.get('behavior') == 'native export, immutable preview, explicit committed restore']
if len(rows) != 1 or rows[0].get('exportedPath') != output or rows[0].get('preview', {}).get('path') != output:
    raise SystemExit('The real native export/restore did not use the requested sandbox path.')
with pathlib.Path(output).open('rb') as f:
    if f.read(16) != b'SQLite format 3\x00':
        raise SystemExit('The actual selected output is not a SQLite backup.')
PY
[[ -s "$PROFILE/native-screenshot.png" ]] || die 'Owned-window image is missing.'
note 'Native dialog scenario passed; real backup and requested export/restore path verified.'
printf 'Proof: %s\nNative image: %s\nBackup: %s\n' \
  "$PROFILE/smoke-proof.json" "$PROFILE/native-screenshot.png" "$OUTPUT"
