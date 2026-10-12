use crate::{
    diagnostics::Diagnostics,
    discovery,
    helper::{Endpoint, Request, Role},
    model::*,
    setup::{self, LocalStep, PreparedSetup, PrivateSsh, SetupRun},
    store::Store,
};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tauri::ipc::{Channel, InvokeResponseBody};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const WINDOW: usize = 512 * 1024;
const HEADER: usize = 36;
const CHUNK: usize = 16 * 1024;
const INPUT_CALL: usize = 64 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(180);

fn hold<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
fn error(stage: Stage, code: ErrorCode, text: &str) -> AppError {
    AppError::new(stage, code, text)
}
fn canceled() -> AppError {
    error(
        Stage::Local,
        ErrorCode::Canceled,
        "This connection attempt is no longer active.",
    )
}
fn native_io(_: io::Error) -> AppError {
    error(
        Stage::Local,
        ErrorCode::Unavailable,
        "The local OpenSSH terminal resources are unavailable.",
    )
}
fn valid_size(cols: u16, rows: u16) -> Result<(), AppError> {
    if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
        return Err(error(
            Stage::Local,
            ErrorCode::InvalidInput,
            "Terminal dimensions must be between 1 and 1000.",
        ));
    }
    Ok(())
}

struct Flow {
    next: Option<u32>,
    acknowledged: u32,
    pending: VecDeque<(u32, usize)>,
    bytes: usize,
}
impl Default for Flow {
    fn default() -> Self {
        Self {
            next: Some(1),
            acknowledged: 0,
            pending: VecDeque::new(),
            bytes: 0,
        }
    }
}
impl Flow {
    fn credit(&self) -> usize {
        WINDOW - self.bytes
    }
    fn reserve(&mut self, bytes: usize) -> Result<u32, AppError> {
        if bytes <= HEADER || bytes > self.credit() {
            return Err(error(
                Stage::Local,
                ErrorCode::Busy,
                "Terminal output is awaiting renderer acknowledgement.",
            ));
        }
        let sequence = self.next.ok_or_else(|| {
            error(
                Stage::Local,
                ErrorCode::Unavailable,
                "The terminal output sequence is exhausted. Open a new connection.",
            )
        })?;
        self.next = sequence.checked_add(1);
        self.pending.push_back((sequence, bytes));
        self.bytes += bytes;
        Ok(sequence)
    }
    fn acknowledge(&mut self, sequence: u32) -> Result<(), AppError> {
        if sequence == 0 {
            return Err(error(
                Stage::Local,
                ErrorCode::InvalidInput,
                "Output sequence zero was never sent.",
            ));
        }
        if sequence <= self.acknowledged {
            return Ok(());
        }
        if self.pending.front().map(|p| p.0) != Some(sequence) {
            return Err(error(Stage::Local, ErrorCode::InvalidInput, "Acknowledge each rendered output frame in order; unsent frames cannot release credit."));
        }
        if let Some((_, bytes)) = self.pending.pop_front() {
            self.bytes -= bytes;
        }
        self.acknowledged = sequence;
        Ok(())
    }
}

struct Answer {
    bytes: Option<Zeroizing<Vec<u8>>>,
    declined: bool,
}
struct Control {
    started: bool,
    finished: bool,
    live: bool,
    events: Option<Channel<ConnectionEvent>>,
    input: VecDeque<Zeroizing<Vec<u8>>>,
    input_offset: usize,
    input_bytes: usize,
    size: (u16, u16),
    resize_pending: bool,
    challenge: Option<Challenge>,
    answer: Option<Answer>,
    flow: Flow,
}
struct Attempt {
    lease: ConnectionLease,
    canceled: AtomicBool,
    setup: AtomicBool,
    delivery: Mutex<()>,
    control: Mutex<Control>,
    done: Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Attempt {
    fn new(lease: ConnectionLease) -> Self {
        Self {
            lease,
            canceled: AtomicBool::new(false),
            setup: AtomicBool::new(false),
            delivery: Mutex::new(()),
            control: Mutex::new(Control {
                started: false,
                finished: false,
                live: false,
                events: None,
                input: VecDeque::new(),
                input_offset: 0,
                input_bytes: 0,
                size: (80, 24),
                resize_pending: false,
                challenge: None,
                answer: None,
                flow: Flow::default(),
            }),
            done: Condvar::new(),
            worker: Mutex::new(None),
        }
    }
    fn matches(&self, lease: &ConnectionLease) -> bool {
        self.lease == *lease && !self.canceled.load(Ordering::Acquire)
    }
    fn active(&self) -> Result<(), AppError> {
        if self.canceled.load(Ordering::Acquire) {
            Err(canceled())
        } else {
            Ok(())
        }
    }
    fn emit_locked(&self, body: EventBody) -> Result<(), AppError> {
        let channel = hold(&self.control).events.clone();
        if let Some(channel) = channel {
            channel
                .send(ConnectionEvent {
                    lease: self.lease,
                    body,
                })
                .map_err(|_| {
                    error(
                        Stage::Local,
                        ErrorCode::Unavailable,
                        "The terminal view is no longer available.",
                    )
                })?;
        }
        Ok(())
    }
    fn emit(&self, body: EventBody) -> Result<(), AppError> {
        let _delivery = hold(&self.delivery);
        self.active()?;
        self.emit_locked(body)
    }
    fn state(&self, state: ConnectionState) -> Result<(), AppError> {
        self.emit(EventBody::State { state })
    }
    fn connected(&self, mode: ConnectionMode) -> Result<(), AppError> {
        let _delivery = hold(&self.delivery);
        self.active()?;
        hold(&self.control).live = true;
        self.emit_locked(EventBody::State {
            state: ConnectionState::Connected { mode },
        })
    }
    fn cancel(&self, reason: &str) {
        let _delivery = hold(&self.delivery);
        if self.canceled.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut control = hold(&self.control);
            control.live = false;
            control.input.clear();
            control.input_bytes = 0;
            control.challenge = None;
            control.answer = None;
            if !control.started {
                control.finished = true;
            }
        }
        let _ = self.emit_locked(EventBody::State {
            state: ConnectionState::Disconnected {
                reason: reason.into(),
            },
        });
        self.done.notify_all();
    }
    fn finish(&self) {
        let mut control = hold(&self.control);
        control.live = false;
        control.input.clear();
        control.input_bytes = 0;
        control.challenge = None;
        control.answer = None;
        control.finished = true;
        self.done.notify_all();
    }
    fn wait(&self) {
        let mut control = hold(&self.control);
        while !control.finished {
            control = self.done.wait(control).unwrap_or_else(|e| e.into_inner());
        }
        drop(control);
        if let Some(worker) = hold(&self.worker).take() {
            let _ = worker.join();
        }
    }
}
struct Completion(Arc<Attempt>);
impl Drop for Completion {
    fn drop(&mut self) {
        self.0.finish();
    }
}
struct ClearChallenge<'a>(&'a Attempt);
impl Drop for ClearChallenge<'_> {
    fn drop(&mut self) {
        let mut control = hold(&self.0.control);
        control.challenge = None;
        control.answer = None;
    }
}

#[allow(clippy::large_enum_variant)] // The worker owns its session; boxing adds an unnecessary allocation.
enum Plan {
    Plain,
    Create(String),
    Attach(SavedSession),
}
enum Work {
    Connect(Plan),
    Setup(PreparedSetup),
}
const SETUP_CANCEL: &str =
    "Disconnected locally. Setup commands were stopped; changes they already made may remain.";
