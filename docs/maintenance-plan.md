# Independent fleet app maintenance plan

Status: operating plan for the authorized app implementation in the [implementation plan](implementation-plan.md). Historical native build and behavioral evidence, plus remaining acceptance gates, are recorded in the [release checklist](release-checklist.md). TUIOS remains independently usable. Signed local builds and safe existing test environments are selected; public notarization, real-OS reboot, and fresh macOS-account installation remain explicitly unproved distribution gates.

## Ownership and operating cost

Keep one local application and one release pipeline. Do not introduce an always-on fleet server or a cloud control plane. Remote machines retain their existing SSH service, Tailscale client, and optional tmux installation.

Assign a named release owner before shipping. The owner maintains application code, dependency updates, packaging, data migrations, diagnostics, and recovery documentation. The machine owner controls SSH accounts, host keys, firewall rules, tailnet policy, and remote software. Application maintenance must not silently change those settings.

Planning estimates, not measured costs:

| Work | Initial allowance | Trigger |
| --- | --- | --- |
| Dependency and security review | 1 to 2 hours per month | Scheduled review and published security advisories |
| Release and compatibility checks | Half a day per release | A candidate build or supported platform change |
| Backup restore and upgrade drill | 1 to 2 hours per quarter | Scheduled drill or database schema change |
| Terminal, SSH, or packaging regressions | Unbounded incident work | User-visible breakage or an urgent security fix |

Re-estimate after the first few releases. A dedicated app adds this work beyond maintaining the current TUIOS setup.

## Release and compatibility policy

Maintain a tested matrix with the actual app build, macOS version, CPU architecture, OpenSSH version, Tailscale CLI version, terminal renderer version, and remote tmux version. Mark combinations as tested, unsupported, or unknown. Do not imply that an untested combination works.

Keep Rust, Tauri, the terminal renderer, native PTY dependencies, and SQLite dependencies pinned through their normal lockfiles. Group routine updates into reviewable releases. Investigate urgent security advisories immediately and ship an affected fix as a separate release when needed.

Before each public release (a signed local evaluation build is not a public release):

1. Build the app from a tagged source revision with pinned dependencies.
2. Run automated checks for host parsing, connection transitions, schema migration, and diagnostic redaction.
3. Exercise the packaged app using the smoke scenarios below. Tests alone do not prove native terminal behavior.
4. Sign and notarize the macOS artifact. Keep signing credentials in the platform keychain or release-secret store, never the repository or app database.
5. Install the artifact on a clean profile. Verify the artifact version, signature, native dialogs, discovery, and a real SSH terminal.
6. Publish release notes with supported combinations, schema changes, known failures, and the rollback path.

Do not build an automatic updater initially. Add one only with verified signed downloads, failed-update recovery, and an explicit update policy.

## Behavior checks for every release

| Scenario | Required observation |
| --- | --- |
| Tailnet discovery and enrollment | Only selected nodes become enrolled hosts. Offline nodes and missing Tailscale produce accurate states. |
| Non-tailnet SSH target | Manual enrollment and connection work independently of discovery. |
| Existing SSH alias | The configured username, identity, proxy behavior, and port remain in use. |
| Unknown or changed host key | Unknown keys require informed consent. Changed keys block the connection. No silent trust update occurs. |
| TCP timeout or rejected credentials | The UI identifies the failed stage and preserves the host record. |
| Two simultaneous terminals | Input and output reach the correct host. Closing or disconnecting one does not affect the other. |
| Discovery refresh | Terminal output, scroll, selection, focus, and unfinished forms remain intact. |
| App restart and tmux reattach | The original remote process continues. A counter proves survival rather than a recreated shell. |
| Remote session exit or reboot | The saved reference is marked ended. No replacement starts without user action. |
| Resize, Unicode, and paste | Interactive terminal programs draw and receive input correctly. |
| Backup and restore | Native dialogs operate on the selected file. A clean profile recovers the expected metadata. |
| Invalid restore or canceled dialog | Current data remains unchanged and usable. |
| Upgrade and rollback | Compatible data survives upgrade. The previous app opens its matching restored database after rollback. |

Run destructive scenarios only on disposable targets or sessions created for verification. Never reboot an ordinary fleet node for a maintenance check.

## Database backup and migration

Back up enrolled host metadata and saved remote session references through SQLite's consistent backup mechanism. Do not copy a live database file without accounting for its journal. The backup contains hostnames and usernames, so treat it as private even though it contains no credentials.

Provide a native save dialog for exports and a native file picker for restores. If the user chooses a backup directory, use a native directory picker. Keep manual path entry optional. Do not export SSH private keys, agent material, or terminal transcripts.

