//! SSH tunnels: the OS `ssh` binary as a child process under a pseudo-terminal, so password,
//! passphrase and host-key prompts can be answered from the app (one code path on Windows
//! ConPTY and macOS/Linux openpty via `portable-pty`; `platform::ssh_path()` is the only
//! platform difference).
//!
//! ```text
//! ssh -N [-C] -o ServerAliveInterval=60 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes
//!     -o NumberOfPasswordPrompts=1 -o ConnectTimeout=20 -o StrictHostKeyChecking=ask
//!     -o LogLevel=ERROR -L 127.0.0.1:<free port>:<db host>:<db port> -p <port>
//!     [auth options] user@bastion
//! ```
//!
//! * Never `StrictHostKeyChecking=no` and never `UserKnownHostsFile=/dev/null`: unknown hosts
//!   raise a [`HostKeyPrompt`] and the user decides; `~/.ssh/known_hosts` is used as is.
//! * Tunnels are shared by key (`user@bastion:port->target:port`); the last lease drop starts
//!   the idle timer (60 s by default) after which a reaper task kills the process.
//! * Readiness: poll the local port while the child is alive; on failure the collected ssh
//!   output (secrets masked) is classified into `Auth` / `Tunnel` / `Connect` errors.
//!
//! Threading model: `ssh` output is drained by a dedicated OS thread (PTY reads are blocking
//! and `portable-pty` is synchronous). Prompts are answered from that thread by running the
//! async [`PromptHandler`] on the tokio runtime captured at [`TunnelManager::acquire`] time
//! (`Handle::block_on`), so a prompt is answered while `ssh` is still waiting for it.
//!
//! ConPTY detail: conhost opens every session with a cursor-position request (`ESC [ 6 n`)
//! and renders nothing, holding the child's console writes, until it is answered. The reader
//! threads reply `ESC [ 1 ; 1 R` as soon as they see it (see `DSR_REQUEST`); a unix pty never
//! asks.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::error::{CoreError, Result};
use crate::logging::mask_secrets;
use crate::types::{HostKeyPrompt, SshAuth, SshConfig, SshPasswordPrompt};

/// How long `acquire` waits for the forwarded port to accept connections. The clock pauses
/// while a prompt is shown to the user and restarts after each answered prompt.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Default idle time after the last lease is dropped before a tunnel is closed.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(60);
/// Port probe interval while waiting for readiness.
const READY_POLL: Duration = Duration::from_millis(100);
/// How often the idle reaper runs.
const REAPER_INTERVAL: Duration = Duration::from_secs(5);
/// Maximum ssh output kept per tunnel (older output is discarded).
const MAX_OUTPUT: usize = 64 * 1024;
/// Output quoted in error messages is cut to this many trailing characters.
const ERROR_OUTPUT_TAIL: usize = 2000;
/// Placeholder written over secrets in anything that can reach a log or an error message.
const MASK: &str = "***";
/// Line terminator written after a prompt answer. `\r` is the Enter key on a Windows console
/// (ConPTY) and is mapped to `\n` by `ICRNL` on a unix pty, so one byte works on both.
const ENTER: &[u8] = b"\r";
/// PTY size handed to ssh. Wide enough that no ssh message is ever wrapped by ConPTY (which
/// renders the screen and would otherwise insert a line break inside "Could not resolve
/// hostname ...").
const PTY_SIZE: PtySize = PtySize { rows: 50, cols: 240, pixel_width: 0, pixel_height: 0 };
/// Cursor-position request (`DSR 6`) that conhost sends as the very first bytes of a ConPTY
/// session: `portable-pty` creates the pseudo console with `PSEUDOCONSOLE_INHERIT_CURSOR`,
/// so conhost renders nothing, and even holds the child's console writes, until a terminal
/// reports the cursor position. We are that terminal, so every reader loop answers it with
/// [`CPR_REPLY`]. Never emitted by a unix pty, where the detector simply stays idle.
const DSR_REQUEST: &[u8] = b"\x1b[6n";
/// Cursor position report (row 1, column 1) answering [`DSR_REQUEST`].
const CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// How the tunnel asks the user things. The service provides an implementation that turns
/// prompts into events; the CLI reads stdin.
#[async_trait]
pub trait PromptHandler: Send + Sync {
    async fn host_key(&self, prompt: HostKeyPrompt) -> Result<bool>;
    /// Returns `None` when the user cancels.
    async fn ssh_secret(&self, prompt: SshPasswordPrompt) -> Result<Option<String>>;
}

/// Non-interactive handler: rejects unknown host keys, provides no secrets.
pub struct DenyAllPrompts;

#[async_trait]
impl PromptHandler for DenyAllPrompts {
    async fn host_key(&self, prompt: HostKeyPrompt) -> Result<bool> {
        Err(CoreError::Tunnel(format!("unknown host key for {} ({}) and no way to ask", prompt.host, prompt.fingerprint)))
    }
    async fn ssh_secret(&self, prompt: SshPasswordPrompt) -> Result<Option<String>> {
        Err(CoreError::Tunnel(format!("ssh {} for {}@{} required and no way to ask", prompt.kind, prompt.user, prompt.host)))
    }
}

/// A live tunnel endpoint. Dropping it releases the lease (the process stays up until idle).
pub struct TunnelLease {
    pub local_host: String,
    pub local_port: u16,
    _release: Option<Box<dyn FnOnce() + Send>>,
}

impl TunnelLease {
    pub fn local(&self) -> (String, u16) {
        (self.local_host.clone(), self.local_port)
    }
}

