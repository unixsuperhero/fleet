use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use ts_rs::TS;
use uuid::Uuid;

macro_rules! id {
    ($name:ident, $brand:literal) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS,
        )]
        #[serde(transparent)]
        #[ts(type = $brand)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
            pub fn simple(&self) -> uuid::fmt::Simple {
                self.0.simple()
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}
id!(HostId, "string & { readonly __brand: \"HostId\" }");
id!(SessionId, "string & { readonly __brand: \"SessionId\" }");
id!(
    ConnectionId,
    "string & { readonly __brand: \"ConnectionId\" }"
);
id!(AttemptId, "string & { readonly __brand: \"AttemptId\" }");
id!(
    ChallengeId,
    "string & { readonly __brand: \"ChallengeId\" }"
);
id!(
    RestoreTicketId,
    "string & { readonly __brand: \"RestoreTicketId\" }"
);
id!(
    SetupCheckoutId,
    "string & { readonly __brand: \"SetupCheckoutId\" }"
);
id!(
    SetupTicketId,
    "string & { readonly __brand: \"SetupTicketId\" }"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Local,
    Resolve,
    Tcp,
    HostKey,
    Auth,
    Session,
    Storage,
    Discovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    InvalidInput,
    Unavailable,
    Missing,
    Declined,
    Changed,
    Authentication,
    ConnectionLost,
    ConnectionFailed,
    Canceled,
    Busy,
    Timeout,
    CorruptBackup,
    NewerSchema,
    MigrationFailed,
    Storage,
    Ended,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppError {
    pub stage: Stage,
    pub code: ErrorCode,
    pub message: String,
}
impl AppError {
    pub fn new(stage: Stage, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            stage,
            code,
            message: message.into(),
        }
    }
}
impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for AppError {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostDraft {
    pub label: String,
    pub ssh_target: String,
    pub port_override: Option<u16>,
    pub host_name_override: Option<String>,
    pub host_key_alias_override: Option<String>,
    pub tailnet_node_id: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Host {
    pub id: HostId,
    pub input: HostDraft,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionLease {
    pub connection_id: ConnectionId,
    pub attempt_id: AttemptId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ConnectIntent {
    Plain,
    CreatePersistent { label: String },
    AttachSaved { session_id: SessionId },
    HostSetup { ticket_id: SetupTicketId },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ConnectionMode {
    Plain,
    Tmux { session_id: SessionId },
    Setup { label: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ConnectionState {
    Disconnected { reason: String },
    Connecting { stage: Stage },
    AwaitingConsent { challenge: Challenge },
    Connected { mode: ConnectionMode },
    Failed { error: AppError },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum ChallengeKind {
    HostKey,
    Authentication,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Challenge {
    pub id: ChallengeId,
    pub kind: ChallengeKind,
    pub host: String,
    pub algorithm: Option<String>,
    pub fingerprint: Option<String>,
    pub prompt: String,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ChallengeAnswer {
    Accept,
    Decline,
    Authentication { value: String },
}
impl fmt::Debug for ChallengeAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Accept => "Accept",
            Self::Decline => "Decline",
            Self::Authentication { .. } => "Authentication { value: [redacted] }",
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionEvent {
    pub lease: ConnectionLease,
    pub body: EventBody,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[allow(clippy::large_enum_variant)] // Avoid allocating a second owner for transient IPC payloads.
pub enum EventBody {
    State { state: ConnectionState },
    Session { session: SavedSession },
    Exit { exit_code: Option<i32> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionIdentity {
    pub boot_marker: String,
    pub instance_token: String,
    pub tmux_session_id: String,
    #[ts(type = "number")]
    pub created_epoch: i64,
    pub original_pane_id: String,
    pub original_pane_pid: u32,
    pub original_pane_birth_marker: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedSession {
    pub id: SessionId,
    pub host_id: HostId,
    pub label: String,
    pub remote_session_name: String,
    pub ssh_binding: HostDraft,
    pub identity: Option<SessionIdentity>,
    pub status: SessionStatus,
    pub created_at: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SessionStatus {
    Unchecked,
    Live,
    Ended { reason: String },
    Unknown { error: AppError },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoveryPeer {
    pub node_id: String,
    pub dns_name: String,
    pub display_name: String,
    pub addresses: Vec<String>,
    pub online: bool,
    pub last_seen_at: Option<String>,
    pub observed_at: String,
    pub excluded: bool,
    pub exclusion_reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoverySnapshot {
    pub observed_at: String,
    pub availability: DiscoveryAvailability,
    pub peers: Vec<DiscoveryPeer>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum DiscoveryAvailability {
    Available,
    Unavailable { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppInfo {
    pub version: String,
    pub schema_version: u32,
    pub database_path: String,
    pub backup_directory: String,
    pub platform: String,
    pub arch: String,
    pub ssh_version: Option<String>,
    pub tailscale_version: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestorePreview {
    pub ticket_id: RestoreTicketId,
    pub path: String,
    pub schema_version: u32,
    pub hosts: u32,
    pub sessions: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PickerResult<T> {
    Canceled,
    Selected { value: T },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum OperationResult {
    Canceled,
    Completed { path: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticEntry {
    pub at: String,
    pub host_id: Option<HostId>,
    pub stage: Stage,
    pub code: Option<ErrorCode>,
    pub exit_code: Option<i32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticBundle {
    pub version: String,
    pub platform: String,
    pub arch: String,
    pub entries: Vec<DiagnosticEntry>,
    pub hosts: Vec<Host>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum SetupHarness {
    Claude,
    Codex,
    Omp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SetupAction {
    Inspect,
    InstallLocalTools,
    InstallRemoteTools {
        harnesses: Vec<SetupHarness>,
    },
    StartRemoteDaemon,
    PreviewCapture,
    InstallCapture,
    CaptureStatus,
    Login {
        harness: SetupHarness,
        callback_port: Option<u16>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetupCheckout {
    pub id: SetupCheckoutId,
    pub path: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetupGuidance {
    pub title: String,
    pub detail: String,
    pub command: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SetupPrepared {
    Canceled,
    Ready {
        ticket_id: SetupTicketId,
        label: String,
        guidance: Vec<SetupGuidance>,
    },
}

macro_rules! args {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Serialize, Deserialize, TS)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
    };
}
args!(SaveHostArgs { host_id: Option<HostId>, draft: HostDraft });
args!(HostIdArgs { host_id: HostId });
args!(OptionalHostIdArgs { host_id: Option<HostId> });
args!(LeaseArgs {
    lease: ConnectionLease
});
args!(TerminalInputArgs { lease: ConnectionLease, bytes: Vec<u8> });
args!(TerminalResizeArgs {
    lease: ConnectionLease,
    cols: u16,
    rows: u16
});
args!(AnswerChallengeArgs {
    lease: ConnectionLease,
    challenge_id: ChallengeId,
    answer: ChallengeAnswer
});
args!(AcknowledgeOutputArgs {
    lease: ConnectionLease,
    sequence: u32
});
args!(SessionIdArgs {
    session_id: SessionId
});
args!(RestoreTicketArgs {
    ticket_id: RestoreTicketId
});
args!(DiagnosticsArgs {
    include_host_metadata: bool
});
args!(OpenLinkArgs { url: String });
args!(PrepareHostSetupArgs {
    host_id: HostId,
    checkout_id: Option<SetupCheckoutId>,
    alias: String,
    action: SetupAction
});

#[derive(TS)]
#[ts(rename_all = "camelCase")]
pub struct ConnectArgs {
    pub lease: ConnectionLease,
    pub host_id: HostId,
    pub intent: ConnectIntent,
    pub cols: u16,
    pub rows: u16,
    #[ts(type = "Channel<ArrayBuffer>")]
    pub output: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>,
    #[ts(type = "Channel<ConnectionEvent>")]
    pub events: tauri::ipc::Channel<ConnectionEvent>,
}
