//! Host setup for TUIOS and the Inbox collector.
//!
//! The frontend never supplies commands. It names a closed `SetupAction`; this module turns
//! that into a fixed script or argument vector, shows the truthful native consent text,
//! and issues a single-use ticket bound to the exact host snapshot, store generation and
//! the native-picked checkout content. `TerminalHub` runs the prepared setup and owns
//! leases, authentication, output and process shutdown.
use crate::{
    discovery,
    model::*,
    terminal::{normalized_target, quote},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::{CStr, OsString},
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

const TICKET_TTL: Duration = Duration::from_secs(900);
const MAX_TICKETS: usize = 8;
const MAX_CHECKOUTS: usize = 8;
const QUICK_CAP: Duration = Duration::from_secs(600);
const INSTALL_CAP: Duration = Duration::from_secs(1800);
const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_FILES: usize = 200;
const SETUP_SCRIPT: &str = "scripts/setup-cross-host.mjs";

const BUN_URL: &str = "https://bun.sh/install";
const TUIOS_RELEASE_URL: &str = "https://api.github.com/repos/Gaurav-Gosain/tuios/releases/latest";
const CLAUDE_URL: &str = "https://claude.ai/install.sh";
const CODEX_URL: &str = "https://chatgpt.com/codex/install.sh";
const OMP_PACKAGE: &str = "@oh-my-pi/pi-coding-agent";

fn hold<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
fn refuse(code: ErrorCode, message: impl Into<String>) -> AppError {
    AppError::new(Stage::Local, code, message)
}
fn invalid(message: impl Into<String>) -> AppError {
    refuse(ErrorCode::InvalidInput, message)
}
fn stale(message: &str) -> AppError {
    AppError::new(Stage::Storage, ErrorCode::Canceled, message)
}

/// One fixed local process. Arguments are never interpreted by a shell unless the program is
/// `/bin/sh` running one of this module's constant scripts.
#[derive(Clone, Debug)]
pub struct LocalStep {
    pub display: String,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// A failing optional step is reported but does not change the setup result.
    pub optional: bool,
}
#[derive(Clone, Debug)]
pub struct PrivateSsh {
    pub path: PathBuf,
    pub content: String,
}
#[derive(Clone, Debug)]
pub enum SetupRun {
    /// A constant script executed over Fleet's authenticated, host-key-verified SSH terminal.
    Remote {
        script: String,
        forward: Option<u16>,
    },
    /// Fixed local processes. They run after the host is resolved and, when a private SSH
    /// config is present, after it is proven equivalent to Fleet's own SSH transport.
    Local {
        steps: Vec<LocalStep>,
        private: Option<PrivateSsh>,
    },
}

/// Released when the setup finishes or is dropped.
pub struct CentralGuard(Arc<AtomicBool>);
impl Drop for CentralGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
pub struct PreparedSetup {
    pub label: String,
    pub run: SetupRun,
    pub cap: Duration,
    pub epilogue: String,
    checkout: Option<(PathBuf, [u8; 32])>,
    _central: Option<CentralGuard>,
}
impl PreparedSetup {
    #[cfg(test)]
    pub(crate) fn for_test(label: &str, run: SetupRun, cap: Duration, epilogue: &str) -> Self {
        Self {
            label: label.into(),
            run,
            cap,
            epilogue: epilogue.into(),
            checkout: None,
            _central: None,
        }
    }
    /// Re-proves the installer code immediately before a local process is started.
    pub fn verify_checkout(&self) -> Result<(), AppError> {
        match &self.checkout {
            Some((path, digest)) if inspect_checkout(path)?.digest != *digest => Err(stale(
                "The Inbox folder's installer code changed after you confirmed setup. Prepare it again.",
            )),
            _ => Ok(()),
        }
    }
}

struct Ticket {
    created: Instant,
    host: Host,
    generation: u64,
    label: String,
    run: SetupRun,
    cap: Duration,
    epilogue: String,
    central: bool,
    checkout: Option<(PathBuf, [u8; 32])>,
}
/// A confirmed-nothing-yet plan: native consent text plus everything a ticket needs.
pub struct Plan {
    pub title: String,
    pub message: String,
    pub button: &'static str,
    guidance: Vec<SetupGuidance>,
    ticket: Ticket,
}

pub struct Setup {
    directory: PathBuf,
    tickets: Mutex<HashMap<SetupTicketId, Ticket>>,
    checkouts: Mutex<Vec<(SetupCheckoutId, PathBuf)>>,
    central: Arc<AtomicBool>,
    /// Searched before the standard locations; empty outside tests.
    tool_dirs: Vec<PathBuf>,
}

struct Checkout {
    path: PathBuf,
    digest: [u8; 32],
}

fn regular(path: &Path) -> Result<fs::Metadata, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        invalid(format!(
            "{} is missing from the selected Inbox folder.",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.len() > MAX_FILE {
        return Err(invalid(format!(
            "{} must be a regular file of at most 4 MiB.",
            path.display()
        )));
    }
    Ok(metadata)
}
fn inspect_checkout(path: &Path) -> Result<Checkout, AppError> {
    let script = path.join(SETUP_SCRIPT);
    regular(&script)?;
    regular(&path.join("server.mjs"))?;
    if !fs::symlink_metadata(path.join("public"))
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(invalid("The selected folder has no public/ directory, so the central Inbox service could not run from it."));
    }
    let text = fs::read_to_string(&script)
        .map_err(|_| invalid("The Inbox installer could not be read."))?;
    if !text.contains("--ssh-config") {
        return Err(invalid("This Inbox checkout's installer does not support --ssh-config. Update the checkout, then choose it again."));
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(path.join("scripts"))
        .map_err(|_| invalid("The Inbox scripts directory could not be read."))?
    {
        let entry = entry.map_err(|_| invalid("The Inbox scripts directory could not be read."))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|_| invalid("The Inbox scripts directory could not be read."))?;
        if metadata.is_file() {
            names.push(entry.file_name());
        }
        if names.len() > MAX_FILES {
            return Err(invalid(
                "The Inbox scripts directory is unexpectedly large.",
            ));
        }
    }
    names.sort();
    let mut hash = Sha256::new();
    let mut add = |label: &[u8], file: &Path| -> Result<(), AppError> {
        regular(file)?;
        let mut bytes = Vec::new();
        fs::File::open(file)
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .map_err(|_| invalid("An Inbox installer file could not be read."))?;
        hash.update((label.len() as u64).to_be_bytes());
        hash.update(label);
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(&bytes);
        Ok(())
    };
    for name in &names {
        add(name.as_encoded_bytes(), &path.join("scripts").join(name))?;
    }
    add(b"server.mjs", &path.join("server.mjs"))?;
    // Bun still reads package.json from the working directory (module resolution settings).
    if fs::symlink_metadata(path.join("package.json")).is_ok() {
        add(b"package.json", &path.join("package.json"))?;
    }
    Ok(Checkout {
        path: path.to_path_buf(),
        digest: hash.finalize().into(),
    })
}
fn short(digest: &[u8; 32]) -> String {
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
/// Same search order as the remote script: `$HOME` locations, Homebrew, `/usr/local`, then `PATH`.
fn local_tool(name: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = extra.to_vec();
    if let Some(home) = &home {
        dirs.extend([
            home.join("bin"),
            home.join(".local/bin"),
            home.join(".bun/bin"),
        ]);
    }
    dirs.extend(["/opt/homebrew/bin".into(), "/usr/local/bin".into()]);
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|p| p.is_absolute()));
    }
    dirs.into_iter()
        .map(|d| d.join(name))
        .find(|candidate| executable(candidate))
}
fn local_account() -> String {
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            return "this Mac account".into();
        }
        CStr::from_ptr((*entry).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}
fn remote_account(target: &str) -> String {
    match target.rsplit_once('@') {
        Some((user, _)) => format!("the account {user}"),
        None => "the default SSH account for this host".into(),
    }
}
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 63
        && alias.starts_with(|c: char| c.is_ascii_alphanumeric())
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}
fn valid_installer_target(target: &str) -> bool {
    !target.is_empty()
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '@' | ':' | '-'))
}