impl Drop for TunnelLease {
    fn drop(&mut self) {
        if let Some(f) = self._release.take() {
            f();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelStatus {
    pub key: String,
    pub local_port: u16,
    pub leases: usize,
    pub up: bool,
}

// ---------------------------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------------------------

/// Sharing key for a tunnel: `user@bastion:port->target:port`.
pub fn tunnel_key(cfg: &SshConfig, target_host: &str, target_port: u16) -> String {
    format!("{}@{}:{}->{}:{}", cfg.user, cfg.host, cfg.port, target_host, target_port)
}

/// Build the `ssh` argument list (without the program name) for a local forward of
/// `127.0.0.1:local_port` to `target_host:target_port` through `cfg`.
///
/// Host-key policy is always `StrictHostKeyChecking=ask` with the default known_hosts file.
/// Authentication:
/// * `Password`: keyboard-interactive / password only, public keys disabled;
/// * `Key`: `-i <key_path>` with `IdentitiesOnly=yes` (the path is passed as given, `~`
///   expanded; validate it with [`validate_key_path`] first);
/// * `Agent`: public key authentication through the running agent, no `-i`.
pub fn build_ssh_args(cfg: &SshConfig, target_host: &str, target_port: u16, local_port: u16) -> Vec<String> {
    let mut args: Vec<String> = vec!["-N".into()];
    if cfg.compression {
        args.push("-C".into());
    }
    for opt in [
        "ServerAliveInterval=60",
        "ServerAliveCountMax=3",
        "ExitOnForwardFailure=yes",
        "NumberOfPasswordPrompts=1",
        "ConnectTimeout=20",
        "StrictHostKeyChecking=ask",
        "LogLevel=ERROR",
    ] {
        args.push("-o".into());
        args.push(opt.into());
    }
    // `-L` splits on ':', so an IPv6 literal target must be bracketed.
    let target = if target_host.contains(':') && !target_host.starts_with('[') { format!("[{target_host}]") } else { target_host.to_string() };
    args.push("-L".into());
    args.push(format!("127.0.0.1:{local_port}:{target}:{target_port}"));
    args.push("-p".into());
    args.push(cfg.port.to_string());
    match cfg.auth {
        SshAuth::Password => {
            args.push("-o".into());
            args.push("PreferredAuthentications=keyboard-interactive,password".into());
            args.push("-o".into());
            args.push("PubkeyAuthentication=no".into());
        }
        SshAuth::Key => {
            if let Some(key) = cfg.key_path.as_deref().filter(|k| !k.trim().is_empty()) {
                args.push("-i".into());
                args.push(expand_home(key).to_string_lossy().into_owned());
            }
            args.push("-o".into());
            args.push("IdentitiesOnly=yes".into());
            args.push("-o".into());
            args.push("PreferredAuthentications=publickey".into());
        }
        SshAuth::Agent => {
            args.push("-o".into());
            args.push("PreferredAuthentications=publickey".into());
        }
    }
    args.push(format!("{}@{}", cfg.user, cfg.host));
    args
}

/// Expand a leading `~` to the home directory (ssh does this itself on unix; OpenSSH for
/// Windows also accepts it, but an absolute path is unambiguous for the existence check).
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest.trim_start_matches(['/', '\\']));
            }
        }
    }
    PathBuf::from(path)
}

/// Pick a free local TCP port on 127.0.0.1.
pub fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(l.local_addr()?.port())
}

/// Can a listener bind `127.0.0.1:port` right now? Checked before ssh is started: a port that
/// another program listens on would also answer the readiness probe and make a tunnel look up
/// while ssh is failing to bind it.
fn local_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Reject PuTTY keys with a conversion hint.
pub fn validate_key_path(path: &str) -> Result<()> {
    if path.to_ascii_lowercase().ends_with(".ppk") {
        return Err(CoreError::Invalid("PuTTY .ppk keys are not supported by OpenSSH. Convert it with PuTTYgen: Conversions > Export OpenSSH key".into()));
    }
    Ok(())
}

/// Validate an [`SshConfig`] before spawning anything: host / user present, key path present,
/// not a `.ppk`, and the key file exists. Warns (via `tracing`) when the key file is readable
/// by other users, which OpenSSH would refuse.
pub fn validate_config(cfg: &SshConfig) -> Result<()> {
    if cfg.host.trim().is_empty() {
        return Err(CoreError::Invalid("ssh bastion host is required".into()));
    }
    if cfg.user.trim().is_empty() {
        return Err(CoreError::Invalid("ssh user is required".into()));
    }
    if cfg.host.contains(char::is_whitespace) || cfg.user.contains(char::is_whitespace) {
        return Err(CoreError::Invalid("ssh host and user must not contain whitespace".into()));
    }
    if cfg.auth == SshAuth::Key {
        let key = cfg
            .key_path
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| CoreError::Invalid("ssh private key path is required for key authentication".into()))?;
        validate_key_path(key)?;
        let path = expand_home(key);
        if !path.is_file() {
            return Err(CoreError::Invalid(format!("ssh private key not found: {}", path.display())));
        }
        if crate::platform::key_file_permissions_too_open(&path) {
            tracing::warn!(path = %path.display(), "ssh private key is readable by other users; OpenSSH will refuse it");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Output classification
// ---------------------------------------------------------------------------------------------

/// Details extracted from an OpenSSH "authenticity of host ... can't be established" prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyInfo {
    /// Host name as ssh printed it (without the parenthesised address).
    pub host: String,
    /// `SHA256:...` (or whatever ssh printed after "key fingerprint is").
    pub fingerprint: String,
    /// `ED25519`, `RSA`, `ECDSA`, ... when present.
    pub key_type: Option<String>,
    /// The whole prompt text, for the dialog.
    pub message: String,
}

/// What the ssh output collected so far means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Nothing actionable (yet).
    None,
    /// ssh is waiting for a yes/no answer about an unknown host key.
    HostKey(HostKeyInfo),
    /// ssh is waiting for the account password (password or keyboard-interactive).
    Password,
    /// ssh is waiting for the private key passphrase.
    Passphrase,
    /// Authentication was rejected (`Permission denied`, too many failures, unusable key).
    PermissionDenied(String),
    /// `Host key verification failed` (changed or rejected host key).
    HostKeyVerificationFailed(String),
    /// Name resolution / TCP connection to the bastion failed.
    ConnectFailed(String),
    /// The local or remote port forward could not be set up.
    ForwardFailed(String),
}

impl Classification {
    /// Is this a terminal failure (as opposed to a prompt or nothing)?
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Classification::PermissionDenied(_) | Classification::HostKeyVerificationFailed(_) | Classification::ConnectFailed(_) | Classification::ForwardFailed(_)
        )
    }

    /// Is ssh waiting for input?
    pub fn is_prompt(&self) -> bool {
        matches!(self, Classification::HostKey(_) | Classification::Password | Classification::Passphrase)
    }

    /// Convert a fatal classification into the error `acquire` returns. `user` / `host`
    /// name the bastion account in the message.
    pub fn into_error(self, user: &str, host: &str) -> Option<CoreError> {
        match self {
            Classification::PermissionDenied(_) => Some(CoreError::Auth(format!("ssh: permission denied for {user}@{host}"))),
            Classification::HostKeyVerificationFailed(line) => Some(CoreError::Tunnel(line)),
            Classification::ConnectFailed(line) => Some(CoreError::Connect(line)),
            Classification::ForwardFailed(line) => Some(CoreError::Tunnel(line)),
            Classification::None | Classification::HostKey(_) | Classification::Password | Classification::Passphrase => None,
        }
    }
}

