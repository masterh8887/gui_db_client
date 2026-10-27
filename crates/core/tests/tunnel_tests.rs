//! SSH tunnel tests that need no bastion: argument building, output classification, the PTY
//! layer on this OS, `acquire()` failing fast against an unresolvable host (skipped when no
//! `ssh` binary is installed), and the whole prompt / readiness / sharing / reaper machinery
//! driven by a fake `ssh` script (PowerShell on Windows, `sh` elsewhere) under the real PTY.
//!
//! With `--features it` and `BULKDB_SSH_URL` set, the `openssh_it` module at the end also runs
//! the real `ssh` client against a real OpenSSH server (the `sshd` service of
//! `docker/compose.yml`): password and key authentication, a changed host key, a taken local
//! port and re-acquiring after the ssh session was killed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bulk_core::tunnel::{build_ssh_args, classify_output, free_port, run_under_pty, validate_key_path, Classification, DenyAllPrompts, PromptHandler, TunnelManager};
use bulk_core::types::{HostKeyPrompt, SshAuth, SshConfig, SshPasswordPrompt};
use bulk_core::CoreError;

fn cfg(auth: SshAuth) -> SshConfig {
    SshConfig {
        enabled: true,
        host: "bastion.example".into(),
        port: 22,
        user: "alice".into(),
        auth,
        key_path: Some("/home/alice/.ssh/id_ed25519".into()),
        compression: true,
    }
}