pub struct TerminalHub {
    attempts: Mutex<HashMap<ConnectionId, Arc<Attempt>>>,
    store: Arc<Mutex<Store>>,
    diagnostics: Arc<Mutex<Diagnostics>>,
    closing: AtomicBool,
}
impl TerminalHub {
    pub fn new(store: Arc<Mutex<Store>>, diagnostics: Arc<Mutex<Diagnostics>>) -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
            store,
            diagnostics,
            closing: AtomicBool::new(false),
        }
    }
    pub fn allocate(&self) -> ConnectionLease {
        let lease = ConnectionLease {
            connection_id: ConnectionId::new(),
            attempt_id: AttemptId::new(),
        };
        let attempt = Arc::new(Attempt::new(lease));
        let mut attempts = hold(&self.attempts);
        if self.closing.load(Ordering::Acquire) {
            attempt.cancel("Fleet is closing.");
        }
        attempts.insert(lease.connection_id, attempt);
        lease
    }
    fn lookup(&self, lease: &ConnectionLease) -> Result<Arc<Attempt>, AppError> {
        let attempt = hold(&self.attempts)
            .get(&lease.connection_id)
            .cloned()
            .ok_or_else(canceled)?;
        if !attempt.matches(lease) {
            return Err(canceled());
        }
        Ok(attempt)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        &self,
        lease: ConnectionLease,
        host: Host,
        intent: ConnectIntent,
        cols: u16,
        rows: u16,
        output: Channel<InvokeResponseBody>,
        events: Channel<ConnectionEvent>,
    ) -> Result<(), AppError> {
        valid_size(cols, rows)?;
        validate_binding(&host.input)?;
        if self.closing.load(Ordering::Acquire) {
            return Err(canceled());
        }
        let attempt = self.lookup(&lease)?;
        let (host, generation, plan) = {
            let store = hold(&self.store);
            let current = store.host(&host.id)?;
            if current != host {
                return Err(error(Stage::Storage, ErrorCode::Canceled, "Host metadata changed before this operation started. Select the current host and retry."));
            }
            let plan = match intent {
                ConnectIntent::Plain => Plan::Plain,
                ConnectIntent::CreatePersistent { label } => {
                    if label.trim().is_empty()
                        || label.len() > 128
                        || label.chars().any(char::is_control)
                    {
                        return Err(error(
                            Stage::Session,
                            ErrorCode::InvalidInput,
                            "A persistent session needs a label of at most 128 characters.",
                        ));
                    }
                    Plan::Create(label)
                }
                ConnectIntent::AttachSaved { session_id } => {
                    let session = store.session(&session_id)?;
                    if session.host_id != host.id {
                        return Err(error(
                            Stage::Session,
                            ErrorCode::InvalidInput,
                            "The saved session belongs to another enrolled host.",
                        ));
                    }
                    validate_saved(&session)?;
                    Plan::Attach(session)
                }
                ConnectIntent::HostSetup { .. } => {
                    return Err(error(
                        Stage::Local,
                        ErrorCode::InvalidInput,
                        "Host setup starts only from its confirmed ticket.",
                    ))
                }
            };
            (current, store.generation(), plan)
        };
        self.launch(
            attempt,
            host,
            generation,
            Work::Connect(plan),
            cols,
            rows,
            output,
            events,
        )
    }
    /// Starts a confirmed setup. The caller has already consumed and validated its ticket.
    #[allow(clippy::too_many_arguments)]
    pub fn connect_setup(
        &self,
        lease: ConnectionLease,
        host: Host,
        prepared: PreparedSetup,
        cols: u16,
        rows: u16,
        output: Channel<InvokeResponseBody>,
        events: Channel<ConnectionEvent>,
    ) -> Result<(), AppError> {
        valid_size(cols, rows)?;
        validate_binding(&host.input)?;
        if self.closing.load(Ordering::Acquire) {
            return Err(canceled());
        }
        let attempt = self.lookup(&lease)?;
        let generation = {
            let store = hold(&self.store);
            if store.host(&host.id)? != host {
                return Err(error(Stage::Storage, ErrorCode::Canceled, "Host metadata changed before this operation started. Select the current host and retry."));
            }
            store.generation()
        };
        attempt.setup.store(true, Ordering::Release);
        self.launch(
            attempt,
            host,
            generation,
            Work::Setup(prepared),
            cols,
            rows,
            output,
            events,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn launch(
        &self,
        attempt: Arc<Attempt>,
        host: Host,
        generation: u64,
        work: Work,
        cols: u16,
        rows: u16,
        output: Channel<InvokeResponseBody>,
        events: Channel<ConnectionEvent>,
    ) -> Result<(), AppError> {
        {
            let _delivery = hold(&attempt.delivery);
            attempt.active()?;
            let mut control = hold(&attempt.control);
            if control.started {
                return Err(error(
                    Stage::Local,
                    ErrorCode::Busy,
                    "This lease has already started. Allocate a new lease for a new attempt.",
                ));
            }
            control.started = true;
            control.events = Some(events);
            control.size = (cols, rows);
        }
        let store = self.store.clone();
        let diagnostics = self.diagnostics.clone();
        let worker_attempt = attempt.clone();
        let host_id = host.id;
        match thread::Builder::new().name("fleet-ssh".into()).spawn(move || {
            let _completion = Completion(worker_attempt.clone());
            let epilogue = match &work {
                Work::Setup(prepared) => Some(prepared.epilogue.clone()),
                Work::Connect(_) => None,
            };
            let result = match work {
                Work::Connect(plan) => run_connection(&worker_attempt, &store, host, generation, plan, output),
                Work::Setup(prepared) => run_setup(&worker_attempt, &host, prepared, output),
            };
            hold(&worker_attempt.control).live = false;
            let (stage, code, exit_code) = match &result {
                Ok(code) => (Stage::Session, None, *code),
                Err(failure) => (failure.stage, Some(failure.code), None),
            };
            hold(&diagnostics).record(DiagnosticEntry { at: chrono::Utc::now().to_rfc3339(), host_id: Some(host_id), stage, code, exit_code });
            if worker_attempt.canceled.load(Ordering::Acquire) { return; }
            match result {
                Ok(exit_code) => {
                    let reason = match (&epilogue, exit_code) {
                        (Some(done), Some(0)) => done.clone(),
                        (Some(_), Some(code)) => format!("The setup command exited with code {code}. Changes it already made may remain; review the console."),
                        (Some(_), None) => "The setup command ended without an exit code. Changes it already made may remain; review the console.".into(),
                        (None, _) => "The remote terminal exited. Persistent sessions may be checked and attached explicitly.".into(),
                    };
                    let _ = worker_attempt.state(ConnectionState::Disconnected { reason });
                    let _ = worker_attempt.emit(EventBody::Exit { exit_code });
                }
                Err(failure) => { let _ = worker_attempt.state(ConnectionState::Failed { error: failure }); }
            }
        }) {
            Ok(worker) => { *hold(&attempt.worker) = Some(worker); Ok(()) }
            Err(_) => { attempt.finish(); Err(error(Stage::Local, ErrorCode::Unavailable, "The connection worker could not start.")) }
        }
    }
    pub fn input(&self, lease: &ConnectionLease, bytes: Vec<u8>) -> Result<(), AppError> {
        let bytes = Zeroizing::new(bytes);
        if bytes.len() > INPUT_CALL {
            return Err(error(
                Stage::Local,
                ErrorCode::InvalidInput,
                "Send terminal input in chunks of at most 64 KiB.",
            ));
        }
        let attempt = self.lookup(lease)?;
        let mut control = hold(&attempt.control);
        attempt.active()?;
        if !control.live {
            return Err(error(
                Stage::Session,
                ErrorCode::Busy,
                "Terminal input is unavailable until the authenticated remote session is ready.",
            ));
        }
        if control.input_bytes + bytes.len() > WINDOW {
            return Err(error(
                Stage::Local,
                ErrorCode::Busy,
                "The terminal input queue is full. Wait for the remote terminal to consume input.",
            ));
        }
        if !bytes.is_empty() {
            control.input_bytes += bytes.len();
            control.input.push_back(bytes);
        }
        Ok(())
    }
    pub fn resize(&self, lease: &ConnectionLease, cols: u16, rows: u16) -> Result<(), AppError> {
        valid_size(cols, rows)?;
        let attempt = self.lookup(lease)?;
        let mut control = hold(&attempt.control);
        attempt.active()?;
        control.size = (cols, rows);
        control.resize_pending = true;
        Ok(())
    }
    pub fn disconnect(&self, lease: &ConnectionLease) -> Result<(), AppError> {
        let attempt = hold(&self.attempts).get(&lease.connection_id).cloned();
        let Some(attempt) = attempt else {
            return Ok(());
        };
        if attempt.lease != *lease {
            return Err(canceled());
        }
        attempt.cancel(if attempt.setup.load(Ordering::Acquire) {
            SETUP_CANCEL
        } else {
            "Disconnected locally. No remote tmux session was deleted."
        });
        attempt.wait();
        let mut attempts = hold(&self.attempts);
        if attempts
            .get(&lease.connection_id)
            .is_some_and(|a| Arc::ptr_eq(a, &attempt))
        {
            attempts.remove(&lease.connection_id);
        }
        Ok(())
    }
    pub fn answer(
        &self,
        lease: &ConnectionLease,
        challenge_id: ChallengeId,
        answer: ChallengeAnswer,
    ) -> Result<(), AppError> {
        let attempt = self.lookup(lease)?;
        let mut control = hold(&attempt.control);
        attempt.active()?;
        let challenge = control
            .challenge
            .as_ref()
            .filter(|c| c.id == challenge_id)
            .ok_or_else(|| {
                error(
                    Stage::Local,
                    ErrorCode::InvalidInput,
                    "This consent challenge is no longer pending.",
                )
            })?;
        if control.answer.is_some() {
            return Err(error(
                Stage::Local,
                ErrorCode::InvalidInput,
                "This challenge has already been answered.",
            ));
        }
        let response = match answer {
            ChallengeAnswer::Accept if challenge.kind == ChallengeKind::HostKey => Answer {
                bytes: Some(Zeroizing::new(
                    challenge
                        .fingerprint
                        .as_ref()
                        .ok_or_else(|| {
                            error(
                                Stage::HostKey,
                                ErrorCode::InvalidInput,
                                "A host key without a verified fingerprint cannot be accepted.",
                            )
                        })?
                        .as_bytes()
                        .to_vec(),
                )),
                declined: false,
            },
            ChallengeAnswer::Decline => Answer {
                bytes: if challenge.kind == ChallengeKind::HostKey {
                    Some(Zeroizing::new(b"no".to_vec()))
                } else {
                    None
                },
                declined: true,
            },
            ChallengeAnswer::Authentication { value } => {
                let bytes = Zeroizing::new(value.into_bytes());
                if challenge.kind != ChallengeKind::Authentication
                    || bytes.len() > 1022
                    || bytes.iter().any(|c| matches!(*c, b'\n' | b'\r' | 0))
                {
                    return Err(error(Stage::Auth, ErrorCode::InvalidInput, "This challenge requires a single authentication response of at most 1022 bytes."));
                }
                Answer {
                    bytes: Some(bytes),
                    declined: false,
                }
            }
            _ => {
                return Err(error(
                    Stage::Auth,
                    ErrorCode::InvalidInput,
                    "Host-key acceptance is not an authentication response.",
                ))
            }
        };
        control.answer = Some(response);
        Ok(())
    }
    pub fn acknowledge(&self, lease: &ConnectionLease, sequence: u32) -> Result<(), AppError> {
        let attempt = self.lookup(lease)?;
        let mut control = hold(&attempt.control);
        attempt.active()?;
        control.flow.acknowledge(sequence)
    }
    pub fn check_session(&self, id: SessionId) -> Result<SavedSession, AppError> {
        if self.closing.load(Ordering::Acquire) {
            return Err(canceled());
        }
        let (mut session, generation) = {
            let store = hold(&self.store);
            let session = store.session(&id)?;
            store.host(&session.host_id)?;
            (session, store.generation())
        };
        let lease = ConnectionLease {
            connection_id: ConnectionId::new(),
            attempt_id: AttemptId::new(),
        };
        let attempt = Arc::new(Attempt::new(lease));
        hold(&attempt.control).started = true;
        {
            let mut attempts = hold(&self.attempts);
            if self.closing.load(Ordering::Acquire) {
                return Err(canceled());
            }
            attempts.insert(lease.connection_id, attempt.clone());
        }
        let check = (|| {
            validate_saved(&session)?;
            preflight(&attempt, &session.ssh_binding)?;
            inspect_saved(&attempt, &session, true)
        })();
        attempt.finish();
        {
            let mut attempts = hold(&self.attempts);
            if attempts
                .get(&lease.connection_id)
                .is_some_and(|a| Arc::ptr_eq(a, &attempt))
            {
                attempts.remove(&lease.connection_id);
            }
        }
        attempt.active()?;
        session.status = match check {
            Ok(()) => SessionStatus::Live,
            Err(failure) if failure.code == ErrorCode::Ended => SessionStatus::Ended {
                reason: failure.message,
            },
            Err(failure) => SessionStatus::Unknown { error: failure },
        };
        persist(&attempt, &self.store, session.clone(), generation)?;
        Ok(session)
    }
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        let attempts: Vec<_> = hold(&self.attempts).drain().map(|(_, a)| a).collect();
        for attempt in &attempts {
            attempt.cancel(if attempt.setup.load(Ordering::Acquire) {
                "Fleet is closing. Setup commands were stopped; changes they already made may remain."
            } else {
                "Fleet is closing. Remote tmux sessions are retained."
            });
        }
        for attempt in attempts {
            attempt.wait();
        }
    }
}
impl Drop for TerminalHub {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn validate_binding(draft: &HostDraft) -> Result<(), AppError> {
    discovery::validate_draft(draft)?;
    if discovery::is_excluded_draft(draft) {
        return Err(error(
            Stage::Local,
            ErrorCode::InvalidInput,
            "The NAS and existing local hub are excluded from SSH connections.",
        ));
    }
    Ok(())
}
pub(crate) fn normalized_target(target: &str) -> String {
    let (user, host) = target
        .rsplit_once('@')
        .map_or((None, target), |(u, h)| (Some(u), h));
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match user {
        Some(user) => format!("{user}@{host}"),
        None => host.into(),
    }
}
fn ssh_base(binding: &HostDraft) -> Command {
    let mut command = Command::new("/usr/bin/ssh");
    command.env("LC_ALL", "C").env("LANG", "C");
    #[cfg(debug_assertions)]
    if let Some(config) = std::env::var_os("FLEET_SMOKE_SSH_CONFIG") {
        command.arg("-F").arg(config);
    }
    if let Some(port) = binding.port_override {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(host) = &binding.host_name_override {
        command
            .arg("-o")
            .arg(format!("HostName={}", normalized_target(host)));
    }
    if let Some(alias) = &binding.host_key_alias_override {
        command.arg("-o").arg(format!("HostKeyAlias={alias}"));
    }
    command
}
fn wildcard(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t, mut star, mut mark) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = t;
        } else if let Some(s) = star {
            mark += 1;
            t = mark;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
fn effective_config(attempt: &Attempt, command: Command, target: &str) -> Result<String, AppError> {
    let result = drive(
        attempt,
        command,
        None,
        None,
        Duration::from_secs(10),
        256 * 1024,
        target,
        None,
    )?;
    if !result.status.success() {
        return Err(error(
            Stage::Resolve,
            ErrorCode::Unavailable,
            "OpenSSH could not resolve the local SSH configuration.",
        ));
    }
    String::from_utf8(result.bytes).map_err(|_| {
        error(
            Stage::Resolve,
            ErrorCode::Unavailable,
            "OpenSSH returned an unsupported configuration format.",
        )
    })
}
fn preflight(attempt: &Attempt, binding: &HostDraft) -> Result<String, AppError> {
    discovery::guard_host(binding)?;
    attempt.state(ConnectionState::Connecting {
        stage: Stage::Resolve,
    })?;
    let mut command = ssh_base(binding);
    command
        .arg("-G")
        .arg("--")
        .arg(normalized_target(&binding.ssh_target));
    let text = effective_config(attempt, command, &binding.ssh_target)?;
    check_effective(binding, &text)?;
    attempt.active()?;
    Ok(text)
}
/// The exclusion, option and jump-host policy applied to every `ssh -G` result.
fn check_effective(binding: &HostDraft, text: &str) -> Result<(), AppError> {
    let config: HashMap<_, _> = text
        .lines()
        .filter_map(|line| line.split_once(' '))
        .collect();
    let hostname = config.get("hostname").ok_or_else(|| {
        error(
            Stage::Resolve,
            ErrorCode::Unavailable,
            "OpenSSH did not report an effective hostname.",
        )
    })?;
    let mut effective = binding.clone();
    effective.host_name_override = Some(normalized_target(hostname));
    discovery::validate_draft(&effective)?;
    discovery::guard_target(hostname)?;
    for option in ["localcommand", "remotecommand", "logverbose"] {
        if config
            .get(option)
            .is_some_and(|v| !v.is_empty() && *v != "none")
        {
            return Err(AppError::new(Stage::Local, ErrorCode::Unavailable, format!("Configured OpenSSH {option} is incompatible with Fleet's verified transport. Use a dedicated alias without that option; Fleet did not replace it.")));
        }
    }
    for option in [
        "forkafterauthentication",
        "stdinnull",
        "nohostauthenticationforlocalhost",
    ] {
        if config.get(option) == Some(&"yes") {
            return Err(AppError::new(Stage::Local, ErrorCode::Unavailable, format!("Configured OpenSSH {option}=yes is incompatible with Fleet's owned, host-key-verified terminal.")));
        }
    }
    if config.get("sessiontype").is_some_and(|v| *v != "default") {
        return Err(error(Stage::Session, ErrorCode::Unavailable, "This alias requests a noninteractive SSH session type. Select an interactive-shell alias."));
    }
    // The helper's role credentials must never be sent by a broad user SendEnv rule.
    for line in text
        .lines()
        .filter_map(|line| line.strip_prefix("sendenv "))
    {
        for pattern in line.split_whitespace().filter(|p| !p.starts_with('-')) {
            if [
                "FLEET_HELPER_NONCE",
                "FLEET_HELPER_SOCKET",
                "FLEET_HELPER_MODE",
            ]
            .iter()
            .any(|name| wildcard(pattern, name))
            {
                return Err(error(Stage::Local, ErrorCode::Unavailable, "This alias has a SendEnv rule that would disclose private Fleet helper credentials. Narrow that rule before connecting."));
            }
        }
    }
    if let Some(jumps) = config.get("proxyjump").filter(|j| **j != "none") {
        for jump in jumps.split(',') {
            let (user, host) = jump
                .rsplit_once('@')
                .map_or((None, jump), |(u, h)| (Some(u), h));
            let host = if host.starts_with('[') {
                host.split_once(']').map(|(h, _)| &h[1..]).unwrap_or(host)
            } else {
                host.split_once(':').map_or(host, |(h, _)| h)
            };
            let mut proxy = binding.clone();
            proxy.ssh_target = match user {
                Some(user) => format!("{user}@{host}"),
                None => host.into(),
            };
            proxy.host_name_override = None;
            proxy.host_key_alias_override = None;
            proxy.tailnet_node_id = None;
            discovery::validate_draft(&proxy)?;
            discovery::guard_target(&proxy.ssh_target)?;
        }
    }
    Ok(())
}

fn ssh_command(
    binding: &HostDraft,
    endpoint: &Endpoint,
    remote: Option<&str>,
    terminal: bool,
    batch: bool,
    forward: Option<u16>,
) -> Result<Command, AppError> {
    let mut command = ssh_base(binding);
    for option in [
        "ControlMaster=no",
        "ControlPath=none",
        "ControlPersist=no",
        "ForkAfterAuthentication=no",
        "StdinNull=no",
        "NoHostAuthenticationForLocalhost=no",
        "VerifyHostKeyDNS=no",
        "FingerprintHash=SHA256",
        "UpdateHostKeys=no",
        "EscapeChar=none",
        "ConnectionAttempts=1",
        "ConnectTimeout=15",
    ] {
        command.arg("-o").arg(option);
    }
    command.arg("-o").arg(if batch {
        "StrictHostKeyChecking=yes"
    } else {
        "StrictHostKeyChecking=ask"
    });
    command.arg("-o").arg(if batch {
        "BatchMode=yes"
    } else {
        "BatchMode=no"
    });
    endpoint.configure(&mut command).map_err(native_io)?;
    if let Some(port) = forward {
        // Login callback only: loopback on both ends, never a wildcard bind.
        command
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-L")
            .arg(format!("127.0.0.1:{port}:127.0.0.1:{port}"));
    }
    command.arg(if terminal { "-tt" } else { "-T" });
    command
        .arg("--")
        .arg(normalized_target(&binding.ssh_target));
    if let Some(remote) = remote {
        command.arg(remote);
    }
    command.env("TERM", "xterm-256color");
    Ok(command)
}

struct ChildOwner {
    child: Child,
    reaped: bool,
}
impl ChildOwner {
    fn status(&mut self) -> io::Result<Option<ExitStatus>> {
        // WNOWAIT observes exit without giving the PID/process-group identity back to the OS.
        let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut information,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            let failure = io::Error::last_os_error();
            if failure.raw_os_error() == Some(libc::ECHILD) {
                self.reaped = true;
            }
            return Err(failure);
        }
        if unsafe { information.si_pid() } == 0 {
            return Ok(None);
        }
        let status = unsafe { information.si_status() };
        let raw = if information.si_code == libc::CLD_EXITED {
            status << 8
        } else if information.si_code == libc::CLD_DUMPED {
            status | 0x80
        } else {
            status
        };
        Ok(Some(ExitStatus::from_raw(raw)))
    }
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        if !self.reaped {
            // The unreaped child reserves this fresh process-group ID through natural exit and cancellation.
            unsafe {
                libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}
fn nonblocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn spawn(
    mut command: Command,
    size: Option<(u16, u16)>,
) -> Result<(ChildOwner, File, File), AppError> {
    let mut master = None;
    if let Some((cols, rows)) = size {
        let (mut master_fd, mut slave_fd) = (-1, -1);
        let mut winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut winsize,
            )
        } != 0
        {
            return Err(native_io(io::Error::last_os_error()));
        }
        let m = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        for fd in [master_fd, slave_fd] {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(native_io(io::Error::last_os_error()));
            }
        }
        nonblocking(&m).map_err(native_io)?;
        command
            .stdin(Stdio::from(slave.try_clone().map_err(native_io)?))
            .stdout(Stdio::from(slave));
        master = Some(m);
    } else {
        command.stdin(Stdio::null()).stdout(Stdio::piped());
    }
    command.stderr(Stdio::piped());
    let tty = size.is_some();
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if tty && libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(native_io)?;
    let mut owner = ChildOwner {
        child,
        reaped: false,
    };
    let output = match master {
        Some(master) => master,
        None => unsafe {
            File::from_raw_fd(
                owner
                    .child
                    .stdout
                    .take()
                    .ok_or_else(|| {
                        error(
                            Stage::Local,
                            ErrorCode::Unavailable,
                            "OpenSSH stdout was not allocated.",
                        )
                    })?
                    .into_raw_fd(),
            )
        },
    };
    let stderr = unsafe {
        File::from_raw_fd(
            owner
                .child
                .stderr
                .take()
                .ok_or_else(|| {
                    error(
                        Stage::Local,
                        ErrorCode::Unavailable,
                        "OpenSSH stderr was not allocated.",
                    )
                })?
                .into_raw_fd(),
        )
    };
    nonblocking(&output).map_err(native_io)?;
    nonblocking(&stderr).map_err(native_io)?;
    Ok((owner, output, stderr))
}