/// Classify ssh's (ANSI-stripped) output. Fatal messages win over prompts; a prompt is only
/// reported when the text ends with it, i.e. ssh is actually waiting.
pub fn classify_output(text: &str) -> Classification {
    const AUTH: &[&str] = &[
        "Permission denied",
        "Too many authentication failures",
        "UNPROTECTED PRIVATE KEY FILE",
        "are too open",
        "incorrect passphrase",
        "No supported authentication methods",
    ];
    const HOSTKEY_FAILED: &[&str] = &["Host key verification failed", "REMOTE HOST IDENTIFICATION HAS CHANGED"];
    const CONNECT: &[&str] = &[
        "Could not resolve hostname",
        "Connection refused",
        "Connection timed out",
        "Operation timed out",
        "Network is unreachable",
        "No route to host",
        "Connection reset by peer",
        "Connection closed by",
        "kex_exchange_identification",
        "ssh: connect to host",
    ];
    const FORWARD: &[&str] =
        &["remote port forwarding failed", "Address already in use", "Could not request local forwarding", "cannot listen to port", "Bad local forwarding specification"];

    if let Some(line) = find_line(text, AUTH) {
        return Classification::PermissionDenied(line);
    }
    if let Some(line) = find_line(text, HOSTKEY_FAILED) {
        return Classification::HostKeyVerificationFailed(line);
    }
    if let Some(line) = find_line(text, CONNECT) {
        return Classification::ConnectFailed(line);
    }
    if let Some(line) = find_line(text, FORWARD) {
        return Classification::ForwardFailed(line);
    }
    if let Some(line) = text.lines().map(str::trim).find(|l| l.contains("forward") && l.contains("failed")) {
        return Classification::ForwardFailed(line.to_string());
    }

    let trimmed = text.trim_end();
    if trimmed.contains("(yes/no") && trimmed.ends_with('?') {
        return Classification::HostKey(parse_host_key_prompt(text));
    }
    if trimmed.contains("Enter passphrase for key") && trimmed.ends_with(':') {
        return Classification::Passphrase;
    }
    if trimmed.to_ascii_lowercase().ends_with("password:") {
        return Classification::Password;
    }
    Classification::None
}

/// First line containing any of `needles`, trimmed and capped to a sane length.
fn find_line(text: &str, needles: &[&str]) -> Option<String> {
    text.lines().map(str::trim).find(|l| needles.iter().any(|n| l.contains(n))).map(|l| {
        let mut s: String = l.chars().take(300).collect();
        if let Some(rest) = s.strip_prefix("ssh: ") {
            s = rest.to_string();
        }
        s
    })
}

/// Pull host, key type and fingerprint out of the host-key prompt text (regex based, so it
/// also works when ConPTY has joined lines without a newline).
fn parse_host_key_prompt(text: &str) -> HostKeyInfo {
    let host = regex::Regex::new(r"host '([^']+)'")
        .ok()
        .and_then(|re| re.captures(text))
        .map(|c| c[1].to_string())
        .map(|h| match h.find(" (") {
            Some(i) => h[..i].to_string(),
            None => h,
        })
        .unwrap_or_default();
    let (key_type, fingerprint) = regex::Regex::new(r"(?i)([A-Za-z0-9-]+) key fingerprint is (\S+)")
        .ok()
        .and_then(|re| re.captures(text))
        .map(|c| (Some(c[1].to_string()), c[2].trim_end_matches('.').to_string()))
        .unwrap_or((None, String::new()));
    // Keep only the prompt block (from "The authenticity" on) for the dialog.
    let start = text.find("The authenticity of host").unwrap_or(0);
    HostKeyInfo { host, fingerprint, key_type, message: text[start..].trim().to_string() }
}

/// Incremental ANSI / VT escape stripper: ConPTY renders the screen and emits cursor movement,
/// colour and mode sequences around ssh's plain text. Sequences may be split across reads,
/// so the parser keeps its state between `feed` calls. `\r` and other control characters
/// (except `\n` and `\t`) are dropped.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum StripState {
    #[default]
    Text,
    Esc,
    EscIntermediate,
    Csi,
    Str,
    StrEsc,
}

#[derive(Debug, Default)]
pub struct AnsiStripper {
    state: StripState,
}

impl AnsiStripper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append the plain-text content of `input` to `out`.
    pub fn feed(&mut self, input: &str, out: &mut String) {
        for ch in input.chars() {
            self.state = match self.state {
                StripState::Text => match ch {
                    '\x1b' => StripState::Esc,
                    '\n' | '\t' => {
                        out.push(ch);
                        StripState::Text
                    }
                    c if c.is_control() => StripState::Text,
                    c => {
                        out.push(c);
                        StripState::Text
                    }
                },
                StripState::Esc => match ch {
                    '[' => StripState::Csi,
                    ']' | 'P' | 'X' | '^' | '_' => StripState::Str,
                    '\x20'..='\x2f' => StripState::EscIntermediate,
                    _ => StripState::Text,
                },
                StripState::EscIntermediate => match ch {
                    '\x20'..='\x2f' => StripState::EscIntermediate,
                    _ => StripState::Text,
                },
                StripState::Csi => match ch {
                    '\x40'..='\x7e' => StripState::Text,
                    _ => StripState::Csi,
                },
                StripState::Str => match ch {
                    '\x07' => StripState::Text,
                    '\x1b' => StripState::StrEsc,
                    _ => StripState::Str,
                },
                StripState::StrEsc => match ch {
                    '\\' => StripState::Text,
                    '\x1b' => StripState::StrEsc,
                    _ => StripState::Str,
                },
            };
        }
    }
}

/// Strip escape sequences from a complete string.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    AnsiStripper::new().feed(input, &mut out);
    out
}

/// Finds [`DSR_REQUEST`] in the raw byte stream read from the PTY, also when a read boundary
/// falls inside the sequence.
#[derive(Debug, Default)]
struct DsrDetector {
    /// Bytes of [`DSR_REQUEST`] matched so far.
    matched: usize,
}

impl DsrDetector {
    /// Number of complete requests found in `bytes`.
    fn feed(&mut self, bytes: &[u8]) -> usize {
        let mut found = 0;
        for &b in bytes {
            if b == DSR_REQUEST[self.matched] {
                self.matched += 1;
                if self.matched == DSR_REQUEST.len() {
                    found += 1;
                    self.matched = 0;
                }
            } else {
                self.matched = usize::from(b == DSR_REQUEST[0]);
            }
        }
        found
    }
}

/// Answer conhost's cursor-position request so it starts rendering (see [`DSR_REQUEST`]).
/// A failed write means the child side is gone; the read loop sees EOF next, so it is ignored.
fn answer_cursor_request(writer: &mut dyn Write) {
    let _ = writer.write_all(CPR_REPLY).and_then(|_| writer.flush());
}

/// Last `n` characters of `s` (on a char boundary).
fn tail(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let skip = count - n;
    let idx = s.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
    &s[idx..]
}

/// Lock a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------------------------
// PTY reader thread
// ---------------------------------------------------------------------------------------------

