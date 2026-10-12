# Fleet recovery, rollback and security boundaries

## What a backup restores

A Fleet SQLite snapshot restores enrolled hosts and saved-session references. It does not restore SSH keys, agent state, passwords, host trust, terminal contents, remote tmux processes, or remote services. A saved reference remains unchecked until an explicit session check or attach. Missing identity, a different boot marker, a replacement session, or a dead original pane must not be presented as a resumed process.

The profile is the Tauri application-data directory for `dev.hiiro.fleet`, normally `~/Library/Application Support/dev.hiiro.fleet` on macOS. The database is `fleet.sqlite3`. Use the app information display as the authority for its actual path and configured backup directory. Debug builds may select a different profile through `FLEET_PROFILE_DIR`; release builds ignore that variable.

Keep backups private. They contain host labels, destinations, usernames, supported SSH overrides and session identity metadata. Use a user-controlled backup directory with appropriate filesystem access. Fleet does not encrypt backups or upload them.

The ignored root `.env` and optional `.fleet-ssh-config` are private local build and script inputs, not metadata-backup contents. Preserve them separately when moving your local development setup. Keep mode `0600`, retain trusted host-key aliases, and never publish their contents. See [local build settings](../README.md#develop-and-build) for signing and compile-time exclusions.

## Routine export and restore

1. Export through Fleet's native save dialog. Fleet uses SQLite's online backup mechanism, validates the exported snapshot, and replaces the selected destination only after a complete staged export. Do not copy a running `fleet.sqlite3` without its SQLite journal semantics.
2. Retain the matching app version and schema version with the backup. App information reports both versions.
3. Choose restore through the native file picker. Fleet copies a consistent snapshot into an app-owned temporary directory, validates it, makes it read-only and records its digest. The preview identifies the original selected file, schema and record counts.
4. Review the preview. Selecting a source does not replace current data. The source can later move or change without altering the staged snapshot.
5. Approve restore explicitly and then approve the native "Replace metadata" confirmation. Fleet rechecks the staged digest, schema and ticket generation, takes a backup of current data, closes handles and replaces the database through the Store's recovery-safe path.
6. Reopen the host and saved-session lists. Existing terminal tabs stay mounted; restore does not reconnect or start sessions. Late session writes from a prior database generation are rejected.
7. Check and attach a saved session explicitly. Confirm the original remote process rather than merely seeing a new shell.

Cancel any native picker or restore confirmation to leave current metadata unchanged. Canceling a preview disposes its one-use ticket. A consumed, canceled, expired or outdated ticket cannot be reused. Preview tickets expire after fifteen minutes; selecting again produces a new snapshot. App exit disposes outstanding tickets.

If restore fails, retain the current database, selected backup, and any Store-reported rollback/pre-restore backup locations. Do not repeatedly start replacement operations or delete journal files while Fleet is open. Inspect the error stage and code, then follow the offline procedure if the application cannot open its database.

## Corrupt or unsupported database

Fleet refuses a corrupt or unsupported newer database instead of silently recreating it. Startup may stop before the UI opens. Preserve the entire profile, including `fleet.sqlite3`, `fleet.sqlite3-wal`, and `fleet.sqlite3-shm` if present.

For an emergency offline restore:

1. Quit every Fleet instance using the profile. Confirm its local SSH transports have exited. Do not stop unrelated SSH processes or remote tmux sessions.
2. Make a private copy of the entire closed profile in a new recovery directory. Preserve permissions. Do not discard corrupt metadata or its sidecars.
3. Select a known Fleet backup produced by the compatible app. Work with a copy. With `/usr/bin/sqlite3`, open it read-only and run `PRAGMA integrity_check`, `PRAGMA application_id`, and `PRAGMA user_version`. Integrity must report `ok`; the application identity and schema must match that app's Store implementation. A SQLite file that passes integrity but is not a Fleet database is not a usable backup.
4. Install the matching app binary. Use a debug isolated profile to validate the backup through Fleet's actual restore picker before touching the production profile. For a release-only recovery, the release owner must validate with the matching source/toolchain rather than bypass schema checks.
5. With Fleet still closed, move the current database and any associated sidecars together into the preserved recovery directory. Copy the verified backup to a new temporary file in the original profile directory. Set private permissions, flush it, then rename it to `fleet.sqlite3`. Do not leave old WAL/SHM files at the restored database's name.
6. Start the matching app. Confirm the expected host and session records. Open no terminal automatically. Check session identity explicitly before attach.

Never run an older app against an unsupported newer schema. Do not change `user_version` to force an old app to accept new data. If no verified compatible backup exists, preserve the evidence and seek database-specific repair; do not replace it with an empty database.

## Upgrade and binary rollback

Before upgrading, export a verified backup through the current app and retain the current signed `.app`. Fleet's Store also takes and validates a pre-migration backup before a transactional schema change. A failed migration must preserve a recoverable database; an error reporting a backup location is not, by itself, proof that recovery was exercised.

Rollback pairs the old signed binary with its matching pre-upgrade database:

1. Quit the upgraded app and export/preserve the current profile. Metadata created after the old backup will not survive rollback. Remote processes are not rolled back.
2. Restore the matching database using a compatible app or the closed-profile procedure above. Do not leave newer WAL/SHM sidecars attached to the old snapshot.
3. Replace the binary with the retained older signed app. Verify its signature without removing quarantine or disabling Gatekeeper.
4. Reopen the metadata lists and explicitly check a saved session. Record the app/schema versions and exact surviving host/session identities.

There is no automatic updater or downgrade migration. Version changes require a recoverable binary/database pair. A real drill must verify both upgrade preservation and opening the matching old database with the old binary.

## Connection recovery

| Observed failure | Action |
| --- | --- |
| Discovery unavailable/offline | Preserve enrolled metadata. Use manual SSH independently and refresh discovery on request. |
| DNS/TCP/authentication failure | Correct the existing target, network or OpenSSH credentials outside Fleet. Retry explicitly; the app does not provision access. |
| Unknown host key | Verify the displayed algorithm and SHA256 fingerprint through a trusted channel, then accept or decline. |
| Changed host key | Stop. Verify the change externally. Fleet does not accept it or rewrite trust as a fallback. |
| Local app/SSH exit | Plain terminals have no process-survival promise. Explicitly reattach a validated saved tmux reference. |
| Missing tmux | Choose a warned plain terminal or arrange installation outside the app. Fleet never installs it. |
| Ended session/original pane or changed remote boot | Treat the process as ended. Create a replacement only through explicit user action. |
| Restore changed metadata while a terminal runs | Keep the terminal independent. Reject stale session writes; do not silently reinsert old metadata. |

Never reboot a production node for verification. Do not kill an existing user's program to create an ended-session test. Use uniquely named app-created tmux sessions or a controlled localhost SSH fixture. NAS is excluded. Studio remains the local hub.

## Security boundaries

Only the local `main` webview has Fleet's command capability. App command permissions are generated by `tauri-build::AppManifest`; plugin dialog/opener commands are not granted to JavaScript. Native file paths arrive through actual native dialogs, not arbitrary frontend path arguments. URLs are restricted to HTTP/HTTPS without embedded credentials and require a frontend user click. The webview denies external navigation and new-window requests. CSP blocks remote scripts, frames, object content and arbitrary web networking. Inline styles are allowed because the terminal/UI renderer needs them; inline JavaScript is not.

There is no generic process execution, shell, HTTP client, filesystem or eval IPC. Terminal operations are scoped to backend-issued connection/attempt IDs. SSH output is data, not an authorization signal. Configured local OpenSSH proxy programs remain trusted local input, not an app sandbox guarantee.

Debug-only smoke evaluation is a native startup option, not an IPC API. Its script is trusted developer input and may invoke the real app API. The proof handler accepts a bounded JSON object and writes only `smoke-proof.json` inside the selected app profile. It is registered only in debug builds, and its capability is added only for an explicitly selected script. Release builds omit that handler and the script/profile-override loaders. The transport's fixture SSH configuration override must also be debug-only.

Diagnostic exports exclude host metadata unless explicitly opted in. The diagnostics owner limits records to 256 entries and 24 hours, without terminal content, authentication answers, prompt text, key paths or raw SSH debugging. Do not share native smoke logs or screenshots without reviewing their potentially private host/session content. The smoke runner does not accept or store credentials intentionally.

## Current recovery evidence

The glue-authoring investigation verified that the approved Developer ID identity and native tools are installed. It did not apply files, build Fleet, launch a webview, exercise dialogs, perform a restore, upgrade, rollback, notarize, or reboot a machine. These are not passed checks. Attach actual proof paths and observed versions to the [release checklist](release-checklist.md) after running the integrated artifact. Local signing and a debug isolated-profile test do not establish fresh macOS-account installation or public release readiness.