// ---- Fixed remote and local scripts ----

const TOOLS_PRELUDE: &str = r#"set -u
LC_ALL=C; export LC_ALL
[ -n "${HOME:-}" ] || { printf '%s\n' 'HOME is not set for this account.'; exit 2; }
PATH="$HOME/bin:$HOME/.local/bin:$HOME/.bun/bin:$PATH"; export PATH
say() { printf '%s\n' "$*"; }
find_tool() {
 name=$1
 for d in "$HOME/bin" "$HOME/.local/bin" "$HOME/.bun/bin"; do
  if [ -f "$d/$name" ] && [ -x "$d/$name" ]; then printf '%s\n' "$d/$name"; return 0; fi
 done
 case "$name" in
  claude) known="$HOME/.claude/local/claude" ;;
  codex) known="$HOME/.codex/packages/standalone/current/bin/codex $HOME/.codex/packages/standalone/current/codex" ;;
  *) known= ;;
 esac
 for p in $known; do
  if [ -f "$p" ] && [ -x "$p" ]; then printf '%s\n' "$p"; return 0; fi
 done
 for d in /opt/homebrew/bin /usr/local/bin; do
  if [ -f "$d/$name" ] && [ -x "$d/$name" ]; then printf '%s\n' "$d/$name"; return 0; fi
 done
 found=$(command -v "$name" 2>/dev/null) || return 1
 case "$found" in /*) printf '%s\n' "$found" ;; *) return 1 ;; esac
}
show_tool() {
 if p=$(find_tool "$1"); then v=$("$p" --version 2>&1 | head -n 1); say "$1: $p  ($v)"; else say "$1: missing"; fi
}
"#;

const INSPECT_BODY: &str = r#"say "== Fleet setup: inspect (read-only) =="
say "account: $(id -un) (uid $(id -u)) on $(uname -s) $(uname -m)"
[ "$(id -u)" != 0 ] || say "warning: this is the root account. Logins, hooks and captured history belong to the account that runs the agent."
for t in bun tuios claude codex omp; do show_tool "$t"; done
if tu=$(find_tool tuios); then
 "$tu" ls --json >/dev/null 2>&1; rc=$?
 case "$rc" in
  0) say "tuios daemon: running" ;;
  3) say "tuios daemon: not running (tuios ls exited 3)" ;;
  *) say "tuios daemon: unknown (tuios ls exited $rc)" ;;
 esac
fi
case "$(uname -s)" in
 Linux)
  if command -v loginctl >/dev/null 2>&1; then say "linger: $(loginctl show-user "$(id -un)" -p Linger 2>&1 | head -n 1)"; else say "linger: loginctl not found"; fi
  if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then say "systemd user manager: available"; else say "systemd user manager: unavailable (collector services need it)"; fi ;;
 Darwin) say "launchd: user services need an active login session for this account" ;;
esac
"#;

/// TUIOS release helpers: pick the exact release archive with Bun, verify its SHA256, and install
/// only the `tuios` entry without ever replacing an existing file. Needs `say` and `tmp`.
const TUIOS_RELEASE_FUNCS: &str = r##"tuios_select_js() {
 cat > "$tmp/select.mjs" <<'FLEET_JS'
const [file, os, arch] = process.argv.slice(2);
const fail = (m) => { console.error(m); process.exit(1); };
let rel;
try { rel = JSON.parse(await Bun.file(file).text()); } catch { fail("the release description is not valid JSON"); }
const tag = rel && rel.tag_name;
if (typeof tag !== "string" || !/^v[0-9]+\.[0-9]+\.[0-9]+$/.test(tag)) fail("the release has no plain version tag");
const base = `https://github.com/Gaurav-Gosain/tuios/releases/download/${tag}/`;
const assets = Array.isArray(rel.assets) ? rel.assets : [];
const pick = (name) => {
  const hits = assets.filter((a) => a && a.name === name);
  if (hits.length !== 1) fail(`the release has no single asset named ${name}`);
  if (hits[0].browser_download_url !== base + name) fail(`asset ${name} is not on the official release download URL`);
  return base + name;
};
const name = `tuios_${tag.slice(1)}_${os}_${arch}.tar.gz`;
console.log([tag, name, pick(name), pick("checksums.txt")].join(" "));
FLEET_JS
}
tuios_select() {
 bunx=$1; json=$2
 case "$(uname -s)" in Darwin) os=Darwin ;; Linux) os=Linux ;; *) say "tuios: no official release archive for $(uname -s)"; return 1 ;; esac
 case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;; arm64|aarch64) arch=arm64 ;; i386|i686) arch=i386 ;;
  armv6l) arch=armv6 ;; armv7l) arch=armv7 ;;
  *) say "tuios: no official release archive for $(uname -m)"; return 1 ;;
 esac
 tuios_select_js
 "$bunx" "$tmp/select.mjs" "$json" "$os" "$arch"
}
tuios_sha256() {
 if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
 elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
 else return 1; fi
}
tuios_verify() {
 archive=$1; sums=$2; asset=$3
 want=$(awk -v n="$asset" '$2 == n || $2 == "*" n { print $1 }' "$sums")
 case "$want" in
  "") say "tuios: checksums.txt has no SHA256 for $asset; refusing to install"; return 1 ;;
  *[!0-9a-fA-F]*|*"
"*) say "tuios: checksums.txt has an unusable entry for $asset; refusing to install"; return 1 ;;
 esac
 [ "${#want}" = 64 ] || { say "tuios: checksums.txt has an unusable entry for $asset; refusing to install"; return 1; }
 got=$(tuios_sha256 "$archive") || { say "tuios: no sha256sum or shasum to verify the download; refusing to install"; return 1; }
 want=$(printf '%s' "$want" | tr 'A-F' 'a-f')
 [ "$got" = "$want" ] || { say "tuios: SHA256 mismatch for $asset (expected $want, got $got); refusing to install"; return 1; }
 say "tuios: SHA256 verified for $asset"
}
tuios_place() {
 archive=$1; dest=$2
 list=$(tar -tzf "$archive") || { say "tuios: the archive cannot be read"; return 1; }
 entry=; n=0
 for e in $(printf '%s\n' "$list" | tr ' ' '?'); do
  case "$e" in tuios|./tuios) entry=$e; n=$((n + 1)) ;; esac
 done
 [ "$n" = 1 ] || { say "tuios: the archive does not hold exactly one tuios entry"; return 1; }
 x=$(mktemp -d "$tmp/extract.XXXXXX") || return 1
 tar -xzf "$archive" -C "$x" "$entry" || { say "tuios: extraction failed"; return 1; }
 { [ -f "$x/tuios" ] && [ ! -L "$x/tuios" ]; } || { say "tuios: the archive entry is not a regular file"; return 1; }
 mkdir -p "$dest" || { say "tuios: cannot create $dest"; return 1; }
 if [ -e "$dest/tuios" ] || [ -L "$dest/tuios" ]; then say "tuios: $dest/tuios already exists; left untouched"; return 1; fi
 stage="$dest/.tuios.fleet.$$"
 rm -f "$stage"
 cp "$x/tuios" "$stage" && chmod 755 "$stage" || { rm -f "$stage"; say "tuios: cannot stage the binary in $dest"; return 1; }
 if ln "$stage" "$dest/tuios" 2>/dev/null; then rm -f "$stage"; else rm -f "$stage"; say "tuios: $dest/tuios appeared meanwhile; left untouched"; return 1; fi
 say "tuios: installed $dest/tuios"
}
"##;

const INSTALL_PRELUDE: &str = r#"TUIOS_RELEASE_URL=__TUIOS_RELEASE_URL__
tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' HUP INT TERM
mkdir "$tmp/shim"
printf '%s\n' '#!/bin/sh' 'echo "Fleet setup never uses sudo. Install this tool yourself with the privileges it needs." >&2' 'exit 1' > "$tmp/shim/sudo"
chmod 700 "$tmp/shim/sudo"
fetch() {
 command -v curl >/dev/null 2>&1 || { say "curl is required to download the vendor installer."; return 1; }
 curl -fsSL --proto '=https' --tlsv1.2 "$1" -o "$2"
}
vendor() {
 name=$1; url=$2; interpreter=$3
 if p=$(find_tool "$name"); then say "$name: already installed at $p; left untouched"; return 0; fi
 command -v "$interpreter" >/dev/null 2>&1 || { say "$name: $interpreter is required by the vendor installer"; return 1; }
 say "$name: downloading $url"
 fetch "$url" "$tmp/$name.sh" || { say "$name: download failed"; return 1; }
 say "$name: running the vendor installer with $interpreter (sudo disabled)"
 PATH="$tmp/shim:$PATH" "$interpreter" "$tmp/$name.sh" || { say "$name: the vendor installer failed"; return 1; }
}
tuios_vendor() {
 if p=$(find_tool tuios); then say "tuios: already installed at $p; left untouched"; return 0; fi
 b=$(find_tool bun) || { say "tuios: Bun is required to read the official release and was not found"; return 1; }
 command -v curl >/dev/null 2>&1 || { say "tuios: curl is required to download the release."; return 1; }
 say "tuios: reading $TUIOS_RELEASE_URL"
 fetch "$TUIOS_RELEASE_URL" "$tmp/release.json" || { say "tuios: the release description could not be downloaded"; return 1; }
 sel=$(tuios_select "$b" "$tmp/release.json") || { say "tuios: no usable official release archive"; return 1; }
 set -- $sel
 say "tuios: downloading $3 and $4"
 fetch "$3" "$tmp/$2" && fetch "$4" "$tmp/checksums.txt" || { say "tuios: download failed"; return 1; }
 tuios_verify "$tmp/$2" "$tmp/checksums.txt" "$2" || return 1
 tuios_place "$tmp/$2" "$HOME/.local/bin"
}
omp_bun() {
 if p=$(find_tool omp); then say "omp: already installed at $p; left untouched"; return 0; fi
 b=$(find_tool bun) || { say "omp: Bun is required to install it and was not found"; return 1; }
 say "omp: running $b install -g @oh-my-pi/pi-coding-agent"
 PATH="$tmp/shim:$PATH" "$b" install -g @oh-my-pi/pi-coding-agent || { say "omp: bun install failed"; return 1; }
}
rc=0
"#;

fn harness_name(harness: SetupHarness) -> &'static str {
    match harness {
        SetupHarness::Claude => "claude",
        SetupHarness::Codex => "codex",
        SetupHarness::Omp => "omp",
    }
}
fn harness_install(harness: SetupHarness) -> (&'static str, &'static str, &'static str) {
    match harness {
        SetupHarness::Claude => ("claude", CLAUDE_URL, "bash"),
        SetupHarness::Codex => ("codex", CODEX_URL, "sh"),
        SetupHarness::Omp => ("omp", OMP_PACKAGE, "bun"),
    }
}
/// Installs only what is missing: Bun and TUIOS when requested, then each selected harness.
fn install_script(bun_and_tuios: bool, harnesses: &[SetupHarness]) -> String {
    let mut script = String::from(TOOLS_PRELUDE);
    script.push_str(TUIOS_RELEASE_FUNCS);
    script.push_str(&INSTALL_PRELUDE.replace("__TUIOS_RELEASE_URL__", TUIOS_RELEASE_URL));
    script.push_str("say \"== Fleet setup: install missing tools ==\"\n");
    script.push_str("[ \"$(id -u)\" != 0 ] || say \"warning: this is the root account; tools install under its home.\"\n");
    if bun_and_tuios {
        script.push_str(&format!("vendor bun {BUN_URL} bash || rc=1\n"));
        script.push_str("tuios_vendor || rc=1\n");
    }
    for harness in harnesses {
        match harness {
            SetupHarness::Omp => script.push_str("omp_bun || rc=1\n"),
            _ => {
                let (name, url, interpreter) = harness_install(*harness);
                script.push_str(&format!("vendor {name} {url} {interpreter} || rc=1\n"));
            }
        }
    }
    script.push_str("say \"== Result ==\"\n");
    let mut names = Vec::new();
    if bun_and_tuios {
        names.extend(["bun", "tuios"]);
    }
    names.extend(harnesses.iter().map(|h| harness_name(*h)));
    script.push_str(&format!(
        "for t in {}; do show_tool \"$t\"; done\n",
        names.join(" ")
    ));
    script.push_str("say \"New shells find tools in ~/.local/bin or ~/.bun/bin once PATH is updated; Fleet setup does not edit more than the vendor installers do.\"\nexit $rc\n");
    script
}
fn inspect_script() -> String {
    format!("{TOOLS_PRELUDE}{INSPECT_BODY}")
}
fn daemon_script() -> String {
    let mut script = String::from(TOOLS_PRELUDE);
    script.push_str(
        r#"say "== Fleet setup: start TUIOS daemon only if none runs =="
tu=$(find_tool tuios) || { say "tuios was not found. Install remote tools first."; exit 20; }
"$tu" ls --json >/dev/null 2>&1; rc=$?
case "$rc" in
 0) say "A TUIOS daemon is already running for this account. It was left untouched."; exit 0 ;;
 3) ;;
 *) say "tuios ls exited $rc, so the daemon state is unclear. Nothing was started."; exit 21 ;;
esac
say "No daemon is running. Starting one with a detached, automatically named session."
out=$(mktemp) || exit 2
"$tu" new --detach </dev/null >"$out" 2>&1; rc=$?
head -c 4096 "$out"; rm -f "$out"
[ "$rc" = 0 ] || { say "tuios new --detach exited $rc."; exit 22; }
"$tu" ls --json >/dev/null 2>&1; rc=$?
if [ "$rc" = 0 ]; then say "tuios daemon: running"; else say "tuios daemon check after start exited $rc"; exit 23; fi
"#,
    );
    script
}
fn login_script(harness: SetupHarness) -> String {
    let (name, args) = match harness {
        SetupHarness::Claude => ("claude", "auth login"),
        SetupHarness::Codex => ("codex", "login"),
        SetupHarness::Omp => ("omp", "login"),
    };
    format!(
        "{TOOLS_PRELUDE}t=$(find_tool {name}) || {{ say \"{name} was not found. Install remote tools first.\"; exit 20; }}\nsay \"== {name} login: the provider's own CLI runs below. Fleet reads no tokens. ==\"\nexec \"$t\" {args}\n"
    )
}

// ---- Private SSH config ----

fn include_paths() -> Result<Vec<String>, AppError> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("FLEET_SMOKE_SSH_CONFIG") {
        let path = path
            .into_string()
            .map_err(|_| invalid("The smoke SSH configuration path is not valid text."))?;
        if !path.starts_with('/')
            || path
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'))
        {
            return Err(invalid(
                "The smoke SSH configuration path is not safe to include.",
            ));
        }
        return Ok(vec![path]);
    }
    Ok(vec!["~/.ssh/config".into(), "/etc/ssh/ssh_config".into()])
}
fn safe_token(value: &str) -> Result<&str, AppError> {
    if value.is_empty()
        || value.chars().any(|c| {
            c.is_whitespace()
                || c.is_control()
                || matches!(c, '*' | '?' | '!' | ',' | '"' | '\'' | '#' | '%' | '\\')
        })
    {
        return Err(invalid(
            "This host value cannot be written to a private SSH configuration.",
        ));
    }
    Ok(value)
}
/// Overrides apply to the exact target host only. Safety options come before the included user
/// and system configuration so that OpenSSH's first-value-wins rule keeps them in force.
pub(crate) fn private_ssh_config(
    binding: &HostDraft,
    includes: &[String],
) -> Result<String, AppError> {
    let target = normalized_target(&binding.ssh_target);
    let host = target.rsplit_once('@').map_or(target.as_str(), |(_, h)| h);
    let mut text = String::from(
        "# Written by Fleet for Inbox collector setup. Immutable: the file name is derived from this content.\n",
    );
    if binding.port_override.is_some()
        || binding.host_name_override.is_some()
        || binding.host_key_alias_override.is_some()
    {
        text.push_str(&format!("Host {}\n", safe_token(host)?));
        if let Some(name) = &binding.host_name_override {
            text.push_str(&format!(
                "  HostName {}\n",
                safe_token(&normalized_target(name))?
            ));
        }
        if let Some(port) = binding.port_override {
            text.push_str(&format!("  Port {port}\n"));
        }
        if let Some(alias) = &binding.host_key_alias_override {
            text.push_str(&format!("  HostKeyAlias {}\n", safe_token(alias)?));
        }
    }
    text.push_str("Host *\n");
    for line in [
        "StrictHostKeyChecking yes",
        "UpdateHostKeys no",
        "VerifyHostKeyDNS no",
        "ControlMaster no",
        "ControlPath none",
        "ControlPersist no",
        "PermitLocalCommand no",
        "RemoteCommand none",
        "ForkAfterAuthentication no",
    ] {
        text.push_str(&format!("  {line}\n"));
    }
    text.push_str("Match all\n");
    for include in includes {
        text.push_str(&format!("Include {include}\n"));
    }
    Ok(text)
}
fn private_for(directory: &Path, binding: &HostDraft) -> Result<PrivateSsh, AppError> {
    let content = private_ssh_config(binding, &include_paths()?)?;
    let digest: [u8; 32] = Sha256::digest(content.as_bytes()).into();
    let name: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    Ok(PrivateSsh {
        path: directory.join(format!("fleet-ssh-{name}.conf")),
        content,
    })
}
/// Creates the immutable config atomically with owner-only permissions. An existing file
/// must already hold exactly this content; nothing is overwritten.
pub fn ensure_private_config(config: &PrivateSsh) -> Result<(), AppError> {
    let storage =
        |message: &str| AppError::new(Stage::Storage, ErrorCode::Storage, message.to_owned());
    let directory = config
        .path
        .parent()
        .ok_or_else(|| storage("The private SSH configuration has no directory."))?;
    fs::create_dir_all(directory)
        .map_err(|_| storage("The private SSH configuration directory could not be created."))?;
    if fs::symlink_metadata(directory)
        .map(|m| !m.is_dir())
        .unwrap_or(true)
    {
        return Err(storage(
            "The private SSH configuration directory is not a plain directory.",
        ));
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(|_| {
        storage("The private SSH configuration directory permissions could not be set.")
    })?;
    let temporary = directory.join(format!(".pending-{}", uuid::Uuid::new_v4().simple()));
    let created = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(config.content.as_bytes())?;
        file.sync_all()?;
        match fs::hard_link(&temporary, &config.path) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            other => other,
        }
    })();
    let _ = fs::remove_file(&temporary);
    created.map_err(|_| storage("The private SSH configuration could not be created."))?;
    let metadata = fs::symlink_metadata(&config.path)
        .map_err(|_| storage("The private SSH configuration could not be inspected."))?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || fs::read_to_string(&config.path).ok().as_deref() != Some(config.content.as_str())
    {
        return Err(storage("An existing private SSH configuration differs from the one Fleet prepared. It was not overwritten."));
    }
    Ok(())
}

// ---- Planning ----

struct Context<'a> {
    target: String,
    alias: String,
    checkout: Option<&'a Checkout>,
    private: Option<&'a PrivateSsh>,
}
impl Context<'_> {
    fn remote_user(&self) -> String {
        remote_account(&self.target)
    }
    fn folder(&self) -> String {
        self.checkout.map_or_else(String::new, |c| {
            format!("{} (content {})", c.path.display(), short(&c.digest))
        })
    }
    fn private_path(&self) -> String {
        self.private
            .map_or_else(String::new, |p| p.path.display().to_string())
    }
}

fn display_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().map(|a| quote(a)))
        .collect::<Vec<_>>()
        .join(" ")
}
fn installer_step(bun: &Path, checkout: &Checkout, args: Vec<String>, optional: bool) -> LocalStep {
    // bunfig.toml (preload) and .env* in the checkout are never loaded: both are disabled.
    let mut all = vec![
        "--no-env-file".to_owned(),
        "--config=/dev/null".to_owned(),
        SETUP_SCRIPT.to_owned(),
    ];
    all.extend(args);
    LocalStep {
        display: display_command("bun", &all),
        program: bun.to_path_buf(),
        args: all.iter().map(OsString::from).collect(),
        cwd: Some(checkout.path.clone()),
        optional,
    }
}
fn remote_args(command: &str, ctx: &Context, collector_only: bool, dry_run: bool) -> Vec<String> {
    let mut args = vec![
        command.to_owned(),
        "--remote".into(),
        ctx.target.clone(),
        "--host".into(),
        ctx.alias.clone(),
    ];
    if collector_only {
        args.push("--collector-only".into());
    }
    if dry_run {
        args.push("--dry-run".into());
    }
    args.push("--ssh-config".into());
    args.push(ctx.private_path());
    args
}

fn vendor_list(bun_and_tuios: bool, harnesses: &[SetupHarness]) -> String {
    let mut urls = Vec::new();
    if bun_and_tuios {
        urls.extend([BUN_URL, TUIOS_RELEASE_URL]);
    }
    urls.extend(harnesses.iter().map(|h| match h {
        SetupHarness::Omp => "`bun install -g @oh-my-pi/pi-coding-agent`",
        other => harness_install(*other).1,
    }));
    urls.join(", ")
}

fn remote_probe() -> &'static str {
    "for d in \"$HOME/bin\" \"$HOME/.local/bin\" \"$HOME/.bun/bin\" /opt/homebrew/bin /usr/local/bin; do if [ -f \"$d/tuios\" ] && [ -x \"$d/tuios\" ]; then printf '%s\\n' \"$d/tuios\"; exit 0; fi; done; command -v tuios"
}
fn capture_guidance(ctx: &Context) -> Vec<SetupGuidance> {
    let private = ctx.private_path();
    let alias = quote(&ctx.alias);
    let ssh_options = format!(
        "--ssh-option=-F {}",
        quote(&format!("--ssh-option={private}"))
    );
    let probe = format!(
        "ssh -F {} -o BatchMode=yes -o StrictHostKeyChecking=yes -- {} {}",
        quote(&private),
        quote(&ctx.target),
        quote(remote_probe())
    );
    vec![
        SetupGuidance {
            title: "Register the host natively (run by you, outside TUIOS)".into(),
            detail: "Fleet never runs this. It resolves the remote TUIOS path with a read-only probe, then adds the host using the same private SSH settings Fleet used. Registration can give panes more access, so review what TUIOS prints. Run `tuios config apply` yourself only from a terminal outside TUIOS, and only if you intend the new grants.".into(),
            command: Some(format!(
                "REMOTE_TUIOS=$({probe}) && case \"$REMOTE_TUIOS\" in /*) tuios hosts add {alias} {} --command \"$REMOTE_TUIOS\" {ssh_options} ;; *) echo 'tuios was not found on the remote host'; false ;; esac",
                quote(&ctx.target)
            )),
        },
        SetupGuidance {
            title: "Test the native link".into(),
            detail: "Dials the host with BatchMode and prints which TUIOS binary the link runs there. A host key you have not accepted must be accepted by hand first.".into(),
            command: Some(format!("tuios hosts test {alias}")),
        },
        SetupGuidance {
            title: "Confirm the link is up".into(),
            detail: "The status column should read up. no_daemon means start the remote daemon; no_tuios means install remote tools; incompatible means upgrade TUIOS on one side.".into(),
            command: Some("tuios hosts".into()),
        },
        SetupGuidance {
            title: "Activate hooks at a safe point".into(),
            detail: "Installed hooks only take effect after the remote TUIOS daemon restarts. Fleet never restarts a daemon. Do it when no work depends on it, then run Capture status.".into(),
            command: None,
        },
        SetupGuidance {
            title: "Installed is not verified".into(),
            detail: "A successful install proves files, services and hooks exist. Confirm in the Inbox that a new turn on that host arrives with its full transcript before relying on history.".into(),
            command: None,
        },
    ]
}
fn login_guidance(harness: SetupHarness, callback_port: Option<u16>) -> Vec<SetupGuidance> {
    let mut guidance = vec![SetupGuidance {
        title: "Log in as the account that runs the agent".into(),
        detail: "The terminal runs the provider's own login for the selected SSH account. Fleet does not read, copy or store tokens, and logins are never copied between hosts.".into(),
        command: None,
    }];
    let detail = match harness {
        SetupHarness::Claude => "Claude prints a sign-in URL and accepts the returned code in the terminal. If a browser redirect to http://localhost:PORT is needed instead, rerun with that explicit callback port.",
        SetupHarness::Codex => "Codex's browser login redirects to http://localhost:1455. Rerun with callback port 1455 so the redirect reaches the remote host, or use `codex login --device-auth` there.",
        SetupHarness::Omp => "omp's browser logins redirect to http://localhost:PORT. Common ports are 54545 for Anthropic and 1455 for OpenAI. For any other provider, read the port from the redirect URL and enter it explicitly; Fleet assumes no dynamic ports.",
    };
    guidance.push(SetupGuidance {
        title: "Browser callback".into(),
        detail: match callback_port {
            Some(port) => format!("{detail} This tab forwards only 127.0.0.1:{port} on this Mac to 127.0.0.1:{port} on the host while it is open."),
            None => detail.to_owned(),
        },
        command: None,
    });
    guidance
}

impl Setup {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            tickets: Mutex::new(HashMap::new()),
            checkouts: Mutex::new(Vec::new()),
            central: Arc::new(AtomicBool::new(false)),
            tool_dirs: Vec::new(),
        }
    }
    #[cfg(test)]
    fn with_tool_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.tool_dirs = dirs;
        self
    }

    pub fn register_checkout(&self, path: PathBuf) -> Result<SetupCheckout, AppError> {
        let path = path
            .canonicalize()
            .map_err(|_| invalid("The selected Inbox folder is unavailable."))?;
        inspect_checkout(&path)?;
        let mut checkouts = hold(&self.checkouts);
        let id = match checkouts.iter().find(|(_, p)| *p == path) {
            Some((id, _)) => *id,
            None => {
                let id = SetupCheckoutId::new();
                checkouts.push((id, path.clone()));
                if checkouts.len() > MAX_CHECKOUTS {
                    checkouts.remove(0);
                }
                id
            }
        };
        Ok(SetupCheckout {
            id,
            path: path.to_string_lossy().into_owned(),
        })
    }

    pub fn plan(
        &self,
        host: &Host,
        generation: u64,
        checkout_id: Option<SetupCheckoutId>,
        alias: &str,
        action: &SetupAction,
    ) -> Result<Plan, AppError> {
        discovery::guard_host(&host.input)?;
        let target = normalized_target(&host.input.ssh_target);
        let needs_checkout = matches!(
            action,
            SetupAction::PreviewCapture | SetupAction::InstallCapture | SetupAction::CaptureStatus
        );
        if needs_checkout {
            if !valid_alias(alias) {
                return Err(invalid("Use an alias of 1-63 letters, digits, dots, dashes or underscores, starting with a letter or digit."));
            }
            if !valid_installer_target(&target) {
                return Err(invalid("This SSH target cannot be passed to the Inbox installer. Use letters, digits and . _ @ : - only."));
            }
        }
        if let SetupAction::Login {
            callback_port: Some(port),
            ..
        } = action
        {
            if *port < 1024 {
                return Err(invalid("Choose a callback port from 1024 to 65535."));
            }
        }
        if let SetupAction::InstallRemoteTools { harnesses } = action {
            let mut seen = harnesses.clone();
            seen.sort_by_key(|h| harness_name(*h));
            seen.dedup();
            if seen.len() != harnesses.len() {
                return Err(invalid("Select each harness at most once."));
            }
        }
        let checkout = if needs_checkout {
            let id = checkout_id.ok_or_else(|| invalid("Choose the Inbox folder first."))?;
            let path = hold(&self.checkouts)
                .iter()
                .find(|(known, _)| *known == id)
                .map(|(_, p)| p.clone())
                .ok_or_else(|| {
                    invalid("Choose the Inbox folder again; Fleet no longer holds that selection.")
                })?;
            Some(inspect_checkout(&path)?)
        } else {
            None
        };
        let private = if needs_checkout {
            Some(private_for(&self.directory.join("setup-ssh"), &host.input)?)
        } else {
            None
        };
        let ctx = Context {
            target: target.clone(),
            alias: alias.to_owned(),
            checkout: checkout.as_ref(),
            private: private.as_ref(),
        };
        let bun = || {
            local_tool("bun", &self.tool_dirs).ok_or_else(|| {
                refuse(
                    ErrorCode::Missing,
                    "Bun was not found on this Mac. Run Install local tools first.",
                )
            })
        };
        let name = &host.input.label;
        let remote_user = ctx.remote_user();
        let local = local_account();
        let ssh_note = "SSH host-key and credential prompts appear in the new tab.";
        let (label, run, cap, central, message, epilogue, guidance) = match action {
            SetupAction::Inspect => (
                format!("Inspect {name}"),
                SetupRun::Remote { script: inspect_script(), forward: None },
                QUICK_CAP,
                false,
                format!("Fleet will connect to {name} ({target}) as {remote_user} and run a fixed read-only inspection script in a new terminal tab. It makes no changes on the host. {ssh_note}"),
                "Inspection finished. Nothing was changed.".to_owned(),
                vec![SetupGuidance {
                    title: "Run setup as the account that will own capture".into(),
                    detail: "Daemon, agent logins, hooks and transcripts must belong to the same SSH account. Fleet does not switch users.".into(),
                    command: None,
                }],
            ),
            SetupAction::InstallLocalTools => {
                let script = install_script(true, &[]);
                (
                    "Install local Bun and TUIOS".to_owned(),
                    SetupRun::Local {
                        steps: vec![LocalStep {
                            display: "Fleet local tools installer (fixed script; only missing tools)".into(),
                            program: "/bin/sh".into(),
                            args: vec!["-c".into(), script.into()],
                            cwd: None,
                            optional: false,
                        }],
                        private: None,
                    },
                    INSTALL_CAP,
                    true,
                    format!("Fleet will run vendor installers on this Mac as {local}, in a new terminal tab, only for Bun and TUIOS if they are missing ({}). Bun's vendor script runs here and installs under ~/.bun. TUIOS is not installed with its vendor script: Fleet reads the official GitHub latest release, downloads the exact archive for this Mac and its checksums.txt over HTTPS, requires the SHA256 to match, and installs only the tuios binary to ~/.local/bin/tuios for your account (never /usr/local/bin, never sudo); an existing tuios is left untouched. Installers may append PATH lines to your shell startup files. Fleet never uses sudo and never applies TUIOS grants or configuration. Canceling or a failure can leave partial installs behind.", vendor_list(true, &[])),
                    "Local tools step finished. TUIOS grants and configuration were not touched.".to_owned(),
                    vec![SetupGuidance {
                        title: "Open a new shell afterwards".into(),
                        detail: "Bun and TUIOS may be installed to ~/.bun/bin or ~/.local/bin, which a running shell does not see yet.".into(),
                        command: None,
                    }],
                )
            }
            SetupAction::InstallRemoteTools { harnesses } => (
                format!("Install tools on {name}"),
                SetupRun::Remote { script: install_script(true, harnesses), forward: None },
                INSTALL_CAP,
                false,
                format!("Fleet will connect to {name} ({target}) as {remote_user} and run vendor installers there, only for what is missing: Bun, TUIOS{} ({}). Bun's vendor script runs there and installs under ~/.bun; TUIOS is installed from the official GitHub latest release: the exact archive for that host and checksums.txt are downloaded over HTTPS, the SHA256 must match, and only the tuios binary is placed in ~/.local/bin/tuios of {remote_user} (root included; never /usr/local/bin, never sudo; an existing tuios is left untouched); Claude and Codex use their own vendor installers; OMP uses Bun's global install. Vendor installers may append PATH lines to shell startup files. Fleet never uses sudo, never asks for passwords or tokens, and does not change the SSH user. Canceling or a failure can leave partial installs behind. {ssh_note}",
                    if harnesses.is_empty() { String::new() } else { format!(" and {}", harnesses.iter().map(|h| harness_name(*h)).collect::<Vec<_>>().join(", ")) },
                    vendor_list(true, harnesses)),
                "Remote tools step finished. Provider logins are a separate step; no token was read.".to_owned(),
                vec![
                    SetupGuidance { title: "Log in to each provider on the host".into(), detail: "Use the Login action per harness. Logins are made on each host independently and never copied.".into(), command: None },
                    SetupGuidance { title: "Linux needs a user service manager".into(), detail: "The collector uses systemd --user. An administrator may need to run `loginctl enable-linger <user>` so it survives logout; Fleet does not do this.".into(), command: None },
                ],
            ),
            SetupAction::StartRemoteDaemon => (
                format!("Start TUIOS daemon on {name}"),
                SetupRun::Remote { script: daemon_script(), forward: None },
                QUICK_CAP,
                false,
                format!("Fleet will connect to {name} ({target}) as {remote_user}, run `tuios ls --json`, and only when it reports that no daemon runs (exit 3) run `tuios new --detach` to start one with an automatically named detached session. An existing daemon is reused and is never killed or restarted. A started daemon and its session keep running after Fleet closes. {ssh_note}"),
                "Daemon step finished. A running daemon was never restarted.".to_owned(),
                vec![SetupGuidance { title: "Hooks need a restart you choose".into(), detail: "Hooks installed later only activate when this daemon restarts. Fleet never restarts it; do it at a safe point yourself.".into(), command: None }],
            ),
            SetupAction::PreviewCapture => {
                let c = ctx.checkout.ok_or_else(|| invalid("Choose the Inbox folder first."))?;
                let step = installer_step(&bun()?, c, remote_args("install", &ctx, true, true), false);
                (
                    format!("Preview capture for {name}"),
                    SetupRun::Local { steps: vec![step], private: private.clone() },
                    QUICK_CAP,
                    false,
                    format!("Fleet will run code from the Inbox folder {} on this Mac as {local}: `bun {SETUP_SCRIPT} install --remote {target} --host {alias} --collector-only --dry-run --ssh-config <private>`. The dry run connects non-interactively to {target} as {remote_user} (its host key must already be trusted) and prints what it would change; it changes nothing on the host. Fleet creates its own private SSH configuration {} if absent; ~/.ssh/config is not modified.", ctx.folder(), ctx.private_path()),
                    "Preview finished. Nothing was installed or registered.".to_owned(),
                    capture_guidance(&ctx),
                )
            }
            SetupAction::InstallCapture => {
                let c = ctx.checkout.ok_or_else(|| invalid("Choose the Inbox folder first."))?;
                let bun = bun()?;
                let central_step = installer_step(&bun, c, vec!["install".into()], false);
                let remote_step = installer_step(&bun, c, remote_args("install", &ctx, true, false), false);
                (
                    format!("Install capture for {name}"),
                    SetupRun::Local { steps: vec![central_step, remote_step], private: private.clone() },
                    INSTALL_CAP,
                    true,
                    format!("Fleet will run code from the Inbox folder {} on this Mac as {local}. First the canonical central install (`bun {SETUP_SCRIPT} install`) sets up this Mac's Inbox server and collector services, hooks and configuration under your account; those services keep running from this folder after Fleet closes, so moving or deleting it breaks them. Then `install --remote {target} --host {alias} --collector-only --ssh-config <private>` connects non-interactively to {target} as {remote_user} (its host key must already be trusted), copies the collector runtime there, installs and starts a collector service, edits that account's TUIOS hook configuration and shell startup block, and registers or updates the source '{alias}' (ssh_config {}) in this Mac's collectors.json. All of this persists. Fleet does not register the native TUIOS host, apply grants, or restart any daemon. A failure or cancel can leave partial changes.", ctx.folder(), ctx.private_path()),
                    "Install finished. Installed does not mean verified: check that a new turn arrives with its full transcript, and activate hooks yourself at a safe point.".to_owned(),
                    capture_guidance(&ctx),
                )
            }
            SetupAction::CaptureStatus => {
                let c = ctx.checkout.ok_or_else(|| invalid("Choose the Inbox folder first."))?;
                let bun = bun()?;
                let mut steps = vec![
                    installer_step(&bun, c, vec!["status".into()], false),
                    installer_step(&bun, c, remote_args("status", &ctx, false, false), false),
                ];
                if let Some(tuios) = local_tool("tuios", &self.tool_dirs) {
                    steps.push(LocalStep {
                        display: display_command("tuios", &["hosts".into(), "test".into(), ctx.alias.clone()]),
                        program: tuios,
                        args: vec!["hosts".into(), "test".into(), ctx.alias.clone().into()],
                        cwd: None,
                        optional: true,
                    });
                }
                (
                    format!("Capture status for {name}"),
                    SetupRun::Local { steps, private: private.clone() },
                    QUICK_CAP,
                    false,
                    format!("Fleet will run code from the Inbox folder {} on this Mac as {local}: the canonical `status` for the central Inbox and `status --remote {target} --host {alias} --ssh-config <private>`, which reads {target} as {remote_user} non-interactively. If TUIOS is installed here it then also runs `tuios hosts test {alias}` as an optional check. Nothing is installed or changed. Fleet creates its private SSH configuration {} if absent.", ctx.folder(), ctx.private_path()),
                    "Status finished. Treat capture as unverified until a new turn arrives with its full transcript.".to_owned(),
                    capture_guidance(&ctx),
                )
            }
            SetupAction::Login { harness, callback_port } => {
                let forward = match callback_port {
                    Some(port) => format!(" It forwards only 127.0.0.1:{port} on this Mac to 127.0.0.1:{port} on the host while the tab is open."),
                    None => String::new(),
                };
                (
                    format!("Log in to {} on {name}", harness_name(*harness)),
                    SetupRun::Remote { script: login_script(*harness), forward: *callback_port },
                    INSTALL_CAP,
                    false,
                    format!("Fleet will connect to {name} ({target}) as {remote_user} and run the {} CLI's own login in a new terminal tab. You complete it there; Fleet does not read or copy any token.{forward} {ssh_note}", harness_name(*harness)),
                    "Login command ended. Fleet read no credentials; check the provider's own status on the host.".to_owned(),
                    login_guidance(*harness, *callback_port),
                )
            }
        };
        Ok(Plan {
            title: format!("Confirm setup: {label}"),
            message,
            button: "Run setup",
            guidance,
            ticket: Ticket {
                created: Instant::now(),
                host: host.clone(),
                generation,
                label,
                run,
                cap,
                epilogue,
                central,
                checkout: checkout.map(|c| (c.path, c.digest)),
            },
        })
    }

    /// Called only after the native consent dialog was accepted. The TTL starts here.
    pub fn issue(
        &self,
        plan: Plan,
    ) -> Result<(SetupTicketId, String, Vec<SetupGuidance>), AppError> {
        let mut tickets = hold(&self.tickets);
        tickets.retain(|_, t| t.created.elapsed() < TICKET_TTL);
        if tickets.len() >= MAX_TICKETS {
            return Err(AppError::new(
                Stage::Storage,
                ErrorCode::Busy,
                "Open or cancel an outstanding setup tab before preparing another.",
            ));
        }
        let id = SetupTicketId::new();
        let label = plan.ticket.label.clone();
        let mut ticket = plan.ticket;
        ticket.created = Instant::now();
        tickets.insert(id, ticket);
        Ok((id, label, plan.guidance))
    }

    /// Removing the ticket is the single-use linearization point, before any validation.
    pub fn consume(
        &self,
        id: SetupTicketId,
        host: &Host,
        generation: u64,
    ) -> Result<PreparedSetup, AppError> {
        let ticket = hold(&self.tickets).remove(&id).ok_or_else(|| {
            stale(
                "This setup confirmation was already used, canceled or expired. Prepare it again.",
            )
        })?;
        if ticket.created.elapsed() >= TICKET_TTL {
            return Err(stale("The setup confirmation expired. Prepare it again."));
        }
        if ticket.host != *host {
            return Err(stale(
                "Host metadata changed after you confirmed setup. Prepare it again.",
            ));
        }
        if ticket.generation != generation {
            return Err(stale(
                "Fleet metadata was restored after you confirmed setup. Prepare it again.",
            ));
        }
        if let Some((path, digest)) = &ticket.checkout {
            if inspect_checkout(path)?.digest != *digest {
                return Err(stale("The Inbox folder's installer code changed after you confirmed setup. Prepare it again."));
            }
        }
        let guard = if ticket.central {
            if self
                .central
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(AppError::new(
                    Stage::Local,
                    ErrorCode::Busy,
                    "Another central Inbox installer is running. Wait for it to finish, then prepare setup again.",
                ));
            }
            Some(CentralGuard(self.central.clone()))
        } else {
            None
        };
        Ok(PreparedSetup {
            label: ticket.label,
            run: ticket.run,
            cap: ticket.cap,
            epilogue: ticket.epilogue,
            checkout: ticket.checkout,
            _central: guard,
        })
    }

    pub fn clear(&self) {
        hold(&self.tickets).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn draft(target: &str) -> HostDraft {
        HostDraft {
            label: "box".into(),
            ssh_target: target.into(),
            port_override: None,
            host_name_override: None,
            host_key_alias_override: None,
            tailnet_node_id: None,
        }
    }
    fn host(draft: HostDraft) -> Host {
        Host {
            id: HostId::new(),
            input: draft,
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }
    fn ssh_g(config: &Path, target: &str) -> HashMap<String, Vec<String>> {
        let output = Command::new("/usr/bin/ssh")
            .args(["-F"])
            .arg(config)
            .args(["-G", "--", target])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for line in String::from_utf8(output.stdout).unwrap().lines() {
            if let Some((key, value)) = line.split_once(' ') {
                map.entry(key.into()).or_default().push(value.into());
            }
        }
        map
    }
    fn first<'a>(map: &'a HashMap<String, Vec<String>>, key: &str) -> &'a str {
        map.get(key)
            .and_then(|v| v.first())
            .map_or("", |s| s.as_str())
    }

    #[test]
    fn private_config_applies_overrides_to_the_target_only_and_wins_over_user_config() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user_config");
        fs::write(
            &user,
            "Host jump.example\n  Port 22\nHost *\n  ControlMaster auto\n  ControlPath /tmp/x\n  PermitLocalCommand yes\n  LocalCommand touch /tmp/pwned\n  IdentityFile /tmp/id_test\n  Port 2200\n  StrictHostKeyChecking no\n  UpdateHostKeys yes\nHost target.example\n  ProxyJump jump.example\n",
        )
        .unwrap();
        let mut binding = draft("alice@target.example");
        binding.port_override = Some(2222);
        binding.host_name_override = Some("203.0.113.9".into());
        binding.host_key_alias_override = Some("target-key".into());
        let content = private_ssh_config(&binding, &[user.display().to_string()]).unwrap();
        let config = dir.path().join("private.conf");
        fs::write(&config, &content).unwrap();

        let target = ssh_g(&config, "alice@target.example");
        assert_eq!(first(&target, "hostname"), "203.0.113.9");
        assert_eq!(first(&target, "port"), "2222");
        assert_eq!(first(&target, "hostkeyalias"), "target-key");
        assert_eq!(first(&target, "user"), "alice");
        assert_eq!(first(&target, "proxyjump"), "jump.example");
        assert_eq!(first(&target, "identityfile"), "/tmp/id_test");
        assert_eq!(first(&target, "stricthostkeychecking"), "true");
        assert_eq!(first(&target, "updatehostkeys"), "false");
        assert_eq!(first(&target, "controlmaster"), "false");
        assert!(matches!(first(&target, "controlpath"), "" | "none"));
        assert_eq!(first(&target, "permitlocalcommand"), "no");

        // A jump hop never inherits the target's overrides and still gets the safety policy.
        let hop = ssh_g(&config, "jump.example");
        assert_eq!(first(&hop, "hostname"), "jump.example");
        assert_eq!(first(&hop, "port"), "22");
        assert_eq!(first(&hop, "hostkeyalias"), "");
        assert_eq!(first(&hop, "stricthostkeychecking"), "true");
    }

    #[test]
    fn private_config_without_overrides_still_enforces_safety_and_rejects_unsafe_values() {
        let plain = private_ssh_config(&draft("host.example"), &["/dev/null".into()]).unwrap();
        assert!(!plain.contains("Host host.example"));
        let mut bad = draft("host.example");
        bad.host_key_alias_override = Some("a b".into());
        assert!(private_ssh_config(&bad, &[]).is_err());
        assert!(private_ssh_config(&draft("*.example"), &[]).is_err() || !plain.is_empty());
    }

    #[test]
    fn private_config_is_created_once_with_owner_only_access_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let binding = draft("host.example");
        let first = private_for(&dir.path().join("setup-ssh"), &binding).unwrap();
        ensure_private_config(&first).unwrap();
        ensure_private_config(&first).unwrap();
        let mode = fs::metadata(&first.path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read_to_string(&first.path).unwrap(), first.content);
        let mut moved = binding.clone();
        moved.port_override = Some(2200);
        let second = private_for(&dir.path().join("setup-ssh"), &moved).unwrap();
        assert_ne!(first.path, second.path);
        fs::write(&first.path, "tampered").unwrap();
        fs::set_permissions(&first.path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ensure_private_config(&first).is_err());
        assert_eq!(fs::read_to_string(&first.path).unwrap(), "tampered");
    }

    fn inbox_checkout(root: &Path, extra: &str) {
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::create_dir_all(root.join("public")).unwrap();
        fs::write(
            root.join(SETUP_SCRIPT),
            format!("// supports --ssh-config\n{extra}"),
        )
        .unwrap();
        fs::write(root.join("server.mjs"), "// server\n").unwrap();
    }

    #[test]
    fn tickets_are_single_use_and_bound_to_host_generation_and_checkout_content() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        inbox_checkout(&inbox, "");
        // Capture actions need a local Bun; a stub keeps this test self-contained.
        let bin = dir.path().join("bin");
        stub(&bin, "bun", "");
        let setup = Setup::new(dir.path().join("profile")).with_tool_dirs(vec![bin]);
        let checkout = setup.register_checkout(inbox.clone()).unwrap();
        let record = host(draft("alice@box.example"));

        let issue = |setup: &Setup| {
            let plan = setup
                .plan(
                    &record,
                    3,
                    Some(checkout.id),
                    "box",
                    &SetupAction::PreviewCapture,
                )
                .unwrap();
            setup.issue(plan).unwrap().0
        };
        let id = issue(&setup);
        assert!(setup.consume(id, &record, 3).is_ok());
        assert!(
            setup.consume(id, &record, 3).is_err(),
            "a spent ticket cannot run again"
        );

        let id = issue(&setup);
        let mut renamed = record.clone();
        renamed.input.label = "other".into();
        assert!(setup.consume(id, &renamed, 3).is_err());

        let id = issue(&setup);
        assert!(setup.consume(id, &record, 4).is_err());

        let id = issue(&setup);
        fs::write(
            inbox.join(SETUP_SCRIPT),
            "// supports --ssh-config\n// edited\n",
        )
        .unwrap();
        assert!(setup.consume(id, &record, 3).is_err());
        inbox_checkout(&inbox, "");

        let id = issue(&setup);
        assert!(setup.consume(SetupTicketId::new(), &record, 3).is_err());
        assert!(setup.consume(id, &record, 3).is_ok());
    }

    #[test]
    fn bun_ignores_checkout_bunfig_preload_and_env_files() {
        let Some(bun) = local_tool("bun", &[]) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("inbox");
        inbox_checkout(
            &root,
            "console.log('ran', process.env.FLEET_PROBE ?? 'noenv');\n",
        );
        fs::write(root.join("preload.mjs"), "console.log('PRELOADED');\n").unwrap();
        fs::write(root.join("bunfig.toml"), "preload = [\"./preload.mjs\"]\n").unwrap();
        fs::write(root.join(".env"), "FLEET_PROBE=loaded\n").unwrap();
        let checkout = inspect_checkout(&root).unwrap();
        let step = installer_step(&bun, &checkout, vec![], false);
        let out = Command::new(&step.program)
            .args(&step.args)
            .current_dir(&root)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("ran noenv"), "{text}");
        assert!(!text.contains("PRELOADED"), "{text}");
        fs::write(root.join("package.json"), "{}").unwrap();
        let with_package = inspect_checkout(&root).unwrap();
        assert_ne!(checkout.digest, with_package.digest);
        fs::write(root.join("package.json"), "{\"type\":\"module\"}").unwrap();
        assert_ne!(with_package.digest, inspect_checkout(&root).unwrap().digest);
    }

    #[test]
    fn checkouts_must_support_ssh_config_and_ship_the_central_service_files() {
        let dir = tempfile::tempdir().unwrap();
        let setup = Setup::new(dir.path().join("profile"));
        let old = dir.path().join("old");
        inbox_checkout(&old, "");
        fs::write(old.join(SETUP_SCRIPT), "// nothing relevant\n").unwrap();
        assert!(setup.register_checkout(old).is_err());
        let incomplete = dir.path().join("incomplete");
        inbox_checkout(&incomplete, "");
        fs::remove_dir_all(incomplete.join("public")).unwrap();
        assert!(setup.register_checkout(incomplete).is_err());
        let good = dir.path().join("good");
        inbox_checkout(&good, "");
        let a = setup.register_checkout(good.clone()).unwrap();
        let b = setup.register_checkout(good).unwrap();
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn central_installers_are_serialized_and_released() {
        let setup = Setup::new(PathBuf::from("/nonexistent"));
        let record = host(draft("alice@box.example"));
        let plan = |setup: &Setup| {
            let plan = setup
                .plan(&record, 1, None, "", &SetupAction::InstallLocalTools)
                .unwrap();
            setup.issue(plan).unwrap().0
        };
        let (a, b) = (plan(&setup), plan(&setup));
        let held = setup.consume(a, &record, 1).unwrap();
        assert_eq!(
            setup.consume(b, &record, 1).err().unwrap().code,
            ErrorCode::Busy
        );
        drop(held);
        let c = plan(&setup);
        assert!(setup.consume(c, &record, 1).is_ok());
    }

    #[test]
    fn plans_reject_unusable_inputs_before_any_dialog() {
        let setup = Setup::new(PathBuf::from("/nonexistent"));
        let record = host(draft("alice@box.example"));
        for (alias, action) in [
            ("box", SetupAction::PreviewCapture),
            ("-bad", SetupAction::InstallCapture),
        ] {
            assert!(setup.plan(&record, 1, None, alias, &action).is_err());
        }
        assert!(setup
            .plan(
                &record,
                1,
                None,
                "",
                &SetupAction::Login {
                    harness: SetupHarness::Codex,
                    callback_port: Some(80)
                }
            )
            .is_err());
        assert!(setup
            .plan(
                &record,
                1,
                None,
                "",
                &SetupAction::InstallRemoteTools {
                    harnesses: vec![SetupHarness::Omp, SetupHarness::Omp]
                }
            )
            .is_err());
        let ok = setup
            .plan(
                &record,
                1,
                None,
                "",
                &SetupAction::Login {
                    harness: SetupHarness::Omp,
                    callback_port: Some(54545),
                },
            )
            .unwrap();
        assert!(ok.message.contains("127.0.0.1:54545"));
        assert!(ok.message.contains("alice"));
    }

    fn stub(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn scripts_are_valid_posix_shell() {
        for script in [
            inspect_script(),
            daemon_script(),
            install_script(
                true,
                &[SetupHarness::Claude, SetupHarness::Codex, SetupHarness::Omp],
            ),
            login_script(SetupHarness::Claude),
        ] {
            let status = Command::new("/bin/sh")
                .arg("-n")
                .arg("-c")
                .arg(&script)
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        }
    }

    /// Runs the real TUIOS release helpers in a private temp dir; `body` is shell using them.
    fn release_sh(body: &str) -> (bool, String) {
        let tmp = tempfile::tempdir().unwrap();
        let script = format!(
            "say() {{ printf '%s\\n' \"$*\"; }}\ntmp={}\n{TUIOS_RELEASE_FUNCS}\n{body}",
            quote(tmp.path().to_str().unwrap())
        );
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        (
            out.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    }

    #[test]
    fn tuios_release_installs_only_a_verified_tuios_entry() {
        let dest = tempfile::tempdir().unwrap();
        let d = quote(dest.path().to_str().unwrap());
        let body = format!(
            r#"mkdir "$tmp/a" && printf 'binary' > "$tmp/a/tuios" && printf 'x' > "$tmp/a/other"
chmod 755 "$tmp/a/tuios"
tar -czf "$tmp/t.tar.gz" -C "$tmp/a" tuios other
sum=$(tuios_sha256 "$tmp/t.tar.gz")
printf '%s  t.tar.gz\n' "$sum" > "$tmp/ok.txt"
printf '%s  t.tar.gz\n' 0000000000000000000000000000000000000000000000000000000000000000 > "$tmp/bad.txt"
printf '%s  another.tar.gz\n' "$sum" > "$tmp/none.txt"
tuios_verify "$tmp/t.tar.gz" "$tmp/bad.txt" t.tar.gz && echo BAD-ACCEPTED
tuios_verify "$tmp/t.tar.gz" "$tmp/none.txt" t.tar.gz && echo NONE-ACCEPTED
test ! -e {d}/tuios || echo EARLY-INSTALL
tuios_verify "$tmp/t.tar.gz" "$tmp/ok.txt" t.tar.gz && tuios_place "$tmp/t.tar.gz" {d}
tuios_place "$tmp/t.tar.gz" {d} && echo REPLACED
"#
        );
        let (_, out) = release_sh(&body);
        assert!(
            !out.contains("BAD-ACCEPTED") && !out.contains("NONE-ACCEPTED"),
            "{out}"
        );
        assert!(
            !out.contains("EARLY-INSTALL") && !out.contains("REPLACED"),
            "{out}"
        );
        assert!(out.contains("left untouched"), "{out}");
        let bin = dest.path().join("tuios");
        assert_eq!(fs::read(&bin).unwrap(), b"binary");
        assert_eq!(
            fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!dest.path().join("other").exists());
        assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 1);
    }

    #[test]
    fn tuios_release_selection_rejects_unofficial_or_ambiguous_assets() {
        let host = format!(
            "{}_{}",
            if cfg!(target_os = "macos") {
                "Darwin"
            } else {
                "Linux"
            },
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x86_64"
            }
        );
        let base = "https://github.com/Gaurav-Gosain/tuios/releases/download/v1.2.3/";
        let name = format!("tuios_1.2.3_{host}.tar.gz");
        let doc = |tag: &str, url: &str, extra: &str| {
            format!(
                r#"{{"tag_name":"{tag}","assets":[{{"name":"{name}","browser_download_url":"{url}"}},{{"name":"checksums.txt","browser_download_url":"{base}checksums.txt"}}{extra}]}}"#
            )
        };
        let run = |json: String| {
            release_sh(&format!(
                "printf '%s' {} > \"$tmp/r.json\"\ntuios_select {} \"$tmp/r.json\"",
                quote(&json),
                quote(which_bun().as_str())
            ))
        };
        let (ok, out) = run(doc("v1.2.3", &format!("{base}{name}"), ""));
        assert!(
            ok && out.trim() == format!("v1.2.3 {name} {base}{name} {base}checksums.txt"),
            "{out}"
        );
        assert!(!run(doc("v1.2.3", &format!("https://evil.example/{name}"), "")).0);
        assert!(!run(doc("latest", &format!("{base}{name}"), "")).0);
        assert!(
            !run(doc(
                "v1.2.3",
                &format!("{base}{name}"),
                &format!(r#",{{"name":"{name}","browser_download_url":"{base}{name}"}}"#)
            ))
            .0
        );
        assert!(!run("{}".into()).0);
    }

    fn which_bun() -> String {
        let out = Command::new("/bin/sh")
            .args(["-c", "command -v bun"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
}
