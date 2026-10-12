# Fleet

Fleet is an independent, local-first macOS SSH terminal app. It uses installed OpenSSH, an app-owned SQLite database, Tailscale discovery, and explicitly requested remote tmux sessions. Ordinary terminals do not depend on TUIOS. Optional, per-host TUIOS and Inbox setup is separate from that transport. Discovery never enrolls a node automatically. The current Studio is the local hub; NAS enrollment is excluded.

## Status and scope

This is a signed-local evaluation project, not a public release. The user approved local Developer ID builds. Notarization, public distribution, actual remote reboot, and a fresh macOS account/OS installation have not been proved and are not performed by the local build script. See [release checks](docs/release-checklist.md) for the evidence requirements and current status.

Fleet does not change SSH trust or tailnet policy, schedule jobs, or reconnect automatically. Remote software installation is available only through the explicitly confirmed setup flow below. A plain SSH terminal has no process-survival guarantee. A saved tmux reference is useful only if its original session and pane remain alive; reattach does not create a replacement.

## Develop and build

Requirements are macOS, Xcode command-line tools, Rust 1.88 or newer, Node 22.12 or newer, npm, and installed OpenSSH. Tailscale is optional for manual targets. tmux is required on a selected remote machine only for persistent sessions.

Retain `package-lock.json` and `src-tauri/Cargo.lock`. When initializing this greenfield source tree, resolve them once with `npm install` and `cargo generate-lockfile --manifest-path src-tauri/Cargo.toml`. Subsequent installations use `npm ci`, and scripts pass `--locked` to Cargo. Do not ignore or regenerate lockfiles casually during a release check.

```sh
npm ci
npm run bindings
npm run tauri -- dev
```

`model.rs` is the wire-model authority. `npm run bindings` generates branded UUID types, command argument/result maps, and `invokeCommand` in `src/bindings.ts`. Do not hand-edit generated bindings. Native commands allocate connection and attempt IDs.

```sh
npm run check
npm run build:local
```

Set `APPLE_SIGNING_IDENTITY` to a Developer ID Application identity installed in your local Keychain. Supply it through the shell environment or the ignored root `.env` file. The local build script sources `.env`, so values there override inherited shell values. Do not put a personal certificate name or team identifier in the shared Tauri configuration.

`build:local` bundles only the `.app` and verifies its signature against the selected identity. It clears notarization credentials for the bundler. It does not upload to Apple or bypass Gatekeeper. The expected output is `src-tauri/target/release/bundle/macos/Fleet.app`. Copy it to a user-owned installation directory only after verifying the artifact; do not overwrite an existing app without retaining the prior binary and a matching database backup.

### Configure local exclusions

Fleet compiles the following exclusion settings into the native app. The defaults are documentation examples, not live network addresses. Set the four variables in the build environment, or in the ignored root `.env` used by the local build script, to match your excluded NAS and local Studio hub.

| Setting | Default |
| --- | --- |
| `FLEET_NAS_DNS` | `nas.node.example` |
| `FLEET_STUDIO_DNS` | `studio.node.example` |
| `FLEET_NAS_IPV4` | `192.0.2.3` |
| `FLEET_NAS_IPV6` | `2001:db8::3` |

The build rejects empty or invalid overrides. DNS values must be ASCII DNS names, not IP literals. The address settings must parse as IPv4 and IPv6 respectively. Rust does not load `.env` itself; export the settings when invoking Cargo or Tauri directly. Rebuild after changing them. The generic `nas` and `studio` aliases and default example exclusions remain blocked when you configure custom values.

The live SSH evidence scripts `configure-fleet-starship.sh`, `configure-hscale-startup.sh`, and `smoke-fleet-starship.py` use an optional ignored root `.fleet-ssh-config` when it exists. This file can hold local endpoint overrides and trusted `HostKeyAlias` settings. Include your normal `~/.ssh/config` there if the overrides depend on its identities, proxies, or trust configuration. This is a trusted OpenSSH configuration, not an app-wide enrollment override.

Keep `.env` and `.fleet-ssh-config` private with mode `0600`. Never publish those files, keys, certificates, credentials, local databases, backups, or raw operational logs.

## Use

