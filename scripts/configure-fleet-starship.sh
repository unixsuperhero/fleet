#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ssh_options=(-o BatchMode=yes -o StrictHostKeyChecking=yes -o UpdateHostKeys=no -o ConnectTimeout=8)
[[ ! -f "$root/.fleet-ssh-config" ]] || ssh_options+=(-F "$root/.fleet-ssh-config")

apply_host() {
  local host=$1 shell rc encoded
  local transport=()
  case "$host" in
    studio) shell=zsh; rc=.zshrc.d/prompt.zsh ;;
    m3pro)
      shell=zsh; rc=.zshrc
      transport=(ssh "${ssh_options[@]}" m3pro)
      ;;
    hscale)
      shell=bash; rc=.bashrc
      transport=(ssh "${ssh_options[@]}" hscale)
      ;;
    vps) shell=bash; rc=.bashrc; transport=(ssh "${ssh_options[@]}" vps) ;;
    *) printf 'Unknown fleet machine: %s\n' "$host" >&2; return 1 ;;
  esac
  encoded=$(base64 < "$root/deploy/starship/$host.toml" | tr -d '\n')
  "${transport[@]}" bash -s -- "$encoded" "$shell" "$rc" "$host" <<'APPLY'
set -euo pipefail
encoded=$1
shell=$2
rc="$HOME/$3"
host=$4
[[ -f "$rc" ]] || { printf 'Missing startup file: %s\n' "$rc" >&2; exit 1; }
export PATH="$HOME/.local/bin:/opt/homebrew/bin:$PATH"
tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT
if ! command -v starship >/dev/null 2>&1; then
  [[ $(uname -sm) == 'Linux x86_64' ]] || { printf 'Install Starship for this platform first.\n' >&2; exit 1; }
  curl -fL --max-time 120 https://github.com/starship/starship/releases/download/v1.26.0/starship-x86_64-unknown-linux-musl.tar.gz -o "$tmp/starship.tar.gz"
  printf 'b7c232b0e8249d8e55a40beb79c5c43a7d370f3f9408bd215deb0170daeaadf3  %s\n' "$tmp/starship.tar.gz" | sha256sum -c -
  tar -xzf "$tmp/starship.tar.gz" -C "$tmp"
  mkdir -p "$HOME/.local/bin"
  install -m 0755 "$tmp/starship" "$HOME/.local/bin/starship"
fi
if [[ $(uname -s) == Darwin ]]; then
  printf '%s' "$encoded" | base64 -D > "$tmp/fleet.toml"
else
  printf '%s' "$encoded" | base64 --decode > "$tmp/fleet.toml"
fi
TERM=xterm-256color STARSHIP_CONFIG="$tmp/fleet.toml" starship prompt --status=0 --path "$HOME" > "$tmp/prompt" 2> "$tmp/errors"
[[ ! -s "$tmp/errors" ]] || { cat "$tmp/errors" >&2; exit 1; }
backup="$HOME/.local/state/fleet-starship/backups/$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "$HOME/.config/starship"
dest="$HOME/.config/starship/fleet.toml"
if [[ ! -f "$dest" ]] || ! cmp -s "$tmp/fleet.toml" "$dest"; then
  mkdir -p "$backup"
  [[ ! -e "$dest" ]] || cp -p "$dest" "$backup/fleet.toml"
  install -m 0644 "$tmp/fleet.toml" "$dest"
fi
if [[ "$shell" == bash ]]; then
  cat > "$tmp/init" <<'BASH_INIT'
# Fleet machine-specific Starship prompt
export STARSHIP_CONFIG="$HOME/.config/starship/fleet.toml"
export PATH="$HOME/.local/bin:$PATH"
eval "$(starship init bash)"
# Starship redraws PS1; retain the existing TUIOS command-start boundary.
if [[ ${_TUIOS_INBOX_OSC133:-} == 1 && ${PROMPT_COMMAND:-} != *fleet_starship_prompt_end* ]]; then
  fleet_starship_prompt_end() { PS1+='\[\e]133;B\a\]'; }
  PROMPT_COMMAND+=';fleet_starship_prompt_end'
fi
BASH_INIT
else
  cat > "$tmp/init" <<'ZSH_INIT'
# Fleet machine-specific Starship prompt
export STARSHIP_CONFIG="$HOME/.config/starship/fleet.toml"
ZSH_INIT
fi
# The deployed block is appended once; unrelated startup content stays intact.
existing=$(< "$rc")
block=$(< "$tmp/init")
if [[ "$existing" != *"$block"* ]]; then
  [[ "$existing" != *'# Fleet machine-specific Starship prompt'* ]] || {
    printf 'Existing Fleet init differs in %s; review it before replacing.\n' "$rc" >&2; exit 1;
  }
  mkdir -p "$backup"
  cp -p "$rc" "$backup/$(basename "$rc")"
  printf '\n%s\n' "$block" >> "$rc"
fi
printf 'Configured %s: %s, startup %s\n' "$host" "$dest" "$rc"
[[ ! -d "$backup" ]] || printf 'Backups: %s\n' "$backup"
starship --version
APPLY
}

if [[ $# == 0 ]]; then
  set -- studio m3pro hscale vps
fi
for host in "$@"; do apply_host "$host"; done
