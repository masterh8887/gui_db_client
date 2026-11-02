//! Several windows in one process (docs/db-copy.md 1.1): the window registry, window creation,
//! focus tracking, window-targeted events (menu actions, tunnel prompts, job hand-over, the quit
//! question) and the end of the session.
//!
//! The registry is the state of record for windows: `main` (from `tauri.conf.json`) and
//! `w-<n>` (n counts up, never reused), each with a 1-based display index (the smallest free
//! one), the connection it shows and its title. After every change `windows:changed`
//! (`WindowInfo[]`) goes to every window. The pure parts (index allocation, titles, routing
//! targets, prompt and job hand-over) are methods of [`RegistryState`] with unit tests; the
//! functions taking an `AppHandle` only apply their results.
//!
//! Window-targeted events are sent with `emit_to(EventTarget::WebviewWindow)`; the frontend
//! listens to them only with its own window as the target (`listenWindow` in `lib/ipc.ts`).
//!
//! Readiness: an event emitted to a window whose page has not registered its listeners yet is
//! lost (a new window gets the OS focus long before its page loaded; a reloaded page starts
//! over). Every window is therefore *unready* from its `window_init` until its page calls
//! `window_ready` (after its window-targeted listeners are attached). Tunnel prompts, the quit
//! question and job hand-overs go to a ready window when there is one
//! ([`RegistryState::delivery_target`]); what went to an unready window is re-sent when it
//! becomes ready ([`RegistryState::mark_ready`]).
//!
//! Closing: whether a closing window is the *last* one is decided here, under the registry
//! lock (`window_close_begin` marks the caller as closing; it is the last when every window is
//! closing), never from a page's possibly stale copy of `windows:changed`. Several windows
//! closing at once (the Windows taskbar's "Close all windows", Option-click on macOS) then
//! still ask "실행 중인 작업 {n}개를 취소하고 종료할까요?" exactly once, in the last of them.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bulk_core::types::{HostKeyPrompt, SshPasswordPrompt, WindowInfo, WindowInit};
use bulk_core::BulkDb;
use serde::Serialize;
use tauri::{AppHandle, Emitter, EventTarget, Manager, Runtime, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::state::{CmdError, CmdResult};

/// Label of the window declared in `tauri.conf.json`.
pub const MAIN_WINDOW: &str = "main";
/// Most windows one process shows.
pub const MAX_WINDOWS: usize = 8;
/// Window title on Welcome; a workspace window is `BulkDB — <connection name>`.
pub const APP_TITLE: &str = "BulkDB";
/// Broadcast after every registry change (`WindowInfo[]`).
pub const WINDOWS_CHANGED: &str = "windows:changed";
/// Window-targeted: jobs of a closed window handed to this one (`{ jobIds }`).
pub const JOBS_CLAIMED: &str = "jobs:claimed";
/// Window-targeted: quitting needs the user's confirmation (`{ jobs }`).
pub const QUIT_REQUESTED: &str = "app:quit-requested";
/// `INVALID` message of the ninth window.
pub const TOO_MANY_WINDOWS: &str = "창은 최대 8개까지 열 수 있습니다 (at most 8 windows)";
/// A new window opens this far (logical px) right of and below the focused one.
const CASCADE: f64 = 24.0;
/// The window options of `main` (`tauri.conf.json`).
const WIDTH: f64 = 1280.0;
const HEIGHT: f64 = 820.0;
const MIN_WIDTH: f64 = 1024.0;
const MIN_HEIGHT: f64 = 680.0;

/// One window of this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    pub label: String,
    /// 1-based display number ("창 2"), the smallest one free when the window opened.
    pub index: u32,
    pub conn_id: Option<String>,
    pub conn_name: Option<String>,
    pub title: String,
    /// Its page listens to window-targeted events (`window_ready` since its last `window_init`).
    pub ready: bool,
    /// Its page accepted a close request (`window_close_begin`) and the window is going away.
    pub closing: bool,
}

/// What a window that just became ready is sent again (it may have missed it).
#[derive(Debug, Default, PartialEq)]
pub struct ReadyResend {
    /// Its pending tunnel prompts.
    pub prompts: Vec<PromptPayload>,
    /// The active jobs it holds the claim of (`jobs:claimed`; a reloaded page lost them).
    pub jobs: Vec<String>,
    /// The quit question went to it while it was not ready.
    pub quit: bool,
}

/// Answer of `window_close_begin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseCheck {
    /// Every window of this process is closing: this close ends the session.
    pub last: bool,
    /// Active jobs of this process (cancelled when the session ends).
    pub jobs: usize,
}

/// A tunnel prompt as core raised it.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptPayload {
    HostKey(HostKeyPrompt),
    Password(SshPasswordPrompt),
}

impl PromptPayload {
    /// The window-targeted event that carries it.
    pub fn event(&self) -> &'static str {
        match self {
            PromptPayload::HostKey(_) => "tunnel:hostkey",
            PromptPayload::Password(_) => "tunnel:password",
        }
    }

    pub fn prompt_id(&self) -> &str {
        match self {
            PromptPayload::HostKey(p) => &p.prompt_id,
            PromptPayload::Password(p) => &p.prompt_id,
        }
    }
}

/// A tunnel prompt shown in `label` until core resolves it (`prompt:resolved`).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPrompt {
    pub label: String,
    pub payload: PromptPayload,
}

