use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const MAX_REQUEST: usize = 16 * 1024;
const MAX_RESPONSE: usize = 1022;
const ENDPOINT_LIFETIME: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Role {
    Ready,
    Prompt,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Envelope {
    nonce: String,
    request_id: Uuid,
    role: Role,
    prompt: Option<String>,
    hint: Option<String>,
}

pub(crate) struct Request {
    pub role: Role,
    pub prompt: Option<String>,
    pub hint: Option<String>,
    stream: UnixStream,
}
impl Request {
    pub fn reply(mut self, value: Option<&[u8]>) -> io::Result<()> {
        self.stream.set_nonblocking(false)?;
        self.stream
            .set_write_timeout(Some(Duration::from_secs(1)))?;
        let value = value.filter(|v| v.len() <= MAX_RESPONSE);
        self.stream.write_all(&[u8::from(value.is_none())])?;
        let bytes = value.unwrap_or_default();
        self.stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
        self.stream.write_all(bytes)
    }
}

struct Pending {
    stream: UnixStream,
    bytes: Vec<u8>,
    started: Instant,
}

pub(crate) struct Endpoint {
    directory: TempDir,
    listener: UnixListener,
    pub log: File,
    prompt_nonce: String,
    ready_nonce: String,
    ready_used: bool,
    seen: HashSet<Uuid>,
    pending: Vec<Pending>,
    created: Instant,
}

fn nonce() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn equal_nonce(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.as_bytes()
            .iter()
            .zip(b.as_bytes())
            .fold(0u8, |n, (x, y)| n | (x ^ y))
            == 0
}
fn valid_nonce(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn same_user(stream: &UnixStream) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid == unsafe { libc::geteuid() })
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut credentials as *mut _ as *mut libc::c_void,
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.uid == unsafe { libc::geteuid() })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Peer credentials are unsupported on this platform",
        ))
    }
}

impl Endpoint {
    pub fn new() -> io::Result<Self> {
        // A short, owner-only path also fits Darwin's sockaddr_un limit.
        let directory = tempfile::Builder::new()
            .prefix("fleet-")
            .tempdir_in("/tmp")?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let listener = UnixListener::bind(directory.path().join("socket"))?;
        fs::set_permissions(
            directory.path().join("socket"),
            fs::Permissions::from_mode(0o600),
        )?;
        listener.set_nonblocking(true)?;
        let path = directory.path().join("client-log");
        let cpath =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
        if unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // A FIFO, not a file: DEBUG2 bytes are bounded in the kernel and never stored on disk.
        let log = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        Ok(Self {
            directory,
            listener,
            log,
            prompt_nonce: nonce(),
            ready_nonce: nonce(),
            ready_used: false,
            seen: HashSet::new(),
            pending: Vec::new(),
            created: Instant::now(),
        })
    }