/// State shared between the reader thread and the owner of the process.
#[derive(Default)]
struct ReaderShared {
    /// ANSI-stripped output with secrets masked: safe to quote in errors and logs.
    masked: String,
    /// Terminal failure detected by the reader (classified ssh message, rejected host key,
    /// cancelled prompt, handler error).
    fatal: Option<CoreError>,
    /// A prompt is currently shown to the user; the readiness clock is paused.
    prompt_pending: bool,
    /// When the last prompt was answered; the readiness clock restarts from here.
    last_answer: Option<Instant>,
    /// The reader thread has finished (EOF / read error).
    eof: bool,
}

/// Everything the reader thread needs to answer prompts.
struct ReaderCtx {
    prompts: Arc<dyn PromptHandler>,
    handle: tokio::runtime::Handle,
    user: String,
    host: String,
    /// Stored password / passphrase, used for the first secret prompt without asking.
    stored_secret: Option<String>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    shared: Arc<Mutex<ReaderShared>>,
}

impl ReaderCtx {
    fn fail(&mut self, err: CoreError) {
        let mut s = lock(&self.shared);
        if s.fatal.is_none() {
            s.fatal = Some(err);
        }
        drop(s);
        let _ = self.killer.kill();
    }

    fn set_pending(&self, pending: bool) {
        let mut s = lock(&self.shared);
        s.prompt_pending = pending;
        if !pending {
            s.last_answer = Some(Instant::now());
        }
    }
}

/// Drain the PTY until EOF, answering prompts and classifying failures.
fn run_reader(mut reader: Box<dyn Read + Send>, mut writer: Box<dyn Write + Send>, mut ctx: ReaderCtx) {
    let mut raw = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();
    let mut dsr = DsrDetector::default();
    let mut stripper = AnsiStripper::new();
    let mut text = String::new();
    let mut handled = 0usize;
    let mut secrets: Vec<String> = Vec::new();
    let mut answered_host_key = false;
    let mut answered_secret = false;

    loop {
        let n = match reader.read(&mut raw) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO / broken pipe: the child is gone
        };
        if dsr.feed(&raw[..n]) > 0 {
            answer_cursor_request(writer.as_mut());
        }
        pending.extend_from_slice(&raw[..n]);
        // Decode complete UTF-8 only; keep an incomplete trailing sequence for the next read.
        let chunk = match std::str::from_utf8(&pending) {
            Ok(s) => s.to_string(),
            Err(e) if e.error_len().is_none() => String::from_utf8_lossy(&pending[..e.valid_up_to()]).into_owned(),
            Err(_) => String::from_utf8_lossy(&pending).into_owned(),
        };
        let consumed = match std::str::from_utf8(&pending) {
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            _ => pending.len(),
        };
        pending.drain(..consumed);
        stripper.feed(&chunk, &mut text);

        // Keep memory bounded: drop the oldest half once the buffer grows too large.
        if text.len() > MAX_OUTPUT {
            let mut cut = text.len() / 2;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text.drain(..cut);
            handled = handled.saturating_sub(cut);
        }
        publish(&ctx.shared, &text, &secrets);

        match classify_output(&text[handled..]) {
            Classification::None => {}
            Classification::HostKey(info) => {
                handled = text.len();
                if answered_host_key {
                    continue; // ConPTY repainted the prompt we already answered
                }
                answered_host_key = true;
                tracing::info!(host = %info.host, fingerprint = %info.fingerprint, "ssh host key confirmation requested");
                let prompt = HostKeyPrompt {
                    prompt_id: uuid::Uuid::new_v4().to_string(),
                    host: if info.host.is_empty() { ctx.host.clone() } else { info.host.clone() },
                    fingerprint: info.fingerprint.clone(),
                    key_type: info.key_type.clone(),
                    message: info.message.clone(),
                };
                ctx.set_pending(true);
                let answer = ctx.handle.block_on(ctx.prompts.host_key(prompt));
                ctx.set_pending(false);
                match answer {
                    Ok(true) => {
                        if write_answer(writer.as_mut(), "yes").is_err() {
                            ctx.fail(CoreError::Tunnel("ssh closed its terminal before the host key answer".into()));
                            break;
                        }
                    }
                    Ok(false) => {
                        let _ = write_answer(writer.as_mut(), "no");
                        ctx.fail(CoreError::Tunnel(format!("host key for {} ({}) rejected by user", info.host, info.fingerprint)));
                    }
                    Err(e) => ctx.fail(e),
                }
            }
            c @ (Classification::Password | Classification::Passphrase) => {
                handled = text.len();
                if answered_secret {
                    continue;
                }
                answered_secret = true;
                let kind = if c == Classification::Password { "password" } else { "passphrase" };
                let secret = match ctx.stored_secret.take() {
                    Some(s) => {
                        tracing::debug!(kind, "answering ssh prompt with the stored secret");
                        Some(s)
                    }
                    None => {
                        tracing::info!(kind, user = %ctx.user, host = %ctx.host, "ssh secret requested");
                        let prompt =
                            SshPasswordPrompt { prompt_id: uuid::Uuid::new_v4().to_string(), host: ctx.host.clone(), user: ctx.user.clone(), kind: kind.to_string() };
                        ctx.set_pending(true);
                        let r = ctx.handle.block_on(ctx.prompts.ssh_secret(prompt));
                        ctx.set_pending(false);
                        match r {
                            Ok(s) => s,
                            Err(e) => {
                                ctx.fail(e);
                                continue;
                            }
                        }
                    }
                };
                match secret {
                    Some(s) => {
                        let ok = write_answer(writer.as_mut(), &s).is_ok();
                        secrets.push(s);
                        publish(&ctx.shared, &text, &secrets);
                        if !ok {
                            ctx.fail(CoreError::Tunnel(format!("ssh closed its terminal before the {kind} could be sent")));
                            break;
                        }
                    }
                    None => ctx.fail(CoreError::Cancelled),
                }
            }
            fatal => {
                handled = text.len();
                if let Some(err) = fatal.into_error(&ctx.user, &ctx.host) {
                    tracing::warn!(error = %err, "ssh tunnel failed");
                    ctx.fail(err);
                }
            }
        }
    }
    lock(&ctx.shared).eof = true;
}

/// Write one prompt answer followed by Enter. The answer itself is never logged.
fn write_answer(writer: &mut dyn Write, answer: &str) -> std::io::Result<()> {
    writer.write_all(answer.as_bytes())?;
    writer.write_all(ENTER)?;
    writer.flush()
}

/// Store the masked output for error messages / diagnostics.
fn publish(shared: &Mutex<ReaderShared>, text: &str, secrets: &[String]) {
    let mut masked = text.to_string();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        masked = masked.replace(s.as_str(), MASK);
    }
    lock(shared).masked = mask_secrets(&masked);
}