/// What closing a window hands to the others.
#[derive(Debug, Default, PartialEq)]
pub struct Handover {
    /// Its pending prompts, re-targeted (`label` = the receiving window).
    pub prompts: Vec<PendingPrompt>,
    /// Its active job claims and the window that takes them.
    pub jobs: Option<(String, Vec<String>)>,
    /// No window is left.
    pub last: bool,
}

#[derive(Debug, Default)]
pub struct RegistryState {
    pub entries: BTreeMap<String, WindowEntry>,
    /// Most recently focused first.
    pub focus_order: Vec<String>,
    /// The window that has the OS focus now (`None` while another app is active).
    pub focused: Option<String>,
    /// Connection a new window opens with, taken by its `window_init`.
    pub pending_init: HashMap<String, String>,
    /// n of the next `w-<n>` label.
    pub next_n: u32,
    pub prompts: Vec<PendingPrompt>,
    /// Active jobs and the window that started (claimed) them: completion toasts show there.
    pub job_windows: HashMap<String, String>,
    /// The unready window the quit question went to: it is asked again once it is ready.
    pub quit_pending: Option<String>,
}

/// `BulkDB` on Welcome, `BulkDB — <name>` (em dash) for a workspace.
pub fn window_title(conn_name: Option<&str>) -> String {
    match conn_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("{APP_TITLE} — {name}"),
        None => APP_TITLE.to_string(),
    }
}

impl RegistryState {
    /// A registry holding `main` as window 1.
    pub fn with_main() -> Self {
        let mut s = RegistryState { next_n: 2, ..Default::default() };
        s.add(MAIN_WINDOW);
        s
    }

    /// The smallest display index no window uses.
    pub fn smallest_free_index(&self) -> u32 {
        (1..).find(|i| !self.entries.values().any(|e| e.index == *i)).unwrap_or(1)
    }

    /// Register `label` (no-op when known) with the smallest free index.
    fn add(&mut self, label: &str) -> &WindowEntry {
        if !self.entries.contains_key(label) {
            let entry = WindowEntry {
                label: label.to_string(),
                index: self.smallest_free_index(),
                conn_id: None,
                conn_name: None,
                title: window_title(None),
                ready: false,
                closing: false,
            };
            self.entries.insert(label.to_string(), entry);
            self.focus_order.push(label.to_string());
        }
        &self.entries[label]
    }

    /// Reserve the label and index of a new window (at most [`MAX_WINDOWS`]); `conn_id` is
    /// what its `window_init` will open.
    pub fn reserve(&mut self, conn_id: Option<String>) -> Result<(String, u32), String> {
        if self.entries.len() >= MAX_WINDOWS {
            return Err(TOO_MANY_WINDOWS.to_string());
        }
        let label = format!("w-{}", self.next_n.max(2));
        self.next_n = self.next_n.max(2) + 1;
        let index = self.add(&label).index;
        if let Some(id) = conn_id {
            self.pending_init.insert(label.clone(), id);
        }
        Ok((label, index))
    }

    /// `window_init` of `label`: who it is and the connection it was opened for (once). A
    /// (re)loading page listens to nothing yet and has not been asked to close: not ready, not
    /// closing.
    pub fn init(&mut self, label: &str) -> WindowInit {
        let index = self.add(label).index;
        if let Some(entry) = self.entries.get_mut(label) {
            entry.ready = false;
            entry.closing = false;
        }
        WindowInit { label: label.to_string(), index, open_conn_id: self.pending_init.remove(label) }
    }

    /// `window_ready`: `label`'s page listens to its window-targeted events now. Returns what it
    /// must be sent again: whatever went to it while it was not listening was lost.
    pub fn mark_ready(&mut self, label: &str) -> ReadyResend {
        let Some(entry) = self.entries.get_mut(label) else { return ReadyResend::default() };
        entry.ready = true;
        let prompts = self.prompts.iter().filter(|p| p.label == label).map(|p| p.payload.clone()).collect();
        let mut jobs: Vec<String> = self.job_windows.iter().filter(|(_, l)| l.as_str() == label).map(|(j, _)| j.clone()).collect();
        jobs.sort();
        let quit = self.quit_pending.as_deref() == Some(label);
        if quit {
            self.quit_pending = None;
        }
        ReadyResend { prompts, jobs, quit }
    }

    /// `window_close_begin`: `label`'s page lets it close. Returns whether it is the last
    /// window, i.e. every window of this process is closing now. Atomic under the registry
    /// lock, so of several windows closing at once exactly the last one to ask learns `true`.
    pub fn begin_close(&mut self, label: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(label) {
            entry.closing = true;
        }
        self.entries.values().all(|e| e.closing)
    }

    /// `window_close_cancel`: the user kept `label` open after all.
    pub fn cancel_close(&mut self, label: &str) {
        if let Some(entry) = self.entries.get_mut(label) {
            entry.closing = false;
        }
    }

    /// `WindowEvent::Focused`. Returns whether anything changed.
    pub fn set_focused(&mut self, label: &str, focused: bool) -> bool {
        if !self.entries.contains_key(label) {
            return false;
        }
        if focused {
            let before = (self.focused.clone(), self.focus_order.first().cloned());
            self.focused = Some(label.to_string());
            self.focus_order.retain(|l| l != label);
            self.focus_order.insert(0, label.to_string());
            before != (self.focused.clone(), Some(label.to_string()))
        } else if self.focused.as_deref() == Some(label) {
            self.focused = None;
            true
        } else {
            false
        }
    }