Before a schema migration:

1. Create a consistent pre-upgrade database backup and record the app and schema versions.
2. Verify the backup can open and passes an integrity check.
3. Apply transactional migrations where supported.
4. On failure, preserve the previous data and show the recovery location. Do not repeatedly retry destructive steps on startup.

For restore, validate integrity and version compatibility in a temporary database before replacing current data. Take a backup of current data and close database handles before the final replacement. A restore requires explicit consent because it replaces metadata. Verify known host and session records in a fresh profile during each restore drill.

Do not run an older app against a newer unsupported schema. Roll back the binary together with the matching pre-upgrade database. Explain that metadata created after that backup is not retained by rollback.

## Failure recovery

| Failure | Recovery boundary |
| --- | --- |
| Tailnet offline | Keep enrolled hosts and saved session references. Allow manual SSH targets. Recheck discovery on request without resetting the UI. |
| SSH port blocked | Report network-stage failure. The machine owner checks service, firewall, and tailnet policy. Do not alter access controls. |
| Authentication failure | Preserve the connection target and report the error. Let the user repair existing SSH credentials outside the database. |
| Changed host key | Stop. Verify the replacement fingerprint through a trusted channel before the user changes trust. |
| App crash or local SSH exit | Release local process resources. Offer explicit reattachment to existing tmux sessions. Plain sessions have no survival guarantee. |
| Remote tmux session ended | Mark it ended and offer explicit creation of a new session. Never label a replacement as resumed. |
| Remote reboot | Reconnect only through user action and check whether the referenced session exists. Do not infer process survival. |
| Missing remote tmux | Offer a plain terminal with an accurate persistence warning. Installation remains the machine owner's choice. |
| Corrupt local database | Preserve the corrupt file for user-directed diagnosis. Restore a verified backup. Do not silently discard enrollment data. |
| Unsupported dependency version | Report the specific incompatibility and retain metadata. Avoid empty results presented as successful discovery. |

Do not reconnect automatically in a way that runs commands or duplicates sessions. If bounded background reachability checks are added later, they must leave active terminals and user input unchanged.

## Diagnostics and incident handling

Collect application version, connection stage, stable local host ID, duration, exit status, and bounded redacted error messages. Do not record authentication responses, private keys, passwords, access tokens, SSH-agent data, or terminal content by default. Hostnames and usernames also need review before support export.

Set a finite log size and retention period before release. Let the user preview and save a redacted diagnostic bundle through a native save dialog. No cloud upload is required or implied.

For an incident, capture the failed stage and dependency versions, reproduce on a safe target, and verify the fixed packaged artifact against the original scenario. Add a deterministic regression check for a plausible consumer-visible bug where a useful test boundary exists. Keep a security advisory handling path separate from ordinary feature work.

## Independence and migration from TUIOS

Maintain an app-owned database, settings, processes, and release channel. Do not use TUIOS configuration as live storage or depend on its daemon to create terminals.

Enroll existing SSH aliases by user selection. Optional remote setup is an explicit, confirmed operation, not an inventory import. If a TUIOS inventory importer is later requested, make it read-only with a preview. Never copy credentials, import every discovered node automatically, or apply native grants from an in-TUIOS process.

The user can keep TUIOS installed while evaluating the app. Removing TUIOS is not an acceptance criterion. Shared OpenSSH and Tailscale dependencies still need compatibility checks for both tools.

Keep Inbox's canonical installer as the owner of collector, hook, shell-marker, service, and queue changes. Check its `--ssh-config` support before preparing a launch. Host metadata, metadata generation, and checkout-content changes invalidate unused setup tickets. Test the actual packaged picker, confirmation cancellation, real SSH endpoint overrides, output framing, and owned-process cancellation before accepting setup changes.

Central sources retain paths into Fleet's private `setup-ssh` directory. Metadata backups alone do not preserve those profiles or the Inbox checkout and queues. Migration must retain or explicitly replace referenced profiles; do not delete an old profile merely because host metadata changed. No setup heartbeat may reset a terminal, host draft, or setup form.

## Stop conditions before release

Do not publicly release if the owner cannot demonstrate process-survival semantics, host-key protection, accurate connection failures or verified backup restoration. Public release also requires notarization, a clean macOS-account package install and a recoverable schema upgrade. The current user-approved local-build scope does not waive these distribution gates or turn unproved scenarios into successful checks.

Use verified SSH aliases from private local configuration or controlled fixtures for smoke targets. Preserve strict host-key checking. Do not publish live endpoint or routing records. TUIOS verification does not replace testing the independent app's packaged artifact.