// ---------------------------------------------------------------------------------------------
// Process handle
// ---------------------------------------------------------------------------------------------

/// A running ssh process with its PTY.
struct TunnelProcess {
    child: Option<Box<dyn Child + Send + Sync>>,
    /// Dropping the master closes the pseudo console, which is what makes the reader thread
    /// see EOF on Windows (the ConPTY pipe does not close when the child exits).
    master: Option<Box<dyn MasterPty + Send>>,
    shared: Arc<Mutex<ReaderShared>>,
}

impl TunnelProcess {
    fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn exit_code(&mut self) -> Option<u32> {
        self.child.as_mut().and_then(|c| c.try_wait().ok().flatten()).map(|s| s.exit_code())
    }

    /// Terminate ssh and release the PTY. Reaping happens on a detached thread so callers
    /// (async tasks, `Drop`) never block on `wait()`.
    fn kill(&mut self) {
        let master = self.master.take();
        let Some(mut child) = self.child.take() else {
            drop(master);
            return;
        };
        // TerminateProcess / SIGHUP returns immediately; only `wait()` can block, so it
        // runs on a detached thread. Should the thread fail to start, the closure (and with
        // it the PTY master and the child handle) is simply dropped.
        let _ = child.kill();
        let _ = std::thread::Builder::new().name("bulkdb-ssh-reap".into()).spawn(move || {
            drop(master);
            let _ = child.wait();
        });
    }

    fn masked_output(&self) -> String {
        lock(&self.shared).masked.clone()
    }
}

impl Drop for TunnelProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A shared tunnel: process + lease bookkeeping.
struct TunnelEntry {
    key: String,
    local_port: u16,
    process: Mutex<TunnelProcess>,
    leases: AtomicUsize,
    last_used: Mutex<Instant>,
}

impl TunnelEntry {
    fn alive(&self) -> bool {
        lock(&self.process).alive()
    }

    fn kill(&self) {
        lock(&self.process).kill();
    }

    fn status(&self) -> TunnelStatus {
        TunnelStatus { key: self.key.clone(), local_port: self.local_port, leases: self.leases.load(Ordering::SeqCst), up: self.alive() }
    }

    fn lease(self: &Arc<Self>) -> TunnelLease {
        self.leases.fetch_add(1, Ordering::SeqCst);
        *lock(&self.last_used) = Instant::now();
        let entry = Arc::clone(self);
        TunnelLease {
            local_host: "127.0.0.1".into(),
            local_port: self.local_port,
            _release: Some(Box::new(move || {
                entry.leases.fetch_sub(1, Ordering::SeqCst);
                *lock(&entry.last_used) = Instant::now();
            })),
        }
    }
}

type Entries = Mutex<HashMap<String, Arc<TunnelEntry>>>;

/// Chooses the local port of a new forward (see [`TunnelManager::with_local_port_picker`]).
type PortPicker = Arc<dyn Fn() -> Result<u16> + Send + Sync>;

// ---------------------------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------------------------

pub struct TunnelManager {
    prompts: Arc<dyn PromptHandler>,
    idle: Duration,
    entries: Arc<Entries>,
    /// Serialises tunnel start-ups so two concurrent `acquire`s for one key spawn one ssh.
    spawn_lock: tokio::sync::Mutex<()>,
    reaper: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Program that is started as `ssh`, and the arguments placed before the ones from
    /// [`build_ssh_args`]. Defaults to the platform ssh binary with no leading arguments.
    ssh_program: PathBuf,
    ssh_leading_args: Vec<String>,
    /// Picks the local end of each new forward: [`free_port`] unless replaced with
    /// [`TunnelManager::with_local_port_picker`].
    port_picker: PortPicker,
}

impl TunnelManager {
    /// `idle`: how long a tunnel without leases is kept before the reaper kills it
    /// ([`DEFAULT_IDLE`] in the app settings).
    pub fn new(prompts: Arc<dyn PromptHandler>, idle: Duration) -> Self {
        TunnelManager {
            prompts,
            idle,
            entries: Arc::new(Mutex::new(HashMap::new())),
            spawn_lock: tokio::sync::Mutex::new(()),
            reaper: Mutex::new(None),
            ssh_program: crate::platform::ssh_path(),
            ssh_leading_args: Vec::new(),
            port_picker: Arc::new(free_port),
        }
    }

    /// Start `program leading_args... <ssh args>` instead of the platform `ssh` binary: a
    /// wrapper script, an OpenSSH installed somewhere unusual, or (in tests) a fake ssh.
    /// The program must speak OpenSSH's prompts and messages.
    pub fn with_ssh_command(mut self, program: impl Into<PathBuf>, leading_args: Vec<String>) -> Self {
        self.ssh_program = program.into();
        self.ssh_leading_args = leading_args;
        self
    }

    /// Choose the local end of new forwards with `pick` instead of [`free_port`], e.g. to stay
    /// inside a port range that a host firewall allows. Every ssh start-up calls it once; when
    /// the returned port is taken (checked before ssh starts, and ssh itself runs with
    /// `ExitOnForwardFailure=yes`, so it exits instead of leaving a tunnel without a
    /// listener), `acquire` retries once with the next pick. An error from `pick` fails the
    /// `acquire`.
    pub fn with_local_port_picker(mut self, pick: impl Fn() -> Result<u16> + Send + Sync + 'static) -> Self {
        self.port_picker = Arc::new(pick);
        self
    }

    /// Program used as `ssh` (see [`TunnelManager::with_ssh_command`]).
    pub fn ssh_program(&self) -> &Path {
        &self.ssh_program
    }

