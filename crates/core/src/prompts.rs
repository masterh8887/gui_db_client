//! Event based [`PromptHandler`]: the tunnel asks a question, the service turns it into a
//! `tunnel:hostkey` / `tunnel:password` [`CoreEvent`] carrying a prompt id, and the shell
//! answers through `BulkDb::tunnel_confirm_hostkey` / `tunnel_provide_secret`. The ssh child
//! waits on a oneshot in the meantime; an unanswered prompt times out after five minutes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};

use crate::error::{CoreError, Result};
use crate::tunnel::PromptHandler;
use crate::types::{CoreEvent, HostKeyPrompt, SshPasswordPrompt};

/// Default time the user gets to answer a prompt before the tunnel gives up.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

enum Pending {
    HostKey(oneshot::Sender<bool>),
    Secret(oneshot::Sender<Option<String>>),
}

impl Pending {
    fn kind(&self) -> &'static str {
        match self {
            Pending::HostKey(_) => "hostkey",
            Pending::Secret(_) => "password",
        }
    }
}

/// Turns tunnel prompts into events and waits for the answer.
pub struct EventPromptHandler {
    events: broadcast::Sender<CoreEvent>,
    pending: Mutex<HashMap<String, Pending>>,
    timeout: Duration,
}

impl EventPromptHandler {
    pub fn new(events: broadcast::Sender<CoreEvent>) -> Self {
        Self::with_timeout(events, PROMPT_TIMEOUT)
    }

    pub fn with_timeout(events: broadcast::Sender<CoreEvent>, timeout: Duration) -> Self {
        EventPromptHandler { events, pending: Mutex::new(HashMap::new()), timeout }
    }

    /// Ids of prompts still waiting for an answer.
    pub fn pending_ids(&self) -> Vec<String> {
        self.pending.lock().unwrap().keys().cloned().collect()
    }

    /// Answer a host key prompt. `NotFound` when the id is unknown (already answered, timed
    /// out, or raised by a different handler), `Invalid` when the id belongs to a password prompt.
    pub fn confirm_hostkey(&self, prompt_id: &str, accept: bool) -> Result<()> {
        let tx = self.take(prompt_id, "hostkey")?;
        match tx {
            Pending::HostKey(tx) => {
                tracing::info!(prompt_id, accept, "host key prompt answered");
                tx.send(accept).map_err(|_| CoreError::Tunnel("the tunnel no longer waits for this host key answer".into()))
            }
            Pending::Secret(_) => unreachable!("take() checks the kind"),
        }
    }

    /// Answer a password / passphrase prompt. `None` cancels the tunnel attempt.
    pub fn provide_secret(&self, prompt_id: &str, secret: Option<String>) -> Result<()> {
        let tx = self.take(prompt_id, "password")?;
        match tx {
            Pending::Secret(tx) => {
                tracing::info!(prompt_id, provided = secret.is_some(), "ssh secret prompt answered");
                tx.send(secret).map_err(|_| CoreError::Tunnel("the tunnel no longer waits for this secret".into()))
            }
            Pending::HostKey(_) => unreachable!("take() checks the kind"),
        }
    }

    fn take(&self, prompt_id: &str, kind: &str) -> Result<Pending> {
        let mut pending = self.pending.lock().unwrap();
        match pending.get(prompt_id) {
            None => Err(CoreError::NotFound(format!("tunnel prompt {prompt_id}"))),
            Some(p) if p.kind() != kind => Err(CoreError::Invalid(format!("prompt {prompt_id} is a {} prompt, not a {kind} prompt", p.kind()))),
            Some(_) => Ok(pending.remove(prompt_id).expect("checked above")),
        }
    }

    fn new_id() -> String {
        uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
    }

    fn register(&self, id: &str, p: Pending) {
        self.pending.lock().unwrap().insert(id.to_string(), p);
    }

    fn forget(&self, id: &str) {
        self.pending.lock().unwrap().remove(id);
    }
}