    /// Every window in routing order: the focused one, then by most recent focus, then by index.
    fn routing_order(&self) -> Vec<&WindowEntry> {
        let mut by_index: Vec<&WindowEntry> = self.entries.values().collect();
        by_index.sort_by_key(|e| e.index);
        let mut out: Vec<&WindowEntry> = Vec::with_capacity(self.entries.len());
        for e in self.focused.iter().chain(self.focus_order.iter()).filter_map(|l| self.entries.get(l)).chain(by_index) {
            if !out.iter().any(|o| o.label == e.label) {
                out.push(e);
            }
        }
        out
    }

    /// The window that receives what the user just asked for (menu actions, Close Window): the
    /// focused one, else the most recently focused, else the one with the smallest index. Used
    /// even while its page loads: an action must not land in a window the user is not looking at.
    pub fn target(&self) -> Option<String> {
        self.routing_order().first().map(|e| e.label.clone())
    }

    /// The window that receives what must not be lost (tunnel prompts, the quit question, the
    /// jobs of a closed window): in [`Self::target`]'s order, the first window that is ready
    /// and not closing; else one that is not closing (it is sent again once it is ready); else
    /// any window.
    pub fn delivery_target(&self) -> Option<String> {
        // `min_by_key` keeps the first of equal keys: the routing order decides among them.
        self.routing_order().into_iter().min_by_key(|e| (e.closing, !e.ready)).map(|e| e.label.clone())
    }

    /// Where the quit question goes ([`Self::delivery_target`]); remembered when that window is
    /// not ready yet, so it is asked again on its `window_ready`.
    pub fn route_quit(&mut self) -> Option<String> {
        let label = self.delivery_target()?;
        if self.entries.get(&label).is_some_and(|e| !e.ready) {
            self.quit_pending = Some(label.clone());
        }
        Some(label)
    }

    /// The window `label` shows `conn` (`(id, name)`) or Welcome (`None`). Returns its new
    /// title when it changed.
    pub fn set_connection(&mut self, label: &str, conn: Option<(String, String)>) -> Option<String> {
        let entry = self.entries.get_mut(label)?;
        let (id, name) = conn.map_or((None, None), |(i, n)| (Some(i), Some(n)));
        let title = window_title(name.as_deref());
        let changed = entry.conn_id != id || entry.conn_name != name || entry.title != title;
        entry.conn_id = id;
        entry.conn_name = name;
        entry.title = title.clone();
        changed.then_some(title)
    }

    /// After `connections:changed`: windows whose connection was renamed get a new title.
    /// `name_of(id)` is `None` for a deleted connection (the window keeps its last name until
    /// the frontend reports what it shows). Returns `(label, title)` of the changed windows.
    pub fn rename_connections(&mut self, name_of: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
        let mut changed = Vec::new();
        for entry in self.entries.values_mut() {
            let Some(id) = entry.conn_id.as_deref() else { continue };
            let Some(name) = name_of(id) else { continue };
            if entry.conn_name.as_deref() != Some(name.as_str()) {
                entry.title = window_title(Some(&name));
                entry.conn_name = Some(name);
                changed.push((entry.label.clone(), entry.title.clone()));
            }
        }
        changed
    }

    /// Show a tunnel prompt in the delivery target and keep it until it is resolved (sent again
    /// when that window becomes ready or handed on when it closes). Returns the window it goes
    /// to (`None`: no window at all).
    pub fn add_prompt(&mut self, payload: PromptPayload) -> Option<String> {
        let label = self.delivery_target()?;
        self.prompts.retain(|p| p.payload.prompt_id() != payload.prompt_id());
        self.prompts.push(PendingPrompt { label: label.clone(), payload });
        Some(label)
    }

    /// `prompt:resolved`: forget the prompt.
    pub fn resolve_prompt(&mut self, prompt_id: &str) -> bool {
        let before = self.prompts.len();
        self.prompts.retain(|p| p.payload.prompt_id() != prompt_id);
        self.prompts.len() != before
    }

    /// `window_claim_job`: `label` started `job_id`.
    pub fn claim_job(&mut self, label: &str, job_id: &str) {
        if self.entries.contains_key(label) {
            self.job_windows.insert(job_id.to_string(), label.to_string());
        }
    }

    /// The job ended: nothing to hand over any more.
    pub fn release_job(&mut self, job_id: &str) {
        self.job_windows.remove(job_id);
    }

    /// `WindowEvent::Destroyed`: forget `label` and hand its prompts and job claims to the next
    /// window ([`Self::delivery_target`]: ready ones first, in focus order).
    pub fn remove(&mut self, label: &str) -> Handover {
        if self.entries.remove(label).is_none() {
            return Handover { last: self.entries.is_empty(), ..Default::default() };
        }
        self.focus_order.retain(|l| l != label);
        if self.focused.as_deref() == Some(label) {
            self.focused = None;
        }
        self.pending_init.remove(label);
        // Nobody was asked: a later quit request asks again.
        if self.quit_pending.as_deref() == Some(label) {
            self.quit_pending = None;
        }
        let next = self.delivery_target();
        let mut handover = Handover { last: self.entries.is_empty(), ..Default::default() };
        let (moved, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.prompts).into_iter().partition(|p| p.label == label);
        self.prompts = kept;
        if let Some(next) = next.as_deref() {
            for mut p in moved {
                p.label = next.to_string();
                self.prompts.push(p.clone());
                handover.prompts.push(p);
            }
            let mut jobs: Vec<String> = self.job_windows.iter().filter(|(_, l)| l.as_str() == label).map(|(j, _)| j.clone()).collect();
            jobs.sort();
            for j in &jobs {
                self.job_windows.insert(j.clone(), next.to_string());
            }
            if !jobs.is_empty() {
                handover.jobs = Some((next.to_string(), jobs));
            }
        } else {
            self.job_windows.retain(|_, l| l != label);
        }
        handover
    }