    /// Get (or start) a tunnel to `target_host:target_port` through `cfg`. `secret` is the
    /// stored ssh password / key passphrase, if any; when it is missing or wrong the prompt
    /// handler is asked once.
    pub async fn acquire(&self, cfg: &SshConfig, target_host: &str, target_port: u16, secret: Option<String>) -> Result<TunnelLease> {
        if !cfg.enabled {
            return Err(CoreError::Invalid("ssh tunnel is disabled for this connection".into()));
        }
        validate_config(cfg)?;
        if target_host.trim().is_empty() {
            return Err(CoreError::Invalid("tunnel target host is required".into()));
        }
        self.ensure_reaper();
        let key = tunnel_key(cfg, target_host, target_port);

        if let Some(lease) = self.reuse(&key) {
            return Ok(lease);
        }
        let _guard = self.spawn_lock.lock().await;
        if let Some(lease) = self.reuse(&key) {
            return Ok(lease);
        }

        let stored = if cfg.auth == SshAuth::Agent { None } else { secret.filter(|s| !s.is_empty()) };
        let mut result = self.start(cfg, target_host, target_port, stored.clone()).await;
        if let Err(CoreError::Tunnel(msg)) = &result {
            // The picked local port was taken (by another program, or between the pick and
            // ssh's bind): one retry on the next pick, with the same stored secret.
            if msg.contains("Address already in use") || msg.contains("cannot listen to port") {
                tracing::warn!(key = %key, "local forward port was taken; retrying on another port");
                result = self.start(cfg, target_host, target_port, stored.clone()).await;
            }
        }
        if let Err(CoreError::Auth(_)) = &result {
            if stored.is_some() {
                tracing::warn!(key = %key, "stored ssh secret was rejected; asking the user");
                result = self.start(cfg, target_host, target_port, None).await;
            }
        }
        let (process, local_port) = result?;

        let entry =
            Arc::new(TunnelEntry { key: key.clone(), local_port, process: Mutex::new(process), leases: AtomicUsize::new(0), last_used: Mutex::new(Instant::now()) });
        let lease = entry.lease();
        if let Some(old) = lock(&self.entries).insert(key.clone(), entry) {
            old.kill();
        }
        tracing::info!(key = %key, local_port, "ssh tunnel up");
        Ok(lease)
    }

    /// Reuse a live entry for `key`, dropping a dead one.
    fn reuse(&self, key: &str) -> Option<TunnelLease> {
        let mut entries = lock(&self.entries);
        let entry = entries.get(key)?.clone();
        if entry.alive() {
            tracing::debug!(key, "reusing ssh tunnel");
            return Some(entry.lease());
        }
        tracing::warn!(key, "ssh tunnel process has exited; restarting");
        entries.remove(key);
        drop(entries);
        entry.kill();
        None
    }

    /// Spawn ssh and wait until the forwarded port accepts connections.
    async fn start(&self, cfg: &SshConfig, target_host: &str, target_port: u16, stored_secret: Option<String>) -> Result<(TunnelProcess, u16)> {
        let local_port = (self.port_picker)()?;
        if !local_port_free(local_port) {
            // Same wording as ssh's own bind failure, so `acquire` retries on the next pick.
            return Err(CoreError::Tunnel(format!("bind [127.0.0.1]:{local_port}: Address already in use (another program listens on the local forward port)")));
        }
        let args = build_ssh_args(cfg, target_host, target_port, local_port);
        let ssh = &self.ssh_program;
        if ssh.is_absolute() && !ssh.is_file() {
            return Err(CoreError::Tunnel(format!("ssh binary not found at {}; install OpenSSH client", ssh.display())));
        }
        tracing::info!(ssh = %ssh.display(), leading = ?self.ssh_leading_args, args = ?args, "starting ssh tunnel");

        let pty = native_pty_system();
        let pair = pty.openpty(PTY_SIZE).map_err(|e| CoreError::Tunnel(format!("cannot open a pseudo-terminal for ssh: {e:#}")))?;
        let mut cmd = CommandBuilder::new(ssh);
        cmd.args(&self.ssh_leading_args);
        cmd.args(&args);
        // Prompts must come to our PTY, not to an askpass helper.
        cmd.env_remove("SSH_ASKPASS");
        cmd.env_remove("SSH_ASKPASS_REQUIRE");
        cmd.env("TERM", "dumb");
        let child = pair.slave.spawn_command(cmd).map_err(|e| CoreError::Tunnel(format!("cannot start {}: {e:#}", ssh.display())))?;
        // The child owns its copy of the slave; ours must go so the master sees EOF on exit.
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().map_err(|e| CoreError::Tunnel(format!("pty reader: {e:#}")))?;
        let writer = pair.master.take_writer().map_err(|e| CoreError::Tunnel(format!("pty writer: {e:#}")))?;

        let shared = Arc::new(Mutex::new(ReaderShared::default()));
        let ctx = ReaderCtx {
            prompts: Arc::clone(&self.prompts),
            handle: tokio::runtime::Handle::current(),
            user: cfg.user.clone(),
            host: cfg.host.clone(),
            stored_secret,
            killer: child.clone_killer(),
            shared: Arc::clone(&shared),
        };
        std::thread::Builder::new()
            .name(format!("bulkdb-ssh-pty-{local_port}"))
            .spawn(move || run_reader(reader, writer, ctx))
            .map_err(|e| CoreError::Tunnel(format!("cannot start the pty reader thread: {e}")))?;

        let mut process = TunnelProcess { child: Some(child), master: Some(pair.master), shared };
        match self.wait_ready(&mut process, local_port, cfg).await {
            Ok(()) => Ok((process, local_port)),
            Err(e) => {
                process.kill();
                Err(e)
            }
        }
    }

    /// Poll the local port until ssh listens, the child exits, the reader reports a failure
    /// or the readiness clock (paused during prompts) runs out.
    async fn wait_ready(&self, process: &mut TunnelProcess, local_port: u16, cfg: &SshConfig) -> Result<()> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], local_port));
        let started = Instant::now();
        loop {
            let (fatal, pending, last_answer) = {
                let s = lock(&process.shared);
                (s.fatal.clone(), s.prompt_pending, s.last_answer)
            };
            if let Some(err) = fatal {
                self.settle(process).await;
                return Err(with_output(err, &process.masked_output()));
            }
            if !process.alive() {
                self.settle(process).await;
                let code = process.exit_code();
                let out = process.masked_output();
                let base = classify_output(&out).into_error(&cfg.user, &cfg.host).unwrap_or_else(|| {
                    CoreError::Tunnel(format!("ssh exited{} before the tunnel was ready", code.map(|c| format!(" with status {c}")).unwrap_or_default()))
                });
                return Err(with_output(base, &out));
            }
            if std::net::TcpStream::connect_timeout(&addr, READY_POLL).is_ok() {
                return Ok(());
            }
            if !pending {
                let clock_start = last_answer.unwrap_or(started);
                if clock_start.elapsed() > READY_TIMEOUT {
                    let out = process.masked_output();
                    return Err(with_output(
                        CoreError::Timeout(format!("ssh tunnel via {}@{} did not come up within {} s", cfg.user, cfg.host, READY_TIMEOUT.as_secs())),
                        &out,
                    ));
                }
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Give the reader thread a moment to drain what ssh printed before exiting.
    async fn settle(&self, process: &mut TunnelProcess) {
        let mut last_len = usize::MAX;
        for _ in 0..10 {
            let (eof, len) = {
                let s = lock(&process.shared);
                (s.eof, s.masked.len())
            };
            if eof || len == last_len {
                return;
            }
            last_len = len;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Start the idle reaper once (needs a runtime, so it is deferred to the first acquire).
    fn ensure_reaper(&self) {
        let mut slot = lock(&self.reaper);
        if slot.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let entries: Weak<Entries> = Arc::downgrade(&self.entries);
        let idle = self.idle;
        *slot = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAPER_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(entries) = entries.upgrade() else { break };
                reap(&entries, idle);
            }
        }));
    }

    /// Tunnels currently managed (including ones whose process has died but was not yet
    /// reaped: `up == false`).
    pub fn status(&self) -> Vec<TunnelStatus> {
        let mut v: Vec<TunnelStatus> = lock(&self.entries).values().map(|e| e.status()).collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        v
    }

    /// The masked ssh output collected for a tunnel (diagnostics, e.g. "connect_to ... failed"
    /// after the tunnel came up).
    pub fn recent_output(&self, key: &str) -> Option<String> {
        let entry = lock(&self.entries).get(key).cloned()?;
        let out = lock(&entry.process).masked_output();
        Some(out)
    }

    /// Kill every tunnel (app shutdown).
    pub async fn shutdown(&self) {
        if let Some(h) = lock(&self.reaper).take() {
            h.abort();
        }
        let entries: Vec<Arc<TunnelEntry>> = lock(&self.entries).drain().map(|(_, e)| e).collect();
        for e in entries {
            tracing::info!(key = %e.key, "closing ssh tunnel");
            e.kill();
        }
    }
}

