# Independent fleet app implementation plan

Status: native implementation present; acceptance verification in progress. The stack is Tauri 2/Rust, vanilla TypeScript, xterm.js, installed OpenSSH, SQLite and explicit remote tmux. Actual native hscale SSH, paste, Unicode and initial tmux counter execution passed on 2026-10-12. TUIOS remains usable independently. Ongoing operating requirements are in the [maintenance plan](maintenance-plan.md).

## Current TUIOS setup

Setup verified on 2026-10-11; the TUIOS host alias was renamed on 2026-10-12:

- `hscale` and `m3pro` are active in the local TUIOS fleet. Both use authenticated tailnet IPv6 SSH with strict host-key verification and negotiate control protocol 1. Fresh cross-host terminal commands passed on both nodes. The old TUIOS alias was removed after the user activated `m3pro` from a terminal outside TUIOS; the Mac daemon PID, instance, and its two remote sessions were unchanged.
- `hscale` retains its SSH alias and identity. Its saved TUIOS SSH options select the verified tailnet IPv6 endpoint and retain strict host-key checking. Authenticated IPv6 SSH succeeded without changing tunnels, firewall rules, or access policy. Exact endpoint and routing details belong in private local configuration.
- Built the Linux amd64 pure-Go binary from the existing local TUIOS checkout and installed it at `/root/.local/bin/tuios`. Local and remote SHA-256 checksums match. The saved host entry selects this binary explicitly.
- Started a new remote daemon and created the persistent `fleet` session. `tuios hosts test hscale` reports `up` and one session. A command sent to its terminal produced `FLEET_SMOKE host=hscale os=Linux user=root`, confirmed by pane capture.
- Boot startup is enabled through [tuios-hscale.service](../deploy/tuios-hscale.service), installed with the idempotent [startup script](../scripts/configure-hscale-startup.sh). The current manually started daemon was not restarted. An isolated systemd-managed daemon answered live client requests; the original production PID, session, pane, and PTY survived. No server reboot was performed. Restored layouts do not imply surviving shell processes.
- `m3pro` uses its existing execution account and verified tailnet IPv6 endpoint. Its SSH destination and trusted host-key alias were preserved; no operating-system or Headscale rename was performed. The user verified its ED25519 fingerprint on the Mac before the public host key was saved locally, then installed this workstation's existing public key for SSH authentication.
- Reused the Mac's existing `/opt/homebrew/bin/tuios` version `0.9.2` and running daemon. Created a separate detached `fleet` session without replacing software, restarting the daemon, or disturbing its attached `m3pro` session.
- The user applied the saved configuration from a terminal outside TUIOS. Both selected hosts now report `up` in the local daemon. The pre-existing `vps` host is also active; its configuration was not edited.
- The Mac's `fleet` terminal returned a fresh proof identifying Darwin and the expected execution account; the server returned `os=Linux user=root`. Commands and output crossed the local daemon's host links, not only direct SSH.

The configuration is in `~/Library/Application Support/tuios/config.toml`. The user confirmed Studio means the current local Mac Studio hub: no duplicate SSH peer or old VPN registration was activated. NAS is explicitly excluded from enrollment, deployment and configuration changes. The pre-existing `vps` record remains unchanged.

### Use the configured fleet

1. Run `tuios hosts` to inspect link state and `tuios ls --all-hosts` to list sessions.
2. Attach the server session with `tuios attach --host hscale fleet`.
3. Attach the Mac session with `tuios attach --host m3pro fleet`.
4. For later host configuration changes, run `tuios config apply` from a terminal outside TUIOS. Do not restart an active daemon to apply connection changes.

## Scope and independence

The app manages an explicitly selected fleet, opens remote terminals, and restores saved session references. Most nodes use the tailnet. Non-tailnet machines use manual SSH targets. Discovery does not authorize enrollment; local/self and NAS entries are not enrollment targets for this setup.

The app owns its UI, persistence, connection lifecycle, releases, and diagnostics. Ordinary terminals do not use the TUIOS daemon or its configuration as live storage. Both tools can use the same existing SSH configuration and Tailscale installation.

The explicitly requested remote-setup exception uses a native-picked, trusted Inbox checkout and the canonical installer. It installs missing tools after native confirmation, previews and installs collector capture, and opens fixed provider-login commands in app-owned terminal tabs. Native host registration, grants, and safe activation of changed hooks remain explicit human steps. It never stops or replaces an active TUIOS daemon, copies credentials, or imports the native inventory automatically.

Exclude infrastructure provisioning, task scheduling, autonomous agents, cloud services, and git worktree creation. The guided setup exception does not make Fleet a general remote command or package-management API.

## Selected architecture

Use a local-first, single-user desktop app. Start on macOS. Add other platforms only after their terminal and packaging paths pass the same acceptance checks.