    pub fn configure(&self, command: &mut Command) -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let path = executable.to_str().ok_or(io::ErrorKind::InvalidInput)?;
        let local = format!(
            "exec /usr/bin/env FLEET_HELPER_MODE=ready FLEET_HELPER_NONCE={} {} --fleet-ready",
            self.ready_nonce,
            quote(path)
        );
        command
            .env("SSH_ASKPASS", &executable)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env_remove("SSH_ASKPASS_PROMPT")
            .env("FLEET_HELPER_MODE", "askpass")
            .env("FLEET_HELPER_SOCKET", self.directory.path().join("socket"))
            .env("FLEET_HELPER_NONCE", &self.prompt_nonce)
            .arg("-E")
            .arg(self.directory.path().join("client-log"))
            .arg("-o")
            .arg("LogLevel=DEBUG2")
            .arg("-o")
            .arg("PermitLocalCommand=yes")
            .arg("-o")
            .arg(format!("LocalCommand={}", local.replace('%', "%%")));
        Ok(())
    }

    pub fn poll(&mut self) -> io::Result<Vec<Request>> {
        for _ in 0..16 {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if self.created.elapsed() >= ENDPOINT_LIFETIME
                        || self.pending.len() >= 8
                        || !same_user(&stream)?
                    {
                        continue;
                    }
                    stream.set_nonblocking(true)?;
                    self.pending.push(Pending {
                        stream,
                        bytes: Vec::new(),
                        started: Instant::now(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let mut requests = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let mut remove = self.pending[i].started.elapsed() > Duration::from_secs(5);
            let mut complete = false;
            let mut chunk = [0u8; 1024];
            if !remove {
                match self.pending[i].stream.read(&mut chunk) {
                    Ok(0) => remove = true,
                    Ok(n) => {
                        self.pending[i].bytes.extend_from_slice(&chunk[..n]);
                        remove = self.pending[i].bytes.len() > MAX_REQUEST;
                        complete = self.pending[i].bytes.contains(&b'\n');
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => remove = true,
                }
            }
            if !remove && !complete {
                i += 1;
                continue;
            }
            let mut pending = self.pending.swap_remove(i);
            if remove {
                pending.bytes.zeroize();
                continue;
            }
            let parsed = serde_json::from_slice::<Envelope>(&pending.bytes);
            pending.bytes.zeroize();
            let Ok(mut envelope) = parsed else {
                continue;
            };
            let expected = match envelope.role {
                Role::Ready => &self.ready_nonce,
                Role::Prompt => &self.prompt_nonce,
            };
            let valid = equal_nonce(&envelope.nonce, expected)
                && self.created.elapsed() < ENDPOINT_LIFETIME
                && !self.seen.contains(&envelope.request_id)
                && self.seen.len() < 64
                && match envelope.role {
                    Role::Ready => {
                        !self.ready_used && envelope.prompt.is_none() && envelope.hint.is_none()
                    }
                    Role::Prompt => {
                        envelope.prompt.as_ref().is_some_and(|p| {
                            !p.is_empty() && p.len() <= 12 * 1024 && !p.contains('\0')
                        }) && matches!(
                            envelope.hint.as_deref(),
                            None | Some("confirm") | Some("none")
                        )
                    }
                };
            envelope.nonce.zeroize();
            if !valid {
                continue;
            }
            self.seen.insert(envelope.request_id);
            if envelope.role == Role::Ready {
                self.ready_used = true;
            }
            requests.push(Request {
                role: envelope.role,
                prompt: envelope.prompt,
                hint: envelope.hint,
                stream: pending.stream,
            });
        }
        Ok(requests)
    }
}

fn private_socket(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.file_name().is_none_or(|n| n != "socket") {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let directory = fs::symlink_metadata(parent)?;
    let socket = fs::symlink_metadata(path)?;
    let uid = unsafe { libc::geteuid() };
    if !directory.is_dir()
        || directory.uid() != uid
        || directory.mode() & 0o777 != 0o700
        || !socket.file_type().is_socket()
        || socket.uid() != uid
        || socket.mode() & 0o777 != 0o600
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn run_helper() -> io::Result<i32> {
    let path = std::env::var_os("FLEET_HELPER_SOCKET").ok_or(io::ErrorKind::PermissionDenied)?;
    let mut nonce =
        std::env::var("FLEET_HELPER_NONCE").map_err(|_| io::ErrorKind::PermissionDenied)?;
    if !valid_nonce(&nonce) {
        nonce.zeroize();
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mode = std::env::var("FLEET_HELPER_MODE").unwrap_or_default();
    let (role, prompt, hint) = match mode.as_str() {
        "ready" if arguments.as_slice() == ["--fleet-ready"] => (Role::Ready, None, None),
        "askpass"
            if arguments.len() == 1
                && !arguments[0].is_empty()
                && arguments[0].len() <= 12 * 1024 =>
        {
            (
                Role::Prompt,
                Some(arguments[0].clone()),
                std::env::var("SSH_ASKPASS_PROMPT").ok(),
            )
        }
        _ => {
            nonce.zeroize();
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    };
    let path = Path::new(&path);
    private_socket(path)?;
    let mut stream = UnixStream::connect(path)?;
    if !same_user(&stream)? {
        nonce.zeroize();
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_read_timeout(Some(Duration::from_secs(130)))?;
    let mut envelope = Envelope {
        nonce,
        request_id: Uuid::new_v4(),
        role,
        prompt,
        hint,
    };
    let mut bytes =
        Zeroizing::new(serde_json::to_vec(&envelope).map_err(|_| io::ErrorKind::InvalidData)?);
    envelope.nonce.zeroize();
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(
        header[1..5]
            .try_into()
            .map_err(|_| io::ErrorKind::InvalidData)?,
    ) as usize;
    if header[0] != 0 || length > MAX_RESPONSE {
        return Ok(1);
    }
    let mut response = Zeroizing::new(vec![0u8; length]);
    stream.read_exact(&mut response)?;
    if role == Role::Prompt {
        if response.iter().any(|c| matches!(*c, b'\n' | b'\r' | 0)) {
            return Ok(1);
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(&response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(0)
}

pub fn run_if_helper() -> Option<i32> {
    let requested = [
        "FLEET_HELPER_MODE",
        "FLEET_HELPER_SOCKET",
        "FLEET_HELPER_NONCE",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some())
        || std::env::args_os()
            .skip(1)
            .any(|arg| arg == "--fleet-ready");
    requested.then(|| run_helper().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn role_credentials_cannot_cross_authorize() {
        let prompt = "a".repeat(64);
        let ready = "b".repeat(64);
        assert!(valid_nonce(&prompt));
        assert!(!equal_nonce(&prompt, &ready));
        assert!(!equal_nonce(&prompt, &format!("{}b", "a".repeat(63))));
        assert!(!valid_nonce(&"a".repeat(63)));
    }
    #[test]
    fn unknown_helper_roles_and_request_fields_fail_closed() {
        let id = Uuid::nil();
        let valid = format!(
            r#"{{"nonce":"{}","requestId":"{}","role":"ready","prompt":null,"hint":null}}"#,
            "a".repeat(64),
            id
        );
        assert_eq!(
            serde_json::from_str::<Envelope>(&valid).unwrap().role,
            Role::Ready
        );
        assert!(serde_json::from_str::<Envelope>(&valid.replace("ready", "execute")).is_err());
        assert!(serde_json::from_str::<Envelope>(
            &valid.replace("\"hint\":null", "\"command\":\"true\",\"hint\":null")
        )
        .is_err());
    }
}