impl Drop for TunnelManager {
    fn drop(&mut self) {
        if let Some(h) = lock(&self.reaper).take() {
            h.abort();
        }
        for (_, e) in lock(&self.entries).drain() {
            e.kill();
        }
    }
}

/// Remove dead tunnels and kill idle ones.
fn reap(entries: &Entries, idle: Duration) {
    let mut map = lock(entries);
    let doomed: Vec<String> =
        map.iter().filter(|(_, e)| !e.alive() || (e.leases.load(Ordering::SeqCst) == 0 && lock(&e.last_used).elapsed() >= idle)).map(|(k, _)| k.clone()).collect();
    for key in doomed {
        if let Some(e) = map.remove(&key) {
            tracing::info!(key = %key, "closing idle or dead ssh tunnel");
            e.kill();
        }
    }
}

/// Attach the (masked) ssh output to an error message.
fn with_output(err: CoreError, output: &str) -> CoreError {
    let out = output.trim();
    if out.is_empty() {
        return err;
    }
    let out = tail(out, ERROR_OUTPUT_TAIL);
    let attach = |m: &str| -> String {
        if m.trim() == out {
            m.to_string()
        } else {
            format!("{m}\n--- ssh output ---\n{out}")
        }
    };
    match err {
        CoreError::Connect(m) => CoreError::Connect(attach(&m)),
        CoreError::Auth(m) => CoreError::Auth(attach(&m)),
        CoreError::Tunnel(m) => CoreError::Tunnel(attach(&m)),
        CoreError::Timeout(m) => CoreError::Timeout(attach(&m)),
        other => other,
    }
}

// ---------------------------------------------------------------------------------------------
// PTY diagnostics
// ---------------------------------------------------------------------------------------------

/// Result of [`run_under_pty`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyCapture {
    /// ANSI-stripped output.
    pub output: String,
    /// Exit code, `None` when the command was killed on timeout.
    pub exit_code: Option<u32>,
}