#[async_trait]
impl PromptHandler for EventPromptHandler {
    async fn host_key(&self, mut prompt: HostKeyPrompt) -> Result<bool> {
        let id = Self::new_id();
        prompt.prompt_id = id.clone();
        let (tx, rx) = oneshot::channel();
        self.register(&id, Pending::HostKey(tx));
        let host = prompt.host.clone();
        if self.events.send(CoreEvent::TunnelHostKey(prompt)).is_err() {
            self.forget(&id);
            return Err(CoreError::Tunnel(format!("unknown host key for {host} and nobody is listening for the confirmation prompt")));
        }
        tracing::info!(prompt_id = %id, host = %host, "host key confirmation requested");
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(accept)) => Ok(accept),
            Ok(Err(_)) => {
                self.forget(&id);
                Err(CoreError::Tunnel(format!("host key prompt for {host} was dropped")))
            }
            Err(_) => {
                self.forget(&id);
                Err(CoreError::Tunnel(format!("host key confirmation for {host} timed out after {} s", self.timeout.as_secs())))
            }
        }
    }

    async fn ssh_secret(&self, mut prompt: SshPasswordPrompt) -> Result<Option<String>> {
        let id = Self::new_id();
        prompt.prompt_id = id.clone();
        let (tx, rx) = oneshot::channel();
        self.register(&id, Pending::Secret(tx));
        let (host, user, kind) = (prompt.host.clone(), prompt.user.clone(), prompt.kind.clone());
        if self.events.send(CoreEvent::TunnelPassword(prompt)).is_err() {
            self.forget(&id);
            return Err(CoreError::Tunnel(format!("ssh {kind} for {user}@{host} required and nobody is listening for the prompt")));
        }
        tracing::info!(prompt_id = %id, host = %host, user = %user, kind = %kind, "ssh secret requested");
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(secret)) => Ok(secret),
            Ok(Err(_)) => {
                self.forget(&id);
                Err(CoreError::Tunnel(format!("ssh {kind} prompt for {user}@{host} was dropped")))
            }
            Err(_) => {
                self.forget(&id);
                Err(CoreError::Tunnel(format!("ssh {kind} for {user}@{host} not provided within {} s", self.timeout.as_secs())))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn prompt() -> HostKeyPrompt {
        HostKeyPrompt { prompt_id: String::new(), host: "bastion".into(), fingerprint: "SHA256:abc".into(), key_type: Some("ED25519".into()), message: "?".into() }
    }

    #[tokio::test]
    async fn hostkey_round_trip() {
        let (tx, mut rx) = broadcast::channel(8);
        let h = Arc::new(EventPromptHandler::new(tx));
        let h2 = h.clone();
        let waiter = tokio::spawn(async move { h2.host_key(prompt()).await });
        let ev = rx.recv().await.unwrap();
        let CoreEvent::TunnelHostKey(p) = ev else { panic!("expected hostkey event") };
        assert!(!p.prompt_id.is_empty());
        assert_eq!(h.pending_ids(), vec![p.prompt_id.clone()]);
        assert!(matches!(h.confirm_hostkey("nope", true), Err(CoreError::NotFound(_))));
        assert!(matches!(h.provide_secret(&p.prompt_id, None), Err(CoreError::Invalid(_))));
        h.confirm_hostkey(&p.prompt_id, true).unwrap();
        assert!(waiter.await.unwrap().unwrap());
        assert!(h.pending_ids().is_empty());
    }

    #[tokio::test]
    async fn secret_round_trip_and_cancel() {
        let (tx, mut rx) = broadcast::channel(8);
        let h = Arc::new(EventPromptHandler::new(tx));
        let h2 = h.clone();
        let waiter =
            tokio::spawn(async move { h2.ssh_secret(SshPasswordPrompt { prompt_id: String::new(), host: "b".into(), user: "u".into(), kind: "password".into() }).await });
        let CoreEvent::TunnelPassword(p) = rx.recv().await.unwrap() else { panic!("expected password event") };
        h.provide_secret(&p.prompt_id, None).unwrap();
        assert_eq!(waiter.await.unwrap().unwrap(), None);
    }

    #[tokio::test]
    async fn times_out_and_errors_without_listener() {
        let (tx, _rx) = broadcast::channel(8);
        let h = EventPromptHandler::with_timeout(tx, Duration::from_millis(30));
        let err = h.host_key(prompt()).await.unwrap_err();
        assert!(matches!(err, CoreError::Tunnel(ref m) if m.contains("timed out")), "{err}");
        assert!(h.pending_ids().is_empty());

        let (tx, _) = broadcast::channel(8);
        let h = EventPromptHandler::new(tx);
        assert!(matches!(h.host_key(prompt()).await, Err(CoreError::Tunnel(_))));
    }
}
