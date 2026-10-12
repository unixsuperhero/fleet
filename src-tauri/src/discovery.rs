use crate::model::{
    AppError, DiscoveryAvailability, DiscoveryPeer, DiscoverySnapshot, ErrorCode, HostDraft, Stage,
};
use serde::Deserialize;
use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    io::{self, Read},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

const CLI_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_STDOUT: usize = 2 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
const EXCLUSION_LIMIT: usize = 256;

#[derive(Default)]
struct Observations {
    peers: Vec<DiscoveryPeer>,
    excluded: Vec<DiscoveryPeer>,
    policy_overflow: bool,
}
static OBSERVATIONS: OnceLock<Mutex<Observations>> = OnceLock::new();
fn observations() -> &'static Mutex<Observations> {
    OBSERVATIONS.get_or_init(|| Mutex::new(Observations::default()))
}
fn failure(code: ErrorCode, text: &str) -> AppError {
    AppError::new(Stage::Discovery, code, text)
}
fn excluded_error() -> AppError {
    AppError::new(
        Stage::Resolve,
        ErrorCode::InvalidInput,
        "NAS and the existing local Studio hub are excluded; no SSH connection was started.",
    )
}

struct CliOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain<R: Read>(stream: &mut R, data: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(length) => {
                if data.len() + length > limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "CLI output limit exceeded",
                    ));
                }
                data.extend_from_slice(&buffer[..length]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn run_cli(executable: &Path, arguments: &[&str]) -> Result<CliOutput, AppError> {
    let mut child = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|_| {
            failure(
                ErrorCode::Unavailable,
                "The installed CLI could not be started.",
            )
        })?;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or_else(|| {
            failure(
                ErrorCode::Unavailable,
                "The CLI output pipe is unavailable.",
            )
        })?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| failure(ErrorCode::Unavailable, "The CLI error pipe is unavailable."))?;
        nonblocking(stdout.as_raw_fd())
            .and_then(|_| nonblocking(stderr.as_raw_fd()))
            .map_err(|_| failure(ErrorCode::Unavailable, "Cannot bound CLI output reads."))?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let deadline = Instant::now() + CLI_TIMEOUT;
        let mut status = None;
        let mut out_closed = false;
        let mut err_closed = false;
        loop {
            if Instant::now() >= deadline {
                return Err(failure(ErrorCode::Timeout, "The local CLI timed out."));
            }
            if !out_closed {
                out_closed = drain(&mut stdout, &mut out, MAX_STDOUT).map_err(|_| {
                    failure(
                        ErrorCode::Unavailable,
                        "The CLI returned excessive or unreadable output.",
                    )
                })?;
            }
            if !err_closed {
                err_closed = drain(&mut stderr, &mut err, MAX_STDERR).map_err(|_| {
                    failure(
                        ErrorCode::Unavailable,
                        "The CLI returned excessive or unreadable error output.",
                    )
                })?;
            }
            if status.is_none() {
                status = child.try_wait().map_err(|_| {
                    failure(
                        ErrorCode::Unavailable,
                        "Cannot observe the local CLI exit status.",
                    )
                })?;
            }
            if out_closed && err_closed {
                if let Some(status) = status {
                    return Ok(CliOutput {
                        success: status.success(),
                        stdout: out,
                        stderr: err,
                    });
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn tailscale_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path).filter(|p| p.is_absolute()) {
            let candidate = directory.join("tailscale");
            if executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    for path in [
        "/opt/homebrew/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        let candidate = PathBuf::from(path);
        if executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    backend_state: String,
    #[serde(rename = "Self")]
    local: Option<Node>,
    peer: Option<HashMap<String, Node>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Node {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "DNSName")]
    dns_name: String,
    host_name: String,
    #[serde(rename = "TailscaleIPs")]
    addresses: Vec<String>,
    online: bool,
    #[serde(default)]
    last_seen: Option<String>,
}

fn endpoint(value: &str) -> &str {
    let value = value.rsplit_once('@').map_or(value, |(_, host)| host);
    value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value)
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim_end_matches('.')
        .eq_ignore_ascii_case(b.trim_end_matches('.'))
}

fn nas_name(value: &str) -> bool {
    let value = endpoint(value);
    same_name(value, "nas")
        || same_name(value, "nas.node.example")
        || option_env!("FLEET_NAS_DNS").is_some_and(|name| same_name(value, name))
}

fn studio_name(value: &str) -> bool {
    let value = endpoint(value);
    same_name(value, "studio")
        || same_name(value, "studio.node.example")
        || option_env!("FLEET_STUDIO_DNS").is_some_and(|name| same_name(value, name))
}

fn canonical_ip(value: &str) -> Option<IpAddr> {
    endpoint(value)
        .parse::<IpAddr>()
        .ok()
        .map(canonical_address)
}

fn canonical_address(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
        ip => ip,
    }
}

