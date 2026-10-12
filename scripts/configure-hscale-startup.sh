#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
unit="$root/deploy/tuios-hscale.service"
test -f "$unit"
encoded=$(base64 < "$unit" | tr -d '\n')

ssh_config=()
[[ ! -f "$root/.fleet-ssh-config" ]] || ssh_config=(-F "$root/.fleet-ssh-config")
ssh "${ssh_config[@]}" \
  -o BatchMode=yes \
  -o StrictHostKeyChecking=yes \
  -o UpdateHostKeys=no \
  -o ConnectTimeout=8 \
  hscale bash -se -- "$encoded" <<'REMOTE'
set -euo pipefail
[[ $(id -u) == 0 ]]
[[ -x /root/.local/bin/tuios ]]
name=tuios-hscale.service
dest=/etc/systemd/system/$name

fail() { printf '%s\n' "$*" >&2; exit 1; }
check_definition() {
  local fragment drops load
  fragment=$(systemctl show "$name" -p FragmentPath --value)
  drops=$(systemctl show "$name" -p DropInPaths --value)
  load=$(systemctl show "$name" -p LoadState --value)
  [[ -z "$drops" ]] || fail "Refusing existing drop-ins: $drops"
  [[ "$load" == not-found || "$load" == loaded ]] || fail "Refusing LoadState=$load"
  [[ -z "$fragment" || "$fragment" == "$dest" ]] || fail "Refusing alternate unit: $fragment"
  [[ ! -L "$dest" ]] || fail "Refusing symlink or masked unit: $dest"
  if [[ -e "$dest" ]]; then
    [[ -f "$dest" ]] || fail "Refusing non-file: $dest"
    cmp -s "$tmp/$name" "$dest" || fail "Refusing nonmatching unit: $dest"
  fi
}

# Same filesystem permits an atomic, non-overwriting installation.
tmp=$(mktemp -d /etc/systemd/system/.tuios-hscale.XXXXXX)
trap 'rm -rf -- "$tmp"' EXIT
printf '%s' "$1" | base64 --decode > "$tmp/$name"
chmod 0644 "$tmp/$name"
systemd-analyze verify "$tmp/$name"
check_definition
if [[ ! -e "$dest" ]]; then
  # ln deliberately has no -f: a concurrent unexpected file is never replaced.
  ln -- "$tmp/$name" "$dest"
fi
systemctl daemon-reload
check_definition
systemctl enable "$name"
[[ $(systemctl is-enabled "$name") == enabled ]]
systemctl show "$name" -p FragmentPath -p UnitFileState -p ActiveState -p MainPID
printf '%s\n' 'Boot startup enabled; production daemon was not started, stopped, or restarted.'
REMOTE
