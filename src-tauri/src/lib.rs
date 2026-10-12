mod diagnostics;
mod discovery;
pub mod helper;
pub mod model;
mod setup;
mod store;
mod terminal;

use diagnostics::Diagnostics;
use model::*;
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use store::Store;
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    Manager, State, WebviewWindow,
};
use tauri_plugin_dialog::{DialogExt, FilePath, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tempfile::TempDir;
use terminal::TerminalHub;

struct RestoreTicket {
    directory: TempDir,
    original: PathBuf,
    digest: [u8; 32],
    generation: u64,
    created: Instant,
}
impl RestoreTicket {
    fn path(&self) -> PathBuf {
        self.directory.path().join("snapshot.sqlite3")
    }
}

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    diagnostics: Arc<Mutex<Diagnostics>>,
    hub: Arc<TerminalHub>,
    setup: Arc<setup::Setup>,
    tickets: Arc<Mutex<HashMap<RestoreTicketId, RestoreTicket>>>,
    staging: Arc<TempDir>,
    profile: PathBuf,
    #[cfg(debug_assertions)]
    smoke_enabled: bool,
}

fn storage_error(message: &'static str) -> AppError {
    AppError::new(Stage::Storage, ErrorCode::Storage, message)
}
fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, AppError> {
    mutex.lock().map_err(|_| {
        storage_error("A native operation failed. Quit and reopen Fleet before retrying.")
    })
}
fn local_url(url: &tauri::Url) -> bool {
    let app = ((url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (matches!(url.scheme(), "http" | "https") && url.host_str() == Some("tauri.localhost")))
        && url.port().is_none();
    #[cfg(debug_assertions)]
    let app = app
        || (url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(1420));
    app && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/" | "/index.html")
}
fn guard(window: &WebviewWindow) -> Result<(), AppError> {
    if window.label() == "main"
        && local_url(&window.url().map_err(|_| {
            AppError::new(
                Stage::Local,
                ErrorCode::Unavailable,
                "The local window is unavailable.",
            )
        })?)
    {
        Ok(())
    } else {
        Err(AppError::new(
            Stage::Local,
            ErrorCode::Declined,
            "Native operations are restricted to Fleet's local main window.",
        ))
    }
}
async fn background<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|_| {
        AppError::new(
            Stage::Local,
            ErrorCode::Unknown,
            "A native operation could not complete.",
        )
    })?
}
fn selected_path(path: FilePath) -> Result<PathBuf, AppError> {
    let path = path.into_path().map_err(|_| {
        AppError::new(
            Stage::Local,
            ErrorCode::InvalidInput,
            "Select a local filesystem path.",
        )
    })?;
    if !path.is_absolute() {
        return Err(AppError::new(
            Stage::Local,
            ErrorCode::InvalidInput,
            "Select an absolute local filesystem path.",
        ));
    }
    Ok(path)
}
fn protect_database(state: &AppState, destination: &Path) -> Result<(), AppError> {
    let database = lock(&state.store)?.path().to_path_buf();
    let equal = destination == database
        || match (destination.canonicalize(), database.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    let shm = PathBuf::from(format!("{}-shm", database.display()));
    if equal || destination == wal || destination == shm {
        return Err(AppError::new(
            Stage::Storage,
            ErrorCode::InvalidInput,
            "Choose a destination outside the active database and its journal files.",
        ));
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path)
        .map_err(|_| storage_error("The private application directory could not be created."))?;
    if fs::symlink_metadata(path)
        .map_err(|_| storage_error("The application directory could not be inspected."))?
        .file_type()
        .is_symlink()
    {
        return Err(storage_error(
            "The application directory must not be a symbolic link.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| {
            storage_error("The application directory permissions could not be set.")
        })?;
    }
    Ok(())
}
fn digest(path: &Path) -> Result<[u8; 32], AppError> {
    let mut file =
        File::open(path).map_err(|_| storage_error("The staged snapshot could not be read."))?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut bytes)
            .map_err(|_| storage_error("The staged snapshot could not be read."))?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(hash.finalize().into())
}
fn atomic_json(destination: &Path, value: &impl serde::Serialize) -> Result<(), AppError> {
    let parent = destination
        .parent()
        .ok_or_else(|| storage_error("The export destination has no directory."))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| storage_error("The private export file could not be created."))?;
    serde_json::to_writer_pretty(file.as_file_mut(), value)
        .map_err(|_| storage_error("The diagnostic bundle could not be serialized."))?;
    file.as_file_mut()
        .write_all(b"\n")
        .map_err(|_| storage_error("The export could not be written."))?;
    file.as_file()
        .sync_all()
        .map_err(|_| storage_error("The export could not be synchronized."))?;
    file.persist(destination)
        .map_err(|_| storage_error("The export could not replace the selected destination."))?;
    Ok(())
}
fn diagnostic_bundle(state: &AppState, include_hosts: bool) -> Result<DiagnosticBundle, AppError> {
    let hosts = if include_hosts {
        lock(&state.store)?.list_hosts()?
    } else {
        Vec::new()
    };
    Ok(lock(&state.diagnostics)?.bundle(hosts))
}