| Decision | Selected choice | Cost or alternative |
| --- | --- | --- |
| Desktop shell | Tauri 2 with a Rust backend and a vanilla TypeScript web UI | Native resource owners for terminal transport and SQLite; no UI framework or service/repository chain. A local web server adds browser authentication and process-management work. |
| SSH transport | Installed OpenSSH, launched with argument arrays | Reuses SSH aliases, agents, and known-hosts policy. An embedded SSH library requires separate compatibility and authentication work. |
| Terminal display | xterm.js with a local PTY connected to OpenSSH | Must verify resize, Unicode, paste, and interactive authentication in the transport spike. |
| Discovery | Local Tailscale CLI status | No admin API credentials. Node discovery is separate from enrollment and authorization. Missing CLI must not prevent manual SSH connections. |
| Local storage | SQLite for enrolled hosts and session references | Own schema migrations and backups. Store no passwords, private keys, or SSH-agent material. |
| Remote process persistence | Explicit remote tmux sessions | Requires tmux on the target. Plain SSH terminals remain available, but cannot promise process survival. |
| Optional remote setup | Single-use native consent tickets launched through the existing terminal owner, with fixed actions | Reuses authentication, host-key proof, cancellation, and output framing. A separate progress coordinator would duplicate lifecycle state. |
| Inbox setup logic | Trusted, native-picked checkout and its canonical installer | Keeps hook/service/queue ownership in Inbox. Requires the checkout to remain available for the central service and upgrades. |
| Setup SSH overrides | Immutable private OpenSSH profiles recorded as `ssh_config` in central collector sources | Keeps installer and subsequent pulls on the enrolled endpoint without rewriting user SSH configuration. Referenced profiles need separate migration backup. |

Do not ship a remote fleet daemon initially. If SSH plus tmux cannot meet the required session behavior, record the failing scenario and reconsider that choice before building more features.

## Data and operation contracts

These are illustrative JavaScript objects, not existing APIs or TypeScript types:

```js
const host = {
	id: "host-01",
	label: "hscale",
	sshTarget: "hscale", // SSH alias, user@host, or DNS name
	portOverride: null, // null preserves SSH configuration
	tailnetNodeId: null, // optional discovery association, not the database key
};

const discovery = {
	nodeId: "example-node-id",
	dnsName: "hscale.node.example",
	online: true,
	observedAt: "2026-10-10T12:00:00Z",
};

const connection = {
	hostId: "host-01",
	attemptId: "attempt-01",
	state: "failed",
	stage: "tcp", // resolve, tcp, hostKey, auth, session
	reason: "timeout",
};

const savedSession = {
	id: "session-01",
	hostId: "host-01",
	mode: "tmux", // plain sessions have no durable remote process guarantee
	remoteSessionName: "fleet-session-01",
};
```

Discovery records describe observations. Host records describe user enrollment. An online discovery record does not establish SSH or tmux readiness. A stable local host ID survives DNS changes.

Connection states are `disconnected`, `connecting`, `awaitingConsent`, `connected`, and `failed`. Only the active attempt can update its connection. Canceling an attempt prevents its late result from replacing a newer state. A disconnect does not delete an enrolled host or saved session.

Backend operations are discover nodes, enroll or update a host, connect, send terminal input, resize, disconnect, list saved sessions, and attach a saved session. Terminal events carry a connection ID. Frontend components do not build shell commands.

## Security and interaction requirements

- Keep private keys and credentials in existing OpenSSH facilities. Do not copy them into SQLite or diagnostics.
- Verify unknown host keys through a dedicated consent flow. Show the host, algorithm, and fingerprint. Never auto-accept a key or disable host-key verification. A changed key is a blocking error.
- Do not treat a discovery response, remote output, or terminal title as a command or authorization.
- Parse supported SSH target syntax and pass arguments directly. Reject option-shaped targets and do not concatenate input into a local shell command. Restrict generated tmux names to an app-owned format.
- SSH configurations are trusted local inputs. Explain that configured proxy commands can execute local programs. Do not edit the user's SSH configuration automatically.
- Allow only local packaged UI content to invoke native operations. Expose narrow IPC commands, not arbitrary process execution. Open external links in the system browser.
- Keep terminal escape handling inside the terminal renderer. Require user action for clipboard writes and external links.
- Use native file, directory, and save dialogs for local path selections. Wire chosen paths into the operation. Manual path entry can supplement a picker, not replace it.
- Discovery refresh updates inventory rows only. It must not remount terminals, clear forms, change focus, reset selection, or move the user's scroll position.

## Ordered implementation tasks

The modules are implemented in this repository. The checkboxes track exercised acceptance, not source-file existence.

Observed native evidence on 2026-10-12: hscale plain SSH returned `user=root`, `os=Linux`, Unicode `雪`, and a `117 × 38` PTY. After native app restart, the same tmux counter advanced from 2 to 1733 with the original pane PID, boot marker, pane birth marker, instance token, and session ID unchanged. The signed debug bundle reattached explicitly; startup opened no terminal automatically. ScreenCaptureKit captured the actual native window. Integrated bindings, TypeScript, 41 Rust tests, and Clippy passed. Isolated multi-terminal/security scenarios and native backup/restore dialogs remained under acceptance verification at that observation. M3 tailnet IPv4/IPv6 timed out; discovery-online is not SSH readiness. A signed production bundle built successfully; actual release interaction is a separate gate.

