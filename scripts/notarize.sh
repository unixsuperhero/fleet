#!/usr/bin/env bash
set -euo pipefail
if [[ $# != 3 || "$1" != --explicit-public-distribution ]]; then
  printf '%s\n' 'Usage: bash scripts/notarize.sh --explicit-public-distribution /absolute/Fleet.app KEYCHAIN_PROFILE' >&2
  printf '%s\n' 'This uploads an app to Apple. Run only after separate public-distribution approval.' >&2
  exit 2
fi
[[ "$(uname -s)" == Darwin ]] || { printf '%s\n' 'Notarization requires macOS tooling.' >&2; exit 1; }
APP="$2"
PROFILE="$3"
[[ "$APP" == /* && -d "$APP" && "$APP" == *.app && -n "$PROFILE" && "$PROFILE" != -* ]] || { printf '%s\n' 'Provide an existing absolute .app path and a stored Keychain credential profile name.' >&2; exit 2; }
: "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY explicitly in the environment}"
codesign --verify --strict --deep "$APP"
SIGNATURE="$(codesign --display --verbose=4 "$APP" 2>&1)"
[[ $'\n'"$SIGNATURE"$'\n' == *$'\n'"Authority=$APPLE_SIGNING_IDENTITY"$'\n'* ]] || { printf '%s\n' 'Unexpected app signing identity.' >&2; exit 1; }
TEMP="$(mktemp -d "${TMPDIR:-/tmp}/fleet-notarize.XXXXXXXX")"
trap 'rm -rf "$TEMP"' EXIT
ARCHIVE="$TEMP/Fleet.zip"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ARCHIVE"
xcrun notarytool submit "$ARCHIVE" --keychain-profile "$PROFILE" --wait --output-format json > "$TEMP/result.json"
python3 - "$TEMP/result.json" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    result = json.load(f)
print(json.dumps(result, indent=2))
if result.get('status') != 'Accepted':
    raise SystemExit('Apple did not accept this submission; app was not stapled.')
PY
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=4 "$APP"
printf '%s\n' 'Notarization and local Gatekeeper assessment completed. Clean-profile install and the release checklist still require evidence.'