#[tauri::command]
async fn app_info(window: WebviewWindow, state: State<'_, AppState>) -> Result<AppInfo, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let (database, backups) = {
            let store = lock(&state.store)?;
            (store.path().to_path_buf(), store.backup_directory()?)
        };
        let db = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| storage_error("The current schema could not be inspected."))?;
        let schema_version = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|_| storage_error("The current schema could not be inspected."))?;
        drop(db);
        let (ssh_version, tailscale_version) = discovery::versions();
        Ok(AppInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            schema_version,
            database_path: database.to_string_lossy().into_owned(),
            backup_directory: backups.to_string_lossy().into_owned(),
            platform: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            ssh_version,
            tailscale_version,
        })
    })
    .await
}
#[tauri::command]
async fn list_hosts(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<Host>, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || lock(&state.store)?.list_hosts()).await
}
#[tauri::command]
async fn save_host(
    window: WebviewWindow,
    state: State<'_, AppState>,
    host_id: Option<HostId>,
    draft: HostDraft,
) -> Result<Host, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || lock(&state.store)?.save_host(host_id, draft)).await
}
#[tauri::command]
async fn delete_host(
    window: WebviewWindow,
    state: State<'_, AppState>,
    host_id: HostId,
) -> Result<(), AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || lock(&state.store)?.delete_host(&host_id)).await
}
#[tauri::command]
async fn discover(window: WebviewWindow) -> Result<DiscoverySnapshot, AppError> {
    guard(&window)?;
    background(discovery::discover).await
}
#[tauri::command]
fn allocate_connection(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<ConnectionLease, AppError> {
    guard(&window)?;
    Ok(state.hub.allocate())
}
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn connect(
    window: WebviewWindow,
    state: State<'_, AppState>,
    lease: ConnectionLease,
    host_id: HostId,
    intent: ConnectIntent,
    cols: u16,
    rows: u16,
    output: Channel<InvokeResponseBody>,
    events: Channel<ConnectionEvent>,
) -> Result<(), AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let (host, generation) = {
            let store = lock(&state.store)?;
            (store.host(&host_id)?, store.generation())
        };
        // The Store lock is released before a ticket is consumed or any process starts.
        match intent {
            ConnectIntent::HostSetup { ticket_id } => {
                let prepared = state.setup.consume(ticket_id, &host, generation)?;
                state
                    .hub
                    .connect_setup(lease, host, prepared, cols, rows, output, events)
            }
            intent => state
                .hub
                .connect(lease, host, intent, cols, rows, output, events),
        }
    })
    .await
}
#[tauri::command]
fn terminal_input(
    window: WebviewWindow,
    state: State<'_, AppState>,
    lease: ConnectionLease,
    bytes: Vec<u8>,
) -> Result<(), AppError> {
    guard(&window)?;
    state.hub.input(&lease, bytes)
}
#[tauri::command]
fn terminal_resize(
    window: WebviewWindow,
    state: State<'_, AppState>,
    lease: ConnectionLease,
    cols: u16,
    rows: u16,
) -> Result<(), AppError> {
    guard(&window)?;
    state.hub.resize(&lease, cols, rows)
}
#[tauri::command]
fn terminal_disconnect(
    window: WebviewWindow,
    state: State<'_, AppState>,
    lease: ConnectionLease,
) -> Result<(), AppError> {
    guard(&window)?;
    state.hub.disconnect(&lease)
}
#[tauri::command]
fn answer_challenge(
    window: WebviewWindow,
    state: State<'_, AppState>,
    lease: ConnectionLease,
    challenge_id: ChallengeId,
    answer: ChallengeAnswer,
) -> Result<(), AppError> {
    guard(&window)?;
    state.hub.answer(&lease, challenge_id, answer)
}
#[tauri::command]
fn acknowledge_output(
    window: WebviewWindow,
    state: State<'_, AppState>,
    lease: ConnectionLease,
    sequence: u32,
) -> Result<(), AppError> {
    guard(&window)?;
    state.hub.acknowledge(&lease, sequence)
}
#[tauri::command]
async fn list_sessions(
    window: WebviewWindow,
    state: State<'_, AppState>,
    host_id: Option<HostId>,
) -> Result<Vec<SavedSession>, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || lock(&state.store)?.list_sessions(host_id.as_ref())).await
}
#[tauri::command]
async fn check_session(
    window: WebviewWindow,
    state: State<'_, AppState>,
    session_id: SessionId,
) -> Result<SavedSession, AppError> {
    guard(&window)?;
    let hub = state.hub.clone();
    background(move || hub.check_session(session_id)).await
}
#[tauri::command]
async fn choose_backup_directory(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<OperationResult, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let Some(path) = window
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Choose Fleet backup directory")
            .blocking_pick_folder()
        else {
            return Ok(OperationResult::Canceled);
        };
        let path = selected_path(path)?;
        lock(&state.store)?.set_backup_directory(path.clone())?;
        Ok(OperationResult::Completed {
            path: path.to_string_lossy().into_owned(),
        })
    })
    .await
}
#[tauri::command]
async fn export_backup(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<OperationResult, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let directory = lock(&state.store)?.backup_directory()?;
        let Some(path) = window
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Export Fleet metadata backup")
            .set_directory(directory)
            .set_file_name(format!(
                "fleet-{}.sqlite3",
                chrono::Utc::now().format("%Y%m%d-%H%M%S")
            ))
            .add_filter("SQLite backup", &["sqlite3", "db"])
            .blocking_save_file()
        else {
            return Ok(OperationResult::Canceled);
        };
        let path = selected_path(path)?;
        protect_database(&state, &path)?;
        let parent = path
            .parent()
            .ok_or_else(|| storage_error("The backup destination has no directory."))?;
        let staging = tempfile::Builder::new()
            .prefix(".fleet-export-")
            .tempdir_in(parent)
            .map_err(|_| storage_error("The backup export could not be staged."))?;
        let snapshot = staging.path().join("backup.sqlite3");
        {
            let store = lock(&state.store)?;
            store.backup(&snapshot)?;
            store.validate_restore(&snapshot)?;
        }
        fs::rename(&snapshot, &path)
            .map_err(|_| storage_error("The selected backup destination could not be replaced."))?;
        Ok(OperationResult::Completed {
            path: path.to_string_lossy().into_owned(),
        })
    })
    .await
}
#[tauri::command]
async fn choose_restore(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<PickerResult<RestorePreview>, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let Some(path) = window
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Select Fleet metadata backup to preview")
            .add_filter("SQLite backup", &["sqlite3", "db"])
            .blocking_pick_file()
        else {
            return Ok(PickerResult::Canceled);
        };
        let original = selected_path(path)?;
        let directory = tempfile::Builder::new()
            .prefix("ticket-")
            .tempdir_in(state.staging.path())
            .map_err(|_| storage_error("A restore ticket directory could not be created."))?;
        let staged = directory.path().join("snapshot.sqlite3");
        let (schema_version, hosts, sessions, generation) = {
            let store = lock(&state.store)?;
            let (schema_version, hosts, sessions) = store.stage_restore(&original, &staged)?;
            (schema_version, hosts, sessions, store.generation())
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o400)).map_err(|_| {
                storage_error("The staged restore ticket could not be made read-only.")
            })?;
        }
        let hash = digest(&staged)?;
        let ticket_id = RestoreTicketId::new();
        let preview = RestorePreview {
            ticket_id,
            path: original.to_string_lossy().into_owned(),
            schema_version,
            hosts,
            sessions,
        };
        let mut tickets = lock(&state.tickets)?;
        tickets.retain(|_, ticket| ticket.created.elapsed() < Duration::from_secs(900));
        if tickets.len() >= 8 {
            return Err(AppError::new(
                Stage::Storage,
                ErrorCode::Busy,
                "Cancel an outstanding restore preview before opening another.",
            ));
        }
        tickets.insert(
            ticket_id,
            RestoreTicket {
                directory,
                original,
                digest: hash,
                generation,
                created: Instant::now(),
            },
        );
        Ok(PickerResult::Selected { value: preview })
    })
    .await
}
#[tauri::command]
async fn commit_restore(
    window: WebviewWindow,
    state: State<'_, AppState>,
    ticket_id: RestoreTicketId,
) -> Result<OperationResult, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        {
            let tickets = lock(&state.tickets)?;
            let ticket = tickets.get(&ticket_id).ok_or_else(|| AppError::new(Stage::Storage, ErrorCode::Canceled, "This restore preview was canceled or consumed."))?;
            if ticket.created.elapsed() >= Duration::from_secs(900) { return Err(AppError::new(Stage::Storage, ErrorCode::Canceled, "The restore preview expired. Select the backup again.")); }
        }
        let accepted = window.dialog().message("Replace all enrolled hosts and saved-session metadata with this previewed snapshot? Fleet first saves a backup of current metadata. Active terminals stay open. No SSH credentials or remote processes are restored.")
            .title("Replace Fleet metadata?").parent(&window).kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom("Replace metadata".into(), "Cancel".into())).blocking_show();
        if !accepted { lock(&state.tickets)?.remove(&ticket_id); return Ok(OperationResult::Canceled); }
        // Removing the ticket is the commit/cancel linearization point. A canceled
        // picker or consent callback cannot reach a Store write.
        let ticket = lock(&state.tickets)?.remove(&ticket_id).ok_or_else(|| AppError::new(Stage::Storage, ErrorCode::Canceled, "This restore preview was canceled."))?;
        if ticket.created.elapsed() >= Duration::from_secs(900) { return Err(AppError::new(Stage::Storage, ErrorCode::Canceled, "The restore preview expired. Select the backup again.")); }
        let path = ticket.path();
        if digest(&path)? != ticket.digest { return Err(AppError::new(Stage::Storage, ErrorCode::Changed, "The staged snapshot changed. Select the backup again.")); }
        let mut store = lock(&state.store)?;
        if store.generation() != ticket.generation { return Err(AppError::new(Stage::Storage, ErrorCode::Changed, "Metadata was restored since this preview. Select the backup again.")); }
        store.validate_restore(&path)?;
        store.restore(&path)?;
        Ok(OperationResult::Completed { path: ticket.original.to_string_lossy().into_owned() })
    }).await
}
#[tauri::command]
fn cancel_restore(
    window: WebviewWindow,
    state: State<'_, AppState>,
    ticket_id: RestoreTicketId,
) -> Result<(), AppError> {
    guard(&window)?;
    lock(&state.tickets)?.remove(&ticket_id);
    Ok(())
}
#[tauri::command]
async fn diagnostics_preview(
    window: WebviewWindow,
    state: State<'_, AppState>,
    include_host_metadata: bool,
) -> Result<DiagnosticBundle, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || diagnostic_bundle(&state, include_host_metadata)).await
}
#[tauri::command]
async fn export_diagnostics(
    window: WebviewWindow,
    state: State<'_, AppState>,
    include_host_metadata: bool,
) -> Result<OperationResult, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let Some(path) = window
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Export Fleet diagnostics")
            .set_file_name("fleet-diagnostics.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file()
        else {
            return Ok(OperationResult::Canceled);
        };
        let path = selected_path(path)?;
        protect_database(&state, &path)?;
        atomic_json(&path, &diagnostic_bundle(&state, include_host_metadata)?)?;
        Ok(OperationResult::Completed {
            path: path.to_string_lossy().into_owned(),
        })
    })
    .await
}
#[tauri::command]
fn open_link(window: WebviewWindow, url: String) -> Result<(), AppError> {
    guard(&window)?;
    let parsed = tauri::Url::parse(&url).map_err(|_| {
        AppError::new(
            Stage::Local,
            ErrorCode::InvalidInput,
            "The link is not a valid HTTP or HTTPS URL.",
        )
    })?;
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || url.chars().any(char::is_control)
    {
        return Err(AppError::new(
            Stage::Local,
            ErrorCode::InvalidInput,
            "Only HTTP and HTTPS links without embedded credentials may be opened.",
        ));
    }
    window
        .opener()
        .open_url(parsed.as_str(), None::<&str>)
        .map_err(|_| {
            AppError::new(
                Stage::Local,
                ErrorCode::Unavailable,
                "The system browser could not open the link.",
            )
        })
}

