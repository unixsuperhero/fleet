use crate::model::{
    AppError, ErrorCode, Host, HostDraft, HostId, SavedSession, SessionId, SessionStatus, Stage,
};
use rusqlite::{
    backup::{Backup, StepResult},
    params, Connection, OpenFlags, OptionalExtension,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const APPLICATION_ID: u32 = 0x464c4554;
pub const SCHEMA_VERSION: u32 = 2;
const HOST_TABLE: &str = "CREATE TABLE hosts(id TEXT PRIMARY KEY NOT NULL, node_id TEXT, data TEXT NOT NULL) WITHOUT ROWID";
const SESSION_TABLE: &str = "CREATE TABLE sessions(id TEXT PRIMARY KEY NOT NULL, host_id TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE, data TEXT NOT NULL) WITHOUT ROWID";
const SETTINGS_TABLE: &str =
    "CREATE TABLE settings(key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL) WITHOUT ROWID";
const NODE_INDEX: &str =
    "CREATE UNIQUE INDEX hosts_node_id ON hosts(node_id) WHERE node_id IS NOT NULL";

pub struct Store {
    path: PathBuf,
    connection: Option<Connection>,
    generation: u64,
    _lock: File,
}

fn error(code: ErrorCode, message: impl Into<String>) -> AppError {
    AppError::new(Stage::Storage, code, message)
}

fn sql_error(operation: &str, failure: rusqlite::Error) -> AppError {
    let detail = match failure {
        rusqlite::Error::SqliteFailure(code, _) => format!("SQLite error {}", code.extended_code),
        other => other.to_string(),
    };
    error(ErrorCode::Storage, format!("{operation}: {detail}"))
}

fn io_error(operation: &str, failure: std::io::Error) -> AppError {
    error(ErrorCode::Storage, format!("{operation}: {failure}"))
}

fn corrupt(message: impl Into<String>) -> AppError {
    error(ErrorCode::CorruptBackup, message)
}

fn decode<T: DeserializeOwned>(data: &str) -> Result<T, AppError> {
    serde_json::from_str(data)
        .map_err(|_| corrupt("Stored metadata has an invalid format; the database was preserved."))
}

fn encode<T: Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|_| error(ErrorCode::Storage, "Cannot encode metadata."))
}

fn private_directory(path: &Path) -> Result<(), AppError> {
    if !path.exists() {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(path)
            .map_err(|e| io_error("Create private database directory", e))?;
    }
    if !path.is_dir() {
        return Err(error(
            ErrorCode::InvalidInput,
            "The selected directory is not a directory.",
        ));
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    let filename = path
        .file_name()
        .ok_or_else(|| error(ErrorCode::InvalidInput, "Select a database file."))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent
        .canonicalize()
        .map_err(|e| io_error("Resolve database directory", e))?
        .join(filename))
}

fn reject_symlink(path: &Path) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => Err(error(
            ErrorCode::InvalidInput,
            "A database must be a regular file, not a symlink or directory.",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("Inspect database file", e)),
    }
}

struct TemporaryDatabase {
    path: PathBuf,
    keep: bool,
}

impl TemporaryDatabase {
    fn new(parent: &Path, purpose: &str) -> Result<Self, AppError> {
        let path = parent.join(format!(
            ".fleet-{purpose}-{}.sqlite",
            Uuid::new_v4().simple()
        ));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| io_error("Reserve private staging database", e))?;
        Ok(Self { path, keep: false })
    }
}

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(sidecar(&self.path, "-wal"));
            let _ = fs::remove_file(sidecar(&self.path, "-shm"));
            let _ = fs::remove_file(sidecar(&self.path, "-journal"));
        }
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn sync_file(path: &Path) -> Result<(), AppError> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_error("Sync database file", e))
}

fn sync_directory(path: &Path) -> Result<(), AppError> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_error("Sync database directory", e))
}

fn reader(path: &Path) -> Result<Connection, AppError> {
    reject_symlink(path)?;
    if !path.is_file() {
        return Err(error(
            ErrorCode::Missing,
            "The selected database does not exist.",
        ));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| corrupt("The selected file cannot be opened as a SQLite database."))?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|e| sql_error("Set backup read timeout", e))?;
    conn.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")
        .map_err(|_| corrupt("Cannot safely read the selected database."))?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<(), AppError> {
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| sql_error("Set storage timeout", e))?;
    conn.execute_batch(
        "PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;",
    )
    .map_err(|e| sql_error("Configure metadata database", e))?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .map_err(|e| sql_error("Enable metadata journal", e))?;
    if mode != "wal" {
        return Err(error(
            ErrorCode::Storage,
            "SQLite could not enable the metadata journal.",
        ));
    }
    Ok(())
}

fn normalized_sql(sql: &str) -> String {
    sql.chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != ';')
        .flat_map(char::to_lowercase)
        .collect()
}

