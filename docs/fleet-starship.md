# Fleet machine prompts

The prompt configurations cover the local Studio role and the `m3pro`, `hscale`, and `vps` remote roles. NAS is excluded. These role names do not disclose or define live network endpoints.

| Machine | Always-visible badge | Colors and layout | Success character |
| --- | --- | --- | --- |
| Studio | `[ STUDIO ]` | Cyan and blue, dark badge, two lines | `>` |
| M3 Pro | `[M3PRO]` | Violet and pink outline, two lines | `>>` |
| Hscale | `<HSCALE>` | Amber and gold badge, one line | `#` |
| VPS | `{VPS}` | Mint and teal badge, two-line frame | `##` |

Badges appear in local and SSH shells. They do not require special fonts or run an identity command. Directory, Git, and duration information remain available. The Macs retain their time, commit, Ruby, and tmux modules. Linux prompts always show the account and failed exit status.

## Apply the configs

1. Open a fresh terminal or SSH connection to see the new prompt. Existing shells are not restarted.
2. Edit the matching TOML file in [deploy/starship](../deploy/starship/).
3. Run `bash scripts/configure-fleet-starship.sh` from the repository root. To update one machine, add `studio`, `m3pro`, `hscale`, or `vps`.
4. Run `TERM=xterm-256color python3 scripts/smoke-fleet-starship.py` to check fresh interactive shells.

The deployment and smoke scripts use the configured `m3pro`, `hscale`, and `vps` SSH aliases. When the ignored root `.fleet-ssh-config` exists, they pass it to OpenSSH with `-F`; otherwise they use normal OpenSSH configuration. Keep real accounts, endpoint overrides, and trusted `HostKeyAlias` values in that private file. Include `~/.ssh/config` when needed and retain strict host-key checking. A selected-machine argument limits deployment contacts; the smoke script checks all configured roles. For different computers, start with [the new-machines checklist](new-machines-checklist.md). Never publish the private SSH configuration or raw smoke logs.

Each machine uses `~/.config/starship/fleet.toml`. Original Starship configs remain untouched. Changed startup files and any prior `fleet.toml` are backed up under `~/.local/state/fleet-starship/backups/`. The deployment prints the backup path. To undo the rollout, restore the backed-up startup file and open a new shell.

The Macs reuse installed Starship. Linux installs the official v1.26.0 x86_64 musl binary into `~/.local/bin` after SHA256 verification. The script does not change SSH trust, restart services, or touch existing terminal sessions. VPS keeps its existing TUIOS command marks.

## Verified on 2026-10-12

- Fresh interactive Studio and M3 Pro Zsh shells selected `fleet.toml` and rendered their distinct badges, ANSI styles, and success/failure characters.
- Fresh SSH login and non-login interactive Bash shells passed on Hscale and VPS.
- VPS emitted the existing OSC-133 A, B, C, and D command marks with Starship active.
- A second deployment left checksums of all deployed configs, startup files, and Linux binaries unchanged.
- Installed versions: Studio 1.24.2, M3 Pro 1.17.1, Hscale and VPS 1.26.0.
