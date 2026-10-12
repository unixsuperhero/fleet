# Fleet local evaluation and release checks

## Approval and current status

Approved application scope is a signed local macOS evaluation build using a locally installed Developer ID Application identity. Public source publication does not approve notarization or public application distribution. Actual remote reboot and a fresh OS/macOS-account installation are explicitly unproved gates. Do not reboot existing nodes or contact NAS. Studio is the current local hub. Configure signing and exclusions through the [private local build settings](../README.md#develop-and-build), never personal values in published source.

Preparatory observations from the read-only glue investigation:

| Item | Observed value | Meaning |
| --- | --- | --- |
| Workstation | macOS 26.4.1, build 25E253, arm64 | Tooling environment only, not a supported-platform claim |
| Node | 24.20.0 | Installed, no frontend build exercised |
| Rust/Cargo | 1.97.1 | Installed, no Fleet build/tests exercised |
| Installed OpenSSH | OpenSSH 10.2p1, LibreSSL 3.3.6 | Version observed, no Fleet connection exercised |
| Signing identity | One valid local Developer ID Application identity | Identity present; Fleet signature not yet produced at this preparatory observation |
| Native tools | xcrun, sqlite3, screencapture; notarytool 1.1.2 | Availability only; no screenshot/notarization performed |

No integrated Fleet build, Rust tests, native UI smoke, SSH smoke, database restore/migration/rollback or installation ran in this authored-file handoff. Do not mark the checklist passed from source inspection. The integrating release owner must record commands, artifact hashes, versions, proof paths and observations. Existing TUIOS checks do not count as independent Fleet evidence.

## Dependency and artifact identity

Direct packages are exact-pinned, with transitive resolutions retained in npm and Cargo lockfiles. Stable Tauri versions were researched through the registry/rustdoc pages:

- [Tauri 2.12.3](https://docs.rs/tauri/2.12.3/tauri/)
- [tauri-build 2.7.2](https://docs.rs/tauri-build/2.7.2/tauri_build/)
- [dialog 2.8.2](https://docs.rs/tauri-plugin-dialog/2.8.2/tauri_plugin_dialog/)
- [opener 2.7.1](https://docs.rs/tauri-plugin-opener/2.7.1/tauri_plugin_opener/)
- [JavaScript API 2.12.1](https://www.npmjs.com/package/@tauri-apps/api/v/2.12.1)
- [CLI 2.12.2](https://www.npmjs.com/package/@tauri-apps/cli/v/2.12.2)

The frontend uses Vite 8.3.4, TypeScript 5.9.3, xterm 6.0.0 and fit 0.11.0. Rust ts-rs 11.1.0 generates types with its stable `decl()` API. SQLite uses rusqlite 0.40.2 with bundled SQLite and online-backup support. Version selection is not compatibility proof.

- [ ] Retain both lockfiles. Record the actual source revision without creating a commit or worktree as a side effect of verification.
- [ ] Run `npm ci` and `npm run check` once after independent owners have integrated.
- [ ] Build `npm run build:smoke` for native verification with Rust debug and Vite `native-smoke` mode. Build `npm run build:local` separately for production frontend/signing/install checks. Record separate artifact hashes; debug evidence does not prove release-hook exclusion.
- [ ] `codesign --verify --strict --deep` succeeds for the actual bundle. Record Developer ID authority, team and hardened runtime.
- [ ] Run the signed release app from a user-owned local installation directory. Verify its version, database location and an actual SSH terminal.
- [ ] Release ignores `FLEET_PROFILE_DIR`, `FLEET_SMOKE_SCRIPT` and `FLEET_SMOKE_SSH_CONFIG`; invoking `smoke_result` is rejected because its handler is absent. Confirm `window.fleetSmoke` is absent and the bundled production JavaScript contains no assignment to it. Confirm no arbitrary exec/eval/write command exists. Do not supply a real destructive script for this check.

## Native UI and transport proof

Use the actual packaged WKWebView and backend, not a browser shim. `scripts/native-smoke.sh` accepts a debug bundle, an isolated absolute profile and a real JavaScript scenario. It records JSON proof and captures a native macOS image. Keep human actions and observed results in the proof. A screenshot must show the actual app, not merely a desktop or another process.

- [ ] Manual enrollment survives app restart with the same HostId, label and supported overrides. No terminal starts on launch.
- [ ] Existing SSH alias preserves configured user, identity, port and supported proxy behavior. A configured unsupported LocalCommand is reported accurately, not silently discarded.
- [ ] Selectively enroll one discovery peer. Observe offline, stale, already-enrolled and excluded/local states. Missing/disconnected/invalid/timed-out Tailscale is unavailable, not empty success.
- [ ] Refresh discovery while typing and while a terminal counter runs. Form nodes, input, focus, terminal selection, scroll and output survive.
- [ ] Known-host verified alias connects through real PTY/OpenSSH. Observe authenticated readiness, not mere process spawn or remote output containing a marker.
- [ ] Controlled unknown key shows the actual algorithm and SHA256 fingerprint. Decline blocks connection; explicit acceptance succeeds. A changed fixture key blocks without altering trust. Never weaken StrictHostKeyChecking.
- [ ] Authentication dialog cancel/decline and failed credentials report the actual stage. Answers are cleared and not stored/logged.
- [ ] Unicode, paste and PTY resize work in an interactive terminal. Raw-byte output acknowledgments drain after xterm write completion without unbounded memory growth.
- [ ] Two real terminals have independent input/output/resize. Cancel one pending attempt and reconnect it; delayed old events cannot overwrite the new attempt or affect the other terminal.
- [ ] Closing a terminal/app kills and waits only for its local transport processes. No remote tmux session is killed.
- [ ] Create a unique Fleet tmux session and run a counter with a recorded PID, tmux session identity, original pane and boot marker. Disconnect, quit, restart the app, explicitly attach, and prove the same PID/process advanced. A replacement shell is failure.
- [ ] End an app-created original pane/session. Checking/attaching reports ended and does not create a replacement. A missing-tmux fixture offers a warned plain terminal without installing anything.
- [ ] Unsafe `file:`, `javascript:`, credential-bearing and malformed links are rejected. Explicit HTTP/HTTPS clicks open the system browser. External webview navigation/new windows get no app IPC.

Safe existing targets may be explicitly selected through the configured `hscale`, `m3pro`, and `vps` SSH aliases. Keep real accounts, addresses, and verified host-key aliases in private local OpenSSH configuration. Do not change their services, trust, configuration, or existing programs. Use only unique app-owned tmux names. Controlled localhost SSH fixtures keep separate keys, config, agent, known-hosts, and port. Never edit user SSH configuration or known-hosts to manufacture test states.

## Native backup, restore and diagnostics proof

`native-dialog-smoke.js` requires actual native save/open/directory panels and confirmations. It covers cancellation, ticket reuse rejection and self-snapshot round-trip in an isolated profile. It does not cover corrupt/newer fixtures or migration/rollback. Seed a meaningful host/session set before the dialog scenario so expected literal identities can be compared.

- [ ] Native directory selection changes the backup destination only after selection. Cancel preserves it.
- [ ] Native save exports a consistent SQLite snapshot containing the expected literal host/session identities. Cancel creates no metadata changes.
- [ ] Native open previews a valid backup without changing current data. Change or remove the original source after preview; approved restore uses the immutable staged snapshot, not the changed source.
- [ ] Cancel picker, preview and native replacement confirmation. Compare expected current records before/after; consumed tickets cannot commit later.
- [ ] Try a corrupt file and a valid SQLite file with unsupported newer Fleet schema using the actual native picker. Both fail without changing current metadata. Do not write directly to the production database.
- [ ] Restore into an isolated empty profile and verify literal HostId/SessionId values. Reopen the app and observe the same metadata. A canceled callback must not perform any delayed Store write.
- [ ] Restore while an active terminal counter runs. Terminal DOM and transport continue. A late save from the old database generation cannot resurrect stale metadata.
- [ ] Preview/export diagnostics with host metadata off, then on. Finite entry/retention limits hold. Check literal secret markers used only in controlled fixture authentication/terminal output are absent; don't put real credentials into proof files.

For corrupt/newer-schema fixtures, prepare separate files in the smoke directory. Corrupt data may be ordinary non-SQLite bytes. A newer-schema fixture must start from an actual Fleet backup, then modify only that throwaway copy's `PRAGMA user_version` beyond the supported schema. Keep its Fleet application identity intact. Native selection must exercise the real validator; source-text checks do not count.

## Upgrade and rollback proof

- [ ] Retain the previous signed binary and its matching pre-upgrade snapshot. Record app/schema versions and expected literal host/session IDs.
- [ ] Upgrade an isolated older profile. Verify the pre-migration backup opens read-only, has the expected identity/version, passes integrity and preserves expected records after migration.
- [ ] Exercise migration failure safely. Current metadata and a verified recoverable snapshot remain available; do not retry destructive startup blindly.
- [ ] Quit, preserve the newer profile, restore the matching old snapshot with WAL/SHM handled, and run the old binary. Verify its expected records and explicit attach behavior. Document metadata lost since the backup.
- [ ] Follow [recovery.md](recovery.md) exactly in the drill. A file named "backup" is not a passed recovery check.

## Optional host setup proof

Use a private SSH fixture with its own HOME, XDG runtime, state, configuration, keys, and agent. The setup fixture records the daemon PID and process identity so cleanup cannot target the production TUIOS daemon. Never run the canonical service installer against the ordinary same-user localhost fixture: launchd labels share the user domain even when HOME differs.

```sh
FLEET_SMOKE_SETUP=1 bash scripts/smoke-ssh-fixture.sh -- \
  bash scripts/native-setup-dialogs.sh \
  "$PWD/src-tauri/target/debug/bundle/macos/Fleet.app" \
  "$HOME/proj/tuios-inbox"
```

The driver requires explicitly granted macOS Accessibility access. It operates only on the Fleet process it launches and approves only inspection, isolated daemon creation, and capture preview. It does not approve vendor installation, provider login, capture service installation, or grants.

- [x] Cancel and choose an actual native checkout folder; canceled selection changes no metadata.
- [x] Cancel a native setup confirmation; no console or process starts.
- [x] Inspect a real SSH endpoint and observe its execution account and installed tools.
- [x] Preserve an existing SSH counter, lease, terminal DOM, and setup drafts across metadata refresh.
- [x] Start an isolated daemon once, then prove another Start retains the same session.
- [x] Preview through deliberately wrong configured HostName, port, and HostKeyAlias corrected by Fleet overrides. Confirm no hook, service, or daemon changes.
- [x] Pull actual collector records over the generated private profile; prove full bodies arrive durably before exact acknowledgement drains the queue.
- [x] Reject unknown launch tickets in the packaged app. Unit checks also cover spent tickets and changed host, generation, or checkout content.
- [x] Unit checks cover verified archive extraction, checksum errors, existing-binary preservation, Bun preload isolation, owned-child cancellation, and finite runtime. Live vendor installation remains unexercised.
- [ ] Install and verify central and remote services on a disposable separate execution account before claiming fresh-host rollout coverage.

Native verification results belong with their proof paths and artifact hashes. Unit tests, a signed build, and a successful dry run do not prove provider authentication, full service rollout, native grant approval, or full transcript capture from a real logged-in harness.

Observed setup evidence on 2026-10-12: the native JSON proof and screenshot recorded the actual packaged run. A separate real-SSH transfer log recorded two imported records, the exact 50,024-character response preserved, zero queued records after acknowledgement, and zero duplicate imports on a second pull. The deliberately wrong base SSH profile failed with exit 255. No production service, grant, daemon restart, or provider login was performed. Local proof directories are omitted from this public account; review logs and screenshots before sharing them.

The reusable combined runner passed again on 2026-10-12 at 15:02 UTC, including its required `scripts/native-setup-transfer.mjs` check. Native JSON, transfer JSON, the screenshot, and driver and transfer logs were recorded in local proof directories. The native picker driver pumps AppKit activation notifications and types the directory through actual keyboard events; it refuses keyboard input when Fleet is not frontmost.

Both historical bundles passed Developer ID signature verification. Native behavior evidence is from the debug bundle, not a claim that release-only or distribution gates passed. Historical executable hashes do not identify a sanitized rebuild; record the actual hashes and signing result for each new candidate.

## Sanitized source verification

On 2026-10-12, `npm run check` passed bindings validation, TypeScript typechecking, all 42 Rust tests, and all-target Clippy. `npm run build:smoke` produced a locally signed debug bundle using private build settings. The actual packaged WKWebView preserved enrollment input during discovery, rejected unsafe links, excluded host metadata from default diagnostics, and rejected seven generic/configured NAS and Studio targets, including IPv4-mapped IPv6. Native JSON proof and an owned-window screenshot were captured outside the repository. No remote SSH connection or notarization ran in this verification.

The publication scanner was exercised with synthetic private addresses, a private node domain, a private-key marker and a provider-token pattern. It accepted documentation addresses and rejected those sensitive examples without printing their values. This remains a heuristic check, not an exhaustive secret detector.

## Explicitly unproved distribution/destructive gates

These remain unproved for local evaluation and must not be silently waived for a public release:

- [ ] Separate approval for notarization/public distribution and a named release owner.
- [ ] Explicit `notarize.sh` run using a stored Keychain profile. Record Accepted submission, staple validation and Gatekeeper assessment. No credential values in the repository or evidence.
- [ ] Fresh macOS account/OS installation of the signed, notarized release artifact, native dialogs, discovery, metadata recovery and real SSH.
- [ ] Actual reboot of a disposable controlled target, followed by correct ended-session detection. Never reboot an existing fleet node.
- [ ] Published tested compatibility matrix, artifact hashes, changes, known failures and binary/database rollback procedure.

Do not call this a public release until every applicable gate has real artifact evidence. Local signing alone does not prove notarization, fresh-profile installation, or process survival.

Before publishing source, run both [publication checks](../README.md#check-publication-contents): `python3 scripts/check-publication.py` and `python3 scripts/check-publication.py --history`. Review commit metadata and evidence manually. Never publish private local configuration, credentials, databases, backups, raw logs, screenshots, or transcripts.