fn validate_schema(conn: &Connection, version: u32) -> Result<(), AppError> {
    let mut expected = vec![
        ("table", "hosts", HOST_TABLE),
        ("table", "sessions", SESSION_TABLE),
        ("table", "settings", SETTINGS_TABLE),
    ];
    if version == 2 {
        expected.push(("index", "hosts_node_id", NODE_INDEX));
    }
    let mut stmt = conn.prepare("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")
        .map_err(|_| corrupt("The database schema cannot be read."))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| corrupt("The database schema cannot be read."))?;
    let mut actual = Vec::new();
    for row in rows {
        actual.push(row.map_err(|_| corrupt("The database schema is invalid."))?);
    }
    if actual.len() != expected.len() {
        return Err(corrupt(
            "The file is not a supported Fleet metadata database.",
        ));
    }
    for (kind, name, sql) in actual {
        let wanted = expected
            .iter()
            .find(|(k, n, _)| *k == kind && *n == name)
            .ok_or_else(|| {
                corrupt("The metadata database contains an unsupported schema object.")
            })?;
        if normalized_sql(&sql) != normalized_sql(wanted.2) {
            return Err(corrupt(format!(
                "The {name} schema is incompatible with this app."
            )));
        }
    }
    Ok(())
}

fn read_hosts(conn: &Connection) -> Result<Vec<Host>, AppError> {
    let mut stmt = conn
        .prepare("SELECT id,node_id,data FROM hosts ORDER BY id")
        .map_err(|e| sql_error("Read hosts", e))?;
    let mut rows = stmt.query([]).map_err(|e| sql_error("Read hosts", e))?;
    let mut hosts = Vec::new();
    let mut nodes = BTreeSet::new();
    while let Some(row) = rows.next().map_err(|e| sql_error("Read hosts", e))? {
        let id: String = row.get(0).map_err(|e| sql_error("Read host identity", e))?;
        let node: Option<String> = row.get(1).map_err(|e| sql_error("Read node identity", e))?;
        let host: Host = decode(
            &row.get::<_, String>(2)
                .map_err(|e| sql_error("Read host metadata", e))?,
        )?;
        if host.id.to_string() != id || host.input.tailnet_node_id != node {
            return Err(corrupt("Host identity disagrees with its metadata."));
        }
        crate::discovery::validate_draft(&host.input)
            .map_err(|_| corrupt("Stored host settings are invalid or excluded."))?;
        if crate::discovery::is_excluded_draft(&host.input) {
            return Err(corrupt(
                "The database contains an excluded NAS or local hub record.",
            ));
        }
        if let Some(node) = node {
            if !nodes.insert(node) {
                return Err(corrupt("A tailnet node is enrolled more than once."));
            }
        }
        if chrono::DateTime::parse_from_rfc3339(&host.created_at).is_err()
            || chrono::DateTime::parse_from_rfc3339(&host.updated_at).is_err()
        {
            return Err(corrupt("Stored host timestamps are invalid."));
        }
        hosts.push(host);
    }
    Ok(hosts)
}

fn read_sessions(
    conn: &Connection,
    host_id: Option<&HostId>,
) -> Result<Vec<SavedSession>, AppError> {
    let host_key = host_id.map(ToString::to_string);
    let mut stmt = conn
        .prepare(
            "SELECT id,host_id,data FROM sessions WHERE (?1 IS NULL OR host_id=?1) ORDER BY id",
        )
        .map_err(|e| sql_error("Read saved sessions", e))?;
    let mut rows = stmt
        .query(params![host_key])
        .map_err(|e| sql_error("Read saved sessions", e))?;
    let mut sessions = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| sql_error("Read saved sessions", e))?
    {
        let id: String = row
            .get(0)
            .map_err(|e| sql_error("Read session identity", e))?;
        let host: String = row.get(1).map_err(|e| sql_error("Read session host", e))?;
        let session: SavedSession = decode(
            &row.get::<_, String>(2)
                .map_err(|e| sql_error("Read saved session", e))?,
        )?;
        if session.id.to_string() != id || session.host_id.to_string() != host {
            return Err(corrupt(
                "Saved session identity disagrees with its metadata.",
            ));
        }
        validate_session(&session)
            .map_err(|_| corrupt("Saved session metadata is invalid or excluded."))?;
        sessions.push(session);
    }
    Ok(sessions)
}

fn validate_session(session: &SavedSession) -> Result<(), AppError> {
    crate::discovery::validate_draft(&session.ssh_binding)?;
    if crate::discovery::is_excluded_draft(&session.ssh_binding) {
        return Err(error(
            ErrorCode::InvalidInput,
            "NAS and the local hub cannot be session targets.",
        ));
    }
    if session.remote_session_name != format!("fleet-{}", session.id.0.simple()) {
        return Err(error(
            ErrorCode::InvalidInput,
            "A saved session must use its app-owned remote name.",
        ));
    }
    if session.label.trim().is_empty()
        || session.label.len() > 128
        || session.label.chars().any(char::is_control)
        || chrono::DateTime::parse_from_rfc3339(&session.created_at).is_err()
    {
        return Err(error(
            ErrorCode::InvalidInput,
            "Saved session label or timestamp is invalid.",
        ));
    }
    Ok(())
}