### 1. Prove the transport and persistence behavior

Dependencies: none. Scope: desktop experiment and terminal transport module.

Acceptance:
- [ ] Open an interactive SSH terminal through a native PTY. Verify an existing alias, key-agent authentication, host-key consent, resize, Unicode, and paste.
- [x] Run a counter in a remote tmux session. Disconnect, restart the app, and reattach to that same session. Counter progress proves process survival.
- [ ] Explain and demonstrate plain SSH disconnect behavior, missing tmux, and remote reboot without claiming a dead process was restored.

Verification: run the actual desktop experiment against a tailnet node and a controlled non-tailnet SSH target. Record failures and decide whether this architecture is viable before task 2.

### 2. Enroll and reopen one host

Dependencies: 1. Scope: host form, SQLite host repository, and connection view.

Acceptance:
- [ ] Enroll a manual SSH target, connect to it, close the app, and reopen the same host record.
- [ ] Preserve alias-specific username, key, and port settings unless the user explicitly overrides a supported field.
- [ ] Show DNS, TCP, host-key, and authentication failures at their actual stage without storing secrets.

Verification: exercise the saved-host flow in the packaged app, including one failed connection followed by a successful correction.

### 3. Discover and select tailnet nodes

Dependencies: 2. Scope: Tailscale discovery adapter and enrollment view.

Acceptance:
- [ ] Discover local tailnet peers without an admin API key. Explicit selection enrolls only the selected nodes.
- [ ] Offline, stale, and already-enrolled rows remain distinct. Refresh neither duplicates hosts nor overwrites user labels or SSH targets.
- [ ] Missing Tailscale leaves manual SSH targets usable. Refresh preserves an active terminal and an unfinished enrollment form.

Verification: exercise discovery with Tailscale present, unavailable, and disconnected. Refresh while typing into a form and while a terminal counter runs.

### 4. Manage multiple independent terminal connections

Dependencies: 2. Can run alongside 3 after the connection contract is fixed. Scope: terminal tabs and connection lifecycle.

Acceptance:
- [ ] Open connections to two hosts. Input, output, resize, and errors stay attached to the correct connection.
- [ ] Cancel and reconnect without a late previous attempt overwriting the new state.
- [ ] Close one terminal without stopping another. Dispose its local PTY and SSH process without deleting remote persistent sessions.

Verification: use two real hosts. Drop one link, reconnect it, resize both, and check the other session remains usable.

### 5. Save and reattach persistent remote sessions

Dependencies: 1, 2, 4. Scope: session metadata and remote tmux operations.

Acceptance:
- [ ] Save a session reference and reattach after app restart to the original remote process, not a replacement.
- [ ] A missing or rebooted remote session is labeled ended. Start a replacement only through explicit user action.
- [ ] A host without tmux offers a plain terminal with the loss-of-persistence warning. Do not install tmux automatically.

Verification: repeat the counter scenario, kill the remote session, and reboot a disposable target. Observe each distinct outcome.

### 6. Back up and restore enrolled metadata

Dependencies: 2, 5. Scope: backup, restore, and native dialogs.

Acceptance:
- [ ] A native save dialog exports a consistent SQLite backup. A native file picker selects a restore source.
- [ ] Validate the backup and schema compatibility before changing current data. An invalid backup leaves the current database usable.
- [ ] Restore into a fresh profile and reopen the expected host and session references. Restore does not restore SSH secrets or remote processes.

Verification: run the actual backup and restore UI with a valid snapshot, a corrupt file, a newer unsupported schema, and canceled dialogs.

### 7. Ship and prove a recoverable desktop release

Dependencies: 3, 4, 5, 6. Scope: packaging, release checks, and recovery documentation.

Acceptance:
- [ ] A signed local macOS app bundle connects to a host. The user selected local signed builds only; notarized public distribution and fresh macOS-account installation remain separate, unproved release gates.
- [ ] Upgrading an older profile preserves host and session records. A failed migration preserves a recoverable pre-upgrade database.
- [ ] Diagnostics redact credentials and terminal content. The release includes the compatibility matrix and recovery procedure in the maintenance plan.

Verification: install the packaged artifact, exercise a real SSH terminal, upgrade a profile, and perform the documented rollback with the matching database backup.

## Checkpoints and decision boundaries

After task 1, approve or reject the transport design based on observed behavior. After tasks 2 through 4, require manual and tailnet enrollment plus simultaneous working terminals. After tasks 5 through 7, require restart, restore, upgrade, and rollback proof before release.

Tasks 3 and 4 can be implemented independently once task 2 defines the host and connection contracts. Do not parallelize writes to shared schema or migration files without coordination.

The macOS-first stack and remote tmux requirement are selected. The user authorized safe existing verification environments: no ordinary fleet node may be rebooted. Real-OS reboot, a fresh macOS-account install and public notarization must not be claimed from local-profile or process-loss checks. Cross-platform support, multiple users and an app-managed remote daemon each require a separate scope decision.