    /// `windows:changed` / `window_list`, by index.
    pub fn infos(&self) -> Vec<WindowInfo> {
        let mut out: Vec<WindowInfo> = self
            .entries
            .values()
            .map(|e| WindowInfo {
                label: e.label.clone(),
                title: e.title.clone(),
                conn_id: e.conn_id.clone(),
                conn_name: e.conn_name.clone(),
                focused: self.focused.as_deref() == Some(e.label.as_str()),
                index: e.index,
            })
            .collect();
        out.sort_by_key(|w| w.index);
        out
    }
}

/// The managed registry.
pub struct WindowRegistry(Mutex<RegistryState>);

impl Default for WindowRegistry {
    fn default() -> Self {
        WindowRegistry(Mutex::new(RegistryState::with_main()))
    }
}

impl WindowRegistry {
    pub fn with<T>(&self, f: impl FnOnce(&mut RegistryState) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobsClaimed {
    job_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuitRequested {
    jobs: usize,
}

fn registry<R: Runtime>(app: &AppHandle<R>) -> Option<tauri::State<'_, WindowRegistry>> {
    app.try_state::<WindowRegistry>()
}

/// Send `event` to window `label` only.
pub fn emit_to_window<R: Runtime, S: Serialize + Clone>(app: &AppHandle<R>, label: &str, event: &str, payload: S) {
    if let Err(e) = app.emit_to(EventTarget::webview_window(label), event, payload) {
        tracing::debug!(event, label, error = %e, "window event emit failed (window gone?)");
    }
}

/// Send `event` to the focused window (else the most recently focused one).
pub fn emit_to_focused<R: Runtime, S: Serialize + Clone>(app: &AppHandle<R>, event: &str, payload: S) {
    let target = registry(app).and_then(|r| r.with(|s| s.target())).unwrap_or_else(|| MAIN_WINDOW.to_string());
    emit_to_window(app, &target, event, payload);
}

/// Broadcast `windows:changed`.
pub fn emit_changed<R: Runtime>(app: &AppHandle<R>) {
    let Some(reg) = registry(app) else { return };
    let infos = reg.with(|s| s.infos());
    if let Err(e) = app.emit(WINDOWS_CHANGED, infos) {
        tracing::debug!(error = %e, "windows:changed emit failed");
    }
}

fn emit_prompt<R: Runtime>(app: &AppHandle<R>, label: &str, payload: &PromptPayload) {
    match payload {
        PromptPayload::HostKey(p) => emit_to_window(app, label, payload.event(), p.clone()),
        PromptPayload::Password(p) => emit_to_window(app, label, payload.event(), p.clone()),
    }
}

/// Show a tunnel prompt in the focused ready window and remember it until `prompt:resolved`.
pub fn route_prompt<R: Runtime>(app: &AppHandle<R>, payload: PromptPayload) {
    let target = registry(app).and_then(|r| r.with(|s| s.add_prompt(payload.clone())));
    emit_prompt(app, target.as_deref().unwrap_or(MAIN_WINDOW), &payload);
}

/// `prompt:resolved` arrived from core.
pub fn prompt_resolved<R: Runtime>(app: &AppHandle<R>, prompt_id: &str) {
    if let Some(reg) = registry(app) {
        reg.with(|s| s.resolve_prompt(prompt_id));
    }
}

/// `window_ready`: window `label` listens now; send it what it may have missed (its pending
/// prompts, its job claims, the quit question that went to it while it loaded).
pub fn window_ready<R: Runtime>(app: &AppHandle<R>, label: &str) {
    let Some(reg) = registry(app) else { return };
    let resend = reg.with(|s| s.mark_ready(label));
    for p in &resend.prompts {
        emit_prompt(app, label, p);
    }
    if !resend.jobs.is_empty() {
        emit_to_window(app, label, JOBS_CLAIMED, JobsClaimed { job_ids: resend.jobs });
    }
    if resend.quit {
        let jobs = app.try_state::<Arc<BulkDb>>().map(|db| active_jobs(&db)).unwrap_or(0);
        if jobs > 0 && !QUIT_CONFIRMED.load(Ordering::SeqCst) {
            emit_to_window(app, label, QUIT_REQUESTED, QuitRequested { jobs });
        }
    }
}

/// `window_close_begin`: window `label` is closing; is it the last one, and how many jobs run?
pub fn close_begin<R: Runtime>(app: &AppHandle<R>, label: &str) -> CmdResult<CloseCheck> {
    let reg = registry(app).ok_or_else(|| CmdError::internal("window registry missing"))?;
    let last = reg.with(|s| s.begin_close(label));
    let jobs = app.try_state::<Arc<BulkDb>>().map(|db| active_jobs(&db)).unwrap_or(0);
    Ok(CloseCheck { last, jobs })
}

/// `window_close_cancel`: window `label` stays open after all.
pub fn close_cancel<R: Runtime>(app: &AppHandle<R>, label: &str) {
    if let Some(reg) = registry(app) {
        reg.with(|s| s.cancel_close(label));
    }
}

/// A job ended (`job:done`): its claim is no longer handed over.
pub fn job_done<R: Runtime>(app: &AppHandle<R>, job_id: &str) {
    if let Some(reg) = registry(app) {
        reg.with(|s| s.release_job(job_id));
    }
}

/// `connections:changed`: retitle windows whose connection was renamed.
pub fn connections_changed<R: Runtime>(app: &AppHandle<R>, db: &BulkDb) {
    let Some(reg) = registry(app) else { return };
    let changed = reg.with(|s| s.rename_connections(|id| db.config.get(id).map(|c| c.name)));
    if changed.is_empty() {
        return;
    }
    for (label, title) in &changed {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.set_title(title);
        }
    }
    emit_changed(app);
}

/// `Builder::on_window_event`: focus tracking and the hand-over of a closed window.
pub fn on_window_event<R: Runtime>(window: &tauri::Window<R>, event: &WindowEvent) {
    let app = window.app_handle();
    match event {
        WindowEvent::Focused(focused) => {
            let changed = registry(app).is_some_and(|r| r.with(|s| s.set_focused(window.label(), *focused)));
            if changed {
                emit_changed(app);
            }
        }
        WindowEvent::Destroyed => window_destroyed(app, window.label()),
        _ => {}
    }
}

fn window_destroyed<R: Runtime>(app: &AppHandle<R>, label: &str) {
    let Some(reg) = registry(app) else { return };
    let handover = reg.with(|s| s.remove(label));
    for p in &handover.prompts {
        emit_prompt(app, &p.label, &p.payload);
    }
    if let Some((to, job_ids)) = handover.jobs {
        emit_to_window(app, &to, JOBS_CLAIMED, JobsClaimed { job_ids });
    }
    if handover.last {
        // The session ends with its last window: kill tunnels, cancel jobs.
        crate::state::shutdown(app);
    } else {
        emit_changed(app);
    }
}

/// Open a new window (Welcome, or `conn_id`'s workspace through its `window_init`), cascaded
/// from the focused window. Async on purpose: building a window from a synchronous command
/// deadlocks on Windows.
pub async fn create_window<R: Runtime>(app: &AppHandle<R>, conn_id: Option<String>) -> CmdResult<String> {
    let reg = registry(app).ok_or_else(|| CmdError::internal("window registry missing"))?;
    let (label, _) = reg.with(|s| s.reserve(conn_id)).map_err(|m| CmdError::new("INVALID", m, None))?;
    let anchor = reg.with(|s| s.target().filter(|t| *t != label));
    let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .title(APP_TITLE)
        .inner_size(WIDTH, HEIGHT)
        .min_inner_size(MIN_WIDTH, MIN_HEIGHT)
        .focused(true);
    if let Some(from) = anchor.and_then(|l| app.get_webview_window(&l)) {
        if let (Ok(pos), Ok(scale)) = (from.outer_position(), from.scale_factor()) {
            let logical = pos.to_logical::<f64>(scale);
            builder = builder.position(logical.x + CASCADE, logical.y + CASCADE);
        }
    }
    match builder.build() {
        Ok(_) => {
            emit_changed(app);
            Ok(label)
        }
        Err(e) => {
            reg.with(|s| s.remove(&label));
            Err(CmdError::internal(format!("cannot open a window: {e}")))
        }
    }
}

/// Bring window `label` to the front.
pub fn focus_window<R: Runtime>(app: &AppHandle<R>, label: &str) -> CmdResult<()> {
    let w = app.get_webview_window(label).ok_or_else(|| CmdError::not_found(format!("window {label}")))?;
    if w.is_minimized().unwrap_or(false) {
        let _ = w.unminimize();
    }
    w.show()?;
    w.set_focus()?;
    Ok(())
}

/// `window_set_connection`: the window shows `conn` (`(id, name)`) or Welcome.
pub fn set_connection<R: Runtime>(app: &AppHandle<R>, label: &str, conn: Option<(String, String)>) -> CmdResult<()> {
    let reg = registry(app).ok_or_else(|| CmdError::internal("window registry missing"))?;
    if let Some(title) = reg.with(|s| s.set_connection(label, conn)) {
        if let Some(w) = app.get_webview_window(label) {
            w.set_title(&title)?;
        }
        emit_changed(app);
    }
    Ok(())
}

/// The window that Close Window (⇧⌘W) closes.
pub fn close_focused<R: Runtime>(app: &AppHandle<R>) {
    let target = registry(app).and_then(|r| r.with(|s| s.target()));
    if let Some(window) = target.and_then(|l| app.get_webview_window(&l)) {
        // `close` asks the page first (`onCloseRequested`: unsaved queries, last window with jobs).
        if let Err(e) = window.close() {
            tracing::warn!(error = %e, "close window failed");
        }
    }
}

static QUIT_CONFIRMED: AtomicBool = AtomicBool::new(false);

fn active_jobs(db: &BulkDb) -> usize {
    db.job_list().iter().filter(|j| j.state.is_active()).count()
}

/// Whether an exit request must wait for the user: a user/OS quit (no exit code) while jobs
/// run, windows are open and nobody confirmed yet. Pure, for tests.
pub fn must_ask_before_exit(code: Option<i32>, confirmed: bool, windows_open: bool, active_jobs: usize) -> bool {
    code.is_none() && !confirmed && windows_open && active_jobs > 0
}

/// `RunEvent::ExitRequested`: `true` when the exit was held and a window (the focused ready
/// one, [`RegistryState::route_quit`]) asked "실행 중인 작업 {n}개를 취소하고 종료할까요?"
/// (`app:quit-requested`); the caller then calls `api.prevent_exit()`. Otherwise the session
/// ends here (`state::shutdown`).
pub fn hold_exit<R: Runtime>(app: &AppHandle<R>, code: Option<i32>) -> bool {
    let jobs = app.try_state::<Arc<BulkDb>>().map(|db| active_jobs(&db)).unwrap_or(0);
    // Both views must agree: after the last window's `Destroyed` the registry is empty even if
    // the runtime still lists the window, and asking a window that is gone would hang the app.
    let registered = registry(app).is_some_and(|r| r.with(|s| !s.entries.is_empty()));
    let windows_open = registered && !app.webview_windows().is_empty();
    if must_ask_before_exit(code, QUIT_CONFIRMED.load(Ordering::SeqCst), windows_open, jobs) {
        let target = registry(app).and_then(|r| r.with(|s| s.route_quit())).unwrap_or_else(|| MAIN_WINDOW.to_string());
        emit_to_window(app, &target, QUIT_REQUESTED, QuitRequested { jobs });
        return true;
    }
    crate::state::shutdown(app);
    false
}

/// `app_quit_confirmed`: the user accepted cancelling the running jobs; exit (the exit path
/// runs `state::shutdown`, which cancels them and kills tunnels).
pub fn confirm_quit<R: Runtime>(app: &AppHandle<R>) {
    QUIT_CONFIRMED.store(true, Ordering::SeqCst);
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> RegistryState {
        RegistryState::with_main()
    }

    #[test]
    fn labels_count_up_and_indexes_reuse_the_smallest_free() {
        let mut s = reg();
        assert_eq!(s.infos().iter().map(|w| (w.label.as_str(), w.index)).collect::<Vec<_>>(), [("main", 1)]);
        assert_eq!(s.reserve(None).unwrap(), ("w-2".to_string(), 2));
        assert_eq!(s.reserve(Some("c1".into())).unwrap(), ("w-3".to_string(), 3));
        s.remove("w-2");
        // Label never reused, index 2 is free again.
        assert_eq!(s.reserve(None).unwrap(), ("w-4".to_string(), 2));
        s.remove("main");
        assert_eq!(s.reserve(None).unwrap(), ("w-5".to_string(), 1));
        assert_eq!(s.infos().iter().map(|w| w.index).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn at_most_eight_windows() {
        let mut s = reg();
        for _ in 1..MAX_WINDOWS {
            s.reserve(None).unwrap();
        }
        assert_eq!(s.entries.len(), MAX_WINDOWS);
        let err = s.reserve(None).unwrap_err();
        assert!(err.contains("창은 최대 8개까지 열 수 있습니다"), "{err}");
    }

    #[test]
    fn init_hands_out_the_connection_once() {
        let mut s = reg();
        let (label, index) = s.reserve(Some("c1".into())).unwrap();
        let init = s.init(&label);
        assert_eq!(init, WindowInit { label: label.clone(), index, open_conn_id: Some("c1".into()) });
        // A reload of the page starts on Welcome.
        assert_eq!(s.init(&label).open_conn_id, None);
        assert_eq!(s.init(MAIN_WINDOW), WindowInit { label: "main".into(), index: 1, open_conn_id: None });
    }

    #[test]
    fn titles() {
        assert_eq!(window_title(None), "BulkDB");
        assert_eq!(window_title(Some("  ")), "BulkDB");
        assert_eq!(window_title(Some("prod")), "BulkDB — prod");
        let mut s = reg();
        assert_eq!(s.set_connection("main", Some(("c1".into(), "prod".into()))), Some("BulkDB — prod".into()));
        assert_eq!(s.set_connection("main", Some(("c1".into(), "prod".into()))), None, "unchanged");
        let info = &s.infos()[0];
        assert_eq!((info.conn_id.as_deref(), info.conn_name.as_deref(), info.title.as_str()), (Some("c1"), Some("prod"), "BulkDB — prod"));
        // A rename arrives with connections:changed.
        let renamed = s.rename_connections(|id| (id == "c1").then(|| "prod-eu".to_string()));
        assert_eq!(renamed, [("main".to_string(), "BulkDB — prod-eu".to_string())]);
        assert!(s.rename_connections(|_| None).is_empty(), "a deleted connection keeps the last name");
        assert_eq!(s.set_connection("main", None), Some("BulkDB".into()));
        assert_eq!(s.set_connection("nope", None), None);
    }

    #[test]
    fn focus_order_and_targets() {
        let mut s = reg();
        let (w2, _) = s.reserve(None).unwrap();
        let (w3, _) = s.reserve(None).unwrap();
        assert_eq!(s.target().as_deref(), Some("main"), "nothing focused yet: main first");
        assert!(s.set_focused(&w3, true));
        assert!(!s.set_focused(&w3, true), "unchanged");
        assert!(s.set_focused(&w2, true));
        assert_eq!(s.target(), Some(w2.clone()));
        assert_eq!(s.infos().iter().filter(|w| w.focused).map(|w| w.label.as_str()).collect::<Vec<_>>(), [w2.as_str()]);
        // Another app is active: the most recently focused window is still the target.
        assert!(s.set_focused(&w2, false));
        assert!(!s.set_focused(&w3, false), "w3 had no focus");
        assert_eq!(s.target(), Some(w2.clone()));
        assert!(s.infos().iter().all(|w| !w.focused));
        assert_eq!(s.focus_order, [w2.clone(), w3.clone(), "main".to_string()]);
        assert!(!s.set_focused("unknown", true));
    }

    /// Menu actions go to the focused window only (`emit_to_focused` uses `target`).
    #[test]
    fn menu_routes_to_the_focused_window() {
        let mut s = reg();
        let (w2, _) = s.reserve(None).unwrap();
        s.set_focused(&w2, true);
        assert_eq!(s.target().as_deref(), Some(w2.as_str()));
        s.set_focused(MAIN_WINDOW, true);
        assert_eq!(s.target().as_deref(), Some(MAIN_WINDOW));
    }

    fn host_key(id: &str) -> PromptPayload {
        PromptPayload::HostKey(HostKeyPrompt {
            prompt_id: id.into(),
            host: "bastion".into(),
            fingerprint: "SHA256:x".into(),
            key_type: None,
            message: String::new(),
            conn_name: Some("prod".into()),
            job_label: None,
        })
    }

    #[test]
    fn prompts_follow_focus_and_move_when_their_window_closes() {
        let mut s = reg();
        let (w2, _) = s.reserve(None).unwrap();
        s.set_focused(MAIN_WINDOW, true);
        s.set_focused(&w2, true);
        let to = s.add_prompt(host_key("p1"));
        assert_eq!(to, Some(w2.clone()));
        let password = PromptPayload::Password(SshPasswordPrompt {
            prompt_id: "p2".into(),
            host: "bastion".into(),
            user: "deploy".into(),
            kind: "password".into(),
            conn_name: None,
            job_label: None,
        });
        assert_eq!(password.event(), "tunnel:password");
        s.set_focused(MAIN_WINDOW, true);
        assert_eq!(s.add_prompt(password).as_deref(), Some(MAIN_WINDOW));
        s.set_focused(&w2, true);
        let h = s.remove(&w2);
        assert_eq!(h.prompts.len(), 1);
        assert_eq!((h.prompts[0].label.as_str(), h.prompts[0].payload.event()), (MAIN_WINDOW, "tunnel:hostkey"));
        assert_eq!(s.prompts[0].label, MAIN_WINDOW);
        assert!(!h.last);
        assert!(s.resolve_prompt("p1"));
        assert!(!s.resolve_prompt("p1"));
        assert!(s.resolve_prompt("p2"));
        let h = s.remove(MAIN_WINDOW);
        assert!(h.last && h.prompts.is_empty());
    }

    #[test]
    fn job_claims_are_handed_to_the_next_window() {
        let mut s = reg();
        let (w2, _) = s.reserve(None).unwrap();
        let (w3, _) = s.reserve(None).unwrap();
        s.set_focused(&w3, true);
        s.set_focused(&w2, true);
        s.claim_job(&w2, "j1");
        s.claim_job(&w2, "j2");
        s.claim_job(MAIN_WINDOW, "j3");
        s.claim_job("gone", "j4");
        s.release_job("j2");
        let h = s.remove(&w2);
        assert_eq!(h.jobs, Some((w3.clone(), vec!["j1".to_string()])), "the next window in focus order");
        assert_eq!(s.job_windows.get("j1"), Some(&w3));
        assert_eq!(s.job_windows.get("j3").map(String::as_str), Some(MAIN_WINDOW));
        assert!(!s.job_windows.contains_key("j4"));
        // A window without claims hands nothing over; main's claim goes to w3.
        let (w4, _) = s.reserve(None).unwrap();
        assert_eq!(s.remove(&w4).jobs, None);
        assert_eq!(s.remove(MAIN_WINDOW).jobs, Some((w3.clone(), vec!["j3".to_string()])));
        // Removing an unknown window changes nothing.
        assert_eq!(s.remove("nope"), Handover::default());
        // The last window takes nothing with it: no receiver, claims dropped.
        let h = s.remove(&w3);
        assert!(h.last && h.jobs.is_none() && s.job_windows.is_empty());
        assert!(s.remove("nope").last, "still no window");
    }

    fn prompt_ids(prompts: &[PromptPayload]) -> Vec<&str> {
        prompts.iter().map(PromptPayload::prompt_id).collect()
    }

    /// ⇧⌘N while a job connects over SSH: the new window has the OS focus long before its page
    /// listens, so the prompt goes to a window that listens; menu actions still follow focus.
    #[test]
    fn prompts_skip_a_window_that_is_not_listening_yet() {
        let mut s = reg();
        assert_eq!(s.mark_ready(MAIN_WINDOW), ReadyResend::default());
        s.set_focused(MAIN_WINDOW, true);
        let (w2, _) = s.reserve(None).unwrap();
        s.set_focused(&w2, true);
        assert_eq!(s.target(), Some(w2.clone()), "menu actions follow the focus");
        assert_eq!(s.add_prompt(host_key("p1")).as_deref(), Some(MAIN_WINDOW), "the prompt goes where it is heard");
        // w2's page loaded and listens: nothing was routed to it, new prompts go there.
        assert_eq!(s.init(&w2).label, w2);
        assert_eq!(s.mark_ready(&w2), ReadyResend::default());
        assert_eq!(s.add_prompt(host_key("p2")), Some(w2.clone()));
        // w2 reloads: p2's event reached the old page only. Until the new page is ready prompts
        // go to main; p2 is sent again when it is.
        s.init(&w2);
        assert_eq!(s.add_prompt(host_key("p3")).as_deref(), Some(MAIN_WINDOW));
        assert_eq!(prompt_ids(&s.mark_ready(&w2).prompts), ["p2"]);
        assert_eq!(prompt_ids(&s.mark_ready(MAIN_WINDOW).prompts), ["p1", "p3"]);
    }

    /// No window listens yet (startup, or the only window reloads): the prompt is kept for the
    /// focused one and sent again when its page is ready, once per `window_ready`.
    #[test]
    fn with_no_window_ready_a_prompt_waits_for_its_window() {
        let mut s = reg();
        assert_eq!(s.add_prompt(host_key("p1")).as_deref(), Some(MAIN_WINDOW));
        let (w2, _) = s.reserve(None).unwrap();
        s.set_focused(&w2, true);
        assert_eq!(s.add_prompt(host_key("p2")), Some(w2.clone()), "focus order among unready windows");
        assert_eq!(prompt_ids(&s.mark_ready(&w2).prompts), ["p2"]);
        assert_eq!(prompt_ids(&s.mark_ready(MAIN_WINDOW).prompts), ["p1"]);
        // A resolved prompt is never sent again.
        assert!(s.resolve_prompt("p1"));
        s.init(MAIN_WINDOW);
        assert!(s.mark_ready(MAIN_WINDOW).prompts.is_empty());
        assert_eq!(s.mark_ready("gone"), ReadyResend::default());
    }

    /// `jobs:claimed` and `app:quit-requested` sent to a loading window are not lost either.
    #[test]
    fn job_claims_and_the_quit_question_reach_a_window_once_it_listens() {
        let mut s = reg();
        s.mark_ready(MAIN_WINDOW);
        s.claim_job(MAIN_WINDOW, "j1");
        let (w2, _) = s.reserve(None).unwrap();
        s.set_focused(&w2, true);
        assert_eq!(s.route_quit().as_deref(), Some(MAIN_WINDOW), "the ready window asks");
        assert_eq!(s.quit_pending, None);
        // main closes: its claim goes to w2, whose page is still loading.
        assert_eq!(s.remove(MAIN_WINDOW).jobs, Some((w2.clone(), vec!["j1".to_string()])));
        assert_eq!(s.route_quit(), Some(w2.clone()));
        assert_eq!(s.quit_pending, Some(w2.clone()));
        assert_eq!(s.mark_ready(&w2), ReadyResend { prompts: vec![], jobs: vec!["j1".into()], quit: true });
        assert!(!s.mark_ready(&w2).quit, "asked once");
        // A reloaded page lost its claims: they come again.
        s.init(&w2);
        assert_eq!(s.mark_ready(&w2).jobs, ["j1"]);
        // The quit question pending for a window that closes before it listens is dropped.
        let (w3, _) = s.reserve(None).unwrap();
        s.set_focused(&w3, true);
        s.init(&w2);
        assert_eq!(s.route_quit(), Some(w3.clone()));
        s.remove(&w3);
        assert_eq!(s.quit_pending, None);
    }

    /// The Windows taskbar's "Close all windows": every page accepts its close before any
    /// `Destroyed` arrives. The registry decides, so exactly the last one asks.
    #[test]
    fn the_last_window_to_close_is_decided_atomically() {
        let mut s = reg();
        let (w2, _) = s.reserve(None).unwrap();
        assert!(!s.begin_close(MAIN_WINDOW), "w2 is still open: main closes without asking");
        assert!(s.begin_close(&w2), "every window is closing: w2 is the last and asks");
        // The user keeps w2: main still goes, and w2 is then the last window.
        s.cancel_close(&w2);
        assert!(!s.remove(MAIN_WINDOW).last);
        assert!(s.begin_close(&w2));
        s.cancel_close(&w2);
        // One window closing while another stays open is not the last one.
        let (w3, _) = s.reserve(None).unwrap();
        assert!(!s.begin_close(&w3));
        // A reload counts as kept open.
        s.init(&w3);
        assert!(!s.entries[&w3].closing);
        assert!(!s.begin_close(&w2));
        assert!(s.begin_close(&w3));
        // Unknown labels change nothing.
        s.cancel_close("gone");
        assert!(s.begin_close("gone"));
    }

    /// A ready window that is closing is the last choice for a prompt (it is going away).
    #[test]
    fn closing_windows_are_the_last_delivery_choice() {
        let mut s = reg();
        let (w2, _) = s.reserve(None).unwrap();
        s.mark_ready(MAIN_WINDOW);
        s.set_focused(MAIN_WINDOW, true);
        s.begin_close(MAIN_WINDOW);
        assert_eq!(s.delivery_target(), Some(w2.clone()), "not ready yet, but staying");
        s.begin_close(&w2);
        assert_eq!(s.delivery_target().as_deref(), Some(MAIN_WINDOW), "all closing: the ready one");
    }

    #[test]
    fn exit_asks_only_for_a_user_quit_with_running_jobs() {
        assert!(must_ask_before_exit(None, false, true, 2));
        assert!(!must_ask_before_exit(Some(0), false, true, 2), "app.exit after the confirmation");
        assert!(!must_ask_before_exit(None, true, true, 2));
        assert!(!must_ask_before_exit(None, false, false, 2), "the last window already asked");
        assert!(!must_ask_before_exit(None, false, true, 0));
    }
}