fn validate_database(conn: &Connection) -> Result<(u32, u32, u32), AppError> {
    let identity: u32 = conn
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .map_err(|_| corrupt("The file has no readable SQLite application identity."))?;
    if identity != APPLICATION_ID {
        return Err(corrupt("This file is not a Fleet metadata database."));
    }
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|_| corrupt("The database schema version is invalid."))?;
    if version > SCHEMA_VERSION {
        return Err(error(
            ErrorCode::NewerSchema,
            format!(
                "Schema {version} requires a newer Fleet app; this app supports {SCHEMA_VERSION}."
            ),
        ));
    }
    if version == 0 {
        return Err(corrupt(
            "The database has no supported Fleet schema version.",
        ));
    }
    let mut integrity = conn
        .prepare("PRAGMA integrity_check")
        .map_err(|_| corrupt("SQLite integrity validation failed."))?;
    let mut rows = integrity
        .query([])
        .map_err(|_| corrupt("SQLite integrity validation failed."))?;
    let first = rows
        .next()
        .map_err(|_| corrupt("SQLite integrity validation failed."))?
        .ok_or_else(|| corrupt("SQLite returned no integrity result."))?;
    let result: String = first
        .get(0)
        .map_err(|_| corrupt("SQLite returned an invalid integrity result."))?;
    if result != "ok"
        || rows
            .next()
            .map_err(|_| corrupt("SQLite integrity validation failed."))?
            .is_some()
    {
        return Err(corrupt(
            "SQLite reports database corruption; the original file was preserved.",
        ));
    }
    drop(rows);
    drop(integrity);
    validate_schema(conn, version)?;
    let mut foreign_keys = conn
        .prepare("PRAGMA foreign_key_check")
        .map_err(|_| corrupt("Cannot validate saved-session host references."))?;
    if foreign_keys
        .query([])
        .map_err(|_| corrupt("Cannot validate saved-session host references."))?
        .next()
        .map_err(|_| corrupt("Cannot validate saved-session host references."))?
        .is_some()
    {
        return Err(corrupt("A saved session refers to a missing host."));
    }
    let hosts = read_hosts(conn)?;
    let sessions = read_sessions(conn, None)?;
    let host_count =
        u32::try_from(hosts.len()).map_err(|_| corrupt("The backup contains too many hosts."))?;
    let session_count = u32::try_from(sessions.len())
        .map_err(|_| corrupt("The backup contains too many sessions."))?;
    Ok((version, host_count, session_count))
}

fn online_backup(source: &Connection, destination: &Path) -> Result<(), AppError> {
    let mut output = Connection::open_with_flags(
        destination,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| sql_error("Open staged backup", e))?;
    output
        .busy_timeout(Duration::from_millis(100))
        .map_err(|e| sql_error("Set backup timeout", e))?;
    {
        let backup = Backup::new(source, &mut output)
            .map_err(|e| sql_error("Begin consistent SQLite backup", e))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() >= deadline {
                return Err(error(
                    ErrorCode::Timeout,
                    "The consistent SQLite backup timed out; current metadata is unchanged.",
                ));
            }
            match backup
                .step(256)
                .map_err(|e| sql_error("Copy consistent SQLite backup", e))?
            {
                StepResult::Done => break,
                StepResult::More => thread::yield_now(),
                StepResult::Busy | StepResult::Locked => thread::sleep(Duration::from_millis(20)),
                _ => {
                    return Err(error(
                        ErrorCode::Storage,
                        "SQLite returned an unsupported backup state.",
                    ))
                }
            }
        }
    }
    let mode: String = output
        .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
        .map_err(|e| sql_error("Finalize standalone backup journal", e))?;
    if mode != "delete" {
        return Err(error(
            ErrorCode::Storage,
            "The backup could not be made standalone.",
        ));
    }
    validate_database(&output)?;
    output
        .close()
        .map_err(|(_, e)| sql_error("Close verified backup", e))?;
    sync_file(destination)
}

fn migrate(conn: &Connection, from: u32) -> Result<(), AppError> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| sql_error("Begin metadata migration", e))?;
    if from == 1 {
        tx.execute_batch(NODE_INDEX)
            .map_err(|e| sql_error("Enforce unique tailnet enrollment", e))?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| sql_error("Record schema version", e))?;
    tx.execute("INSERT INTO settings(key,value) VALUES('schema_app_version',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [env!("CARGO_PKG_VERSION")])
        .map_err(|e| sql_error("Record migration app version", e))?;
    validate_database(&tx)?;
    tx.commit()
        .map_err(|e| sql_error("Commit metadata migration", e))
}

fn open_existing(path: &Path) -> Result<Connection, AppError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| sql_error("Reopen metadata database", e))?;
    validate_database(&conn)?;
    configure(&conn)?;
    Ok(conn)
}