#[derive(Default)]
struct ClientLog {
    line: Vec<u8>,
    discard: bool,
    pty: Option<u32>,
    shell: Option<u32>,
    exec: Option<u32>,
    failure: Option<AppError>,
}
impl ClientLog {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                if !self.discard {
                    let bytes = std::mem::take(&mut self.line);
                    if let Ok(line) = std::str::from_utf8(&bytes) {
                        self.observe(line.trim_end_matches('\r'));
                    }
                    self.line = bytes;
                }
                self.line.clear();
                self.discard = false;
            } else if !self.discard {
                if self.line.len() < 8192 {
                    self.line.push(byte);
                } else {
                    self.line.clear();
                    self.discard = true;
                }
            }
        }
    }
    fn observe(&mut self, line: &str) {
        for (prefix, target) in [
            (
                "debug2: PTY allocation request accepted on channel ",
                &mut self.pty,
            ),
            (
                "debug2: shell request accepted on channel ",
                &mut self.shell,
            ),
            ("debug2: exec request accepted on channel ", &mut self.exec),
        ] {
            if let Some(number) = line
                .strip_prefix(prefix)
                .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            {
                if let Ok(number) = number.parse() {
                    *target = Some(number);
                }
            }
        }
        let failure = if line.starts_with("@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!")
            || line.starts_with("WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!")
            || (line.starts_with("Host key for ") && line.contains("has changed"))
        {
            Some(error(Stage::HostKey, ErrorCode::Changed, "The remote host key changed. Fleet stopped without offering acceptance. Verify the replacement independently before repairing OpenSSH trust."))
        } else if line.starts_with("Host key verification failed.") {
            Some(error(Stage::HostKey, ErrorCode::Declined, "OpenSSH host-key verification failed. Connect explicitly to review an unknown key; changed keys must be repaired outside Fleet."))
        } else if line.starts_with("ssh: Could not resolve hostname ") {
            Some(error(
                Stage::Resolve,
                ErrorCode::ConnectionFailed,
                "OpenSSH could not resolve the configured hostname.",
            ))
        } else if line.starts_with("ssh: connect to host ") {
            Some(error(
                Stage::Tcp,
                if line.contains("timed out") {
                    ErrorCode::Timeout
                } else {
                    ErrorCode::ConnectionFailed
                },
                "OpenSSH could not establish the configured network connection.",
            ))
        } else if line == "debug1: No more authentication methods to try." {
            Some(error(
                Stage::Auth,
                ErrorCode::Authentication,
                "OpenSSH authentication failed using the configured account and credentials.",
            ))
        } else if line.starts_with("PTY allocation request failed")
            || line.starts_with("shell request failed")
            || line.starts_with("exec request failed")
            || (line.starts_with("channel ") && line.contains(": open failed:"))
        {
            Some(error(
                Stage::Session,
                ErrorCode::ConnectionFailed,
                "The SSH server rejected the requested terminal or session channel.",
            ))
        } else {
            None
        };
        if let Some(failure) = failure {
            if self
                .failure
                .as_ref()
                .is_none_or(|previous| previous.code != ErrorCode::Changed)
            {
                self.failure = Some(failure);
            }
        }
    }
    fn drain(&mut self, endpoint: &mut Endpoint) -> Result<(), AppError> {
        let mut bytes = [0u8; 4096];
        for _ in 0..16 {
            match endpoint.log.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => self.feed(&bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(native_io(e)),
            }
        }
        Ok(())
    }
}
impl Drop for ClientLog {
    fn drop(&mut self) {
        self.line.zeroize();
    }
}

