#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# Local configuration takes precedence over inherited shell values.
[[ ! -f "$ROOT/.env" ]] || source "$ROOT/.env"
export FLEET_NAS_DNS FLEET_STUDIO_DNS FLEET_NAS_IPV4 FLEET_NAS_IPV6
MODE=release
ARGS=()
if [[ "${1:-}" == --debug && $# == 1 ]]; then
  MODE=debug
  ARGS+=(--debug --config '{"build":{"beforeBuildCommand":"npm run build -- --mode native-smoke"}}')
elif [[ $# != 0 ]]; then printf '%s\n' 'Usage: bash scripts/build-local.sh [--debug]' >&2; exit 2; fi
[[ "$(uname -s)" == Darwin ]] || { printf '%s\n' 'This signed local build is macOS-only.' >&2; exit 1; }
[[ -f package-lock.json && -f src-tauri/Cargo.lock ]] || { printf '%s\n' 'Generate and retain package-lock.json and src-tauri/Cargo.lock first.' >&2; exit 1; }
: "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY in the environment or ignored root .env}"
IDENTITY="$APPLE_SIGNING_IDENTITY"
IDENTITIES="$(security find-identity -v -p codesigning)"
[[ "$IDENTITIES" == *"\"$IDENTITY\""* ]] || { printf '%s\n' 'The approved Developer ID signing identity is not available.' >&2; exit 1; }
export CARGO_TARGET_DIR="$ROOT/src-tauri/target"
# Keep bundling local even if this shell contains Apple notarization credentials.
(
  unset APPLE_ID APPLE_PASSWORD APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH APPLE_NOTARIZATION_KEYCHAIN_PROFILE
  export APPLE_SIGNING_IDENTITY="$IDENTITY"
  npm run tauri -- build --bundles app "${ARGS[@]}" -- --locked
)
APP="$CARGO_TARGET_DIR/$MODE/bundle/macos/Fleet.app"
[[ -d "$APP" ]] || { printf '%s\n' 'The expected app bundle was not produced.' >&2; exit 1; }
codesign --verify --strict --deep "$APP"
SIGNATURE="$(codesign --display --verbose=4 "$APP" 2>&1)"
[[ $'\n'"$SIGNATURE"$'\n' == *$'\n'"Authority=$IDENTITY"$'\n'* ]] || { printf '%s\n' 'The bundle does not have the supplied signing identity.' >&2; exit 1; }
printf '%s\n' "$SIGNATURE"
printf '\nSigned local %s app: %s\n' "$MODE" "$APP"
printf '%s\n' 'Not notarized. Not a public release. Do not infer native SSH or recovery proof from a successful build.'