#[tauri::command]
async fn choose_setup_checkout(
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<PickerResult<SetupCheckout>, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let Some(path) = window
            .dialog()
            .file()
            .set_parent(&window)
            .set_title("Choose the Inbox folder (the tuios-inbox checkout)")
            .blocking_pick_folder()
        else {
            return Ok(PickerResult::Canceled);
        };
        let value = state.setup.register_checkout(selected_path(path)?)?;
        Ok(PickerResult::Selected { value })
    })
    .await
}
#[tauri::command]
async fn prepare_host_setup(
    window: WebviewWindow,
    state: State<'_, AppState>,
    host_id: HostId,
    checkout_id: Option<SetupCheckoutId>,
    alias: String,
    action: SetupAction,
) -> Result<SetupPrepared, AppError> {
    guard(&window)?;
    let state = state.inner().clone();
    background(move || {
        let (host, generation) = {
            let store = lock(&state.store)?;
            (store.host(&host_id)?, store.generation())
        };
        let plan = state
            .setup
            .plan(&host, generation, checkout_id, &alias, &action)?;
        let accepted = window
            .dialog()
            .message(plan.message.clone())
            .title(plan.title.clone())
            .parent(&window)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                plan.button.into(),
                "Cancel".into(),
            ))
            .blocking_show();
        if !accepted {
            return Ok(SetupPrepared::Canceled);
        }
        let (ticket_id, label, guidance) = state.setup.issue(plan)?;
        Ok(SetupPrepared::Ready {
            ticket_id,
            label,
            guidance,
        })
    })
    .await
}