fn key_details(prompt: &str) -> Result<(String, String, String), AppError> {
    let invalid = || {
        error(Stage::HostKey, ErrorCode::Unavailable, "OpenSSH requested host-key consent without a complete, recognized SHA256 fingerprint. Fleet did not accept it.")
    };
    let host_line = prompt
        .lines()
        .find(|line| line.starts_with("The authenticity of host '"))
        .ok_or_else(invalid)?;
    let host = host_line
        .strip_prefix("The authenticity of host '")
        .and_then(|line| line.split_once('\'').map(|p| p.0))
        .filter(|host| !host.is_empty())
        .ok_or_else(invalid)?;
    let mut parsed = None;
    for line in prompt.lines() {
        let Some((algorithm, suffix)) = line.split_once(" key fingerprint is") else {
            continue;
        };
        let fingerprint = suffix.trim_start_matches(':').trim().trim_end_matches('.');
        let digest = fingerprint.strip_prefix("SHA256:").ok_or_else(invalid)?;
        if parsed.is_some()
            || algorithm.is_empty()
            || algorithm.len() > 80
            || !algorithm
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'@' | b'.'))
            || digest.len() != 43
            || !digest
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/'))
        {
            return Err(invalid());
        }
        parsed = Some((
            host.to_owned(),
            algorithm.to_owned(),
            fingerprint.to_owned(),
        ));
    }
    if !prompt.contains("Are you sure you want to continue connecting") {
        return Err(invalid());
    }
    parsed.ok_or_else(invalid)
}
fn challenge_for(request: &Request, target: &str) -> Result<Challenge, AppError> {
    let prompt = request.prompt.as_deref().ok_or_else(|| {
        error(
            Stage::Auth,
            ErrorCode::InvalidInput,
            "The helper did not supply an authentication prompt.",
        )
    })?;
    if prompt.contains("The authenticity of host ")
        || prompt.contains("key fingerprint is")
        || prompt.contains("continue connecting")
    {
        let (host, algorithm, fingerprint) = key_details(prompt)?;
        return Ok(Challenge {
            id: ChallengeId::new(),
            kind: ChallengeKind::HostKey,
            host,
            algorithm: Some(algorithm),
            fingerprint: Some(fingerprint),
            prompt: prompt.into(),
        });
    }
    if request.hint.as_deref() == Some("confirm")
        || prompt.contains("yes/no")
        || prompt.starts_with("Please type 'yes'")
    {
        return Err(error(Stage::Auth, ErrorCode::Unavailable, "This OpenSSH permission prompt is not a host-key or credential challenge. Complete the configured permission outside Fleet."));
    }
    Ok(Challenge {
        id: ChallengeId::new(),
        kind: ChallengeKind::Authentication,
        host: target.into(),
        algorithm: None,
        fingerprint: None,
        prompt: prompt.into(),
    })
}
fn helpers(
    attempt: &Attempt,
    endpoint: &mut Endpoint,
    waiting: &mut Option<(Request, Instant)>,
    authenticated: &mut bool,
    target: &str,
) -> Result<(), AppError> {
    if waiting.is_some() {
        let response = {
            let mut control = hold(&attempt.control);
            if control.answer.is_some() {
                let kind = control
                    .challenge
                    .take()
                    .map(|c| c.kind)
                    .unwrap_or(ChallengeKind::Authentication);
                control.answer.take().map(|answer| (answer, kind))
            } else {
                None
            }
        };
        if let Some((answer, kind)) = response {
            let (request, _) = waiting.take().ok_or_else(canceled)?;
            request
                .reply(answer.bytes.as_ref().map(|b| b.as_slice()))
                .map_err(native_io)?;
            if answer.declined {
                return Err(error(
                    if kind == ChallengeKind::HostKey {
                        Stage::HostKey
                    } else {
                        Stage::Auth
                    },
                    ErrorCode::Declined,
                    "The connection challenge was declined.",
                ));
            }
            attempt.state(ConnectionState::Connecting { stage: Stage::Auth })?;
        } else if waiting
            .as_ref()
            .is_some_and(|(_, started)| started.elapsed() > Duration::from_secs(120))
        {
            return Err(error(
                Stage::Auth,
                ErrorCode::Timeout,
                "The connection challenge expired without a response.",
            ));
        }
    }
    for request in endpoint.poll().map_err(native_io)? {
        if request.role == Role::Ready {
            request.reply(Some(&[])).map_err(native_io)?;
            *authenticated = true;
            attempt.state(ConnectionState::Connecting {
                stage: Stage::Session,
            })?;
        } else if request.hint.as_deref() == Some("none") {
            // A security-key touch notification is not a password or trust prompt.
            request.reply(Some(&[])).map_err(native_io)?;
            attempt.state(ConnectionState::Connecting { stage: Stage::Auth })?;
        } else {
            if waiting.is_some() {
                let _ = request.reply(None);
                return Err(error(
                    Stage::Auth,
                    ErrorCode::Unavailable,
                    "OpenSSH requested overlapping authentication challenges.",
                ));
            }
            if hold(&attempt.control).events.is_none() {
                let _ = request.reply(None);
                return Err(error(Stage::Auth, ErrorCode::Authentication, "Checking a saved session requires noninteractive OpenSSH authentication and an already trusted host key."));
            }
            let challenge = challenge_for(&request, target)?;
            {
                let _delivery = hold(&attempt.delivery);
                attempt.active()?;
                hold(&attempt.control).challenge = Some(challenge.clone());
                attempt.emit_locked(EventBody::State {
                    state: ConnectionState::AwaitingConsent { challenge },
                })?;
            }
            *waiting = Some((request, Instant::now()));
        }
    }
    Ok(())
}