Enroll an existing SSH alias or an explicit `user@host` target. Leave override fields blank to retain OpenSSH's configured username, identity, port, proxy and trust settings. Existing SSH configuration is a trusted local input; its `ProxyCommand` or related directives may execute local programs. Fleet never silently removes configured behavior to make a connection succeed.

Connect explicitly. Unknown host keys require confirmation with the actual algorithm and SHA256 fingerprint. Verify through a trusted channel before accepting. A changed host key is blocked. Credentials remain in OpenSSH facilities; authentication answers are ephemeral.

Choose a plain terminal or explicitly create a persistent session. Attach saved sessions only after the app validates their identity. A missing session or original pane is ended, not resumed. Disconnecting or quitting disposes Fleet's local SSH/PTY transport; it does not kill remote tmux sessions. A remote reboot does not restore shell processes.

Metadata backup, restore, backup-directory selection and diagnostic export use native dialogs. Restore first stages a private read-only SQLite snapshot. Preview does not modify current metadata. Consent consumes a one-use ticket and requires a native confirmation. Current data is backed up before replacement. Canceling a panel or preview does not change database metadata. Active terminal objects remain mounted across restore.

Diagnostics retain bounded connection metadata, not terminal bytes, passwords, authentication prompts, key paths or SSH debug transcripts. Host labels and targets are excluded by default. Review the opt-in preview before exporting host metadata. Backups contain private host/session metadata even though they contain no credentials.

## Set up remote TUIOS and Inbox capture

Expand **Remote TUIOS + Inbox setup** on an enrolled host. Select a trusted, existing `tuios-inbox` checkout with **Choose folder**. Fleet runs that checkout's canonical installer rather than duplicating collector, hook, shell-marker, service, or queue logic. The checkout must include `server.mjs` and `public/`, and support `--ssh-config`.

1. Use **Check** to inspect the selected remote execution account and installed tools.
2. If needed, install local Bun and TUIOS, then install the missing remote tools and selected Claude, Codex, or OMP harnesses. Each launch requires native confirmation. TUIOS uses the checksum-verified official release under the execution account's `~/.local/bin`; an existing binary stays untouched. Fleet refuses privilege escalation and never copies provider credentials.
3. Use **Start daemon if absent** only when needed. It creates an auto-named detached session when no daemon is running. It never stops or replaces an existing daemon.
4. Use **Preview capture** before **Install capture**. Installation can start or update the local Inbox services and remote collector service. Partial failure or cancellation can leave completed changes in place. It does not apply native grants or restart TUIOS.
5. Use **Login** for interactive provider sign-in. Enter a callback port only when required by that provider. Forwarding binds to `127.0.0.1`; OMP offers the known Anthropic and OpenAI ports.
6. Run the displayed native host-registration and grant commands from a terminal outside TUIOS. Activate changed hooks only at a safe point after preserving valuable sessions. Use **Status**, test the native link, and verify a real full transcript in Inbox history. Installer exit status alone is not capture verification.

Setup uses the enrolled host's exact port, HostName, and HostKeyAlias overrides. Its private, immutable `0600` SSH profiles live under the Fleet profile's `setup-ssh` directory. The installer records `ssh_config` on the central source so later export and acknowledgement pulls use the same endpoint. Do not remove a referenced profile or move it without updating the source. Normal metadata backups do not include this directory, the Inbox checkout, collector queues, or provider credentials. Back those up separately when moving machines.

Confirmation creates a short-lived, single-use launch ticket tied to the host, metadata generation, and checkout scripts, server entry point, and package manifest. The installer runs with Bun's checkout configuration and environment-file loading disabled. Setup consoles retain the existing terminal lease, host-key, output acknowledgement, cancellation, and shutdown protections. Rerun a step from the setup panel for fresh consent; a setup tab cannot replay its spent ticket with Retry. Opening the panel installs nothing and starts no periodic refresh.

## Native smoke runner

Build a debug `.app` to exercise the real packaged WKWebView and backend. This is not a browser with fake IPC.

`npm run build:smoke` selects Rust debug and Vite `native-smoke` mode so the packaged app exposes `window.fleetSmoke`. `npm run build:local` keeps the production frontend and excludes that global and the native smoke handler. Debug smoke results do not prove release-hook exclusion; verify the separate release artifact using the release checklist.