#[cfg(debug_assertions)]
#[tauri::command]
async fn smoke_result(
    window: WebviewWindow,
    state: State<'_, AppState>,
    proof: serde_json::Value,
) -> Result<(), AppError> {
    guard(&window)?;
    if !state.smoke_enabled {
        return Err(AppError::new(
            Stage::Local,
            ErrorCode::Declined,
            "Native smoke recording is not enabled for this run.",
        ));
    }
    let bytes = serde_json::to_vec(&proof)
        .map_err(|_| storage_error("The smoke proof could not be serialized."))?;
    if bytes.len() > 1024 * 1024 {
        return Err(AppError::new(
            Stage::Local,
            ErrorCode::InvalidInput,
            "The smoke proof exceeds the one-megabyte limit.",
        ));
    }
    let destination = state.profile.join("smoke-proof.json");
    background(move || atomic_json(&destination, &proof)).await
}

pub fn run() {
    #[cfg(debug_assertions)]
    let builder = tauri::Builder::default().invoke_handler(tauri::generate_handler![
        app_info,
        list_hosts,
        save_host,
        delete_host,
        discover,
        allocate_connection,
        connect,
        terminal_input,
        terminal_resize,
        terminal_disconnect,
        answer_challenge,
        acknowledge_output,
        list_sessions,
        check_session,
        choose_backup_directory,
        export_backup,
        choose_restore,
        commit_restore,
        cancel_restore,
        diagnostics_preview,
        export_diagnostics,
        open_link,
        choose_setup_checkout,
        prepare_host_setup,
        smoke_result
    ]);
    #[cfg(not(debug_assertions))]
    let builder = tauri::Builder::default().invoke_handler(tauri::generate_handler![
        app_info,
        list_hosts,
        save_host,
        delete_host,
        discover,
        allocate_connection,
        connect,
        terminal_input,
        terminal_resize,
        terminal_disconnect,
        answer_challenge,
        acknowledge_output,
        list_sessions,
        check_session,
        choose_backup_directory,
        export_backup,
        choose_restore,
        commit_restore,
        cancel_restore,
        diagnostics_preview,
        export_diagnostics,
        open_link,
        choose_setup_checkout,
        prepare_host_setup
    ]);
    let app = builder.plugin(tauri_plugin_dialog::init()).plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let profile = app.path().app_data_dir()?;
            #[cfg(debug_assertions)]
            let profile = match std::env::var_os("FLEET_PROFILE_DIR") {
                Some(value) => { let path = PathBuf::from(value); if !path.is_absolute() { return Err("FLEET_PROFILE_DIR must be absolute".into()); } path },
                None => profile,
            };
            private_directory(&profile)?;
            let store = Arc::new(Mutex::new(Store::open(profile.join("fleet.sqlite3"))?));
            let diagnostics = Arc::new(Mutex::new(Diagnostics::new()));
            let hub = Arc::new(TerminalHub::new(store.clone(), diagnostics.clone()));
            let staging = Arc::new(tempfile::Builder::new().prefix("restore-staging-").tempdir_in(&profile)?);
            #[cfg(debug_assertions)]
            let smoke_script = match std::env::var_os("FLEET_SMOKE_SCRIPT") {
                Some(path) => {
                    let metadata = fs::metadata(&path)?;
                    if !metadata.is_file() || metadata.len() > 1024 * 1024 { return Err("FLEET_SMOKE_SCRIPT must be a JavaScript file no larger than one megabyte".into()); }
                    Some(Arc::new(fs::read_to_string(path)?))
                },
                None => None,
            };
            #[cfg(debug_assertions)]
            if smoke_script.is_some() {
                app.add_capability(tauri::ipc::CapabilityBuilder::new("debug-native-smoke")
                    .webview("main").permission("allow-smoke-result"))?;
            }
            let setup = Arc::new(setup::Setup::new(profile.clone()));
            app.manage(AppState { store, diagnostics, hub, setup,
                tickets: Arc::new(Mutex::new(HashMap::new())), staging, profile,
                #[cfg(debug_assertions)] smoke_enabled: smoke_script.is_some(),
            });
            let window = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
                .title("Fleet").inner_size(1280.0, 820.0).min_inner_size(880.0, 560.0)
                .on_navigation(local_url)
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny);
            #[cfg(debug_assertions)]
            let window = {
                let evaluated = Arc::new(std::sync::atomic::AtomicBool::new(false));
                window.on_page_load(move |webview, payload| {
                    if payload.event() == tauri::webview::PageLoadEvent::Finished && local_url(payload.url()) {
                        if let Some(script) = &smoke_script {
                            if !evaluated.swap(true, std::sync::atomic::Ordering::AcqRel)
                                && webview.eval(script.as_str()).is_err()
                            {
                                eprintln!("Fleet native smoke JavaScript could not be evaluated.");
                            }
                        }
                    }
                })
            };
            window.build()?;
            Ok(())
        }).build(tauri::generate_context!()).expect("Fleet could not initialize its native application. Existing metadata was not discarded.");
    app.run(|handle, event| {
        let closing = matches!(event, tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. })
            || matches!(event, tauri::RunEvent::WindowEvent { ref label, event: tauri::WindowEvent::Destroyed, .. } if label == "main");
        if closing {
            if let Some(state) = handle.try_state::<AppState>() {
                state.hub.shutdown();
                state.setup.clear();
                if let Ok(mut tickets) = state.tickets.lock() { tickets.clear(); }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::local_url;

    #[test]
    fn packaged_root_and_only_trusted_main_documents_are_authorized() {
        for value in [
            "tauri://localhost",
            "tauri://localhost/",
            "tauri://localhost/index.html",
            "https://tauri.localhost/",
        ] {
            assert!(local_url(&tauri::Url::parse(value).unwrap()), "{value}");
        }
        for value in [
            "https://example.com/",
            "tauri://other/",
            "tauri://localhost:443/",
            "tauri://user@localhost/",
            "tauri://localhost/other.html",
            "file:///index.html",
            "http://127.0.0.1:1421/",
        ] {
            assert!(!local_url(&tauri::Url::parse(value).unwrap()), "{value}");
        }
    }
}
