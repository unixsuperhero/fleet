#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
[[ -f package-lock.json && -f src-tauri/Cargo.lock ]] || { printf '%s\n' 'Generate and retain npm and Cargo lockfiles before checking.' >&2; exit 1; }
npm run bindings:check
npm run typecheck
cargo test --manifest-path src-tauri/Cargo.toml --locked
cargo clippy --manifest-path src-tauri/Cargo.toml --locked --all-targets -- -D warnings
printf '%s\n' 'Static checks and Rust tests completed. Native packaged UI/SSH/dialog smoke remains a separate gate.'