```sh
npm run build:smoke
bash scripts/native-smoke.sh \
  "$PWD/src-tauri/target/debug/bundle/macos/Fleet.app" \
  "$PWD/.smoke/ui"

bash scripts/native-smoke.sh \
  "$PWD/src-tauri/target/debug/bundle/macos/Fleet.app" \
  "$PWD/.smoke/dialogs" \
  "$PWD/scripts/native-dialog-smoke.js"
```

The second scenario requires actual native panel selections described in the app. Use an isolated profile, never the production database. The runner activates only its launched Fleet PID once, then captures that PID’s native main window with ScreenCaptureKit. It retains the proof and attempts capture even on scenario failure, then terminates only its owned process. Capture does not reactivate the app; production focus behavior is unchanged. The helper requires Swift and macOS 14 or newer. Missing Screen Recording permission or an eligible owned window fails visual proof. Archive an old proof file before rerunning so stale evidence cannot pass.

A custom scenario file can select DOM controls and call `window.__TAURI__.core.invoke` or create `window.__TAURI__.core.Channel` instances. This is the real Tauri API. Prefer clicking actual controls for UI scenarios, and call the narrow API for transport assertions or dialog staging. Complete the script with `invoke('smoke_result', { proof: { native: true, status: 'passed', scenario: 'name', observations: [...] } })` only after literal assertions pass. On failure, write `status: 'failed'` and the observed failure. Never manufacture proof from source inspection or a fake browser IPC adapter.

Debug-only environment options:

- `FLEET_PROFILE_DIR` selects an absolute isolated profile directory.
- `FLEET_SMOKE_SCRIPT` selects a local JavaScript file evaluated once after the native main page loads. This is trusted developer input, not an IPC eval command.
- `FLEET_SMOKE_SSH_CONFIG` selects a separate controlled OpenSSH fixture configuration in the transport implementation. Keep fixture keys, agent, port and known-hosts files separate from the user's SSH configuration.
- `FLEET_SMOKE_TIMEOUT` changes the runner timeout for operator-driven native dialogs.

Release builds compile out the script loader, profile override and proof-write handler. The normal capability file grants no smoke permission. A debug run adds a local-only smoke capability only when a smoke script was explicitly selected. There is no generic execute, eval, filesystem-write, shell, process, HTTP or plugin-opener API exposed to the frontend. The main webview cannot navigate to external content or open another native window. Explicit HTTP/HTTPS clicks open the system browser through a validated app command.

The included smoke scripts cover packaged UI/API calls, discovery form preservation, unsafe-link rejection, diagnostic host exclusion, native dialog cancellation and staged restore. They do not prove SSH Unicode/paste/resize, independent terminals, original-process survival after app restart, migration/rollback, or fresh-profile/public distribution. Run and record those separately using [the release checklist](docs/release-checklist.md).

## Recovery and distribution

See [recovery](docs/recovery.md) before restoring or downgrading. The full architecture and operational requirements are in [the implementation plan](docs/implementation-plan.md) and [the maintenance plan](docs/maintenance-plan.md).

Future explicit notarization uses a stored Keychain credential profile, never credentials in source:

```sh
bash scripts/notarize.sh --explicit-public-distribution /absolute/path/Fleet.app YOUR_KEYCHAIN_PROFILE
```

This command uploads the app to Apple and is not part of local evaluation. Run it only after separate approval. It requires an explicitly exported `APPLE_SIGNING_IDENTITY` and does not load `.env`. A successful notarization does not prove clean-profile installation or the application's behavior.

## Check publication contents

Before publishing, check the tracked working files and then every blob reachable from local Git refs:

```sh
python3 scripts/check-publication.py
python3 scripts/check-publication.py --history
```

Both checks must pass. A clean working tree does not sanitize earlier history. The checker reports finding locations without printing sensitive values; it does not replace a manual review of files, commit metadata, and evidence. Keep real local configuration out of the published history, and use `.example` domains, `192.0.2.x` IPv4 addresses, and `2001:db8::x` IPv6 addresses in documentation.