fn has(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[test]
fn ssh_args_password_mode() {
    let a = build_ssh_args(&cfg(SshAuth::Password), "db.internal", 5432, 54321);
    assert_eq!(a[0], "-N");
    assert!(a.contains(&"-C".to_string()), "compression on -> -C");
    assert!(has(&a, "-L", "127.0.0.1:54321:db.internal:5432"), "{a:?}");
    assert!(has(&a, "-p", "22"));
    assert!(has(&a, "-o", "StrictHostKeyChecking=ask"));
    assert!(has(&a, "-o", "ExitOnForwardFailure=yes"));
    assert!(has(&a, "-o", "NumberOfPasswordPrompts=1"));
    assert!(has(&a, "-o", "ServerAliveInterval=60"));
    assert!(has(&a, "-o", "ConnectTimeout=20"));
    assert!(has(&a, "-o", "LogLevel=ERROR"));
    assert!(has(&a, "-o", "PreferredAuthentications=keyboard-interactive,password"));
    assert!(has(&a, "-o", "PubkeyAuthentication=no"));
    assert!(!a.iter().any(|x| x == "-i"), "password mode must not pass -i");
    assert!(!a.iter().any(|x| x.contains("StrictHostKeyChecking=no")));
    assert!(!a.iter().any(|x| x.contains("UserKnownHostsFile")));
    assert_eq!(a.last().map(String::as_str), Some("alice@bastion.example"));
}

#[test]
fn ssh_args_key_mode() {
    let a = build_ssh_args(&cfg(SshAuth::Key), "db.internal", 3306, 50001);
    assert!(has(&a, "-i", "/home/alice/.ssh/id_ed25519"), "{a:?}");
    assert!(has(&a, "-o", "IdentitiesOnly=yes"));
    assert!(has(&a, "-o", "PreferredAuthentications=publickey"));
    assert!(has(&a, "-L", "127.0.0.1:50001:db.internal:3306"));
    assert!(!a.iter().any(|x| x.contains("PubkeyAuthentication=no")));
    assert!(!a.iter().any(|x| x.contains("StrictHostKeyChecking=no")));
}

#[test]
fn ssh_args_agent_mode_without_compression() {
    let mut c = cfg(SshAuth::Agent);
    c.compression = false;
    c.port = 2200;
    let a = build_ssh_args(&c, "10.1.2.3", 1433, 50002);
    assert!(!a.contains(&"-C".to_string()), "compression off -> no -C");
    assert!(!a.iter().any(|x| x == "-i"), "agent mode must not pass -i (even with key_path set)");
    assert!(has(&a, "-o", "PreferredAuthentications=publickey"));
    assert!(has(&a, "-p", "2200"));
    assert!(has(&a, "-L", "127.0.0.1:50002:10.1.2.3:1433"));
    assert!(!a.iter().any(|x| x.contains("StrictHostKeyChecking=no")));
}

#[test]
fn free_port_is_bindable() {
    let p = free_port().expect("free port");
    assert!(p > 0);
    // The port was released by `free_port`, so binding it again must work right away.
    let l = std::net::TcpListener::bind(("127.0.0.1", p)).expect("bind the returned port");
    assert_eq!(l.local_addr().unwrap().port(), p);
}

#[test]
fn key_path_rejects_ppk() {
    let err = validate_key_path("C:/Users/alice/mykey.PPK").unwrap_err();
    assert!(matches!(err, CoreError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("PuTTYgen"));
    assert!(validate_key_path("/home/alice/.ssh/id_ed25519").is_ok());
}

#[test]
fn classify_host_key_prompt() {
    let text = "The authenticity of host 'bastion.example (203.0.113.7)' can't be established.\r\nED25519 key fingerprint is SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8.\r\nThis key is not known by any other names.\r\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";
    match classify_output(text) {
        Classification::HostKey(info) => {
            assert_eq!(info.host, "bastion.example");
            assert_eq!(info.fingerprint, "SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8");
            assert_eq!(info.key_type.as_deref(), Some("ED25519"));
            assert!(info.message.contains("(yes/no"));
        }
        other => panic!("expected a host key prompt, got {other:?}"),
    }
}

#[test]
fn classify_password_prompt() {
    assert_eq!(classify_output("alice@bastion.example's password: "), Classification::Password);
    assert_eq!(classify_output("Password:"), Classification::Password);
    assert_eq!(classify_output("Enter passphrase for key '/home/alice/.ssh/id_rsa': "), Classification::Passphrase);
}

#[test]
fn classify_permission_denied() {
    let c = classify_output("alice@bastion.example's password: \nPermission denied (publickey,password).\n");
    assert!(matches!(c, Classification::PermissionDenied(_)), "{c:?}");
    let err = c.into_error("alice", "bastion.example").unwrap();
    assert_eq!(err.code(), "AUTH");
    assert!(err.to_string().contains("alice@bastion.example"));
}

#[test]
fn classify_resolve_failure() {
    let c = classify_output("ssh: Could not resolve hostname nonexistent.invalid: No such host is known.\n");
    assert!(matches!(c, Classification::ConnectFailed(ref l) if l.contains("nonexistent.invalid")), "{c:?}");
    assert_eq!(c.into_error("a", "b").unwrap().code(), "CONNECT");
    assert!(matches!(classify_output("Host key verification failed.\n"), Classification::HostKeyVerificationFailed(_)));
    assert!(matches!(classify_output("bind [127.0.0.1]:5000: Address already in use\n"), Classification::ForwardFailed(_)));
    assert_eq!(classify_output("nothing interesting yet"), Classification::None);
}

/// Spawns a trivial command under the native PTY (ConPTY on Windows, openpty elsewhere) and
/// reads its output back, proving the PTY layer works on this machine.
#[test]
fn pty_smoke_echo_hello() {
    let (program, args): (&str, &[&str]) = if std::env::consts::OS == "windows" { ("cmd.exe", &["/c", "echo hello"]) } else { ("sh", &["-c", "echo hello"]) };
    let cap = run_under_pty(Path::new(program), args, None, Duration::from_secs(15)).expect("spawn under pty");
    assert!(cap.output.contains("hello"), "pty output was: {:?}", cap.output);
    assert_eq!(cap.exit_code, Some(0), "exit code; output: {:?}", cap.output);
}

/// Input written to the PTY master reaches the child's line reader.
#[test]
fn pty_input_round_trip() {
    let (program, args): (&str, &[&str]) = if std::env::consts::OS == "windows" {
        // `%X%` would be expanded when cmd parses the line, before `set /p` runs; `!X!` with
        // delayed expansion is read after.
        ("cmd.exe", &["/v:on", "/c", "set /p X=ask: && echo got:!X!"])
    } else {
        ("sh", &["-c", "read X; echo got:$X"])
    };
    let cap = run_under_pty(Path::new(program), args, Some("marco\r"), Duration::from_secs(15)).expect("spawn under pty");
    assert!(cap.output.contains("got:marco"), "pty output was: {:?}", cap.output);
}

fn ssh_available() -> bool {
    let path = bulk_core::platform::ssh_path();
    std::process::Command::new(&path).arg("-V").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok()
}

/// `acquire()` against a bastion that cannot be resolved must fail quickly with a CONNECT
/// (or TUNNEL) error and leave nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acquire_unresolvable_bastion_fails_fast() {
    if !ssh_available() {
        eprintln!("skipping: no ssh binary at {}", bulk_core::platform::ssh_path().display());
        return;
    }
    let mgr = TunnelManager::new(Arc::new(DenyAllPrompts), Duration::from_secs(60));
    let mut c = cfg(SshAuth::Password);
    c.host = "nonexistent.invalid".into();
    let started = Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(60), mgr.acquire(&c, "db.internal", 5432, None)).await.expect("acquire must return within 60 s");
    let err = match res {
        Ok(_) => panic!("a tunnel to nonexistent.invalid came up?!"),
        Err(e) => e,
    };
    eprintln!("acquire failed after {:?}: {err}", started.elapsed());
    assert!(matches!(err, CoreError::Connect(_) | CoreError::Tunnel(_)), "unexpected error kind: {err:?}");
    assert!(mgr.status().is_empty(), "no entry must remain after a failed acquire");
    mgr.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Fake ssh: a script that speaks OpenSSH's prompts and messages, run under the real PTY
// ---------------------------------------------------------------------------------------------

/// A script standing in for `ssh`, written to a private temp dir and removed on drop.
struct FakeSsh {
    dir: PathBuf,
    program: PathBuf,
    leading: Vec<String>,
}

impl FakeSsh {
    /// `ps1` is used on Windows (PowerShell 5.1), `sh` everywhere else.
    fn new(name: &str, ps1: &str, sh: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bulkdb-tunnel-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let windows = std::env::consts::OS == "windows";
        let script = dir.join(if windows { "fake-ssh.ps1" } else { "fake-ssh.sh" });
        std::fs::File::create(&script).and_then(|mut f| f.write_all(if windows { ps1 } else { sh }.as_bytes())).expect("write script");
        let script = script.to_string_lossy().into_owned();
        let (program, leading) = if windows {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            let ps = PathBuf::from(root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe");
            (ps, vec!["-NoProfile".into(), "-NonInteractive".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-File".into(), script])
        } else {
            (PathBuf::from("sh"), vec![script])
        };
        FakeSsh { dir, program, leading }
    }

    fn manager(&self, prompts: Arc<dyn PromptHandler>, idle: Duration) -> TunnelManager {
        TunnelManager::new(prompts, idle).with_ssh_command(self.program.clone(), self.leading.clone())
    }
}

impl Drop for FakeSsh {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Asks for a password after greeting, echoes what it got (which must come out masked), then
/// denies. Exit 255 like ssh.
const DENY_PS1: &str = r#"
[Console]::Out.Write("alice@bastion.example's password: ")
$pw = [Console]::In.ReadLine()
[Console]::Out.WriteLine("")
[Console]::Out.WriteLine("typed $pw")
[Console]::Out.WriteLine("Permission denied (publickey,password).")
exit 255
"#;
const DENY_SH: &str = r#"
printf "alice@bastion.example's password: "
read -r pw
printf "\ntyped %s\nPermission denied (publickey,password).\n" "$pw"
exit 255
"#;

/// Unknown host key first; "yes" leads to a password prompt and a denial, anything else to
/// "Host key verification failed".
const HOSTKEY_PS1: &str = r#"
[Console]::Out.WriteLine("The authenticity of host 'bastion.example (203.0.113.7)' can't be established.")
[Console]::Out.WriteLine("ED25519 key fingerprint is SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8.")
[Console]::Out.WriteLine("This key is not known by any other names.")
[Console]::Out.Write("Are you sure you want to continue connecting (yes/no/[fingerprint])? ")
$answer = [Console]::In.ReadLine()
if ($answer -ne "yes") {
    [Console]::Out.WriteLine("Host key verification failed.")
    exit 255
}
[Console]::Out.Write("alice@bastion.example's password: ")
$pw = [Console]::In.ReadLine()
[Console]::Out.WriteLine("")
[Console]::Out.WriteLine("Permission denied (publickey,password).")
exit 255
"#;
const HOSTKEY_SH: &str = r#"
printf "The authenticity of host 'bastion.example (203.0.113.7)' can't be established.\n"
printf "ED25519 key fingerprint is SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8.\n"
printf "This key is not known by any other names.\n"
printf "Are you sure you want to continue connecting (yes/no/[fingerprint])? "
read -r answer
if [ "$answer" != "yes" ]; then
  printf "Host key verification failed.\n"
  exit 255
fi
printf "alice@bastion.example's password: "
read -r pw
printf "\nPermission denied (publickey,password).\n"
exit 255
"#;

/// Listens on the local port taken from the `-L` argument (what a real ssh does once the
/// session is up) and stays alive until killed. When a `fail-forward-once` file lies next to
/// the script it is removed and the run fails the way OpenSSH does with
/// `ExitOnForwardFailure=yes` when it cannot bind the local port.
const LISTEN_PS1: &str = r#"
$fwd = $null
for ($i = 0; $i -lt $args.Length; $i++) { if ($args[$i] -eq "-L") { $fwd = $args[$i + 1] } }
if (-not $fwd) { [Console]::Out.WriteLine("fake ssh: no -L argument"); exit 255 }
$port = [int]($fwd.Split(":")[1])
$once = Join-Path $PSScriptRoot "fail-forward-once"
if (Test-Path $once) {
    Remove-Item $once
    [Console]::Out.WriteLine("bind [127.0.0.1]:${port}: Address already in use")
    [Console]::Out.WriteLine("channel_setup_fwd_listener_tcpip: cannot listen to port: $port")
    [Console]::Out.WriteLine("Could not request local forwarding.")
    exit 255
}
$listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, $port)
$listener.Start()
Start-Sleep -Seconds 120
"#;
const LISTEN_SH: &str = r#"
fwd=""
once="$(dirname "$0")/fail-forward-once"
while [ $# -gt 0 ]; do
  if [ "$1" = "-L" ]; then fwd="$2"; fi
  shift
done
port=$(printf '%s' "$fwd" | cut -d: -f2)
if [ -f "$once" ]; then
  rm -f "$once"
  printf "bind [127.0.0.1]:%s: Address already in use\n" "$port"
  printf "channel_setup_fwd_listener_tcpip: cannot listen to port: %s\n" "$port"
  printf "Could not request local forwarding.\n"
  exit 255
fi
if command -v python3 >/dev/null 2>&1; then
  exec python3 -c "import socket, time; s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(('127.0.0.1', $port)); s.listen(5); time.sleep(120)"
elif command -v perl >/dev/null 2>&1; then
  exec perl -MIO::Socket::INET -e 'IO::Socket::INET->new(LocalAddr => "127.0.0.1", LocalPort => $ARGV[0], Listen => 5, ReuseAddr => 1) or die $!; sleep 120' "$port"
elif command -v nc >/dev/null 2>&1; then
  exec nc -l 127.0.0.1 "$port"
fi
printf "fake ssh: no listener tool (python3, perl or nc) available\n"
exit 3
"#;

/// Prompt handler that records what it was asked and answers from a script.
#[derive(Default)]
struct RecordingPrompts {
    accept_host_key: bool,
    secret: Option<String>,
    host_keys: Mutex<Vec<HostKeyPrompt>>,
    secret_kinds: Mutex<Vec<String>>,
}

#[async_trait]
impl PromptHandler for RecordingPrompts {
    async fn host_key(&self, prompt: HostKeyPrompt) -> Result<bool, CoreError> {
        self.host_keys.lock().unwrap().push(prompt);
        Ok(self.accept_host_key)
    }
    async fn ssh_secret(&self, prompt: SshPasswordPrompt) -> Result<Option<String>, CoreError> {
        assert_eq!(prompt.user, "alice");
        assert_eq!(prompt.host, "bastion.example");
        self.secret_kinds.lock().unwrap().push(prompt.kind);
        Ok(self.secret.clone())
    }
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(200)).is_ok()
}

/// Wait until the fake ssh process is gone, i.e. nothing listens on `port` any more.
async fn wait_port_closed(port: u16) {
    let started = Instant::now();
    while port_open(port) {
        assert!(started.elapsed() < Duration::from_secs(15), "port {port} still open: the fake ssh process was not killed");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(90);

async fn acquire_err(mgr: &TunnelManager, cfg: &SshConfig, secret: Option<String>) -> CoreError {
    match tokio::time::timeout(ACQUIRE_TIMEOUT, mgr.acquire(cfg, "db.internal", 5432, secret)).await.expect("acquire must return") {
        Ok(_) => panic!("the fake ssh must not produce a tunnel"),
        Err(e) => e,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_password_prompt_is_answered_and_denial_is_auth_error() {
    let fake = FakeSsh::new("deny", DENY_PS1, DENY_SH);
    let prompts = Arc::new(RecordingPrompts { secret: Some("s3cretPW".into()), ..Default::default() });
    let mgr = fake.manager(prompts.clone(), Duration::from_secs(60));
    let err = acquire_err(&mgr, &cfg(SshAuth::Password), None).await;
    let msg = err.to_string();
    eprintln!("acquire failed as expected: {msg}");
    assert!(matches!(err, CoreError::Auth(_)), "expected AUTH, got {err:?}");
    assert!(msg.contains("permission denied for alice@bastion.example"), "{msg}");
    assert_eq!(*prompts.secret_kinds.lock().unwrap(), vec!["password".to_string()], "asked once, for a password");
    assert!(!msg.contains("s3cretPW"), "the secret leaked into the error: {msg}");
    assert!(msg.contains("***"), "the echoed secret must be masked: {msg}");
    assert!(prompts.host_keys.lock().unwrap().is_empty());
    assert!(mgr.status().is_empty(), "nothing must remain after a failed acquire");
    mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_stored_secret_is_tried_before_asking() {
    let fake = FakeSsh::new("stored", DENY_PS1, DENY_SH);
    let prompts = Arc::new(RecordingPrompts { secret: Some("userPW".into()), ..Default::default() });
    let mgr = fake.manager(prompts.clone(), Duration::from_secs(60));
    // The stored secret is sent first without asking; once it is rejected ssh is restarted
    // and the user is asked exactly once.
    let err = acquire_err(&mgr, &cfg(SshAuth::Password), Some("storedPW".into())).await;
    let msg = err.to_string();
    assert!(matches!(err, CoreError::Auth(_)), "expected AUTH, got {err:?}");
    assert_eq!(*prompts.secret_kinds.lock().unwrap(), vec!["password".to_string()]);
    assert!(!msg.contains("storedPW") && !msg.contains("userPW"), "a secret leaked: {msg}");
    assert!(mgr.status().is_empty());
    mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_deny_all_prompts_cannot_answer() {
    let fake = FakeSsh::new("denyall", DENY_PS1, DENY_SH);
    let mgr = fake.manager(Arc::new(DenyAllPrompts), Duration::from_secs(60));
    let err = acquire_err(&mgr, &cfg(SshAuth::Password), None).await;
    assert!(matches!(err, CoreError::Tunnel(ref m) if m.contains("no way to ask")), "{err:?}");
    assert!(mgr.status().is_empty());
    mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_host_key_rejected_by_user() {
    let fake = FakeSsh::new("hostkey-no", HOSTKEY_PS1, HOSTKEY_SH);
    let prompts = Arc::new(RecordingPrompts { accept_host_key: false, ..Default::default() });
    let mgr = fake.manager(prompts.clone(), Duration::from_secs(60));
    let err = acquire_err(&mgr, &cfg(SshAuth::Password), None).await;
    assert!(matches!(err, CoreError::Tunnel(ref m) if m.contains("rejected by user")), "{err:?}");
    {
        let keys = prompts.host_keys.lock().unwrap();
        assert_eq!(keys.len(), 1, "one host key prompt");
        assert_eq!(keys[0].host, "bastion.example");
        assert_eq!(keys[0].fingerprint, "SHA256:nThbg6kXUpJWGl7E1IGOCspRomTxdCARLviKw6E5SY8");
        assert_eq!(keys[0].key_type.as_deref(), Some("ED25519"));
        assert!(keys[0].message.contains("(yes/no"), "{}", keys[0].message);
        assert!(!keys[0].prompt_id.is_empty());
    }
    assert!(prompts.secret_kinds.lock().unwrap().is_empty(), "no secret is asked after a rejected host key");
    assert!(mgr.status().is_empty());
    mgr.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_host_key_accepted_then_secret_cancelled() {
    let fake = FakeSsh::new("hostkey-yes", HOSTKEY_PS1, HOSTKEY_SH);
    let prompts = Arc::new(RecordingPrompts { accept_host_key: true, secret: None, ..Default::default() });
    let mgr = fake.manager(prompts.clone(), Duration::from_secs(60));
    let err = acquire_err(&mgr, &cfg(SshAuth::Password), None).await;
    assert!(matches!(err, CoreError::Cancelled), "cancelling the secret prompt must surface as CANCELLED, got {err:?}");
    assert_eq!(prompts.host_keys.lock().unwrap().len(), 1);
    assert_eq!(*prompts.secret_kinds.lock().unwrap(), vec!["password".to_string()], "\"yes\" reached the script, which then asked for the password");
    assert!(mgr.status().is_empty());
    mgr.shutdown().await;
}

/// Runs the listener script; `None` when this machine has no tool to listen with (unix only).
async fn acquire_listener(mgr: &TunnelManager, cfg: &SshConfig) -> Option<bulk_core::tunnel::TunnelLease> {
    match tokio::time::timeout(ACQUIRE_TIMEOUT, mgr.acquire(cfg, "db.internal", 5432, None)).await.expect("acquire must return") {
        Ok(lease) => Some(lease),
        Err(CoreError::Tunnel(m)) if m.contains("no listener tool") => {
            eprintln!("skipping: {m}");
            None
        }
        Err(e) => panic!("the fake ssh listener did not come up: {e:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_tunnel_comes_up_is_shared_and_shut_down() {
    let fake = FakeSsh::new("listen", LISTEN_PS1, LISTEN_SH);
    let mgr = fake.manager(Arc::new(DenyAllPrompts), Duration::from_secs(60));
    let c = cfg(SshAuth::Agent);
    let Some(lease) = acquire_listener(&mgr, &c).await else { return };
    let (host, port) = lease.local();
    assert_eq!(host, "127.0.0.1");
    assert!(port_open(port), "the lease must point at the listening port");

    let st = mgr.status();
    assert_eq!(st.len(), 1, "{st:?}");
    assert_eq!(st[0].key, "alice@bastion.example:22->db.internal:5432");
    assert_eq!((st[0].local_port, st[0].leases, st[0].up), (port, 1, true));

    // Same key -> same process and port, one more lease; no second script is started.
    let lease2 = mgr.acquire(&c, "db.internal", 5432, None).await.expect("reuse");
    assert_eq!(lease2.local_port, port);
    assert_eq!(mgr.status()[0].leases, 2);
    drop(lease);
    assert_eq!(mgr.status()[0].leases, 1);
    drop(lease2);
    assert_eq!(mgr.status()[0].leases, 0);
    assert!(port_open(port), "an idle tunnel stays up until the idle time passes");
    assert!(mgr.recent_output(&st[0].key).is_some());

    mgr.shutdown().await;
    assert!(mgr.status().is_empty());
    wait_port_closed(port).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_idle_tunnel_is_reaped() {
    let fake = FakeSsh::new("reap", LISTEN_PS1, LISTEN_SH);
    let mgr = fake.manager(Arc::new(DenyAllPrompts), Duration::from_secs(1));
    let Some(lease) = acquire_listener(&mgr, &cfg(SshAuth::Agent)).await else { return };
    let port = lease.local_port;
    drop(lease);
    // The reaper runs every 5 s and kills entries idle for longer than 1 s.
    let started = Instant::now();
    while !mgr.status().is_empty() {
        assert!(started.elapsed() < Duration::from_secs(20), "idle tunnel was not reaped: {:?}", mgr.status());
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    wait_port_closed(port).await;
    mgr.shutdown().await;
}

/// Local port picker for [`TunnelManager::with_local_port_picker`]: hands out `taken` for the
/// first `taken_picks` calls and a free port after that, counting every call in `calls`.
fn taken_then_free(taken: u16, taken_picks: usize, calls: Arc<AtomicUsize>) -> impl Fn() -> Result<u16, CoreError> + Send + Sync + 'static {
    move || {
        if calls.fetch_add(1, Ordering::SeqCst) < taken_picks {
            Ok(taken)
        } else {
            free_port()
        }
    }
}

/// A port that is listening for the duration of the test (ssh must fail to bind it).
fn hold_port() -> (std::net::TcpListener, u16) {
    let holder = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind a local port");
    let port = holder.local_addr().expect("local addr").port();
    (holder, port)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_taken_local_port_is_retried_on_another_port() {
    let fake = FakeSsh::new("taken-retry", LISTEN_PS1, LISTEN_SH);
    let (_holder, taken) = hold_port();
    let calls = Arc::new(AtomicUsize::new(0));
    let mgr = fake.manager(Arc::new(DenyAllPrompts), Duration::from_secs(60)).with_local_port_picker(taken_then_free(taken, 1, calls.clone()));
    let Some(lease) = acquire_listener(&mgr, &cfg(SshAuth::Agent)).await else { return };
    let port = lease.local_port;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the taken port must cost exactly one retry");
    assert_ne!(port, taken, "the lease must not point at the port another program holds");
    assert!(port_open(port), "the retry's listener must be up");
    let st = mgr.status();
    assert_eq!(st.len(), 1, "{st:?}");
    assert_eq!((st[0].local_port, st[0].leases, st[0].up), (port, 1, true));
    assert!(port_open(taken), "the program holding the taken port must be left alone");
    drop(lease);
    mgr.shutdown().await;
    wait_port_closed(port).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_local_port_taken_again_on_retry_is_tunnel_error() {
    let fake = FakeSsh::new("taken-twice", LISTEN_PS1, LISTEN_SH);
    let (_holder, taken) = hold_port();
    let calls = Arc::new(AtomicUsize::new(0));
    let mgr = fake.manager(Arc::new(DenyAllPrompts), Duration::from_secs(60)).with_local_port_picker(taken_then_free(taken, usize::MAX, calls.clone()));
    let err = acquire_err(&mgr, &cfg(SshAuth::Agent), None).await;
    let msg = err.to_string();
    assert!(matches!(err, CoreError::Tunnel(_)), "expected TUNNEL, got {err:?}");
    assert!(msg.contains(&format!("127.0.0.1]:{taken}: Address already in use")), "the error must name the conflict: {msg}");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one retry, then give up");
    assert!(mgr.status().is_empty(), "nothing must remain after a failed acquire");
    assert!(port_open(taken), "the program holding the taken port must be left alone");
    mgr.shutdown().await;
}

/// ssh itself reports that it could not bind the local port (taken after the manager checked
/// it): classified TUNNEL and retried once on the next pick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_ssh_forward_failure_reported_by_ssh_is_retried() {
    let fake = FakeSsh::new("forward-fail", LISTEN_PS1, LISTEN_SH);
    let marker = fake.dir.join("fail-forward-once");
    std::fs::write(&marker, b"").expect("write the fail-once marker");
    let calls = Arc::new(AtomicUsize::new(0));
    let mgr = fake.manager(Arc::new(DenyAllPrompts), Duration::from_secs(60)).with_local_port_picker(taken_then_free(0, 0, calls.clone()));
    let Some(lease) = acquire_listener(&mgr, &cfg(SshAuth::Agent)).await else { return };
    assert!(!marker.exists(), "the first run must have consumed the marker (and failed)");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the forward failure must cost exactly one retry");
    let port = lease.local_port;
    assert!(port_open(port));
    let st = mgr.status();
    assert_eq!((st.len(), st[0].local_port, st[0].up), (1, port, true), "{st:?}");
    drop(lease);
    mgr.shutdown().await;
    wait_port_closed(port).await;
}

// ---------------------------------------------------------------------------------------------
// Real OpenSSH server: `--features it` + BULKDB_SSH_URL
// ---------------------------------------------------------------------------------------------

/// Tunnel tests with the real `ssh` client (`platform::ssh_path()`) against a real OpenSSH
/// server, e.g. the `sshd` service of `docker/compose.yml` (linuxserver/openssh-server with
/// the `openssh-server-ssh-tunnel` mod, which turns `AllowTcpForwarding` on):
///
/// ```text
/// BULKDB_SSH_URL=ssh://tester:tester@127.0.0.1:2222  bastion account with password auth
///                                                    (user and password percent-decoded)
/// BULKDB_SSH_TARGET=127.0.0.1:2222                   optional: forward target as the bastion
///                                                    sees it; must be an SSH server, whose
///                                                    banner proves the forward end to end
///                                                    (default 127.0.0.1:<bastion port>)
/// BULKDB_SSH_KEY=/path/to/id_ed25519                 optional: private key authorized for the
///                                                    same account (key test skipped without)
/// BULKDB_SSH_KEY_PASSPHRASE=...                      optional: that key's passphrase
/// cargo test -p bulk-core --features it --test tunnel_tests openssh_it
/// ```
///
/// `ssh` runs with `-F none` and a private known_hosts file per test, so the developer's
/// `~/.ssh` is neither read nor modified, and host-key checking stays on (`ask`). The tests
/// run one at a time and keep failed logins and unauthenticated connections few: sshd from
/// OpenSSH 9.8 on penalises a source address for them (`PerSourcePenalties`: once 15 s of
/// penalty pile up it refuses connections), and everything through docker's port mapping
/// comes from one address. Each acquire costs about 1 s of `noauth` penalty, because the
/// readiness probe and the banner checks reach the bastion's own sshd without logging in.
#[cfg(feature = "it")]
mod openssh_it {
    use std::io::Read;
    use std::net::{TcpStream, ToSocketAddrs};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use bulk_core::tunnel::{tunnel_key, PromptHandler, TunnelLease, TunnelManager};
    use bulk_core::types::{HostKeyPrompt, SshAuth, SshConfig, SshPasswordPrompt};
    use bulk_core::CoreError;

    use super::{hold_port, port_open, taken_then_free, wait_port_closed, ACQUIRE_TIMEOUT};

    /// A well-formed ed25519 public key that is not the server's (its private half was
    /// discarded when it was generated): written to known_hosts as the "old" host key.
    const FOREIGN_ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEM7lf7hwAIeO46h/991A+pREa50RaaV2Y00+95qU9hx";
    /// How long the first test waits for the server to answer with an SSH banner (the docker
    /// image installs its tunnel mod at start-up).
    const SERVER_WAIT: Duration = Duration::from_secs(120);

    /// One test at a time against the shared server (see the module docs).
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    /// The server answered once; later tests skip the (penalised) readiness probe.
    static SERVER_READY: AtomicBool = AtomicBool::new(false);

    /// The bastion from `BULKDB_SSH_URL` and the forward target from `BULKDB_SSH_TARGET`.
    struct Server {
        host: String,
        port: u16,
        user: String,
        password: String,
        target_host: String,
        target_port: u16,
    }

    impl Server {
        fn from_env() -> Option<Server> {
            let Some(url) = std::env::var("BULKDB_SSH_URL").ok().filter(|u| !u.trim().is_empty()) else {
                eprintln!("skipping: BULKDB_SSH_URL is not set");
                return None;
            };
            let url = url.trim();
            let rest = url.strip_prefix("ssh://").unwrap_or_else(|| panic!("BULKDB_SSH_URL must look like ssh://user:password@host:port, got {url:?}"));
            let (userinfo, hostport) = rest.rsplit_once('@').unwrap_or_else(|| panic!("BULKDB_SSH_URL has no user@: {url:?}"));
            let (user, password) = userinfo.split_once(':').unwrap_or_else(|| panic!("BULKDB_SSH_URL has no password (ssh://user:password@host): {url:?}"));
            let (host, port) = split_host_port(hostport.trim_end_matches('/'), 22);
            let (target_host, target_port) = match std::env::var("BULKDB_SSH_TARGET") {
                Ok(t) if !t.trim().is_empty() => split_host_port(t.trim(), port),
                _ => ("127.0.0.1".to_string(), port),
            };
            Some(Server { host, port, user: percent_decode(user), password: percent_decode(password), target_host, target_port })
        }

        fn cfg(&self, auth: SshAuth, key_path: Option<String>) -> SshConfig {
            SshConfig { enabled: true, host: self.host.clone(), port: self.port, user: self.user.clone(), auth, key_path, compression: false }
        }

        /// known_hosts host pattern ssh uses for this bastion.
        fn known_hosts_pattern(&self) -> String {
            if self.port == 22 {
                self.host.clone()
            } else {
                format!("[{}]:{}", self.host, self.port)
            }
        }
    }

    /// `host:port`, `[v6]:port` or a bare host (then `default_port`).
    fn split_host_port(s: &str, default_port: u16) -> (String, u16) {
        let parse = |p: &str| p.parse::<u16>().unwrap_or_else(|_| panic!("bad port in {s:?}"));
        if let Some(rest) = s.strip_prefix('[') {
            let (host, after) = rest.split_once(']').unwrap_or_else(|| panic!("unterminated '[' in {s:?}"));
            return (host.to_string(), after.strip_prefix(':').map(parse).unwrap_or(default_port));
        }
        match s.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), parse(port)),
            None => (s.to_string(), default_port),
        }
    }

    fn percent_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
            match (bytes[i], hex) {
                (b'%', Some(b)) => {
                    out.push(b);
                    i += 3;
                }
                (c, _) => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).expect("BULKDB_SSH_URL decodes to UTF-8")
    }

    /// First line an SSH server sends (`SSH-2.0-...`), read from `host:port`.
    fn read_banner(host: &str, port: u16) -> std::io::Result<String> {
        let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{host} has no address")))?;
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.len() < 255 && !line.ends_with(b"\n") {
            if stream.read(&mut byte)? == 0 {
                break;
            }
            line.push(byte[0]);
        }
        Ok(String::from_utf8_lossy(&line).trim_end().to_string())
    }

    /// Assert that the tunnel on `local_port` reaches the target SSH server.
    fn assert_forwards(local_port: u16) {
        let banner = read_banner("127.0.0.1", local_port).expect("read through the tunnel");
        assert!(banner.starts_with("SSH-"), "the forward must reach the target's sshd, got {banner:?}");
    }

    /// Private ssh state for one test: a temp dir with its own known_hosts (removed on drop).
    struct Isolated {
        dir: PathBuf,
        known_hosts: PathBuf,
    }

    impl Isolated {
        fn new(name: &str) -> Isolated {
            let dir = std::env::temp_dir().join(format!("bulkdb-tunnel-it-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            let known_hosts = dir.join("known_hosts");
            Isolated { dir, known_hosts }
        }

        /// Options placed before the tunnel's own (ssh keeps the first value of an option): no
        /// config files, this test's known_hosts, no system-wide known hosts.
        fn ssh_options(&self) -> Vec<String> {
            // Forward slashes work for OpenSSH on Windows too and need no escaping inside the
            // quotes that ssh's option parser strips.
            let quoted = |p: &Path| format!("\"{}\"", p.to_string_lossy().replace('\\', "/"));
            vec![
                "-F".into(),
                "none".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", quoted(&self.known_hosts)),
                "-o".into(),
                format!("GlobalKnownHostsFile={}", quoted(&self.dir.join("global_known_hosts"))),
            ]
        }

        fn manager(&self, prompts: Arc<dyn PromptHandler>) -> TunnelManager {
            TunnelManager::new(prompts, Duration::from_secs(60)).with_ssh_command(bulk_core::platform::ssh_path(), self.ssh_options())
        }

        /// Like [`Isolated::manager`], but on unix ssh is started through `sh`, which writes its
        /// pid (ssh keeps it across `exec`) to `pid_file` so the test can kill the session.
        /// Windows finds the process by its listening port instead (see [`kill_ssh`]).
        fn manager_recording_pid(&self, prompts: Arc<dyn PromptHandler>, pid_file: &Path) -> TunnelManager {
            if std::env::consts::OS == "windows" {
                return self.manager(prompts);
            }
            let mut leading: Vec<String> = vec![
                "-c".into(),
                r#"printf '%s\n' "$$" > "$0" && exec "$@""#.into(),
                pid_file.to_string_lossy().into_owned(),
                bulk_core::platform::ssh_path().to_string_lossy().into_owned(),
            ];
            leading.extend(self.ssh_options());
            TunnelManager::new(prompts, Duration::from_secs(60)).with_ssh_command("/bin/sh", leading)
        }
    }

    impl Drop for Isolated {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Kill the ssh process of a live tunnel, as a network drop or a server restart would end it.
    fn kill_ssh(local_port: u16, pid_file: &Path) {
        let status = if std::env::consts::OS == "windows" {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            let ps = PathBuf::from(root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe");
            let script = format!(
                "Get-NetTCPConnection -LocalAddress 127.0.0.1 -LocalPort {local_port} -State Listen -ErrorAction Stop | ForEach-Object {{ Stop-Process -Id $_.OwningProcess -Force -ErrorAction Stop }}"
            );
            std::process::Command::new(ps).args(["-NoProfile", "-NonInteractive", "-Command", &script]).status()
        } else {
            let pid = std::fs::read_to_string(pid_file).expect("ssh pid file").trim().to_string();
            std::process::Command::new("kill").args(["-9", &pid]).status()
        };
        assert!(status.expect("run the kill command").success(), "could not kill the ssh process behind port {local_port}");
    }

    /// Records every prompt; answers host keys with `accept_host_key` and secrets with `secret`.
    struct ItPrompts {
        accept_host_key: bool,
        secret: Option<String>,
        host_keys: Mutex<Vec<HostKeyPrompt>>,
        secrets: Mutex<Vec<SshPasswordPrompt>>,
    }

    impl ItPrompts {
        fn new(accept_host_key: bool, secret: Option<String>) -> Arc<ItPrompts> {
            Arc::new(ItPrompts { accept_host_key, secret, host_keys: Mutex::new(Vec::new()), secrets: Mutex::new(Vec::new()) })
        }

        fn host_keys(&self) -> Vec<HostKeyPrompt> {
            self.host_keys.lock().unwrap().clone()
        }

        /// Kinds of the secret prompts so far, checking that they named the bastion account.
        fn secret_kinds(&self, srv: &Server) -> Vec<String> {
            let asked = self.secrets.lock().unwrap();
            for p in asked.iter() {
                assert_eq!((p.user.as_str(), p.host.as_str()), (srv.user.as_str(), srv.host.as_str()), "secret prompt for the wrong account: {p:?}");
            }
            asked.iter().map(|p| p.kind.clone()).collect()
        }
    }

    #[async_trait]
    impl PromptHandler for ItPrompts {
        async fn host_key(&self, prompt: HostKeyPrompt) -> Result<bool, CoreError> {
            self.host_keys.lock().unwrap().push(prompt);
            Ok(self.accept_host_key)
        }
        async fn ssh_secret(&self, prompt: SshPasswordPrompt) -> Result<Option<String>, CoreError> {
            self.secrets.lock().unwrap().push(prompt);
            Ok(self.secret.clone())
        }
    }

    /// Serialise, read the environment, wait for the server once and make a private ssh
    /// state. `None` (test skipped) without `BULKDB_SSH_URL`.
    async fn setup(name: &str) -> Option<(tokio::sync::MutexGuard<'static, ()>, Server, Isolated)> {
        let srv = Server::from_env()?;
        let serial = SERIAL.lock().await;
        if !SERVER_READY.load(Ordering::SeqCst) {
            let started = Instant::now();
            loop {
                match read_banner(&srv.host, srv.port) {
                    Ok(banner) if banner.starts_with("SSH-") => break,
                    other => {
                        assert!(started.elapsed() < SERVER_WAIT, "no SSH server at {}:{} after {SERVER_WAIT:?}: {other:?}", srv.host, srv.port);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            SERVER_READY.store(true, Ordering::SeqCst);
        }
        Some((serial, srv, Isolated::new(name)))
    }

    async fn acquire(mgr: &TunnelManager, srv: &Server, cfg: &SshConfig, secret: Option<String>) -> Result<TunnelLease, CoreError> {
        tokio::time::timeout(ACQUIRE_TIMEOUT, mgr.acquire(cfg, &srv.target_host, srv.target_port, secret)).await.expect("acquire must return")
    }

    fn expect_up(res: Result<TunnelLease, CoreError>) -> TunnelLease {
        res.unwrap_or_else(|e| {
            let hint = if matches!(e, CoreError::Connect(_)) {
                "\n(hint: a CONNECT error from a server that answered before can be sshd's PerSourcePenalties refusing this address; run the test server with `PerSourcePenalties no`)"
            } else {
                ""
            };
            panic!("the tunnel did not come up: {e:?}{hint}")
        })
    }

    fn expect_err(res: Result<TunnelLease, CoreError>) -> CoreError {
        match res {
            Ok(lease) => panic!("a tunnel came up on port {} although it must not", lease.local_port),
            Err(e) => e,
        }
    }

    /// Unknown host key: one prompt, the stored password is used without asking, the forward
    /// reaches the target, and "yes" was persisted (a second run is not asked again).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_password_auth_with_new_host_key() {
        let Some((_serial, srv, iso)) = setup("password").await else { return };
        let c = srv.cfg(SshAuth::Password, None);
        let prompts = ItPrompts::new(true, None);
        let mgr = iso.manager(prompts.clone());
        let lease = expect_up(acquire(&mgr, &srv, &c, Some(srv.password.clone())).await);
        let keys = prompts.host_keys();
        assert_eq!(keys.len(), 1, "an empty known_hosts must raise exactly one host key prompt: {keys:?}");
        assert!(keys[0].fingerprint.starts_with("SHA256:"), "{:?}", keys[0]);
        assert!(keys[0].host.contains(&srv.host), "{:?}", keys[0]);
        assert!(keys[0].key_type.is_some(), "{:?}", keys[0]);
        assert!(prompts.secret_kinds(&srv).is_empty(), "the stored password must be used without asking");
        assert_forwards(lease.local_port);
        let st = mgr.status();
        assert_eq!(st.len(), 1, "{st:?}");
        assert_eq!((st[0].key.clone(), st[0].local_port, st[0].leases, st[0].up), (tunnel_key(&c, &srv.target_host, srv.target_port), lease.local_port, 1, true));
        let output = mgr.recent_output(&st[0].key).unwrap_or_default();
        assert!(!output.contains(&srv.password), "the password leaked into the ssh output: {output}");
        let recorded = std::fs::read_to_string(&iso.known_hosts).unwrap_or_default();
        assert!(recorded.contains("ssh-") || recorded.contains("ecdsa-"), "ssh must have recorded the accepted host key, known_hosts: {recorded:?}");
        let port = lease.local_port;
        drop(lease);
        mgr.shutdown().await;
        wait_port_closed(port).await;

        // Same known_hosts, new manager: the key is known now, so nothing is asked (a host key
        // prompt would be rejected and fail the acquire).
        let prompts = ItPrompts::new(false, None);
        let mgr = iso.manager(prompts.clone());
        let lease = expect_up(acquire(&mgr, &srv, &c, Some(srv.password.clone())).await);
        assert!(prompts.host_keys().is_empty(), "an accepted host key must not be asked for again");
        assert!(prompts.secret_kinds(&srv).is_empty());
        let port = lease.local_port;
        drop(lease);
        mgr.shutdown().await;
        wait_port_closed(port).await;
    }

    /// A wrong stored password is rejected by the real server (classified AUTH), ssh is
    /// restarted and the user is asked exactly once; the host key accepted on the first
    /// attempt is known on the second.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_wrong_stored_password_then_user_answer() {
        let Some((_serial, srv, iso)) = setup("wrong-stored").await else { return };
        let c = srv.cfg(SshAuth::Password, None);
        let prompts = ItPrompts::new(true, Some(srv.password.clone()));
        let mgr = iso.manager(prompts.clone());
        let wrong = "bulkdb-wrong-password".to_string();
        let lease = expect_up(acquire(&mgr, &srv, &c, Some(wrong.clone())).await);
        assert_eq!(prompts.secret_kinds(&srv), vec!["password".to_string()], "asked once, after the stored password was rejected");
        assert_eq!(prompts.host_keys().len(), 1, "the host key is asked on the first attempt only");
        let key = tunnel_key(&c, &srv.target_host, srv.target_port);
        let output = mgr.recent_output(&key).expect("tunnel entry");
        assert!(!output.contains(&srv.password) && !output.contains(&wrong), "a secret leaked into the ssh output: {output}");
        assert!(port_open(lease.local_port));
        let port = lease.local_port;
        drop(lease);
        mgr.shutdown().await;
        wait_port_closed(port).await;
    }

    /// Public key authentication with `BULKDB_SSH_KEY` (and its passphrase prompt when
    /// `BULKDB_SSH_KEY_PASSPHRASE` is set).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_key_auth() {
        let Some((_serial, srv, iso)) = setup("key").await else { return };
        let Some(key) = std::env::var("BULKDB_SSH_KEY").ok().filter(|k| !k.trim().is_empty()) else {
            eprintln!("skipping: BULKDB_SSH_KEY is not set");
            return;
        };
        let passphrase = std::env::var("BULKDB_SSH_KEY_PASSPHRASE").ok().filter(|p| !p.is_empty());
        let c = srv.cfg(SshAuth::Key, Some(key));
        let prompts = ItPrompts::new(true, passphrase.clone());
        let mgr = iso.manager(prompts.clone());
        let lease = expect_up(acquire(&mgr, &srv, &c, None).await);
        let expected: Vec<String> = if passphrase.is_some() { vec!["passphrase".into()] } else { Vec::new() };
        assert_eq!(prompts.secret_kinds(&srv), expected, "only an encrypted key asks (for its passphrase), never for a password");
        assert_eq!(prompts.host_keys().len(), 1);
        assert!(port_open(lease.local_port));
        let st = mgr.status();
        assert_eq!((st.len(), st[0].up), (1, true), "{st:?}");
        let port = lease.local_port;
        drop(lease);
        mgr.shutdown().await;
        wait_port_closed(port).await;
    }

    /// known_hosts holds another key for the bastion: ssh itself refuses (strict checking),
    /// the user is never offered the new key, no password is sent, known_hosts is untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_changed_host_key_is_rejected() {
        let Some((_serial, srv, iso)) = setup("changed-key").await else { return };
        let known = format!("{} {FOREIGN_ED25519}\n", srv.known_hosts_pattern());
        std::fs::write(&iso.known_hosts, &known).expect("write known_hosts");
        // No stored secret: a password prompt would reach the handler and be recorded.
        let prompts = ItPrompts::new(true, Some(srv.password.clone()));
        let mgr = iso.manager(prompts.clone());
        let err = expect_err(acquire(&mgr, &srv, &srv.cfg(SshAuth::Password, None), None).await);
        let msg = err.to_string();
        assert!(matches!(err, CoreError::Tunnel(_)), "expected TUNNEL, got {err:?}");
        assert!(msg.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || msg.contains("Host key verification failed"), "{msg}");
        assert!(prompts.host_keys().is_empty(), "a changed host key must never be offered for acceptance");
        assert!(prompts.secret_kinds(&srv).is_empty(), "no password may be sent to a host whose key changed");
        assert!(mgr.status().is_empty(), "nothing must remain after a failed acquire");
        assert_eq!(std::fs::read_to_string(&iso.known_hosts).expect("known_hosts"), known, "known_hosts must not be modified");
        mgr.shutdown().await;
    }

    /// Another program listens on the picked local port: `acquire` must not mistake that
    /// listener for the tunnel; it retries once on another port and the real ssh session
    /// comes up there. Taken again on the retry, it fails with TUNNEL naming the conflict.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_local_port_taken() {
        let Some((_serial, srv, iso)) = setup("port-taken").await else { return };
        let c = srv.cfg(SshAuth::Password, None);
        let (_holder, taken) = hold_port();

        let calls = Arc::new(AtomicUsize::new(0));
        let prompts = ItPrompts::new(true, None);
        let mgr = iso.manager(prompts.clone()).with_local_port_picker(taken_then_free(taken, 1, calls.clone()));
        let lease = expect_up(acquire(&mgr, &srv, &c, Some(srv.password.clone())).await);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the taken port must cost exactly one retry");
        assert_ne!(lease.local_port, taken, "the lease must not point at the port another program holds");
        assert_eq!(prompts.host_keys().len(), 1, "ssh is started once (on the free port), so one host key prompt");
        assert!(prompts.secret_kinds(&srv).is_empty(), "the stored password survives the retry and is used without asking");
        assert!(port_open(lease.local_port));
        let port = lease.local_port;
        drop(lease);
        mgr.shutdown().await;
        wait_port_closed(port).await;

        let calls = Arc::new(AtomicUsize::new(0));
        let mgr = iso.manager(ItPrompts::new(false, None)).with_local_port_picker(taken_then_free(taken, usize::MAX, calls.clone()));
        let err = expect_err(acquire(&mgr, &srv, &c, Some(srv.password.clone())).await);
        let msg = err.to_string();
        assert!(matches!(err, CoreError::Tunnel(_)), "expected TUNNEL, got {err:?}");
        assert!(msg.contains("Address already in use") || msg.contains("cannot listen to port"), "the error must name the conflict: {msg}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one retry, then give up");
        assert!(mgr.status().is_empty(), "nothing must remain after a failed acquire");
        assert!(port_open(taken), "the program holding the port must be left alone");
        mgr.shutdown().await;
    }

    /// The ssh session dies under a live tunnel: the manager reports it down, the next
    /// `acquire` for the same key starts a new working tunnel, and the stale lease of the dead
    /// one does not touch the new tunnel's lease count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn it_reacquire_after_ssh_session_killed() {
        let Some((_serial, srv, iso)) = setup("reconnect").await else { return };
        let c = srv.cfg(SshAuth::Password, None);
        let key = tunnel_key(&c, &srv.target_host, srv.target_port);
        let pid_file = iso.dir.join("ssh.pid");
        let prompts = ItPrompts::new(true, None);
        let mgr = iso.manager_recording_pid(prompts.clone(), &pid_file);
        let stale = expect_up(acquire(&mgr, &srv, &c, Some(srv.password.clone())).await);
        let stale_port = stale.local_port;
        assert_forwards(stale_port);

        kill_ssh(stale_port, &pid_file);
        let started = Instant::now();
        while mgr.status().iter().any(|s| s.up) {
            assert!(started.elapsed() < Duration::from_secs(15), "the killed tunnel is still reported up: {:?}", mgr.status());
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        wait_port_closed(stale_port).await;

        let fresh = expect_up(acquire(&mgr, &srv, &c, Some(srv.password.clone())).await);
        assert_forwards(fresh.local_port);
        let st = mgr.status();
        assert_eq!(st.len(), 1, "the dead entry must have been replaced: {st:?}");
        assert_eq!((st[0].key.clone(), st[0].local_port, st[0].leases, st[0].up), (key, fresh.local_port, 1, true));
        drop(stale);
        assert_eq!(mgr.status()[0].leases, 1, "dropping the stale lease must not release the new tunnel");
        assert_eq!(prompts.host_keys().len(), 1, "the restart finds the host key known");
        assert!(prompts.secret_kinds(&srv).is_empty(), "the stored password is reused for the restart");
        let port = fresh.local_port;
        drop(fresh);
        mgr.shutdown().await;
        wait_port_closed(port).await;
    }
}