struct TerminalOutput {
    channel: Channel<InvokeResponseBody>,
    mode: ConnectionMode,
    exec: bool,
}
struct Driven {
    bytes: Vec<u8>,
    status: ExitStatus,
}
fn send_output(
    attempt: &Attempt,
    output: &Channel<InvokeResponseBody>,
    bytes: &[u8],
) -> Result<(), AppError> {
    let _delivery = hold(&attempt.delivery);
    attempt.active()?;
    let sequence = hold(&attempt.control).flow.reserve(HEADER + bytes.len())?;
    let mut frame = Vec::with_capacity(HEADER + bytes.len());
    frame.extend_from_slice(attempt.lease.connection_id.as_bytes());
    frame.extend_from_slice(attempt.lease.attempt_id.as_bytes());
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(bytes);
    output.send(InvokeResponseBody::Raw(frame)).map_err(|_| {
        error(
            Stage::Local,
            ErrorCode::Unavailable,
            "The terminal renderer was disposed before output was delivered.",
        )
    })
}
fn terminal_controls(attempt: &Attempt, master: &mut File) -> Result<(), AppError> {
    let mut control = hold(&attempt.control);
    if control.resize_pending {
        let (cols, rows) = control.size;
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) } != 0 {
            return Err(native_io(io::Error::last_os_error()));
        }
        control.resize_pending = false;
    }
    if !control.live {
        return Ok(());
    }
    for _ in 0..8 {
        let Some(front) = control.input.front() else {
            break;
        };
        let offset = control.input_offset;
        let length = front.len();
        match master.write(&front[offset..]) {
            Ok(0) => {
                return Err(error(
                    Stage::Session,
                    ErrorCode::ConnectionLost,
                    "The local terminal stopped accepting input.",
                ))
            }
            Ok(n) => {
                control.input_offset += n;
                control.input_bytes -= n;
                if control.input_offset == length {
                    control.input.pop_front();
                    control.input_offset = 0;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                return Err(error(
                    Stage::Session,
                    ErrorCode::ConnectionLost,
                    "The SSH terminal closed while sending input.",
                ))
            }
        }
    }
    Ok(())
}
fn output_credit(attempt: &Attempt, terminal: bool) -> usize {
    if terminal {
        hold(&attempt.control)
            .flow
            .credit()
            .saturating_sub(HEADER)
            .min(CHUNK)
    } else {
        CHUNK
    }
}
#[allow(clippy::too_many_arguments)]
fn drive(
    attempt: &Attempt,
    command: Command,
    mut endpoint: Option<Endpoint>,
    terminal: Option<TerminalOutput>,
    timeout: Duration,
    limit: usize,
    target: &str,
    cap: Option<Duration>,
) -> Result<Driven, AppError> {
    attempt.active()?;
    let _challenge_cleanup = ClearChallenge(attempt);
    let size = terminal.as_ref().map(|_| hold(&attempt.control).size);
    let (mut owner, mut stdout, mut stderr) = spawn(command, size)?;
    let mut log = ClientLog::default();
    let mut waiting = None;
    let mut authenticated = false;
    let mut connected = false;
    let mut bytes = Vec::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut status = None;
    let started = Instant::now();
    let mut chunk = [0u8; CHUNK];
    loop {
        attempt.active()?;
        if let Some(endpoint) = endpoint.as_mut() {
            log.drain(endpoint)?;
            helpers(attempt, endpoint, &mut waiting, &mut authenticated, target)?;
        }
        if let Some(failure) = &log.failure {
            return Err(failure.clone());
        }
        if let Some(terminal) = &terminal {
            let session = if terminal.exec { log.exec } else { log.shell };
            if !connected && authenticated && session.is_some() && log.pty == session {
                attempt.connected(terminal.mode.clone())?;
                connected = true;
            }
            terminal_controls(attempt, &mut stdout)?;
        }
        if cap.is_some_and(|cap| started.elapsed() > cap) {
            return Err(error(Stage::Session, ErrorCode::Timeout, "The setup command reached its runtime limit and was stopped. Changes it already made may remain."));
        }
        if (!connected || terminal.is_none()) && started.elapsed() > timeout {
            return Err(error(
                if endpoint.is_none() {
                    Stage::Resolve
                } else if authenticated {
                    Stage::Session
                } else {
                    Stage::Auth
                },
                ErrorCode::Timeout,
                "OpenSSH did not complete a verified remote session before the deadline.",
            ));
        }
        for stream_index in 0..2 {
            let credit = output_credit(attempt, terminal.is_some());
            if credit == 0
                || (stream_index == 0 && stdout_done)
                || (stream_index == 1 && stderr_done)
            {
                continue;
            }
            let stream = if stream_index == 0 {
                &mut stdout
            } else {
                &mut stderr
            };
            match stream.read(&mut chunk[..credit]) {
                Ok(0) => {
                    if stream_index == 0 {
                        stdout_done = true;
                    } else {
                        stderr_done = true;
                    }
                }
                Ok(n) => {
                    if let Some(terminal) = &terminal {
                        send_output(attempt, &terminal.channel, &chunk[..n])?;
                    } else if stream_index == 0 {
                        if bytes.len() + n > limit {
                            return Err(error(
                                Stage::Session,
                                ErrorCode::Unavailable,
                                "The SSH operation exceeded its bounded response size.",
                            ));
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e)
                    if stream_index == 0
                        && terminal.is_some()
                        && e.raw_os_error() == Some(libc::EIO) =>
                {
                    stdout_done = true;
                }
                Err(_) => {
                    return Err(error(
                        Stage::Session,
                        ErrorCode::ConnectionLost,
                        "The local SSH data stream closed unexpectedly.",
                    ))
                }
            }
        }
        if status.is_none() {
            status = owner.status().map_err(native_io)?;
        }
        if status.is_some() && stdout_done && stderr_done {
            if let Some(endpoint) = endpoint.as_mut() {
                log.drain(endpoint)?;
            }
            if let Some(failure) = log.failure.take() {
                return Err(failure);
            }
            let status = status.take().ok_or_else(|| {
                error(
                    Stage::Local,
                    ErrorCode::Unknown,
                    "The SSH exit status is unavailable.",
                )
            })?;
            if terminal.is_some() && !connected {
                return Err(error(if authenticated { Stage::Session } else { Stage::Auth }, ErrorCode::ConnectionFailed, "SSH exited before authenticated PTY and remote session acceptance were verified."));
            }
            if terminal.is_none()
                && endpoint.is_some()
                && status.success()
                && (!authenticated || log.exec.is_none())
            {
                return Err(error(Stage::Session, ErrorCode::Unavailable, "The installed OpenSSH did not provide the required authenticated session confirmation."));
            }
            return Ok(Driven { bytes, status });
        }
        let credit = output_credit(attempt, terminal.is_some());
        let pending_input = terminal.is_some() && hold(&attempt.control).input_bytes > 0;
        let read_stdout = credit > 0 && !stdout_done;
        let read_stderr = credit > 0 && !stderr_done;
        let mut descriptors = [
            libc::pollfd {
                fd: if read_stdout || pending_input {
                    stdout.as_raw_fd()
                } else {
                    -1
                },
                events: (if read_stdout { libc::POLLIN } else { 0 })
                    | (if pending_input { libc::POLLOUT } else { 0 }),
                revents: 0,
            },
            libc::pollfd {
                fd: if read_stderr { stderr.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: endpoint.as_ref().map_or(-1, |e| e.log.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                25,
            )
        } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return Err(native_io(io::Error::last_os_error()));
        }
    }
}

pub(crate) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
const REMOTE_PREFIX: &str = r#"set -eu
export LC_ALL=C
os=$(uname -s)
case "$os" in
 Linux)
  if [ -x /usr/bin/tmux ]; then tm=/usr/bin/tmux; else tm=$(command -v tmux) || exit 42; fi
  boot="linux:$(cat /proc/sys/kernel/random/boot_id)" || exit 44
  [ "$boot" != linux: ] || exit 44
  ;;
 Darwin)
  if [ -x /opt/homebrew/bin/tmux ]; then tm=/opt/homebrew/bin/tmux; elif [ -x /usr/local/bin/tmux ]; then tm=/usr/local/bin/tmux; else tm=$(command -v tmux) || exit 42; fi
  raw=$(sysctl -n kern.boottime) || exit 44
  stamp=$(printf '%s\n' "$raw" | sed -n 's/.*sec = \([0-9][0-9]*\), usec = \([0-9][0-9]*\).*/\1-\2/p')
  [ -n "$stamp" ] || exit 44
  boot="darwin:$stamp"
  ;;
 *) exit 44 ;;
