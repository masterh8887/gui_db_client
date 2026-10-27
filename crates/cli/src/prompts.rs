//! Interactive prompts on the terminal: the `PromptHandler` the SSH tunnel uses for host keys
//! and secrets, plus the yes/no confirmation used by `export` for preflight warnings. Every
//! prompt goes to stderr / the tty so stdout stays parseable; when stdin is not a terminal the
//! answer is always "no" (reject the host key, no secret, no acknowledgement).

use std::io::{BufRead, IsTerminal, Write};

use async_trait::async_trait;
use bulk_core::error::{CoreError, Result};
use bulk_core::tunnel::PromptHandler;
use bulk_core::types::{HostKeyPrompt, SshPasswordPrompt};

/// `true` when a human can answer questions (stdin and stderr are terminals).
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask a yes/no question on stderr and read one line from stdin. `false` on EOF.
pub fn confirm(question: &str) -> std::io::Result<bool> {
    let mut err = std::io::stderr().lock();
    write!(err, "{question}")?;
    err.flush()?;
    let mut line = String::new();
    let n = std::io::stdin().lock().read_line(&mut line)?;
    if n == 0 {
        writeln!(err)?;
        return Ok(false);
    }
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// Read a secret without echo from the tty. The prompt is written to the tty, never stdout.
pub fn prompt_secret(label: &str) -> std::io::Result<String> {
    rpassword::prompt_password(label)
}

/// `PromptHandler` that talks to the person at the terminal.
pub struct StdinPrompts;

#[async_trait]
impl PromptHandler for StdinPrompts {
    async fn host_key(&self, prompt: HostKeyPrompt) -> Result<bool> {
        tokio::task::spawn_blocking(move || {
            eprintln!("The authenticity of host '{}' can't be established.", prompt.host);
            eprintln!("{} key fingerprint: {}", prompt.key_type.as_deref().unwrap_or("host"), prompt.fingerprint);
            let msg = prompt.message.trim();
            if !msg.is_empty() {
                eprintln!("{msg}");
            }
            if !is_interactive() {
                eprintln!("stdin is not a terminal: rejecting the unknown host key (add it to known_hosts first, e.g. `ssh {}`)", prompt.host);
                return Ok(false);
            }
            confirm("Accept and add to known_hosts? [y/N] ").map_err(|e| CoreError::Tunnel(format!("reading host key answer: {e}")))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("prompt task: {e}")))?
    }

    async fn ssh_secret(&self, prompt: SshPasswordPrompt) -> Result<Option<String>> {
        tokio::task::spawn_blocking(move || {
            if !is_interactive() {
                return Err(CoreError::Tunnel(format!(
                    "ssh {} for {}@{} is required but stdin is not a terminal; store it with `bulk-cli conn add` or run interactively",
                    prompt.kind, prompt.user, prompt.host
                )));
            }
            let label = format!("{}@{} ssh {}: ", prompt.user, prompt.host, prompt.kind);
            let secret = prompt_secret(&label).map_err(|e| CoreError::Tunnel(format!("reading ssh {}: {e}", prompt.kind)))?;
            Ok(if secret.is_empty() { None } else { Some(secret) })
        })
        .await
        .map_err(|e| CoreError::Internal(format!("prompt task: {e}")))?
    }
}