/// Run a command under a pseudo-terminal, optionally feeding it `input`, and capture its
/// output until it exits or `timeout` elapses (then it is killed). Used by the PTY smoke test
/// and by diagnostics (`ssh -V`); not for tunnels.
pub fn run_under_pty(program: &Path, args: &[&str], input: Option<&str>, timeout: Duration) -> Result<PtyCapture> {
    let pty = native_pty_system();
    let pair = pty.openpty(PTY_SIZE).map_err(|e| CoreError::Tunnel(format!("cannot open a pseudo-terminal: {e:#}")))?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    let mut child = pair.slave.spawn_command(cmd).map_err(|e| CoreError::Tunnel(format!("cannot start {}: {e:#}", program.display())))?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| CoreError::Tunnel(format!("pty reader: {e:#}")))?;
    // Shared with the reader thread, which answers conhost's cursor request through it.
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| CoreError::Tunnel(format!("pty writer: {e:#}")))?));

    let text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&text);
    let reply_writer = Arc::clone(&writer);
    std::thread::Builder::new()
        .name("bulkdb-pty-capture".into())
        .spawn(move || {
            let mut raw = [0u8; 4096];
            let mut dsr = DsrDetector::default();
            let mut stripper = AnsiStripper::new();
            loop {
                match reader.read(&mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if dsr.feed(&raw[..n]) > 0 {
                            answer_cursor_request(lock(&reply_writer).as_mut());
                        }
                        let chunk = String::from_utf8_lossy(&raw[..n]).into_owned();
                        stripper.feed(&chunk, &mut lock(&sink));
                    }
                }
            }
        })
        .map_err(|e| CoreError::Tunnel(format!("cannot start the pty reader thread: {e}")))?;

    if let Some(input) = input {
        let mut w = lock(&writer);
        w.write_all(input.as_bytes())?;
        w.flush()?;
    }

    let started = Instant::now();
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status.exit_code()),
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(CoreError::Tunnel(format!("waiting for {}: {e}", program.display()))),
        }
    };
    // Let the reader drain what the process printed last, then close the PTY (which ends
    // the reader on Windows, where the ConPTY pipe outlives the child). The reader thread
    // drops its share of the writer when it exits.
    let mut last_len = usize::MAX;
    for _ in 0..10 {
        let len = lock(&text).len();
        if len == last_len {
            break;
        }
        last_len = len;
        std::thread::sleep(Duration::from_millis(30));
    }
    drop(writer);
    drop(pair.master);
    let output = lock(&text).clone();
    Ok(PtyCapture { output, exit_code })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(auth: SshAuth) -> SshConfig {
        SshConfig {
            enabled: true,
            host: "bastion.example".into(),
            port: 2222,
            user: "alice".into(),
            auth,
            key_path: Some("C:/keys/id_ed25519".into()),
            compression: true,
        }
    }

    #[test]
    fn args_password() {
        let a = build_ssh_args(&cfg(SshAuth::Password), "db.internal", 5432, 50000);
        assert_eq!(a[0], "-N");
        assert_eq!(a[1], "-C");
        let joined = a.join(" ");
        assert!(joined.contains("-L 127.0.0.1:50000:db.internal:5432"));
        assert!(joined.contains("-p 2222"));
        assert!(joined.contains("-o StrictHostKeyChecking=ask"));
        assert!(!joined.contains("StrictHostKeyChecking=no"));
        assert!(!joined.contains("UserKnownHostsFile"));
        assert!(joined.contains("PreferredAuthentications=keyboard-interactive,password"));
        assert!(joined.contains("PubkeyAuthentication=no"));
        assert!(!a.iter().any(|x| x == "-i"));
        assert_eq!(a.last().unwrap(), "alice@bastion.example");
    }

    #[test]
    fn args_key_and_agent() {
        let k = build_ssh_args(&cfg(SshAuth::Key), "db", 1, 2);
        let i = k.iter().position(|x| x == "-i").expect("-i present");
        assert_eq!(k[i + 1], "C:/keys/id_ed25519");
        assert!(k.join(" ").contains("IdentitiesOnly=yes"));
        let mut c = cfg(SshAuth::Agent);
        c.compression = false;
        let a = build_ssh_args(&c, "db", 1, 2);
        assert!(!a.iter().any(|x| x == "-i" || x == "-C"));
        assert!(a.join(" ").contains("PreferredAuthentications=publickey"));
    }

    #[test]
    fn args_ipv6_target_is_bracketed() {
        let a = build_ssh_args(&cfg(SshAuth::Agent), "::1", 5432, 50000);
        assert!(a.contains(&"127.0.0.1:50000:[::1]:5432".to_string()));
    }

    #[test]
    fn classify_host_key_prompt() {
        let text = "The authenticity of host 'bastion.example (10.0.0.5)' can't be established.\nED25519 key fingerprint is SHA256:abcDEF123+/=.\nThis key is not known by any other names.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";
        match classify_output(text) {
            Classification::HostKey(info) => {
                assert_eq!(info.host, "bastion.example");
                assert_eq!(info.fingerprint, "SHA256:abcDEF123+/=");
                assert_eq!(info.key_type.as_deref(), Some("ED25519"));
                assert!(info.message.starts_with("The authenticity"));
            }
            other => panic!("unexpected {other:?}"),
        }
        // An incomplete prompt (ConPTY mid-write) is not reported.
        assert_eq!(classify_output("Are you sure you want to continue connecting (yes/no"), Classification::None);
    }

    #[test]
    fn classify_prompts_and_failures() {
        assert_eq!(classify_output("alice@bastion's password: "), Classification::Password);
        assert_eq!(classify_output("(alice@bastion) Password:"), Classification::Password);
        assert_eq!(classify_output("Enter passphrase for key '/home/alice/.ssh/id_rsa': "), Classification::Passphrase);
        assert_eq!(classify_output("alice@bastion's password: \n"), Classification::Password);
        assert!(matches!(classify_output("alice@bastion's password: \nPermission denied (publickey,password).\n"), Classification::PermissionDenied(_)));
        assert!(matches!(classify_output("ssh: Could not resolve hostname nonexistent.invalid: Name or service not known\n"), Classification::ConnectFailed(_)));
        assert!(matches!(classify_output("ssh: connect to host 10.0.0.5 port 22: Connection refused\n"), Classification::ConnectFailed(_)));
        assert!(matches!(classify_output("Host key verification failed.\n"), Classification::HostKeyVerificationFailed(_)));
        assert!(matches!(classify_output("bind [127.0.0.1]:50000: Address already in use\n"), Classification::ForwardFailed(_)));
        assert!(matches!(classify_output("Error: remote port forwarding failed for listen port 5432\n"), Classification::ForwardFailed(_)));
        assert_eq!(classify_output(""), Classification::None);
        assert_eq!(classify_output("Warning: Permanently added 'x' to the list of known hosts.\n"), Classification::None);
    }

    #[test]
    fn classification_errors() {
        let e = Classification::PermissionDenied("x".into()).into_error("alice", "bastion").unwrap();
        assert_eq!(e.code(), "AUTH");
        assert!(e.to_string().contains("alice@bastion"));
        assert_eq!(Classification::ConnectFailed("x".into()).into_error("a", "b").unwrap().code(), "CONNECT");
        assert_eq!(Classification::ForwardFailed("x".into()).into_error("a", "b").unwrap().code(), "TUNNEL");
        assert_eq!(Classification::HostKeyVerificationFailed("x".into()).into_error("a", "b").unwrap().code(), "TUNNEL");
        assert!(Classification::Password.into_error("a", "b").is_none());
    }

    #[test]
    fn ansi_stripper_handles_split_sequences() {
        let mut s = AnsiStripper::new();
        let mut out = String::new();
        s.feed("\x1b[?25l\x1b[2J\x1b[", &mut out);
        s.feed("1;1Hhello\r\n\x1b]0;title\x07wor", &mut out);
        s.feed("ld\x1b(B\x1b[0m\x1bP..\x1b\\!", &mut out);
        assert_eq!(out, "hello\nworld!");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn dsr_detector_finds_requests_across_reads() {
        let mut d = DsrDetector::default();
        assert_eq!(d.feed(b"\x1b[6n"), 1);
        assert_eq!(d.feed(b"\x1b["), 0);
        assert_eq!(d.feed(b"6n\x1b[?9001hhello"), 1, "a request split across reads is still found");
        assert_eq!(d.feed(b"\x1b\x1b[6n\x1b[6"), 1, "a stray ESC restarts the match");
        assert_eq!(d.feed(b"n"), 1);
        assert_eq!(d.feed(b"plain text \x1b[6m"), 0, "SGR with the same parameter is not a request");
        let mut out: Vec<u8> = Vec::new();
        answer_cursor_request(&mut out);
        assert_eq!(out, CPR_REPLY);
    }

    #[test]
    fn output_attached_to_errors_and_tail() {
        let e = with_output(CoreError::Connect("boom".into()), "line1\nline2");
        assert!(e.to_string().contains("boom") && e.to_string().contains("line2"));
        assert_eq!(with_output(CoreError::Connect("same".into()), "same").to_string(), "connection failed: same");
        assert!(matches!(with_output(CoreError::Cancelled, "x"), CoreError::Cancelled));
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
    }

    #[test]
    fn validate_config_rejects_bad_input() {
        let mut c = cfg(SshAuth::Password);
        c.host = "".into();
        assert!(matches!(validate_config(&c), Err(CoreError::Invalid(_))));
        let mut c = cfg(SshAuth::Key);
        c.key_path = Some("k.ppk".into());
        assert!(matches!(validate_config(&c), Err(CoreError::Invalid(_))));
        c.key_path = None;
        assert!(matches!(validate_config(&c), Err(CoreError::Invalid(_))));
        c.key_path = Some("C:/definitely/missing/key".into());
        assert!(matches!(validate_config(&c), Err(CoreError::Invalid(m)) if m.contains("not found")));
        assert!(validate_config(&cfg(SshAuth::Agent)).is_ok());
    }

    #[test]
    fn key_is_stable() {
        assert_eq!(tunnel_key(&cfg(SshAuth::Key), "db", 5432), "alice@bastion.example:2222->db:5432");
    }
}