fn nas_address(value: &str) -> bool {
    const DEFAULTS: [IpAddr; 2] = [
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 3)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 3)),
    ];
    static CONFIGURED: OnceLock<[IpAddr; 2]> = OnceLock::new();
    let Some(ip) = canonical_ip(value) else {
        return false;
    };
    DEFAULTS.contains(&ip)
        || CONFIGURED
            .get_or_init(|| {
                [
                    option_env!("FLEET_NAS_IPV4").map_or(DEFAULTS[0], |value| {
                        IpAddr::V4(value.parse().expect("build.rs validates FLEET_NAS_IPV4"))
                    }),
                    option_env!("FLEET_NAS_IPV6").map_or(DEFAULTS[1], |value| {
                        canonical_address(IpAddr::V6(
                            value.parse().expect("build.rs validates FLEET_NAS_IPV6"),
                        ))
                    }),
                ]
            })
            .contains(&ip)
}

fn static_excluded(value: &str) -> bool {
    nas_name(value) || studio_name(value) || nas_address(value)
}

fn hostname(value: &str) -> bool {
    let host = endpoint(value);
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && (host.parse::<IpAddr>().is_ok()
            || host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')))
}

pub(crate) fn validate_draft(draft: &HostDraft) -> Result<(), AppError> {
    let invalid = || {
        AppError::new(
            Stage::Storage,
            ErrorCode::InvalidInput,
            "Host settings contain an invalid label, SSH target, override, or node identity.",
        )
    };
    if draft.label.trim().is_empty()
        || draft.label.len() > 128
        || draft.label.chars().any(char::is_control)
        || draft.ssh_target.len() > 320
        || draft.ssh_target.starts_with('-')
        || !hostname(&draft.ssh_target)
        || draft.ssh_target.chars().any(char::is_whitespace)
        || draft.ssh_target.matches('@').count() > 1
        || draft.port_override == Some(0)
    {
        return Err(invalid());
    }
    if let Some((user, _)) = draft.ssh_target.split_once('@') {
        if user.is_empty()
            || user.len() > 64
            || !user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        {
            return Err(invalid());
        }
    }
    if draft
        .host_name_override
        .as_ref()
        .is_some_and(|h| h.contains('@') || !hostname(h))
    {
        return Err(invalid());
    }
    if draft.host_key_alias_override.as_ref().is_some_and(|a| {
        a.is_empty()
            || a.len() > 255
            || a.starts_with('-')
            || a.chars().any(|c| c.is_control() || c.is_whitespace())
    }) {
        return Err(invalid());
    }
    if draft.tailnet_node_id.as_ref().is_some_and(|id| {
        id.is_empty() || id.len() > 128 || id.chars().any(|c| c.is_control() || c.is_whitespace())
    }) {
        return Err(invalid());
    }
    if static_excluded(&draft.ssh_target)
        || draft
            .host_name_override
            .as_ref()
            .is_some_and(|h| static_excluded(h))
    {
        return Err(AppError::new(
            Stage::Storage,
            ErrorCode::InvalidInput,
            "NAS and the existing local Studio hub are excluded from enrollment.",
        ));
    }
    Ok(())
}

fn target_matches(target: &str, peer: &DiscoveryPeer) -> bool {
    let target = endpoint(target);
    (!peer.dns_name.is_empty() && same_name(target, &peer.dns_name))
        || (!peer.display_name.is_empty() && same_name(target, &peer.display_name))
        || canonical_ip(target).is_some_and(|target| {
            peer.addresses
                .iter()
                .any(|address| canonical_ip(address) == Some(target))
        })
}

fn matches_peer(draft: &HostDraft, peer: &DiscoveryPeer) -> bool {
    draft.tailnet_node_id.as_deref() == Some(peer.node_id.as_str())
        || target_matches(&draft.ssh_target, peer)
        || draft
            .host_name_override
            .as_ref()
            .is_some_and(|h| target_matches(h, peer))
}

pub(crate) fn is_excluded_draft(draft: &HostDraft) -> bool {
    if static_excluded(&draft.ssh_target)
        || draft
            .host_name_override
            .as_ref()
            .is_some_and(|h| static_excluded(h))
    {
        return true;
    }
    match observations().lock() {
        Ok(cache) => cache.policy_overflow || cache.excluded.iter().any(|p| matches_peer(draft, p)),
        Err(_) => true,
    }
}

// Apply to ssh -G's effective hostname, resolved addresses, and concrete jump hosts before network spawn.
pub fn guard_target(target: &str) -> Result<(), AppError> {
    if static_excluded(target) {
        return Err(excluded_error());
    }
    let cache = observations().lock().map_err(|_| {
        failure(
            ErrorCode::Unavailable,
            "Native exclusion policy is unavailable.",
        )
    })?;
    if cache.policy_overflow {
        return Err(failure(
            ErrorCode::Unavailable,
            "Native exclusion history is full; connection authorization is blocked.",
        ));
    }
    if cache.excluded.iter().any(|p| target_matches(target, p)) {
        return Err(excluded_error());
    }
    Ok(())
}

// Persisted/manual records do not bypass policy, including when current discovery is unavailable.
pub fn guard_host(draft: &HostDraft) -> Result<(), AppError> {
    validate_draft(draft)?;
    let _ = discover()?;
    if is_excluded_draft(draft) {
        return Err(excluded_error());
    }
    Ok(())
}

pub(crate) fn guard_enrollment(draft: &HostDraft) -> Result<(), AppError> {
    let snapshot = discover()?;
    if is_excluded_draft(draft) {
        return Err(AppError::new(
            Stage::Storage,
            ErrorCode::InvalidInput,
            "This NAS or local hub node is excluded from enrollment.",
        ));
    }
    if let Some(id) = &draft.tailnet_node_id {
        if !matches!(snapshot.availability, DiscoveryAvailability::Available) {
            return Err(failure(ErrorCode::Unavailable, "Tailnet enrollment requires a current discovery observation. Manual SSH targets remain available."));
        }
        if !snapshot
            .peers
            .iter()
            .any(|p| p.node_id == *id && !p.excluded)
        {
            return Err(failure(
                ErrorCode::Missing,
                "The selected tailnet node is no longer in the current discovery observation.",
            ));
        }
    }
    Ok(())
}

fn peer(node: Node, local: bool, at: &str) -> Result<DiscoveryPeer, AppError> {
    if node.id.is_empty()
        || node.id.len() > 128
        || node.id.chars().any(|c| c.is_control() || c.is_whitespace())
        || node.host_name.len() > 255
        || node.host_name.chars().any(char::is_control)
        || (!node.dns_name.is_empty() && !hostname(&node.dns_name))
        || node.addresses.is_empty()
        || node.addresses.iter().any(|a| a.parse::<IpAddr>().is_err())
    {
        return Err(failure(
            ErrorCode::Unavailable,
            "Tailscale returned invalid node metadata.",
        ));
    }
    let exclusion_reason = if local {
        Some("Existing local hub; not an additional SSH peer.")
    } else if nas_name(&node.dns_name)
        || nas_name(&node.host_name)
        || node.addresses.iter().any(|a| nas_address(a))
    {
        Some("NAS is explicitly excluded; passive discovery only.")
    } else if studio_name(&node.dns_name) || studio_name(&node.host_name) {
        Some("Studio is the existing local hub, not an additional SSH peer.")
    } else {
        None
    };
    let last_seen_at = match node.last_seen {
        Some(value) if !value.starts_with("0001-") => Some(
            chrono::DateTime::parse_from_rfc3339(&value)
                .map_err(|_| {
                    failure(
                        ErrorCode::Unavailable,
                        "Tailscale returned an invalid observation timestamp.",
                    )
                })?
                .to_rfc3339(),
        ),
        _ => None,
    };
    Ok(DiscoveryPeer {
        node_id: node.id,
        dns_name: node.dns_name.trim_end_matches('.').to_string(),
        display_name: node.host_name,
        addresses: node.addresses,
        online: node.online,
        last_seen_at,
        observed_at: at.to_string(),
        excluded: exclusion_reason.is_some(),
        exclusion_reason: exclusion_reason.map(str::to_string),
    })
}

fn parse_status(bytes: &[u8], at: &str) -> Result<Vec<DiscoveryPeer>, AppError> {
    let status: Status = serde_json::from_slice(bytes).map_err(|_| {
        failure(
            ErrorCode::Unavailable,
            "Tailscale returned invalid status JSON.",
        )
    })?;
    if status.backend_state != "Running" {
        return Err(failure(
            ErrorCode::Unavailable,
            "Tailscale is disconnected or not running.",
        ));
    }
    let local = status.local.ok_or_else(|| {
        failure(
            ErrorCode::Unavailable,
            "Tailscale status is missing the local node.",
        )
    })?;
    let self_id = local.id.clone();
    let mut peers = vec![peer(local, true, at)?];
    let mut ids = BTreeSet::new();
    ids.insert(self_id.clone());
    for (_, node) in status.peer.unwrap_or_default() {
        if node.id == self_id {
            continue;
        }
        if !ids.insert(node.id.clone()) {
            return Err(failure(
                ErrorCode::Unavailable,
                "Tailscale returned duplicate node identities.",
            ));
        }
        peers.push(peer(node, false, at)?);
    }
    peers.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    Ok(peers)
}

fn snapshot_from_result(
    cache: &mut Observations,
    at: String,
    result: Result<Vec<DiscoveryPeer>, AppError>,
) -> DiscoverySnapshot {
    match result {
        Ok(peers) => {
            for peer in peers.iter().filter(|p| p.excluded) {
                if let Some(previous) = cache
                    .excluded
                    .iter_mut()
                    .find(|p| p.node_id == peer.node_id)
                {
                    *previous = peer.clone();
                } else if cache.excluded.len() < EXCLUSION_LIMIT {
                    cache.excluded.push(peer.clone());
                } else {
                    // Never discard a remembered excluded identity to make room for another.
                    cache.policy_overflow = true;
                    return DiscoverySnapshot { observed_at: at, availability: DiscoveryAvailability::Unavailable { reason: "Native exclusion history is full; connection authorization is blocked.".into() }, peers: cache.peers.clone() };
                }
            }
            cache.peers = peers.clone();
            DiscoverySnapshot {
                observed_at: at,
                availability: DiscoveryAvailability::Available,
                peers,
            }
        }
        Err(error) => DiscoverySnapshot {
            observed_at: at,
            availability: DiscoveryAvailability::Unavailable {
                reason: error.message,
            },
            peers: cache.peers.clone(),
        },
    }
}

pub fn discover() -> Result<DiscoverySnapshot, AppError> {
    let at = chrono::Utc::now().to_rfc3339();
    let result = match tailscale_path() {
        None => Err(failure(
            ErrorCode::Missing,
            "Tailscale CLI is not installed. Manual SSH targets remain available.",
        )),
        Some(path) => run_cli(&path, &["status", "--json"]).and_then(|output| {
            if !output.success {
                return Err(failure(
                    ErrorCode::Unavailable,
                    "Tailscale status failed; the local daemon may be unavailable.",
                ));
            }
            parse_status(&output.stdout, &at)
        }),
    };
    let mut cache = observations().lock().map_err(|_| {
        failure(
            ErrorCode::Unavailable,
            "Discovery observations are unavailable.",
        )
    })?;
    Ok(snapshot_from_result(&mut cache, at, result))
}

pub fn versions() -> (Option<String>, Option<String>) {
    let ssh = run_cli(Path::new("/usr/bin/ssh"), &["-V"])
        .ok()
        .and_then(|output| {
            if !output.success {
                return None;
            }
            let bytes = if output.stderr.is_empty() {
                output.stdout
            } else {
                output.stderr
            };
            let text = String::from_utf8(bytes).ok()?;
            let line = text.lines().next()?;
            if line.starts_with("OpenSSH_")
                && line.len() <= 160
                && !line.chars().any(char::is_control)
            {
                Some(line.to_string())
            } else {
                None
            }
        });
    let tailscale = tailscale_path()
        .and_then(|path| run_cli(&path, &["version"]).ok())
        .and_then(|output| {
            if !output.success {
                return None;
            }
            let text = String::from_utf8(output.stdout).ok()?;
            let line = text.lines().next()?;
            if line.len() <= 80
                && line.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && line
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'))
            {
                Some(line.to_string())
            } else {
                None
            }
        });
    (ssh, tailscale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status() -> serde_json::Value {
        json!({"BackendState":"Running", "Self":{"ID":"self", "DNSName":"local.node.example.", "HostName":"current-studio", "TailscaleIPs":["192.0.2.5"],"Online":true,"LastSeen":"0001-01-01T00:00:00Z"},
            "Peer":{"nas-key":{"ID":"nas-id","DNSName":"nas.node.example.","HostName":"example-nas","TailscaleIPs":["192.0.2.3"],"Online":true},
                    "remote-key":{"ID":"remote-id","DNSName":"remote.example.","HostName":"remote","TailscaleIPs":["2001:db8::4"],"Online":false,"LastSeen":"2026-10-05T10:18:23Z"}}})
    }

    #[test]
    fn status_distinguishes_self_nas_offline_and_missing_daemon() {
        let peers = parse_status(
            &serde_json::to_vec(&status()).unwrap(),
            "2026-10-12T10:00:00Z",
        )
        .unwrap();
        assert_eq!(
            peers
                .iter()
                .map(|p| (p.node_id.as_str(), p.excluded, p.online))
                .collect::<Vec<_>>(),
            vec![
                ("nas-id", true, true),
                ("remote-id", false, false),
                ("self", true, true)
            ]
        );
        assert_eq!(
            peers[0].exclusion_reason.as_deref(),
            Some("NAS is explicitly excluded; passive discovery only.")
        );
        assert_eq!(
            peers[2].exclusion_reason.as_deref(),
            Some("Existing local hub; not an additional SSH peer.")
        );
        assert_eq!(
            peers[1].last_seen_at.as_deref(),
            Some("2026-10-05T10:18:23+00:00")
        );
        let mut disconnected = status();
        disconnected["BackendState"] = json!("Stopped");
        assert_eq!(
            parse_status(&serde_json::to_vec(&disconnected).unwrap(), "now")
                .unwrap_err()
                .code,
            ErrorCode::Unavailable
        );
        assert_eq!(
            parse_status(b"not JSON", "now").unwrap_err().code,
            ErrorCode::Unavailable
        );
        let mut missing = status();
        missing["Self"] = serde_json::Value::Null;
        assert_eq!(
            parse_status(&serde_json::to_vec(&missing).unwrap(), "now")
                .unwrap_err()
                .code,
            ErrorCode::Unavailable
        );
    }

    #[test]
    fn failed_refresh_preserves_previous_observations_as_stale_not_empty_success() {
        let mut cache = Observations::default();
        let peers = parse_status(
            &serde_json::to_vec(&status()).unwrap(),
            "2026-10-12T10:00:00Z",
        )
        .unwrap();
        snapshot_from_result(&mut cache, "2026-10-12T10:00:00Z".into(), Ok(peers));
        let snapshot = snapshot_from_result(
            &mut cache,
            "2026-10-12T10:01:00Z".into(),
            Err(failure(ErrorCode::Timeout, "Tailscale timed out")),
        );
        assert!(matches!(
            snapshot.availability,
            DiscoveryAvailability::Unavailable { .. }
        ));
        assert_eq!(
            snapshot
                .peers
                .iter()
                .map(|p| (p.node_id.as_str(), p.observed_at.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("nas-id", "2026-10-12T10:00:00Z"),
                ("remote-id", "2026-10-12T10:00:00Z"),
                ("self", "2026-10-12T10:00:00Z")
            ]
        );
    }

    #[test]
    fn configured_exclusions_preserve_defaults_and_guard_all_static_paths() {
        for target in [
            "nas",
            "studio",
            "USER@NAS.node.example.",
            "studio.node.example.",
            "[2001:db8:0:0:0:0:0:3]",
            "192.0.2.3",
            "::ffff:c000:203",
        ]
        .into_iter()
        .chain(
            [
                option_env!("FLEET_NAS_DNS"),
                option_env!("FLEET_STUDIO_DNS"),
                option_env!("FLEET_NAS_IPV4"),
                option_env!("FLEET_NAS_IPV6"),
            ]
            .into_iter()
            .flatten(),
        ) {
            assert_eq!(
                guard_target(target).unwrap_err().code,
                ErrorCode::InvalidInput
            );
            let mut draft = HostDraft {
                label: "example exclusion".into(),
                ssh_target: target.into(),
                port_override: None,
                host_name_override: None,
                host_key_alias_override: None,
                tailnet_node_id: None,
            };
            assert_eq!(
                validate_draft(&draft).unwrap_err().code,
                ErrorCode::InvalidInput
            );
            assert!(is_excluded_draft(&draft));
            draft.ssh_target = "unrelated-alias".into();
            draft.host_name_override = Some(target.into());
            assert_eq!(
                validate_draft(&draft).unwrap_err().code,
                ErrorCode::InvalidInput
            );
            assert!(is_excluded_draft(&draft));
            let address = canonical_ip(target);
            let observed =
                peer(
                    Node {
                        id: "example-excluded-peer".into(),
                        dns_name: if address.is_none() {
                            endpoint(target).into()
                        } else {
                            String::new()
                        },
                        host_name: "example-peer".into(),
                        addresses: vec![
                            address.map_or_else(|| "192.0.2.7".into(), |ip| ip.to_string())
                        ],
                        online: true,
                        last_seen: None,
                    },
                    false,
                    "2026-10-12T10:00:00Z",
                )
                .unwrap();
            assert!(observed.excluded);
            if address.is_none() {
                let alternate = format!(
                    "user@{}.",
                    endpoint(target).trim_end_matches('.').to_ascii_uppercase()
                );
                assert_eq!(
                    guard_target(&alternate).unwrap_err().code,
                    ErrorCode::InvalidInput
                );
            }
        }
        for configured in [option_env!("FLEET_NAS_IPV4"), option_env!("FLEET_NAS_IPV6")]
            .into_iter()
            .flatten()
        {
            let ip = canonical_ip(configured).unwrap();
            let alternate = match ip {
                IpAddr::V4(ip) => format!("user@[{}]", ip.to_ipv6_mapped()),
                IpAddr::V6(ip) => format!("user@[{ip}]"),
            };
            assert_eq!(
                guard_target(&alternate).unwrap_err().code,
                ErrorCode::InvalidInput
            );
        }
        assert_eq!(guard_target("unrelated-peer.example"), Ok(()));
        assert_eq!(
            canonical_ip("user@[::ffff:192.0.2.7]"),
            Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)))
        );
    }

    #[test]
    fn native_target_policy_blocks_exact_nas_targets_not_similar_names() {
        for target in [
            "user@nas.node.example.",
            "[2001:db8::3]",
            "192.0.2.3",
            "::ffff:192.0.2.3",
        ] {
            assert_eq!(
                guard_target(target).unwrap_err().code,
                ErrorCode::InvalidInput
            );
        }
        assert!(!static_excluded("nas-production.example"));
        assert!(!static_excluded("synapse.example"));
        let peers = parse_status(
            &serde_json::to_vec(&status()).unwrap(),
            "2026-10-12T10:00:00Z",
        )
        .unwrap();
        let nas = peers.iter().find(|p| p.node_id == "nas-id").unwrap();
        let local = peers.iter().find(|p| p.node_id == "self").unwrap();
        let remote = peers.iter().find(|p| p.node_id == "remote-id").unwrap();
        let draft = HostDraft {
            label: "unrelated label".into(),
            ssh_target: "different-alias".into(),
            port_override: None,
            host_name_override: None,
            host_key_alias_override: None,
            tailnet_node_id: Some("nas-id".into()),
        };
        assert!(matches_peer(&draft, nas));
        assert!(target_matches("user@local.node.example.", local));
        assert!(target_matches("192.0.2.5", local));
        assert!(!target_matches("remote.example", local));
        assert!(target_matches("remote.example", remote));
    }

    #[test]
    fn bounded_exclusion_history_fails_closed_instead_of_forgetting_nas() {
        let peers = parse_status(
            &serde_json::to_vec(&status()).unwrap(),
            "2026-10-12T10:00:00Z",
        )
        .unwrap();
        let nas = peers
            .iter()
            .find(|p| p.node_id == "nas-id")
            .unwrap()
            .clone();
        let mut cache = Observations::default();
        cache.excluded.push(nas.clone());
        for index in 1..EXCLUSION_LIMIT {
            let mut local = nas.clone();
            local.node_id = format!("old-local-{index}");
            cache.excluded.push(local);
        }
        let mut extra = nas;
        extra.node_id = "new-local-over-limit".into();
        let snapshot =
            snapshot_from_result(&mut cache, "2026-10-12T10:01:00Z".into(), Ok(vec![extra]));
        assert!(matches!(
            snapshot.availability,
            DiscoveryAvailability::Unavailable { .. }
        ));
        assert!(cache.policy_overflow);
        assert_eq!(cache.excluded[0].node_id, "nas-id");
    }

    #[test]
    fn bounded_runner_drains_both_pipes_and_kills_timeout_group() {
        let output = run_cli(
            Path::new("/bin/sh"),
            &["-c", "printf stdout; printf stderr >&2"],
        )
        .unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");
        let start = Instant::now();
        let timeout = match run_cli(Path::new("/bin/sh"), &["-c", "sleep 30"]) {
            Ok(_) => panic!("Timeout was not enforced"),
            Err(e) => e,
        };
        assert_eq!(timeout.code, ErrorCode::Timeout);
        assert!(start.elapsed() < Duration::from_secs(6));
    }
}