impl Store {
    pub fn open(path: PathBuf) -> Result<Self, AppError> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        private_directory(parent)?;
        let path = absolute_path(&path)?;
        reject_symlink(&path)?;
        let lock_path = sidecar(&path, ".lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(lock_path)
            .map_err(|e| io_error("Open profile ownership lock", e))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(error(
                ErrorCode::Busy,
                "Another Fleet process is already using this profile.",
            ));
        }
        let existing = path.exists();
        if !existing {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| io_error("Create private metadata database", e))?;
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| sql_error("Open metadata database; original file preserved", e))?;
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|e| sql_error("Set startup storage timeout", e))?;
        conn.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON;")
            .map_err(|e| sql_error("Configure safe metadata reads", e))?;
        let _ = crate::discovery::discover();
        if existing {
            let (version, _, _) = validate_database(&conn)?;
            if version < SCHEMA_VERSION {
                let directory = path.parent().unwrap().join("backups");
                private_directory(&directory)?;
                let backup_path = directory.join(format!(
                    "fleet-preupgrade-app-{}-schema-{version}-{}.sqlite",
                    env!("CARGO_PKG_VERSION"),
                    Uuid::new_v4().simple()
                ));
                export_connection(&conn, &backup_path)?;
                migrate(&conn, version).map_err(|e| error(ErrorCode::MigrationFailed, format!("Migration failed: {}. Verified pre-upgrade backup: {}. Original metadata was preserved.", e.message, backup_path.display())))?;
            }
        } else {
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| sql_error("Begin new profile", e))?;
            for definition in [HOST_TABLE, SESSION_TABLE, SETTINGS_TABLE, NODE_INDEX] {
                tx.execute_batch(definition)
                    .map_err(|e| sql_error("Create metadata schema", e))?;
            }
            tx.pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(|e| sql_error("Record app identity", e))?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(|e| sql_error("Record initial schema", e))?;
            tx.execute(
                "INSERT INTO settings(key,value) VALUES('schema_app_version',?1)",
                [env!("CARGO_PKG_VERSION")],
            )
            .map_err(|e| sql_error("Record initial app version", e))?;
            tx.commit()
                .map_err(|e| sql_error("Commit new profile", e))?;
            validate_database(&conn)?;
        }
        configure(&conn)?;
        let store = Self {
            path,
            connection: Some(conn),
            generation: 0,
            _lock: lock,
        };
        // Saved references are observations, not proof of a surviving remote process.
        store.reset_session_observations()?;
        Ok(store)
    }

    fn conn(&self) -> Result<&Connection, AppError> {
        self.connection.as_ref().ok_or_else(|| error(ErrorCode::Storage, "The database is closed after a restore failure. Use the reported verified recovery backup; no replacement empty profile was created."))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn list_hosts(&self) -> Result<Vec<Host>, AppError> {
        read_hosts(self.conn()?)
    }

    pub fn host(&self, id: &HostId) -> Result<Host, AppError> {
        let data: Option<String> = self
            .conn()?
            .query_row(
                "SELECT data FROM hosts WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_error("Read selected host", e))?;
        decode(&data.ok_or_else(|| {
            error(
                ErrorCode::Missing,
                "The selected host is no longer enrolled.",
            )
        })?)
    }

    pub fn save_host(&self, id: Option<HostId>, draft: HostDraft) -> Result<Host, AppError> {
        crate::discovery::validate_draft(&draft)?;
        crate::discovery::guard_enrollment(&draft)?;
        let now = chrono::Utc::now().to_rfc3339();
        let host = match id {
            Some(id) => {
                let previous = self.host(&id)?;
                Host {
                    id,
                    input: draft,
                    created_at: previous.created_at,
                    updated_at: now,
                }
            }
            None => Host {
                id: HostId(Uuid::new_v4()),
                input: draft,
                created_at: now.clone(),
                updated_at: now,
            },
        };
        let key = host.id.to_string();
        if let Some(node) = &host.input.tailnet_node_id {
            let enrolled: Option<String> = self
                .conn()?
                .query_row(
                    "SELECT id FROM hosts WHERE node_id=?1 AND id<>?2",
                    params![node, key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| sql_error("Check node enrollment", e))?;
            if enrolled.is_some() {
                return Err(error(
                    ErrorCode::InvalidInput,
                    "This tailnet node is already enrolled; edit its existing host instead.",
                ));
            }
        }
        self.conn()?.execute("INSERT INTO hosts(id,node_id,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET node_id=excluded.node_id,data=excluded.data",
            params![key, host.input.tailnet_node_id, encode(&host)?]).map_err(|e| sql_error("Save explicit host enrollment", e))?;
        Ok(host)
    }

    pub fn delete_host(&self, id: &HostId) -> Result<(), AppError> {
        let deleted = self
            .conn()?
            .execute("DELETE FROM hosts WHERE id=?1", [id.to_string()])
            .map_err(|e| sql_error("Delete host and its saved references", e))?;
        if deleted == 0 {
            return Err(error(ErrorCode::Missing, "The host is no longer enrolled."));
        }
        Ok(())
    }

    pub fn list_sessions(&self, host_id: Option<&HostId>) -> Result<Vec<SavedSession>, AppError> {
        read_sessions(self.conn()?, host_id)
    }

    pub fn session(&self, id: &SessionId) -> Result<SavedSession, AppError> {
        let data: Option<String> = self
            .conn()?
            .query_row(
                "SELECT data FROM sessions WHERE id=?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_error("Read saved session", e))?;
        decode(
            &data
                .ok_or_else(|| error(ErrorCode::Missing, "The saved session no longer exists."))?,
        )
    }

    pub fn save_session(
        &self,
        session: SavedSession,
        expected_generation: u64,
    ) -> Result<(), AppError> {
        if self.generation != expected_generation {
            return Err(error(ErrorCode::Canceled, "Metadata was restored while this session operation was running; its late result was discarded."));
        }
        validate_session(&session)?;
        self.host(&session.host_id)?;
        let existing: Option<String> = self
            .conn()?
            .query_row(
                "SELECT host_id FROM sessions WHERE id=?1",
                [session.id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_error("Check saved-session ownership", e))?;
        if existing
            .as_deref()
            .is_some_and(|host| host != session.host_id.to_string())
        {
            return Err(error(
                ErrorCode::InvalidInput,
                "A saved session cannot move to another host.",
            ));
        }
        self.conn()?.execute("INSERT INTO sessions(id,host_id,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![session.id.to_string(), session.host_id.to_string(), encode(&session)?]).map_err(|e| sql_error("Save session reference", e))?;
        Ok(())
    }

    fn reset_session_observations(&self) -> Result<(), AppError> {
        reset_observations(self.conn()?)
    }

    pub fn backup_directory(&self) -> Result<PathBuf, AppError> {
        let directory: Option<String> = self
            .conn()?
            .query_row(
                "SELECT value FROM settings WHERE key='backup_directory'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql_error("Read backup directory preference", e))?;
        Ok(directory
            .map(PathBuf::from)
            .unwrap_or_else(|| self.path.parent().unwrap().join("backups")))
    }

    pub fn set_backup_directory(&self, path: PathBuf) -> Result<(), AppError> {
        private_directory(&path)?;
        let directory = path
            .canonicalize()
            .map_err(|e| io_error("Resolve backup directory", e))?;
        let value = directory.to_str().ok_or_else(|| {
            error(
                ErrorCode::InvalidInput,
                "The backup directory cannot be represented as a settings path.",
            )
        })?;
        self.conn()?.execute("INSERT INTO settings(key,value) VALUES('backup_directory',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [value])
            .map_err(|e| sql_error("Save backup directory preference", e))?;
        Ok(())
    }

    pub fn backup(&self, path: &Path) -> Result<(), AppError> {
        let destination = absolute_path(path)?;
        if destination == self.path {
            return Err(error(
                ErrorCode::InvalidInput,
                "A backup cannot replace the active metadata database.",
            ));
        }
        export_connection(self.conn()?, &destination)
    }

    pub fn validate_restore(&self, path: &Path) -> Result<(u32, u32, u32), AppError> {
        let conn = reader(path)?;
        validate_database(&conn)
    }

    // Glue uses this for app-owned, immutable restore tickets after its native picker.
    pub fn stage_restore(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<(u32, u32, u32), AppError> {
        let destination = absolute_path(destination)?;
        if destination == self.path {
            return Err(error(
                ErrorCode::InvalidInput,
                "Restore staging cannot replace the active database.",
            ));
        }
        reject_symlink(&destination)?;
        if destination.exists() {
            return Err(error(
                ErrorCode::InvalidInput,
                "Restore staging requires a new app-owned file.",
            ));
        }
        let source = reader(source)?;
        validate_database(&source)?;
        export_connection(&source, &destination)?;
        self.validate_restore(&destination)
    }

    pub fn restore(&mut self, path: &Path) -> Result<PathBuf, AppError> {
        self.restore_with_reopen(path, open_existing)
    }

    fn restore_with_reopen<F>(&mut self, source: &Path, mut reopen: F) -> Result<PathBuf, AppError>
    where
        F: FnMut(&Path) -> Result<Connection, AppError>,
    {
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| error(ErrorCode::Storage, "Metadata generation exhausted."))?;
        let _ = crate::discovery::discover();
        let parent = self.path.parent().unwrap().to_path_buf();
        let stage = TemporaryDatabase::new(&parent, "restore-stage")?;
        let source = reader(source)?;
        validate_database(&source)?;
        online_backup(&source, &stage.path)?;
        source
            .close()
            .map_err(|(_, e)| sql_error("Close restore source", e))?;
        {
            let candidate = Connection::open_with_flags(
                &stage.path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| sql_error("Open staged restore", e))?;
            candidate
                .execute_batch("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON;")
                .map_err(|e| sql_error("Configure staged restore", e))?;
            let (version, _, _) = validate_database(&candidate)?;
            if version < SCHEMA_VERSION {
                migrate(&candidate, version)?;
            }
            reset_observations(&candidate)?;
            // A backup from another profile does not choose this profile's export directory.
            let directory = self.backup_directory()?;
            let value = directory.to_str().ok_or_else(|| {
                error(
                    ErrorCode::InvalidInput,
                    "The current backup directory cannot be represented.",
                )
            })?;
            candidate.execute("INSERT INTO settings(key,value) VALUES('backup_directory',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [value])
                .map_err(|e| sql_error("Preserve local backup preference", e))?;
            validate_database(&candidate)?;
            candidate
                .close()
                .map_err(|(_, e)| sql_error("Close validated restore stage", e))?;
        }
        sync_file(&stage.path)?;
        let backup_directory = self.backup_directory()?;
        private_directory(&backup_directory)?;
        let recovery = backup_directory.join(format!(
            "fleet-prerestore-app-{}-schema-{}-{}.sqlite",
            env!("CARGO_PKG_VERSION"),
            SCHEMA_VERSION,
            Uuid::new_v4().simple()
        ));
        self.backup(&recovery)?;
        let (busy, _, _): (i64, i64, i64) = self
            .conn()?
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(|e| sql_error("Checkpoint current database before restore", e))?;
        if busy != 0 {
            return Err(error(
                ErrorCode::Busy,
                "The current database is busy; no metadata was replaced.",
            ));
        }
        let mode: String = self
            .conn()?
            .query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))
            .map_err(|e| sql_error("Close current WAL journal", e))?;
        if mode != "delete" {
            return Err(error(
                ErrorCode::Busy,
                "Cannot close the current journal; no metadata was replaced.",
            ));
        }
        let conn = self.connection.take().ok_or_else(|| {
            error(
                ErrorCode::Storage,
                "The current database is already closed.",
            )
        })?;
        if let Err((conn, failure)) = conn.close() {
            self.connection = Some(conn);
            return Err(sql_error(
                "Close all current database handles before restore",
                failure,
            ));
        }
        let mut rollback =
            TemporaryDatabase::new(&parent, "restore-rollback").map_err(|failure| {
                self.connection = open_existing(&self.path).ok();
                error(
                    ErrorCode::Storage,
                    format!(
                        "{}. Recovery backup: {}",
                        failure.message,
                        recovery.display()
                    ),
                )
            })?;
        let mut replaced = false;
        let result = (|| -> Result<Connection, AppError> {
            // This is a closed, checkpointed DELETE-journal file, never a live WAL copy.
            fs::copy(&self.path, &rollback.path)
                .map_err(|e| io_error("Preserve same-filesystem rollback database", e))?;
            sync_file(&rollback.path)?;
            let old = reader(&rollback.path)?;
            validate_database(&old)?;
            old.close()
                .map_err(|(_, e)| sql_error("Close rollback validation handle", e))?;
            for suffix in ["-wal", "-shm", "-journal"] {
                let path = sidecar(&self.path, suffix);
                if path.exists() {
                    return Err(error(
                        ErrorCode::Busy,
                        "A database journal remains after closure; replacement was canceled.",
                    ));
                }
            }
            fs::rename(&stage.path, &self.path)
                .map_err(|e| io_error("Atomically replace metadata database", e))?;
            replaced = true;
            sync_directory(&parent)?;
            reopen(&self.path)
        })();
        match result {
            Ok(conn) => {
                self.connection = Some(conn);
                self.generation = next_generation;
                Ok(recovery)
            }
            Err(failure) => {
                let rollback_result = (|| -> Result<Connection, AppError> {
                    if replaced {
                        // The failed candidate's connection has dropped; its journal cannot accompany the old file.
                        for suffix in ["-wal", "-shm", "-journal"] {
                            let path = sidecar(&self.path, suffix);
                            match fs::remove_file(path) {
                                Ok(()) => {}
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                Err(e) => {
                                    return Err(io_error(
                                        "Remove closed failed-candidate journal",
                                        e,
                                    ))
                                }
                            }
                        }
                        fs::rename(&rollback.path, &self.path).map_err(|e| {
                            io_error("Atomically roll back metadata replacement", e)
                        })?;
                        sync_directory(&parent)?;
                    }
                    reopen(&self.path)
                })();
                match rollback_result {
                    Ok(conn) => {
                        self.connection = Some(conn);
                        Err(error(ErrorCode::Storage, format!("Restore failed and original metadata was reopened: {}. Verified recovery backup: {}", failure.message, recovery.display())))
                    }
                    Err(rollback_failure) => {
                        rollback.keep = true;
                        Err(error(ErrorCode::Storage, format!("Restore failed: {}. Reopening the original also failed: {}. Database is closed, not replaced with an empty profile. Verified recovery backup: {}; local rollback: {}", failure.message, rollback_failure.message, recovery.display(), rollback.path.display())))
                    }
                }
            }
        }
    }
}

fn reset_observations(conn: &Connection) -> Result<(), AppError> {
    let sessions = read_sessions(conn, None)?;
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| sql_error("Begin session-observation reset", e))?;
    for mut session in sessions {
        session.status = SessionStatus::Unchecked;
        tx.execute(
            "UPDATE sessions SET data=?1 WHERE id=?2",
            params![encode(&session)?, session.id.to_string()],
        )
        .map_err(|e| sql_error("Reset saved session observation", e))?;
    }
    tx.commit()
        .map_err(|e| sql_error("Commit session-observation reset", e))
}

fn export_connection(conn: &Connection, path: &Path) -> Result<(), AppError> {
    reject_symlink(path)?;
    let path = absolute_path(path)?;
    // Never leave an older destination journal attached to a new exported file.
    for suffix in ["-wal", "-shm", "-journal"] {
        if sidecar(&path, suffix).exists() {
            return Err(error(
                ErrorCode::Busy,
                "The selected destination has a SQLite journal; choose an inactive backup file.",
            ));
        }
    }
    let stage = TemporaryDatabase::new(path.parent().unwrap(), "backup")?;
    online_backup(conn, &stage.path)?;
    fs::rename(&stage.path, &path)
        .map_err(|e| io_error("Publish verified backup atomically", e))?;
    sync_directory(path.parent().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Profile(PathBuf);
    impl Profile {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("fleet-store-tests-{}", Uuid::new_v4().simple()));
            private_directory(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn draft(label: &str) -> HostDraft {
        HostDraft {
            label: label.into(),
            ssh_target: "fleet-test-remote.invalid".into(),
            port_override: None,
            host_name_override: None,
            host_key_alias_override: None,
            tailnet_node_id: None,
        }
    }

    fn saved(host: &Host) -> SavedSession {
        let id = SessionId(Uuid::new_v4());
        SavedSession {
            remote_session_name: format!("fleet-{}", id.0.simple()),
            id,
            host_id: host.id,
            label: "counter".into(),
            ssh_binding: host.input.clone(),
            identity: None,
            status: SessionStatus::Live,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn online_backup_includes_committed_wal_and_restore_references() {
        let p = Profile::new();
        let source = Store::open(p.path("source.sqlite")).unwrap();
        let host = source.save_host(None, draft("original label")).unwrap();
        let session = saved(&host);
        source
            .save_session(session.clone(), source.generation())
            .unwrap();
        // A second real connection commits metadata into WAL, without closing the Store.
        let writer = Connection::open(source.path()).unwrap();
        let mut changed = host.clone();
        changed.input.label = "committed in WAL".into();
        writer
            .execute(
                "UPDATE hosts SET data=?1 WHERE id=?2",
                params![encode(&changed).unwrap(), host.id.to_string()],
            )
            .unwrap();
        source.backup(&p.path("export.sqlite")).unwrap();
        assert_eq!(
            source.validate_restore(&p.path("export.sqlite")).unwrap(),
            (2, 1, 1)
        );
        let mut target = Store::open(p.path("target.sqlite")).unwrap();
        let recovery = target.restore(&p.path("export.sqlite")).unwrap();
        assert_eq!(
            target.host(&host.id).unwrap().input.label,
            "committed in WAL"
        );
        let restored = target.session(&session.id).unwrap();
        assert_eq!(restored.remote_session_name, session.remote_session_name);
        assert!(matches!(restored.status, SessionStatus::Unchecked));
        assert_eq!(target.validate_restore(&recovery).unwrap(), (2, 0, 0));
    }

    #[test]
    fn invalid_corrupt_foreign_and_newer_restore_preserve_current_data() {
        let p = Profile::new();
        let mut store = Store::open(p.path("current.sqlite")).unwrap();
        let host = store.save_host(None, draft("keep this host")).unwrap();
        let generation = store.generation();
        fs::write(
            p.path("corrupt.sqlite"),
            b"not sqlite, preserve these bytes",
        )
        .unwrap();
        let foreign = Connection::open(p.path("foreign.sqlite")).unwrap();
        foreign
            .execute_batch("CREATE TABLE unrelated(value TEXT)")
            .unwrap();
        drop(foreign);
        store.backup(&p.path("newer.sqlite")).unwrap();
        let newer = Connection::open(p.path("newer.sqlite")).unwrap();
        newer.pragma_update(None, "user_version", 999).unwrap();
        drop(newer);
        for (name, code) in [
            ("corrupt.sqlite", ErrorCode::CorruptBackup),
            ("foreign.sqlite", ErrorCode::CorruptBackup),
            ("newer.sqlite", ErrorCode::NewerSchema),
        ] {
            let failure = store.restore(&p.path(name)).unwrap_err();
            assert_eq!(failure.code, code);
            assert_eq!(store.host(&host.id).unwrap().input.label, "keep this host");
            assert_eq!(store.generation(), generation);
        }
        assert_eq!(
            fs::read(p.path("corrupt.sqlite")).unwrap(),
            b"not sqlite, preserve these bytes"
        );
    }

    #[test]
    fn empty_existing_database_is_not_silently_initialized() {
        let p = Profile::new();
        fs::write(p.path("empty.sqlite"), b"").unwrap();
        let failure = match Store::open(p.path("empty.sqlite")) {
            Ok(_) => panic!("Existing empty database was replaced"),
            Err(e) => e,
        };
        assert_eq!(failure.code, ErrorCode::CorruptBackup);
        assert_eq!(fs::read(p.path("empty.sqlite")).unwrap(), b"");
    }

    #[test]
    fn backup_integrity_schema_and_references_are_required() {
        let p = Profile::new();
        let store = Store::open(p.path("current.sqlite")).unwrap();
        let host = store.save_host(None, draft("expected host")).unwrap();
        store
            .save_session(saved(&host), store.generation())
            .unwrap();
        store.backup(&p.path("missing-host.sqlite")).unwrap();
        let candidate = Connection::open(p.path("missing-host.sqlite")).unwrap();
        candidate
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        candidate.execute("DELETE FROM hosts", []).unwrap();
        drop(candidate);
        assert_eq!(
            store
                .validate_restore(&p.path("missing-host.sqlite"))
                .unwrap_err()
                .code,
            ErrorCode::CorruptBackup
        );
        store.backup(&p.path("bad-schema.sqlite")).unwrap();
        let candidate = Connection::open(p.path("bad-schema.sqlite")).unwrap();
        candidate.execute_batch("DROP INDEX hosts_node_id").unwrap();
        drop(candidate);
        assert_eq!(
            store
                .validate_restore(&p.path("bad-schema.sqlite"))
                .unwrap_err()
                .code,
            ErrorCode::CorruptBackup
        );
        store.backup(&p.path("truncated.sqlite")).unwrap();
        OpenOptions::new()
            .write(true)
            .open(p.path("truncated.sqlite"))
            .unwrap()
            .set_len(512)
            .unwrap();
        assert_eq!(
            store
                .validate_restore(&p.path("truncated.sqlite"))
                .unwrap_err()
                .code,
            ErrorCode::CorruptBackup
        );
        assert_eq!(store.host(&host.id).unwrap().input.label, "expected host");
    }

    #[test]
    fn staged_ticket_is_independent_of_later_source_changes() {
        let p = Profile::new();
        let source = Store::open(p.path("source.sqlite")).unwrap();
        let host = source.save_host(None, draft("selected snapshot")).unwrap();
        let mut target = Store::open(p.path("target.sqlite")).unwrap();
        assert_eq!(
            target
                .stage_restore(source.path(), &p.path("ticket.sqlite"))
                .unwrap(),
            (2, 1, 0)
        );
        let mut updated = host.input.clone();
        updated.label = "source changed after preview".into();
        source.save_host(Some(host.id), updated).unwrap();
        target.restore(&p.path("ticket.sqlite")).unwrap();
        assert_eq!(
            target.host(&host.id).unwrap().input.label,
            "selected snapshot"
        );
        assert_eq!(
            source.host(&host.id).unwrap().input.label,
            "source changed after preview"
        );
    }

    #[test]
    fn restore_generation_rejects_late_session_writes_without_reinsertion() {
        let p = Profile::new();
        let mut store = Store::open(p.path("current.sqlite")).unwrap();
        let host = store.save_host(None, draft("original host")).unwrap();
        let late = saved(&host);
        store.backup(&p.path("before-session.sqlite")).unwrap();
        let old_generation = store.generation();
        store.save_session(late.clone(), old_generation).unwrap();
        store.restore(&p.path("before-session.sqlite")).unwrap();
        assert_eq!(
            store
                .save_session(late.clone(), old_generation)
                .unwrap_err()
                .code,
            ErrorCode::Canceled
        );
        assert_eq!(
            store.session(&late.id).unwrap_err().code,
            ErrorCode::Missing
        );
        store
            .save_session(late.clone(), store.generation())
            .unwrap();
        assert_eq!(store.session(&late.id).unwrap().label, "counter");
    }

    #[test]
    fn failed_reopen_atomically_rolls_back_and_keeps_original_generation() {
        let p = Profile::new();
        let mut store = Store::open(p.path("current.sqlite")).unwrap();
        let host = store.save_host(None, draft("original survives")).unwrap();
        let candidate = Store::open(p.path("candidate.sqlite")).unwrap();
        candidate
            .save_host(None, draft("replacement must not survive"))
            .unwrap();
        candidate.backup(&p.path("replacement.sqlite")).unwrap();
        let original_generation = store.generation();
        let mut calls = 0;
        let failure = store
            .restore_with_reopen(&p.path("replacement.sqlite"), |path| {
                calls += 1;
                if calls == 1 {
                    Err(error(ErrorCode::Storage, "Injected reopen I/O failure"))
                } else {
                    open_existing(path)
                }
            })
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::Storage);
        assert_eq!(
            store
                .list_hosts()
                .unwrap()
                .iter()
                .map(|h| h.input.label.as_str())
                .collect::<Vec<_>>(),
            vec!["original survives"]
        );
        assert_eq!(
            store.host(&host.id).unwrap().input.label,
            "original survives"
        );
        assert_eq!(store.generation(), original_generation);
        store
            .backup(&p.path("usable-after-rollback.sqlite"))
            .unwrap();
        assert_eq!(
            store
                .validate_restore(&p.path("usable-after-rollback.sqlite"))
                .unwrap(),
            (2, 1, 0)
        );
    }

    #[test]
    fn upgrade_preserves_records_and_verified_matching_schema_backup() {
        let p = Profile::new();
        let store = Store::open(p.path("legacy.sqlite")).unwrap();
        let host = store.save_host(None, draft("legacy host")).unwrap();
        let session = saved(&host);
        store
            .save_session(session.clone(), store.generation())
            .unwrap();
        drop(store);
        let legacy = Connection::open(p.path("legacy.sqlite")).unwrap();
        legacy
            .execute_batch("DROP INDEX hosts_node_id; PRAGMA user_version=1;")
            .unwrap();
        drop(legacy);
        let upgraded = Store::open(p.path("legacy.sqlite")).unwrap();
        assert_eq!(upgraded.host(&host.id).unwrap().input.label, "legacy host");
        assert_eq!(
            upgraded.session(&session.id).unwrap().remote_session_name,
            session.remote_session_name
        );
        let backups = fs::read_dir(p.path("backups"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(upgraded.validate_restore(&backups[0]).unwrap(), (1, 1, 1));
        assert_eq!(
            upgraded.validate_restore(upgraded.path()).unwrap(),
            (2, 1, 1)
        );
    }

    #[test]
    fn excluded_targets_and_duplicate_node_rows_cannot_enter_metadata() {
        let p = Profile::new();
        let store = Store::open(p.path("current.sqlite")).unwrap();
        for target in [
            "nas",
            "user@nas.node.example.",
            "192.0.2.3",
            "2001:db8::3",
            "studio",
        ] {
            let mut input = draft("do not enroll");
            input.ssh_target = target.into();
            assert_eq!(
                store.save_host(None, input).unwrap_err().code,
                ErrorCode::InvalidInput
            );
        }
        let mut first = store.save_host(None, draft("allowed first")).unwrap();
        first.input.tailnet_node_id = Some("test-node".into());
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE hosts SET node_id=?1,data=?2 WHERE id=?3",
                params!["test-node", encode(&first).unwrap(), first.id.to_string()],
            )
            .unwrap();
        let mut second = store.save_host(None, draft("allowed second")).unwrap();
        second.input.tailnet_node_id = Some("test-node".into());
        assert!(store
            .conn()
            .unwrap()
            .execute(
                "UPDATE hosts SET node_id=?1,data=?2 WHERE id=?3",
                params!["test-node", encode(&second).unwrap(), second.id.to_string()]
            )
            .is_err());
        assert_eq!(
            store
                .host(&first.id)
                .unwrap()
                .input
                .tailnet_node_id
                .as_deref(),
            Some("test-node")
        );
        assert_eq!(store.host(&second.id).unwrap().input.tailnet_node_id, None);
    }
}