esac
process_birth() {
 case "$pid" in ''|*[!0-9]*) exit 43 ;; esac
 [ "$pid" -gt 0 ] || exit 43
 case "$os" in
  Linux)
   raw=$(cat "/proc/$pid/stat") || exit 43
   rest=${raw##*) }
   set -- $rest
   [ "$#" -ge 20 ] || exit 43
   shift 19
   case "$1" in ''|*[!0-9]*) exit 43 ;; esac
   birth="linux:$1"
   ;;
  Darwin)
   raw=$(/bin/ps -p "$pid" -o lstart=) || exit 43
   born=$(printf '%s' "$raw" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
   [ -n "$born" ] || exit 43
   birth="darwin:$born"
   ;;
 esac
}
"#;
fn remote_command(script: &str) -> String {
    format!("exec /bin/sh -c {}", quote(script))
}
fn valid_name(name: &str) -> bool {
    name.strip_prefix("fleet-").is_some_and(|v| {
        v.len() == 32
            && v.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}
fn valid_identity(identity: &SessionIdentity) -> bool {
    let numeric = |value: &str, prefix: char| {
        value
            .strip_prefix(prefix)
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()))
    };
    let boot = identity
        .boot_marker
        .strip_prefix("linux:")
        .is_some_and(|s| Uuid::parse_str(s).is_ok())
        || identity
            .boot_marker
            .strip_prefix("darwin:")
            .is_some_and(|s| {
                s.split_once('-').is_some_and(|(a, b)| {
                    !a.is_empty()
                        && !b.is_empty()
                        && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit())
                })
            });
    let birth = identity
        .original_pane_birth_marker
        .strip_prefix("linux:")
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        || identity
            .original_pane_birth_marker
            .strip_prefix("darwin:")
            .is_some_and(|s| {
                s.len() >= 20
                    && s.len() <= 64
                    && s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b' ' | b':'))
            });
    boot && birth
        && identity.original_pane_pid > 0
        && identity.created_epoch > 0
        && numeric(&identity.tmux_session_id, '$')
        && numeric(&identity.original_pane_id, '%')
        && identity.instance_token.len() == 32
        && identity
            .instance_token
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        && (identity.boot_marker.starts_with("linux:")
            == identity.original_pane_birth_marker.starts_with("linux:"))
}
fn validate_saved(session: &SavedSession) -> Result<(), AppError> {
    validate_binding(&session.ssh_binding)?;
    if !valid_name(&session.remote_session_name)
        || session.remote_session_name != format!("fleet-{}", session.id.simple())
    {
        return Err(error(
            Stage::Session,
            ErrorCode::InvalidInput,
            "The saved session does not have an app-owned generated name.",
        ));
    }
    if session
        .identity
        .as_ref()
        .is_none_or(|identity| !valid_identity(identity))
    {
        return Err(error(Stage::Session, ErrorCode::Unknown, "The saved reference lacks a valid original process identity. Fleet will not create or attach a replacement implicitly."));
    }
    Ok(())
}
fn identity_script(
    name: &str,
    pane: Option<&str>,
    creation: Option<(&str, u16, u16)>,
    emit: bool,
) -> String {
    let mut script = REMOTE_PREFIX.to_owned();
    script.push_str(&format!("name={}\n", quote(name)));
    if let Some((token, cols, rows)) = creation {
        script.push_str(&format!("token={}\n\"$tm\" new-session -d -s \"$name\" -x {cols} -y {rows} || exit 45\n\"$tm\" -N set-option -t \"=$name:\" @fleet_instance \"$token\" || exit 45\n", quote(token)));
        script.push_str(
            "pane=$(\"$tm\" -N display-message -p -t \"=$name:\" '#{pane_id}') || exit 43\n",
        );
    } else {
        script.push_str(&format!("pane={}\n\"$tm\" -N has-session -t \"=$name\" || exit 43\ntoken=$(\"$tm\" -N show-options -qv -t \"=$name:\" @fleet_instance) || exit 43\n", quote(pane.unwrap_or_default())));
    }
    script.push_str("line=$(\"$tm\" -u -N display-message -p -t \"=$name:\" '#{session_id}\t#{session_created}') || exit 43\npanes=$(\"$tm\" -N list-panes -s -t \"=$name:\" -F '#{pane_id} #{pane_dead}') || exit 43\nlive=0\nwhile IFS=' ' read -r p dead; do\n if [ \"$p\" = \"$pane\" ] && [ \"$dead\" = 0 ]; then live=1; fi\ndone <<FLEET_PANES\n$panes\nFLEET_PANES\n[ \"$live\" = 1 ] || exit 43\npid=$(\"$tm\" -N display-message -p -t \"$pane\" '#{pane_pid}') || exit 43\nprocess_birth\n");
    if emit {
        script.push_str("printf '%s\\t%s\\t%s\\t%s\\t%s\\t%s\\n' \"$boot\" \"$token\" \"$line\" \"$pane\" \"$pid\" \"$birth\"\n");
    }
    script
}
fn parse_identity(bytes: &[u8]) -> Result<SessionIdentity, AppError> {
    let invalid = || {
        error(
            Stage::Session,
            ErrorCode::Unavailable,
            "The remote host returned an unsupported or invalid tmux process identity.",
        )
    };
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let fields: Vec<_> = text.trim_end_matches(['\r', '\n']).split('\t').collect();
    if fields.len() != 7 || fields.iter().any(|f| f.contains(['\r', '\n'])) {
        return Err(invalid());
    }
    let identity = SessionIdentity {
        boot_marker: fields[0].into(),
        instance_token: fields[1].into(),
        tmux_session_id: fields[2].into(),
        created_epoch: fields[3].parse().map_err(|_| invalid())?,
        original_pane_id: fields[4].into(),
        original_pane_pid: fields[5].parse().map_err(|_| invalid())?,
        original_pane_birth_marker: fields[6].into(),
    };
    if !valid_identity(&identity) {
        return Err(invalid());
    }
    Ok(identity)
}
fn remote_failure(status: ExitStatus, authenticated_session: bool) -> AppError {
    match status.code() {
        Some(42) => error(Stage::Session, ErrorCode::Missing, "tmux was not found in the supported installed locations on the remote host. Fleet will not install it. A plain terminal has no process-survival guarantee."),
        Some(43) => error(Stage::Session, ErrorCode::Ended, "The original saved tmux session, pane, or process has ended. No replacement was created."),
        Some(44) => error(Stage::Session, ErrorCode::Unavailable, "The remote host cannot provide a supported boot/process identity. Fleet cannot safely promise reattachment."),
        Some(45) => error(Stage::Session, ErrorCode::ConnectionFailed, "Explicit tmux session creation or identity registration failed. Fleet did not attach an existing session by name."),
        _ => error(if authenticated_session { Stage::Session } else { Stage::Auth }, ErrorCode::ConnectionFailed, "The verified SSH operation failed. Repair the configured connection outside Fleet and retry explicitly."),
    }
}
fn probe(
    attempt: &Attempt,
    binding: &HostDraft,
    script: &str,
    batch: bool,
) -> Result<SessionIdentity, AppError> {
    attempt.state(ConnectionState::Connecting { stage: Stage::Tcp })?;
    let endpoint = Endpoint::new().map_err(native_io)?;
    let remote = remote_command(script);
    let command = ssh_command(binding, &endpoint, Some(&remote), false, batch, None)?;
    let result = drive(
        attempt,
        command,
        Some(endpoint),
        None,
        STARTUP_TIMEOUT,
        64 * 1024,
        &binding.ssh_target,
        None,
    )?;
    if !result.status.success() {
        return Err(remote_failure(result.status, true));
    }
    parse_identity(&result.bytes)
}
fn inspect_saved(attempt: &Attempt, session: &SavedSession, batch: bool) -> Result<(), AppError> {
    let expected = session.identity.as_ref().ok_or_else(|| {
        error(
            Stage::Session,
            ErrorCode::Unknown,
            "The original session identity was not captured.",
        )
    })?;
    let script = identity_script(
        &session.remote_session_name,
        Some(&expected.original_pane_id),
        None,
        true,
    );
    let actual = probe(attempt, &session.ssh_binding, &script, batch)?;
    if actual != *expected {
        return Err(error(Stage::Session, ErrorCode::Ended, "The saved reference no longer identifies the original boot, tmux instance, pane, and process. Fleet will not attach a replacement."));
    }
    Ok(())
}
fn attach_command(session: &SavedSession) -> Result<String, AppError> {
    validate_saved(session)?;
    let identity = session.identity.as_ref().ok_or_else(|| {
        error(
            Stage::Session,
            ErrorCode::Unknown,
            "The original process identity is missing.",
        )
    })?;
    let mut script = identity_script(
        &session.remote_session_name,
        Some(&identity.original_pane_id),
        None,
        false,
    );
    script.push_str(&format!("[ \"$boot\" = {} ] && [ \"$token\" = {} ] && [ \"$line\" = {} ] && [ \"$pid\" = {} ] && [ \"$birth\" = {} ] || exit 43\n",
        quote(&identity.boot_marker), quote(&identity.instance_token), quote(&format!("{}\t{}", identity.tmux_session_id, identity.created_epoch)),
        quote(&identity.original_pane_pid.to_string()), quote(&identity.original_pane_birth_marker)));
    // if-shell -F evaluates synchronously in tmux's command queue. IDs, not names, prevent name-replacement attachment.
    let format = format!(
        "{}{}{}{}{}{}{}{}{}",
        "#{&&:#{==:#{session_id},",
        identity.tmux_session_id,
        "},#{&&:#{==:#{session_created},",
        identity.created_epoch,
        "},#{&&:#{==:#{@fleet_instance},",
        identity.instance_token,
        "},#{&&:#{==:#{pane_pid},",
        identity.original_pane_pid,
        "},#{==:#{pane_dead},0}}}}}"
    );
    let success = format!(
        "select-window -t {} ; select-pane -t {} ; attach-session -t '{}'",
        identity.original_pane_id, identity.original_pane_id, identity.tmux_session_id
    );
    script.push_str(&format!(
        "exec \"$tm\" -N if-shell -F -t {} {} {} {}\n",
        quote(&identity.original_pane_id),
        quote(&format),
        quote(&success),
        quote("display-message -p 'Saved Fleet process ended or changed; no session attached.'")
    ));
    Ok(remote_command(&script))
}
fn persist(
    attempt: &Attempt,
    store: &Arc<Mutex<Store>>,
    session: SavedSession,
    generation: u64,
) -> Result<(), AppError> {
    let _delivery = hold(&attempt.delivery);
    attempt.active()?;
    hold(store).save_session(session.clone(), generation)?;
    // Publication is conditional on the durable generation-checked write, never on a merely successful remote probe.
    attempt.emit_locked(EventBody::Session { session })
}
fn status_failure(
    attempt: &Attempt,
    store: &Arc<Mutex<Store>>,
    session: &mut SavedSession,
    generation: u64,
    failure: &AppError,
) -> Result<(), AppError> {
    if failure.code == ErrorCode::Canceled || attempt.canceled.load(Ordering::Acquire) {
        return Err(canceled());
    }
    session.status = if failure.code == ErrorCode::Ended {
        SessionStatus::Ended {
            reason: failure.message.clone(),
        }
    } else {
        SessionStatus::Unknown {
            error: failure.clone(),
        }
    };
    persist(attempt, store, session.clone(), generation)
}
fn run_connection(
    attempt: &Attempt,
    store: &Arc<Mutex<Store>>,
    host: Host,
    generation: u64,
    plan: Plan,
    output: Channel<InvokeResponseBody>,
) -> Result<Option<i32>, AppError> {
    let binding = match &plan {
        Plan::Attach(session) => session.ssh_binding.clone(),
        _ => host.input.clone(),
    };
    preflight(attempt, &binding)?;
    let mut saved = match plan {
        Plan::Plain => None,
        Plan::Create(label) => {
            let id = SessionId::new();
            let mut session = SavedSession {
                id,
                host_id: host.id,
                label,
                remote_session_name: format!("fleet-{}", id.simple()),
                ssh_binding: binding.clone(),
                identity: None,
                status: SessionStatus::Unchecked,
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            persist(attempt, store, session.clone(), generation)?;
            let token = Uuid::new_v4().simple().to_string();
            let (cols, rows) = hold(&attempt.control).size;
            let script = identity_script(
                &session.remote_session_name,
                None,
                Some((&token, cols, rows)),
                true,
            );
            match probe(attempt, &binding, &script, false) {
                Ok(identity) => {
                    session.identity = Some(identity);
                    session.status = SessionStatus::Live;
                    persist(attempt, store, session.clone(), generation)?;
                }
                Err(failure) => {
                    status_failure(attempt, store, &mut session, generation, &failure)?;
                    return Err(failure);
                }
            }
            Some(session)
        }
        Plan::Attach(mut session) => {
            match inspect_saved(attempt, &session, false) {
                Ok(()) => {
                    session.status = SessionStatus::Live;
                    persist(attempt, store, session.clone(), generation)?;
                }
                Err(failure) => {
                    status_failure(attempt, store, &mut session, generation, &failure)?;
                    return Err(failure);
                }
            }
            Some(session)
        }
    };
    let remote = saved.as_ref().map(attach_command).transpose()?;
    let mode = saved
        .as_ref()
        .map_or(ConnectionMode::Plain, |session| ConnectionMode::Tmux {
            session_id: session.id,
        });
    attempt.state(ConnectionState::Connecting { stage: Stage::Tcp })?;
    let endpoint = Endpoint::new().map_err(native_io)?;
    let command = ssh_command(&binding, &endpoint, remote.as_deref(), true, false, None)?;
    let terminal = TerminalOutput {
        channel: output,
        mode,
        exec: remote.is_some(),
    };
    let result = drive(
        attempt,
        command,
        Some(endpoint),
        Some(terminal),
        STARTUP_TIMEOUT,
        0,
        &binding.ssh_target,
        None,
    );
    match result {
        Ok(result) => {
            if saved.is_some() && !result.status.success() {
                let failure = remote_failure(result.status, true);
                if let Some(session) = &mut saved {
                    status_failure(attempt, store, session, generation, &failure)?;
                }
                Err(failure)
            } else {
                Ok(result.status.code())
            }
        }
        Err(failure) => {
            if let Some(session) = &mut saved {
                status_failure(attempt, store, session, generation, &failure)?;
            }
            Err(failure)
        }
    }
}

// ---- Host setup ----

const LOCAL_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// Waits for renderer credit, then sends text as one console frame sequence. Frames are
/// never dropped, so slow acknowledgement slows setup output instead of losing it.
fn console(
    attempt: &Attempt,
    output: &Channel<InvokeResponseBody>,
    bytes: &[u8],
) -> Result<(), AppError> {
    for part in bytes.chunks(CHUNK) {
        loop {
            attempt.active()?;
            if hold(&attempt.control).flow.credit() >= HEADER + part.len() {
                send_output(attempt, output, part)?;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}
fn crlf(bytes: &[u8], after_cr: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);
    for &byte in bytes {
        if byte == b'\n' && !*after_cr {
            out.push(b'\r');
        }
        *after_cr = byte == b'\r';
        out.push(byte);
    }
    out
}
fn step_command(step: &LocalStep) -> Command {
    let mut command = Command::new(&step.program);
    command.args(&step.args).env_clear();
    let home = std::env::var_os("HOME").unwrap_or_default();
    let tool_path = |suffix: &str| {
        let mut path = std::path::PathBuf::from(&home);
        path.push(suffix);
        path
    };
    let search = std::env::join_paths(
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .into_iter()
            .map(std::path::PathBuf::from)
            .chain([
                tool_path(".bun/bin"),
                tool_path(".local/bin"),
                tool_path("bin"),
            ])
            .chain(std::env::split_paths(LOCAL_PATH)),
    )
    .unwrap_or_else(|_| LOCAL_PATH.into());
    command
        .env("HOME", &home)
        .env("PATH", search)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("NO_COLOR", "1");
    for name in ["USER", "LOGNAME", "SSH_AUTH_SOCK", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(cwd) = &step.cwd {
        command.current_dir(cwd);
    }
    command
}
/// Runs fixed local processes and shows their actual output. This is a local-output
/// transport: it never claims an SSH session, and Fleet's SSH confirmation logic is untouched.
fn local_steps(
    attempt: &Attempt,
    prepared: &PreparedSetup,
    steps: &[LocalStep],
    output: &Channel<InvokeResponseBody>,
    mode: ConnectionMode,
) -> Result<Option<i32>, AppError> {
    attempt.connected(mode)?;
    let started = Instant::now();
    let mut outcome = Some(0);
    for step in steps {
        prepared.verify_checkout()?;
        console(
            attempt,
            output,
            format!("\r\n$ {}\r\n", step.display).as_bytes(),
        )?;
        let (mut owner, mut stdout, mut stderr) = spawn(step_command(step), None)?;
        let (mut stdout_done, mut stderr_done) = (false, false);
        let (mut after_cr_out, mut after_cr_err) = (false, false);
        let mut exited: Option<(ExitStatus, Instant)> = None;
        let mut chunk = [0u8; CHUNK / 2];
        loop {
            attempt.active()?;
            if started.elapsed() > prepared.cap {
                return Err(error(Stage::Session, ErrorCode::Timeout, "The setup command reached its runtime limit and was stopped. Changes it already made may remain."));
            }
            {
                // A local fixed installer takes no keyboard input.
                let mut control = hold(&attempt.control);
                control.input.clear();
                control.input_bytes = 0;
                control.input_offset = 0;
                control.resize_pending = false;
            }
            let mut read_any = false;
            for index in 0..2 {
                let credit = hold(&attempt.control).flow.credit().saturating_sub(HEADER) / 2;
                if credit == 0 || (index == 0 && stdout_done) || (index == 1 && stderr_done) {
                    continue;
                }
                let (stream, after_cr) = if index == 0 {
                    (&mut stdout, &mut after_cr_out)
                } else {
                    (&mut stderr, &mut after_cr_err)
                };
                match stream.read(&mut chunk[..credit.min(CHUNK / 2)]) {
                    Ok(0) => {
                        if index == 0 {
                            stdout_done = true;
                        } else {
                            stderr_done = true;
                        }
                    }
                    Ok(n) => {
                        read_any = true;
                        let bytes = crlf(&chunk[..n], after_cr);
                        console(attempt, output, &bytes)?;
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => {
                        if index == 0 {
                            stdout_done = true;
                        } else {
                            stderr_done = true;
                        }
                    }
                }
            }
            if exited.is_none() {
                if let Some(status) = owner.status().map_err(native_io)? {
                    exited = Some((status, Instant::now()));
                }
            }
            if let Some((_, when)) = &exited {
                // A descendant that keeps a pipe open must not hold the console forever.
                if (stdout_done && stderr_done)
                    || (!read_any && when.elapsed() > Duration::from_secs(2))
                {
                    break;
                }
            }
            let mut descriptors = [
                libc::pollfd {
                    fd: if stdout_done { -1 } else { stdout.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if stderr_done { -1 } else { stderr.as_raw_fd() },
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, 25) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                return Err(native_io(io::Error::last_os_error()));
            }
        }
        let status = exited.map(|(status, _)| status).ok_or_else(|| {
            error(
                Stage::Local,
                ErrorCode::Unknown,
                "The local process status is unavailable.",
            )
        })?;
        drop(owner);
        let code = status.code();
        console(
            attempt,
            output,
            format!(
                "\r\n[exit {}]\r\n",
                code.map_or("signal".to_owned(), |c| c.to_string())
            )
            .as_bytes(),
        )?;
        if code != Some(0) && !step.optional {
            outcome = code;
            console(attempt, output, b"\r\nSetup stopped. Earlier steps and this one may already have changed things; review the output above.\r\n")?;
            break;
        }
        if code != Some(0) {
            console(
                attempt,
                output,
                b"(optional check; it does not change the setup result)\r\n",
            )?;
        }
    }
    if outcome == Some(0) {
        console(
            attempt,
            output,
            format!("\r\n{}\r\n", prepared.epilogue).as_bytes(),
        )?;
    }
    Ok(outcome)
}
/// The private SSH config must resolve exactly as Fleet's own transport does for the target,
/// and must keep the safety policy in force, before any installer is allowed to use it.
fn verify_private_config(
    attempt: &Attempt,
    binding: &HostDraft,
    standard: &str,
    private: &PrivateSsh,
) -> Result<(), AppError> {
    setup::ensure_private_config(private)?;
    let mut command = Command::new("/usr/bin/ssh");
    command
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .arg("-F")
        .arg(&private.path)
        .arg("-G")
        .arg("--")
        .arg(normalized_target(&binding.ssh_target));
    let text = effective_config(attempt, command, &binding.ssh_target)?;
    check_effective(binding, &text)?;
    let values = |text: &str, key: &str| -> Vec<String> {
        text.lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(name, _)| *name == key)
            .map(|(_, value)| value.to_owned())
            .collect()
    };
    for key in [
        "hostname",
        "port",
        "user",
        "hostkeyalias",
        "proxyjump",
        "proxycommand",
        "identityfile",
        "identityagent",
        "identitiesonly",
        "certificatefile",
        "userknownhostsfile",
        "globalknownhostsfile",
    ] {
        if values(standard, key) != values(&text, key) {
            return Err(error(
                Stage::Resolve,
                ErrorCode::Changed,
                &format!("The private SSH configuration would change {key} compared with Fleet's own connection. Nothing was run."),
            ));
        }
    }
    for (key, expected) in [
        ("stricthostkeychecking", "true"),
        ("updatehostkeys", "false"),
        ("controlmaster", "false"),
        ("permitlocalcommand", "no"),
    ] {
        if values(&text, key).first().map(String::as_str) != Some(expected) {
            return Err(error(
                Stage::Resolve,
                ErrorCode::Unavailable,
                &format!("The private SSH configuration does not enforce {key}. Nothing was run."),
            ));
        }
    }
    // OpenSSH omits ControlPath from -G output once it is "none".
    if values(&text, "controlpath")
        .first()
        .is_some_and(|value| value != "none")
    {
        return Err(error(
            Stage::Resolve,
            ErrorCode::Unavailable,
            "The private SSH configuration does not disable connection sharing. Nothing was run.",
        ));
    }
    Ok(())
}
fn run_setup(
    attempt: &Attempt,
    host: &Host,
    prepared: PreparedSetup,
    output: Channel<InvokeResponseBody>,
) -> Result<Option<i32>, AppError> {
    let binding = host.input.clone();
    let standard = preflight(attempt, &binding)?;
    let mode = ConnectionMode::Setup {
        label: prepared.label.clone(),
    };
    match &prepared.run {
        SetupRun::Remote { script, forward } => {
            attempt.state(ConnectionState::Connecting { stage: Stage::Tcp })?;
            let endpoint = Endpoint::new().map_err(native_io)?;
            let remote = remote_command(script);
            let command = ssh_command(&binding, &endpoint, Some(&remote), true, false, *forward)?;
            let terminal = TerminalOutput {
                channel: output,
                mode,
                exec: true,
            };
            let result = drive(
                attempt,
                command,
                Some(endpoint),
                Some(terminal),
                STARTUP_TIMEOUT,
                0,
                &binding.ssh_target,
                Some(prepared.cap),
            )?;
            Ok(result.status.code())
        }
        SetupRun::Local { steps, private } => {
            if let Some(private) = private {
                verify_private_config(attempt, &binding, &standard, private)?;
            }
            local_steps(attempt, &prepared, steps, &output, mode)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn draft(target: &str) -> HostDraft {
        HostDraft {
            label: "test".into(),
            ssh_target: target.into(),
            port_override: None,
            host_name_override: None,
            host_key_alias_override: None,
            tailnet_node_id: None,
        }
    }
    #[test]
    fn target_policy_rejects_options_and_exact_exclusions_not_substrings() {
        for target in [
            "-oProxyCommand=bad",
            "user@host other",
            "u@@host",
            "nas",
            "NAS.node.example.",
            "192.0.2.3",
            "u@[2001:db8::3]",
        ] {
            assert!(validate_binding(&draft(target)).is_err(), "{target}");
        }
        for target in [
            "Nash",
            "Nashville.example",
            "user@host",
            "user@[::1]",
            "2001:db8::2",
        ] {
            assert_eq!(validate_binding(&draft(target)), Ok(()), "{target}");
        }
        let mut override_target = draft("Nashville");
        override_target.host_name_override = Some("nas.node.example".into());
        assert!(validate_binding(&override_target).is_err());
        assert_eq!(normalized_target("user@[::1]"), "user@::1");
    }
    #[test]
    fn host_key_requires_real_fingerprint_even_without_confirm_hint() {
        let fingerprint = format!("SHA256:{}", "A".repeat(43));
        let prompt = format!("The authenticity of host 'example (127.0.0.1)' can't be established.\nED25519 key fingerprint is: {fingerprint}\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ");
        assert_eq!(
            key_details(&prompt).unwrap(),
            ("example (127.0.0.1)".into(), "ED25519".into(), fingerprint)
        );
        assert!(key_details(&prompt.replace("SHA256:", "MD5:")).is_err());
        assert!(key_details("Are you sure you want to continue connecting (yes/no)?").is_err());
    }
    #[test]
    fn acknowledgements_cannot_release_unsent_or_duplicate_credit_or_wrap() {
        let mut flow = Flow::default();
        assert_eq!(flow.reserve(WINDOW).unwrap(), 1);
        assert_eq!(flow.credit(), 0);
        assert!(flow.acknowledge(2).is_err());
        assert_eq!(flow.credit(), 0);
        flow.acknowledge(1).unwrap();
        flow.acknowledge(1).unwrap();
        assert_eq!(flow.credit(), WINDOW);
        assert!(flow.acknowledge(0).is_err());
        flow.next = Some(u32::MAX);
        assert_eq!(flow.reserve(HEADER + 1).unwrap(), u32::MAX);
        flow.acknowledge(u32::MAX).unwrap();
        assert!(flow.reserve(HEADER + 1).is_err());
    }
    #[test]
    fn old_leases_and_canceled_attempts_cannot_act() {
        let lease = ConnectionLease {
            connection_id: ConnectionId::new(),
            attempt_id: AttemptId::new(),
        };
        let attempt = Attempt::new(lease);
        let stale = ConnectionLease {
            connection_id: lease.connection_id,
            attempt_id: AttemptId::new(),
        };
        assert!(attempt.matches(&lease));
        assert!(!attempt.matches(&stale));
        attempt.cancel("test cancellation");
        assert!(!attempt.matches(&lease));
        assert_eq!(attempt.active().unwrap_err().code, ErrorCode::Canceled);
        attempt.wait();
    }
    #[test]
    fn process_incarnation_changes_invalidate_saved_identity() {
        let row = b"linux:00000000-0000-0000-0000-000000000001\taaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\t$2\t1700000000\t%3\t42\tlinux:123456\n";
        let original = parse_identity(row).unwrap();
        for altered in [
            String::from_utf8(row.to_vec())
                .unwrap()
                .replace("\t42\t", "\t43\t"),
            String::from_utf8(row.to_vec())
                .unwrap()
                .replace("linux:123456", "linux:123457"),
            String::from_utf8(row.to_vec())
                .unwrap()
                .replace("\t%3\t", "\t%4\t"),
            String::from_utf8(row.to_vec())
                .unwrap()
                .replace("000000000001", "000000000002"),
        ] {
            assert_ne!(parse_identity(altered.as_bytes()).unwrap(), original);
        }
        assert!(parse_identity(b"linux:anything\tx\t$2\t1\t%3\t42\tlinux:1\n").is_err());
    }
    #[test]
    fn peer_debug_text_cannot_be_a_session_confirmation_record() {
        let mut log = ClientLog::default();
        log.feed(b"debug1: Remote: debug2: PTY allocation request accepted on channel 0\ndebug1: Remote: Permission denied\n");
        assert_eq!(log.pty, None);
        assert!(log.failure.is_none());
        log.feed(b"debug2: PTY allocation request accepted on channel 0\ndebug2: shell request accepted on channel 0\n");
        assert_eq!(log.pty, Some(0));
        assert_eq!(log.shell, Some(0));
    }
    #[test]
    fn broad_environment_patterns_would_leak_private_roles() {
        assert!(wildcard("*", "FLEET_HELPER_NONCE"));
        assert!(wildcard("FLEET_*", "FLEET_HELPER_SOCKET"));
        assert!(!wildcard("LC_*", "FLEET_HELPER_NONCE"));
        assert!(!wildcard("LANG", "FLEET_HELPER_MODE"));
    }

    fn capture() -> (Channel<InvokeResponseBody>, Arc<Mutex<Vec<u8>>>) {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink = collected.clone();
        let channel = Channel::new(move |body| {
            if let InvokeResponseBody::Raw(frame) = body {
                sink.lock().unwrap().extend_from_slice(&frame[HEADER..]);
            }
            Ok(())
        });
        (channel, collected)
    }
    fn attempt() -> Attempt {
        let attempt = Attempt::new(ConnectionLease {
            connection_id: ConnectionId::new(),
            attempt_id: AttemptId::new(),
        });
        hold(&attempt.control).started = true;
        attempt
    }
    fn shell(script: &str, optional: bool) -> LocalStep {
        LocalStep {
            display: format!("sh -c {script}"),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            optional,
        }
    }
    fn prepared(steps: Vec<LocalStep>, cap: Duration) -> PreparedSetup {
        PreparedSetup::for_test(
            "test",
            SetupRun::Local {
                steps,
                private: None,
            },
            cap,
            "All done.",
        )
    }
    fn mode() -> ConnectionMode {
        ConnectionMode::Setup {
            label: "test".into(),
        }
    }
    #[test]
    fn local_setup_shows_real_output_and_stops_after_a_failed_step() {
        let (channel, collected) = capture();
        let steps = vec![
            shell("echo one; echo two >&2", false),
            shell("exit 7", false),
            shell("echo NEVER", false),
        ];
        let prepared = prepared(steps.clone(), Duration::from_secs(30));
        let outcome = local_steps(&attempt(), &prepared, &steps, &channel, mode()).unwrap();
        assert_eq!(outcome, Some(7));
        let text = String::from_utf8(collected.lock().unwrap().clone()).unwrap();
        assert!(text.contains("one\r\n"), "{text:?}");
        assert!(text.contains("two\r\n"), "{text:?}");
        assert!(text.contains("[exit 7]"), "{text:?}");
        assert!(text.contains("may already have changed"), "{text:?}");
        assert!(
            !text.contains("NEVER") && !text.contains("All done."),
            "{text:?}"
        );
    }
    #[test]
    fn optional_local_failures_are_reported_without_failing_setup() {
        let (channel, collected) = capture();
        let steps = vec![shell("exit 3", true), shell("echo fine", false)];
        let prepared = prepared(steps.clone(), Duration::from_secs(30));
        let outcome = local_steps(&attempt(), &prepared, &steps, &channel, mode()).unwrap();
        assert_eq!(outcome, Some(0));
        let text = String::from_utf8(collected.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("optional check") && text.contains("fine") && text.contains("All done."),
            "{text:?}"
        );
    }
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[test]
    fn canceling_a_local_setup_kills_its_whole_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pids");
        let script = format!("sleep 60 & echo $$ $! > {}; wait", pid_file.display());
        let steps = vec![shell(&script, false)];
        let prepared = prepared(steps.clone(), Duration::from_secs(120));
        let attempt = Arc::new(attempt());
        let (channel, _) = capture();
        let worker = {
            let attempt = attempt.clone();
            thread::spawn(move || local_steps(&attempt, &prepared, &steps, &channel, mode()))
        };
        let started = Instant::now();
        while !pid_file.exists() && started.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(100));
        let pids: Vec<i32> = fs::read_to_string(&pid_file)
            .unwrap()
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect();
        assert!(pids.iter().all(|p| alive(*p)));
        attempt.cancel("test");
        let result = worker.join().unwrap();
        assert_eq!(result.unwrap_err().code, ErrorCode::Canceled);
        thread::sleep(Duration::from_millis(200));
        assert!(
            pids.iter().all(|p| !alive(*p)),
            "children survived cancellation"
        );
    }
    #[test]
    fn local_setup_stops_at_its_runtime_cap() {
        let (channel, _) = capture();
        let steps = vec![shell("sleep 60", false)];
        let prepared = prepared(steps.clone(), Duration::from_millis(400));
        let started = Instant::now();
        let failure = local_steps(&attempt(), &prepared, &steps, &channel, mode()).unwrap_err();
        assert_eq!(failure.code, ErrorCode::Timeout);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
