//! GUI state snapshot: everything the views draw, captured off the UI thread.
//!
//! A poller task refreshes the snapshot every ~250 ms (and immediately when the event log
//! signals activity) and requests a repaint only when something changed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use siple_core::App;
use siple_core::audio::{AudioStatus, DeviceInfo, DeviceList, MicPermission, mic_permission};
use siple_core::state::EffectiveDevices;
use siple_core::update::{InstallStatus, SourceName, UpdateAction};
use siple_sip::problem::{Context, Problem, classify};
use siple_sip::{AccountStatus, CallInfo, CallState, RegState};
use tokio::sync::Notify;

/// Lines kept in the log pane.
pub const LOG_LINES: usize = 200;
/// Poll interval of the snapshot task.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Device list refresh interval (rescan every 5 s).
pub const DEVICE_REFRESH: Duration = Duration::from_secs(5);
/// How long an ended call stays visible.
pub const ENDED_CALL_LINGER_SECS: i64 = 8;

/// Everything the views draw.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub accounts: Vec<AccountStatus>,
    /// BLF state of every monitored extension, per account in config order.
    pub blf: Vec<siple_sip::BlfStatus>,
    pub calls: Vec<CallInfo>,
    /// Transfer sessions: the last finished ones, then the live ones (the transfer panel).
    pub transfers: Vec<siple_sip::TransferSession>,
    /// Id of the un-held device-bound call that owns the microphone and speaker.
    pub active_line: Option<String>,
    pub devices: DeviceList,
    pub effective: EffectiveDevices,
    /// `None` when the app runs without an audio layer.
    pub audio: Option<AudioStatus>,
    pub mic: MicPermission,
    pub config_error: Option<ConfigErrorView>,
    /// The `[ui]` table of the loaded config (BLF view and click behaviour, transfer Enter, ...).
    pub ui: siple_core::config::UiConfig,
    /// The effective config (Settings shows and edits it through `config.edit`).
    pub cfg: Arc<siple_core::config::Config>,
    /// `config.view` `hash`: the `base_hash` of the next `config.edit`.
    pub config_hash: String,
    /// `config.view` `provenance`: where every key comes from (`file` / `profile` / `default`).
    pub provenance: serde_json::Value,
    /// The config file on disk does not parse (Settings is read-only until it does).
    pub config_unparsable: bool,
    /// Only filled while the log pane is open.
    pub logs: Vec<String>,
    /// The update check's result (`app.update_status`).
    pub update: siple_core::update::UpdateStatus,
    /// The release version the user skipped in the update pill.
    pub update_skipped: Option<String>,
}

/// An update the header offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOffer<'a> {
    /// The new version; `None` for the Microsoft Store, which does not tell one.
    pub version: Option<&'a str>,
    /// What the click does: install in place, open the release page or open the store page.
    pub action: UpdateAction,
    /// The in-place install (direct builds): whether it is possible and how far it is. Present
    /// for `Install`, and for the release-page fallback it says why there is no install.
    pub install: Option<&'a InstallStatus>,
    /// Where the check looks: decides the wording of the store button.
    pub source: SourceName,
    /// The release page (`OpenReleasePage`). Store pages are never taken from here: the GUI opens
    /// the fixed link of the platform.
    pub release_url: Option<&'a str>,
    /// The Microsoft Store marked the update mandatory (there is no "Not now").
    pub mandatory: bool,
}

/// The sentence for an `install.reason` code (why this copy does not install in place).
pub fn install_reason_text(reason: &str) -> &'static str {
    match reason {
        "disabled" => "Installing updates is turned off in Settings",
        "headless" => "This instance has no window to restart into",
        "release_not_signed" => "This release has no signed checksum list",
        "no_asset" => "This release has no download for this system",
        "unsupported_platform" => "In-app updates are not available on this system",
        "unsigned_build" => "This is not an official release build",
        "not_bundle" => "Siple is not running from its app bundle or install folder",
        "not_writable" => "Siple cannot write to its install location without administrator rights",
        "not_owner" => "Siple's files belong to another user",
        "root" => "Siple does not update itself when run as administrator",
        "package_manager" => "Siple was installed with a package manager; update it there",
        "container" => "Siple runs in a container; update the image instead",
        "sandboxed" => "Siple runs in a sandbox that manages its updates",
        "system_install" => "Siple is installed system-wide; update it with the system's tools",
        "translocated" => {
            "macOS is running Siple from a quarantined location; move it to Applications"
        }
        "read_only_volume" => {
            "Siple runs from a disk image or read-only volume; copy it to Applications"
        }
        "in_use" => "Siple's files are in use and cannot be replaced",
        _ => "Siple cannot install this update here",
    }
}

impl UpdateOffer<'_> {
    /// Text of the button that opens the store page.
    pub fn store_button(&self) -> &'static str {
        match self.source {
            SourceName::MicrosoftStore => "Open Microsoft Store",
            _ => "Open App Store",
        }
    }
}

impl Snapshot {
    /// A call (or one that is still ringing, dialing or on hold) exists: the update never
    /// restarts Siple then.
    pub fn call_active(&self) -> bool {
        self.calls.iter().any(|c| c.state != CallState::Ended)
    }

    /// The update the header offers. `None` when up to date, when the check is off or has no
    /// source, and when the user skipped exactly that version (a newer one is offered again).
    pub fn update_offer(&self) -> Option<UpdateOffer<'_>> {
        let u = &self.update;
        if !(u.enabled && u.available) {
            return None;
        }
        let version = u.latest.as_deref();
        if version.is_some() && self.update_skipped.as_deref() == version {
            return None;
        }
        let action = match u.action {
            UpdateAction::OpenStore => UpdateAction::OpenStore,
            UpdateAction::OpenReleasePage if u.release_url.is_some() => {
                UpdateAction::OpenReleasePage
            }
            UpdateAction::Install if version.is_some() && u.install.is_some() => {
                UpdateAction::Install
            }
            _ => return None,
        };
        Some(UpdateOffer {
            version,
            action,
            install: u.install.as_ref(),
            source: u.source,
            release_url: u.release_url.as_deref(),
            mandatory: u.mandatory == Some(true),
        })
    }
}

/// The last rejected config, as the views show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrorView {
    /// When it was rejected (RFC 3339).
    pub at: String,
    /// What was wrong.
    pub errors: Vec<String>,
}

/// A transient message in the footer: `text` is what is shown, `detail` (the server's raw answer)
/// goes to a hover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub detail: Option<String>,
}

impl Notice {
    /// A message without a details line.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            detail: None,
        }
    }
}

/// State shared between the UI thread and the tokio tasks.
pub struct Shared {
    snapshot: Mutex<Arc<Snapshot>>,
    /// Set by the UI while the log pane is open.
    pub want_logs: AtomicBool,
    /// Wakes the poller early (events, user actions).
    pub wake: Notify,
    notice: Mutex<Option<(Notice, Instant)>>,
    /// Answers of in-process control-API calls, waiting for the UI thread.
    replies: Mutex<Vec<crate::ui::rpc::RpcReply>>,
    /// The egui context (set by the first frame), to repaint when a reply arrives.
    pub ctx: std::sync::OnceLock<eframe::egui::Context>,
    pub config_path: PathBuf,
    pub log_dir: Option<PathBuf>,
    /// Panel layout shared between the UI thread and the control API (`ui.state`/`ui.set_panels`).
    panels: Mutex<PanelLink>,
}

/// Lines rail / extensions panel state as the control API sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PanelLink {
    /// The Lines rail is collapsed.
    pub lines_collapsed: bool,
    /// The extensions panel is showing.
    pub extensions_open: bool,
    /// A `ui.set_panels` request the UI thread has not applied yet.
    pub pending_lines: Option<bool>,
    /// Same, for the extensions panel.
    pub pending_ext: Option<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn new(initial: Snapshot, config_path: PathBuf, log_dir: Option<PathBuf>) -> Self {
        Self {
            snapshot: Mutex::new(Arc::new(initial)),
            want_logs: AtomicBool::new(false),
            wake: Notify::new(),
            notice: Mutex::new(None),
            replies: Mutex::new(Vec::new()),
            ctx: std::sync::OnceLock::new(),
            config_path,
            log_dir,
            panels: Mutex::new(PanelLink {
                lines_collapsed: true,
                ..PanelLink::default()
            }),
        }
    }

    /// Control API: records a `ui.set_panels` request (visible to `ui.state` at once) and returns
    /// the resulting state.
    pub fn request_panels(&self, lines: Option<bool>, ext: Option<bool>) -> PanelLink {
        let mut p = lock(&self.panels);
        if let Some(v) = lines {
            p.lines_collapsed = v;
            p.pending_lines = Some(v);
        }
        if let Some(v) = ext {
            p.extensions_open = v;
            p.pending_ext = Some(v);
        }
        *p
    }

    /// Control API: the panel state.
    pub fn panel_link(&self) -> PanelLink {
        *lock(&self.panels)
    }

    /// UI thread: takes the requests to apply.
    pub fn take_panel_requests(&self) -> (Option<bool>, Option<bool>) {
        let mut p = lock(&self.panels);
        (p.pending_lines.take(), p.pending_ext.take())
    }

    /// UI thread: publishes the effective state (a request still queued wins).
    pub fn publish_panels(&self, lines_collapsed: bool, extensions_open: bool) {
        let mut p = lock(&self.panels);
        if p.pending_lines.is_none() {
            p.lines_collapsed = lines_collapsed;
        }
        if p.pending_ext.is_none() {
            p.extensions_open = extensions_open;
        }
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        lock(&self.snapshot).clone()
    }

    fn store(&self, s: Snapshot) {
        *lock(&self.snapshot) = Arc::new(s);
    }

    /// Queues a reply for the UI thread and wakes it.
    pub fn push_reply(&self, reply: crate::ui::rpc::RpcReply) {
        lock(&self.replies).push(reply);
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    /// Replies that arrived since the last call.
    pub fn take_replies(&self) -> Vec<crate::ui::rpc::RpcReply> {
        std::mem::take(&mut *lock(&self.replies))
    }

    /// Records a transient message for the UI (action failures).
    pub fn set_notice(&self, msg: impl Into<String>) {
        self.set_notice_full(Notice::plain(msg));
    }

    /// Like [`Shared::set_notice`], with a details line for the hover.
    pub fn set_notice_full(&self, notice: Notice) {
        *lock(&self.notice) = Some((notice, Instant::now()));
    }

    /// The current message, if it is younger than `ttl`.
    pub fn notice(&self, ttl: Duration) -> Option<Notice> {
        lock(&self.notice)
            .as_ref()
            .filter(|(_, at)| at.elapsed() < ttl)
            .map(|(m, _)| m.clone())
    }
}

/// Captures a snapshot. `devices` is passed in because enumerating blocks on the audio thread.
pub fn capture(app: &App, devices: DeviceList, with_logs: bool) -> Snapshot {
    let stack = app.stack();
    let logs = if with_logs {
        app.logging().map(|l| l.tail(LOG_LINES)).unwrap_or_default()
    } else {
        Vec::new()
    };
    let cfg = app.config();
    let (config_hash, provenance) = config_view_parts(app, &cfg);
    let config_unparsable = app.last_config_error().is_some()
        && std::fs::read_to_string(app.config_path())
            .map(|t| siple_core::config::Config::parse(&t).is_err())
            .unwrap_or(false);
    Snapshot {
        config_hash,
        provenance,
        config_unparsable,
        ui: cfg.ui.clone(),
        cfg,
        accounts: stack.accounts_status(),
        blf: stack.blf_status(None),
        calls: stack.calls(),
        transfers: stack.transfers(),
        active_line: stack.active_line(),
        devices,
        effective: app.effective_devices(),
        audio: app.audio().map(|a| a.status()),
        mic: mic_permission(),
        config_error: app.last_config_error().map(|e| ConfigErrorView {
            at: e.at,
            errors: e.errors,
        }),
        logs,
        update: app.update_status(),
        update_skipped: app.skipped_update(),
    }
}

/// `(hash, provenance)` of `config.view`. The hash of the file on disk is cheap and read every
/// time; the provenance only changes with the file or the accepted config, so it is rebuilt
/// (a full `config.view`) only then.
fn config_view_parts(
    app: &App,
    cfg: &Arc<siple_core::config::Config>,
) -> (String, serde_json::Value) {
    use siple_core::config_watch::{content_hash, hash_hex};
    static CACHE: Mutex<Option<(usize, String, serde_json::Value)>> = Mutex::new(None);
    let disk = std::fs::read(app.config_path())
        .ok()
        .map(|b| hash_hex(&content_hash(&b)));
    let ptr = Arc::as_ptr(cfg) as usize;
    if let Some(h) = &disk
        && let Some((p, ch, prov)) = lock(&CACHE).as_ref()
        && *p == ptr
        && ch == h
    {
        return (h.clone(), prov.clone());
    }
    let mut view = app.config_view();
    let hash = disk.unwrap_or_else(|| view["hash"].as_str().unwrap_or_default().to_string());
    let prov = view["provenance"].take();
    *lock(&CACHE) = Some((ptr, hash.clone(), prov.clone()));
    (hash, prov)
}

/// What the views draw from a snapshot, as one comparable string (the large `cfg` and
/// `provenance` are represented by their pointer and the file hash).
fn change_key(snap: &Snapshot) -> String {
    let Snapshot {
        accounts,
        blf,
        calls,
        transfers,
        active_line,
        devices,
        effective,
        audio,
        mic,
        config_error,
        ui,
        cfg,
        config_hash,
        provenance: _,
        config_unparsable,
        logs,
        update,
        update_skipped,
    } = snap;
    format!(
        "{accounts:?}{blf:?}{calls:?}{transfers:?}{active_line:?}{devices:?}{effective:?}{audio:?}\
         {mic:?}{config_error:?}{ui:?}{:p}{config_hash}{config_unparsable}{logs:?}{update:?}{update_skipped:?}",
        Arc::as_ptr(cfg)
    )
}

/// Enumerates devices on a blocking thread.
pub async fn list_devices(app: &Arc<App>) -> DeviceList {
    let Some(audio) = app.audio().cloned() else {
        return DeviceList::default();
    };
    tokio::task::spawn_blocking(move || audio.list())
        .await
        .unwrap_or_default()
}

/// Poller: refresh the snapshot until aborted. `repaint` is called when the content changed.
pub async fn poll_loop(app: Arc<App>, shared: Arc<Shared>, repaint: impl Fn()) {
    let mut devices = shared.snapshot().devices.clone();
    let mut devices_at = Instant::now();
    let mut last_generation = shared.snapshot().audio.as_ref().map(|a| a.generation);
    let mut last_key = change_key(&shared.snapshot());
    loop {
        let _ = tokio::time::timeout(POLL_INTERVAL, shared.wake.notified()).await;
        let generation = app.audio().map(|a| a.status().generation);
        if devices_at.elapsed() >= DEVICE_REFRESH || generation != last_generation {
            devices = list_devices(&app).await;
            devices_at = Instant::now();
            last_generation = generation;
        }
        let snap = capture(
            &app,
            devices.clone(),
            shared.want_logs.load(Ordering::Relaxed),
        );
        let key = change_key(&snap);
        if key != last_key {
            last_key = key;
            shared.store(snap);
            repaint();
        }
    }
}

// ---- pure helpers (unit-tested) ----------------------------------------------------------

/// `m:ss`, or `h:mm:ss` from one hour on. Negative values clamp to zero.
pub fn format_duration(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Elapsed seconds of a call: ring time before it is answered, talk time afterwards, frozen
/// at the end time for ended calls.
pub fn call_elapsed_secs(c: &CallInfo, now: DateTime<Utc>) -> i64 {
    let start = c.answered_at.unwrap_or(c.created_at);
    let end = c.ended_at.unwrap_or(now);
    (end - start).num_seconds().max(0)
}

/// Coarse registration class used for the status dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegClass {
    Registered,
    Registering,
    Failed,
    /// Disabled, or enabled but not registered (peer mode / not started).
    Inactive,
}

pub fn reg_class(s: &AccountStatus) -> RegClass {
    if !s.enabled {
        return RegClass::Inactive;
    }
    match s.registration {
        RegState::Registered { .. } => RegClass::Registered,
        RegState::Registering | RegState::WaitingForNetwork => RegClass::Registering,
        RegState::Failed { .. } => RegClass::Failed,
        RegState::Disabled | RegState::Unregistered => RegClass::Inactive,
    }
}

/// Short text for the registration state.
pub fn reg_label(s: &AccountStatus) -> String {
    if !s.enabled {
        return "disabled".into();
    }
    match &s.registration {
        RegState::Registered { .. } => "registered".into(),
        RegState::Registering => "registering".into(),
        RegState::WaitingForNetwork => "waiting for network".into(),
        // An unclassified failure's title is the raw text: keep "failed" in it, so the state is
        // not conveyed by the lamp colour alone.
        RegState::Failed { .. } => reg_failure(s).map_or_else(
            || "failed".into(),
            |f| match f.problem {
                Problem::Other => format!("failed: {}", f.title),
                _ => f.title,
            },
        ),
        RegState::Disabled => "disabled".into(),
        RegState::Unregistered => "not registered".into(),
    }
}

/// Registration timing comes from successful REGISTER responses, never from UI observation.
pub fn reg_tooltip(s: &AccountStatus, now: DateTime<Utc>) -> String {
    if s.enabled
        && let RegState::Registered {
            registered_at,
            last_renewed_at,
            expires_at,
            ..
        } = &s.registration
    {
        let local = |at: &DateTime<Utc>| {
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        };
        let remaining = (*expires_at - now).num_seconds().max(0);
        format!(
            "Registered: {}\nLast renewal: {}\nExpires: {} ({} remaining)",
            local(registered_at),
            local(last_renewed_at),
            local(expires_at),
            format_duration(remaining)
        )
    } else if let Some(f) = reg_failure(s) {
        f.tooltip()
    } else {
        reg_label(s)
    }
}

/// `403 Forbidden`; just the text for a local failure (code 0). Trimmed.
pub fn sip_detail(code: u16, reason: &str) -> String {
    if code == 0 {
        reason.trim().to_string()
    } else {
        format!("{code} {reason}").trim().to_string()
    }
}

/// A failure as the views show it: the catalog title and hint (rendered from the in-process
/// `Problem`, never from API strings) and the server's raw answer as a details line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureView {
    pub problem: Problem,
    /// The catalog title; for `Other` the raw text (a vague title would hide what happened).
    pub title: String,
    pub hint: &'static str,
    /// The raw answer (`403 Forbidden`); empty for `Other`, whose title already is the raw text.
    pub detail: String,
}

impl FailureView {
    /// Title, hint and details as plain lines (tooltips, BLF keys).
    pub fn lines(&self) -> Vec<String> {
        let mut v = vec![self.title.clone(), self.hint.to_string()];
        if !self.detail.is_empty() {
            v.push(format!("Details: {}", self.detail));
        }
        v
    }

    fn tooltip(&self) -> String {
        self.lines().join("\n")
    }
}

/// The view of `p` for a failure whose raw answer is `code` / `reason`.
pub fn failure_view(p: Problem, code: u16, reason: &str) -> FailureView {
    let raw = sip_detail(code, reason);
    if p == Problem::Other {
        FailureView {
            problem: p,
            title: if raw.is_empty() {
                p.title().into()
            } else {
                raw
            },
            hint: p.hint(),
            detail: String::new(),
        }
    } else {
        FailureView {
            problem: p,
            title: p.title().into(),
            hint: p.hint(),
            detail: raw,
        }
    }
}

fn next_attempt(at: DateTime<Utc>) -> String {
    format!(
        "next attempt {}",
        at.with_timezone(&chrono::Local).format("%H:%M:%S")
    )
}

/// The failure of an enabled account whose registration failed; the details also say when the
/// next attempt happens.
pub fn reg_failure(s: &AccountStatus) -> Option<FailureView> {
    if !s.enabled {
        return None;
    }
    let RegState::Failed {
        code,
        reason,
        retry_at,
    } = &s.registration
    else {
        return None;
    };
    let mut f = failure_view(s.registration.problem()?, *code, reason);
    let next = next_attempt(*retry_at);
    f.detail = if f.detail.is_empty() {
        next
    } else {
        format!("{} \u{b7} {next}", f.detail)
    };
    Some(f)
}

/// The failure behind a BLF key whose subscription failed or is being retried.
pub fn blf_failure(b: &siple_sip::BlfStatus) -> Option<FailureView> {
    use siple_sip::SubscriptionPhase::{Failed, Retrying};
    let sub = &b.subscription;
    if !matches!(sub.phase, Failed | Retrying) {
        return None;
    }
    let mut f = failure_view(
        sub.problem()?,
        sub.last_code.unwrap_or(0),
        sub.last_reason.as_deref().unwrap_or(""),
    );
    if sub.phase == Retrying
        && let Some(at) = sub.retry_at
    {
        let next = next_attempt(at);
        f.detail = if f.detail.is_empty() {
            next
        } else {
            format!("{}, {next}", f.detail)
        };
    }
    Some(f)
}

/// The failure that ended a call; `None` for a normal end.
pub fn end_failure(e: &siple_sip::CallEnd) -> Option<FailureView> {
    Some(failure_view(e.problem()?, e.code, &e.reason))
}

/// The failure of a transfer session: its last error, else a failed `refer` NOTIFY.
pub fn transfer_failure(t: &siple_sip::TransferSession) -> Option<FailureView> {
    if let Some(e) = &t.last_error
        && let Some(p) = e.problem()
    {
        return Some(failure_view(p, e.code, &e.reason));
    }
    let n = t.last_notify.as_ref().filter(|n| n.code >= 300)?;
    let p = classify(Context::Call, n.code, &n.reason).unwrap_or(Problem::Other);
    Some(failure_view(p, n.code, &n.reason))
}

/// The footer notice of a failed action: `"Dial failed: Busy. The other side is on another call;
/// try again later."` with the raw error as details. Without a named problem the raw text is shown
/// as it always was.
pub fn action_notice(what: &str, problem: Option<Problem>, raw: &str) -> Notice {
    match problem {
        Some(p) if p != Problem::Other => Notice {
            text: format!("{what} failed: {}. {}", p.title(), p.hint()),
            detail: Some(raw.to_string()),
        },
        _ => Notice::plain(format!("{what} failed: {raw}")),
    }
}

/// Label of a call state.
pub fn call_state_label(s: CallState) -> &'static str {
    match s {
        CallState::Dialing => "Dialing",
        CallState::Ringing => "Ringing",
        CallState::Early => "Ringing (remote)",
        CallState::Active => "Active",
        CallState::Held => "On hold",
        CallState::RemoteHeld => "Held by remote",
        CallState::Ended => "Ended",
    }
}

/// Text of the MWI badge: `2 new / 5 old`, `no messages`.
pub fn mwi_label(m: &siple_sip::MwiSummary) -> String {
    if m.new == 0 && m.old == 0 {
        "no messages".into()
    } else {
        format!("{} new / {} old", m.new, m.old)
    }
}

/// Label of a BLF cell: the configured label, else the URI.
pub fn blf_title(b: &siple_sip::BlfStatus) -> &str {
    if b.label.is_empty() { &b.uri } else { &b.label }
}

/// Chip text of a BLF state.
pub fn blf_state_label(s: siple_sip::BlfState) -> &'static str {
    s.name()
}

/// State text of a BLF key: the state name; a failed subscription reads `Failed` (the problem and
/// the code are in the tooltip, the accessible value and the banner), so Busy and Failed never
/// differ by colour alone.
pub fn blf_state_text(b: &siple_sip::BlfStatus) -> String {
    if b.state == siple_sip::BlfState::Failed {
        "Failed".into()
    } else {
        let s = blf_state_label(b.state);
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default()
    }
}

/// Extra tooltip lines about the subscription health of a BLF key: the problem (title, hint and
/// the server's answer with the next attempt) and the stale marker.
pub fn blf_health_lines(b: &siple_sip::BlfStatus) -> Vec<String> {
    let mut v = blf_failure(b).map(|f| f.lines()).unwrap_or_default();
    if b.stale {
        v.push("Stale: this state may be out of date".into());
    }
    v
}

/// `14:02:11`, or `2026-11-19 14:02:11` when the local date is not `today`.
fn local_time_text(at: DateTime<Utc>, today: chrono::NaiveDate) -> String {
    let local = at.with_timezone(&chrono::Local);
    if local.date_naive() == today {
        local.format("%H:%M:%S").to_string()
    } else {
        local.format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// Tooltip lines with the registration times of a registered account, shown only with
/// developer tools on. Absolute local times: the GUI only repaints on snapshot changes, so
/// relative ones would go stale while hovering.
pub fn reg_time_lines(a: &AccountStatus, developer_tools: bool) -> Vec<String> {
    if !developer_tools {
        return Vec::new();
    }
    reg_time_lines_on(a, chrono::Local::now().date_naive())
}

fn reg_time_lines_on(a: &AccountStatus, today: chrono::NaiveDate) -> Vec<String> {
    // A disabled account can still report Registered until its unREGISTER completes.
    if !a.enabled {
        return Vec::new();
    }
    match &a.registration {
        RegState::Registered {
            registered_at,
            last_renewed_at,
            expires_at,
            granted_secs,
        } => vec![
            format!("Registered {}", local_time_text(*registered_at, today)),
            format!("Last renewal {}", local_time_text(*last_renewed_at, today)),
            format!(
                "Expires {} ({granted_secs} s granted)",
                local_time_text(*expires_at, today)
            ),
        ],
        _ => Vec::new(),
    }
}

/// Tooltip lines with the subscription times of a BLF key, only the known ones, shown only with
/// developer tools on.
pub fn blf_time_lines(b: &siple_sip::BlfStatus, developer_tools: bool) -> Vec<String> {
    if !developer_tools {
        return Vec::new();
    }
    blf_time_lines_on(b, chrono::Local::now().date_naive())
}

fn blf_time_lines_on(b: &siple_sip::BlfStatus, today: chrono::NaiveDate) -> Vec<String> {
    let sub = &b.subscription;
    [
        ("Subscribed", sub.subscribed_at),
        ("Last refresh", sub.last_refreshed_at),
        ("Expires", sub.expires_at),
    ]
    .into_iter()
    .filter_map(|(label, at)| at.map(|at| format!("{label} {}", local_time_text(at, today))))
    .collect()
}

/// Tooltip lines for the dialogs behind a BLF state (`ringing from 1019 (Dana)`).
pub fn blf_dialog_lines(b: &siple_sip::BlfStatus) -> Vec<String> {
    use siple_sip::{DialogDirection, DialogPhase};
    b.dialogs
        .iter()
        .filter(|d| d.state != DialogPhase::Terminated)
        .map(|d| {
            let who = match (&d.remote_display, &d.remote_identity) {
                (Some(n), Some(u)) => format!("{n} <{u}>"),
                (None, Some(u)) => u.clone(),
                (Some(n), None) => n.clone(),
                (None, None) => "unknown".into(),
            };
            let dir = match d.direction {
                Some(DialogDirection::Initiator) => "to",
                Some(DialogDirection::Recipient) => "from",
                None => "with",
            };
            format!("{} {dir} {who}", format!("{:?}", d.state).to_lowercase())
        })
        .collect()
}

/// Calls to show: all live calls plus recently ended ones.
pub fn visible_calls(calls: &[CallInfo], now: DateTime<Utc>) -> Vec<&CallInfo> {
    calls
        .iter()
        .filter(|c| {
            c.state != CallState::Ended
                || c.ended_at
                    .is_none_or(|e| (now - e).num_seconds() < ENDED_CALL_LINGER_SECS)
        })
        .collect()
}

/// One entry of a device combo box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceChoice {
    /// Value stored in the selection (`""` = default).
    pub value: String,
    pub label: String,
}

/// Combo entries: the default entry first, then every device by name. A currently selected
/// name that is not in the list (unplugged) is kept so the combo shows it.
pub fn device_choices(
    devices: &[DeviceInfo],
    selected: &str,
    default_label: &str,
) -> Vec<DeviceChoice> {
    let mut out = vec![DeviceChoice {
        value: String::new(),
        label: default_label.to_string(),
    }];
    for d in devices {
        let label = if d.is_default {
            format!("{} (default)", d.name)
        } else {
            d.name.clone()
        };
        out.push(DeviceChoice {
            value: d.name.clone(),
            label,
        });
    }
    if !selected.is_empty() && !devices.iter().any(|d| d.name == selected) {
        out.push(DeviceChoice {
            value: selected.to_string(),
            label: format!("{selected} (not found)"),
        });
    }
    out
}

/// Label of the entry whose value is `selected`.
pub fn selected_label(choices: &[DeviceChoice], selected: &str) -> String {
    choices
        .iter()
        .find(|c| c.value == selected)
        .map(|c| c.label.clone())
        .unwrap_or_else(|| selected.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use siple_core::config::Transport;

    fn dev(name: &str, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            is_default,
            default_sample_rate: 48_000,
            channels: 2,
        }
    }

    fn status(enabled: bool, registration: RegState) -> AccountStatus {
        AccountStatus {
            id: "a".into(),
            enabled,
            transport: Transport::Udp,
            transport_effective: Some(Transport::Udp),
            registrar: "r".into(),
            aor: "sip:a@r".into(),
            contact: None,
            registration,
            local_addr: None,
            public_addr: None,
            trace_enabled: false,
            pending_rebuild: false,
            srtp_optional_downgraded: Vec::new(),
            dnd: false,
            mwi: None,
        }
    }

    #[test]
    fn duration_format() {
        assert_eq!(format_duration(-5), "0:00");
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(65), "1:05");
        assert_eq!(format_duration(3599), "59:59");
        assert_eq!(format_duration(3600), "1:00:00");
        assert_eq!(format_duration(3 * 3600 + 7 * 60 + 9), "3:07:09");
    }

    #[test]
    fn registration_tooltip_uses_local_times_and_live_remaining() {
        let start = Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap();
        let renewal = start + chrono::Duration::minutes(4);
        let expiry = renewal + chrono::Duration::minutes(5);
        let account = status(
            true,
            RegState::Registered {
                registered_at: start,
                last_renewed_at: renewal,
                expires_at: expiry,
                granted_secs: 300,
            },
        );
        let tip = reg_tooltip(&account, renewal);
        for (label, at) in [
            ("Registered", start),
            ("Last renewal", renewal),
            ("Expires", expiry),
        ] {
            let local = at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %Z");
            assert!(tip.contains(&format!("{label}: {local}")), "{tip}");
        }
        assert!(tip.ends_with("(5:00 remaining)"));
        assert!(
            reg_tooltip(&account, renewal + chrono::Duration::seconds(61))
                .ends_with("(3:59 remaining)")
        );
        assert!(
            reg_tooltip(&account, expiry + chrono::Duration::seconds(1))
                .ends_with("(0:00 remaining)")
        );
        assert_eq!(
            reg_tooltip(&status(false, account.registration), renewal),
            "disabled"
        );
    }

    #[test]
    fn registration_classes() {
        let at = Utc.timestamp_opt(0, 0).unwrap();
        let reg = RegState::Registered {
            registered_at: at,
            last_renewed_at: at,
            expires_at: at,
            granted_secs: 60,
        };
        let failed = RegState::Failed {
            code: 403,
            reason: "Forbidden".into(),
            retry_at: at,
        };
        assert_eq!(reg_class(&status(true, reg.clone())), RegClass::Registered);
        assert_eq!(reg_class(&status(false, reg)), RegClass::Inactive);
        assert_eq!(
            reg_class(&status(true, RegState::Registering)),
            RegClass::Registering
        );
        assert_eq!(reg_class(&status(true, failed.clone())), RegClass::Failed);
        assert_eq!(
            reg_class(&status(true, RegState::Unregistered)),
            RegClass::Inactive
        );
        // No route to the registrar: amber like "registering", with its own label.
        let waiting = status(true, RegState::WaitingForNetwork);
        assert_eq!(reg_class(&waiting), RegClass::Registering);
        assert_eq!(reg_label(&waiting), "waiting for network");
        assert_eq!(reg_tooltip(&waiting, at), "waiting for network");
        assert_eq!(
            reg_class(&status(false, RegState::WaitingForNetwork)),
            RegClass::Inactive
        );
        // A named problem reads as its title; the raw answer goes to the details.
        assert_eq!(
            reg_label(&status(true, failed.clone())),
            "Wrong username or password"
        );
        let f = reg_failure(&status(true, failed.clone())).unwrap();
        assert_eq!(f.problem, Problem::AuthFailed);
        assert_eq!(f.hint, Problem::AuthFailed.hint());
        assert!(
            f.detail.starts_with("403 Forbidden \u{b7} next attempt "),
            "{}",
            f.detail
        );
        let tip = reg_tooltip(&status(true, failed.clone()), at);
        let lines: Vec<&str> = tip.lines().collect();
        assert_eq!(lines[0], "Wrong username or password");
        assert_eq!(lines[1], Problem::AuthFailed.hint());
        assert!(lines[2].starts_with("Details: 403 Forbidden"), "{tip}");
        // Disabled accounts show no failure.
        assert!(reg_failure(&status(false, failed)).is_none());
        assert!(reg_failure(&status(true, RegState::Registering)).is_none());
        // A local failure keeps its text as the detail; one the catalog cannot name shows the
        // raw text as the title.
        let dns = status(
            true,
            RegState::Failed {
                code: 0,
                reason: "DNS: no such host".into(),
                retry_at: at,
            },
        );
        assert_eq!(reg_label(&dns), "Can't find the server");
        assert!(
            reg_failure(&dns)
                .unwrap()
                .detail
                .starts_with("DNS: no such host")
        );
        let odd = status(
            true,
            RegState::Failed {
                code: 0,
                reason: "gremlins".into(),
                retry_at: at,
            },
        );
        assert_eq!(reg_label(&odd), "failed: gremlins");
        let f = reg_failure(&odd).unwrap();
        assert_eq!(f.problem, Problem::Other);
        assert!(f.detail.starts_with("next attempt "), "{}", f.detail);
    }

    #[test]
    fn sip_detail_and_failure_views() {
        assert_eq!(sip_detail(403, "Forbidden"), "403 Forbidden");
        assert_eq!(sip_detail(403, ""), "403");
        assert_eq!(sip_detail(0, "  DNS: x "), "DNS: x");
        let v = failure_view(Problem::Busy, 486, "Busy Here");
        assert_eq!(
            (v.title.as_str(), v.detail.as_str()),
            ("Busy", "486 Busy Here")
        );
        assert_eq!(
            v.lines(),
            ["Busy", Problem::Busy.hint(), "Details: 486 Busy Here"]
        );
        let v = failure_view(Problem::Other, 599, "Weird");
        assert_eq!((v.title.as_str(), v.detail.as_str()), ("599 Weird", ""));
    }

    #[test]
    fn action_notices_name_the_problem_and_keep_the_raw_text() {
        let n = action_notice("Dial", Some(Problem::Busy), "486 Busy Here");
        assert_eq!(
            n.text,
            format!("Dial failed: Busy. {}", Problem::Busy.hint())
        );
        assert_eq!(n.detail.as_deref(), Some("486 Busy Here"));
        // No problem, or `other`: today's text, no details.
        for p in [None, Some(Problem::Other)] {
            let n = action_notice("Dial", p, "account lab is not registered");
            assert_eq!(
                n,
                Notice::plain("Dial failed: account lab is not registered")
            );
        }
    }

    #[test]
    fn notices_expire_and_carry_details() {
        let shared = Shared::new(
            crate::visual_tests::base_snapshot(),
            std::path::PathBuf::new(),
            None,
        );
        assert!(shared.notice(Duration::from_secs(60)).is_none());
        shared.set_notice("plain");
        assert_eq!(
            shared.notice(Duration::from_secs(60)),
            Some(Notice::plain("plain"))
        );
        shared.set_notice_full(Notice {
            text: "t".into(),
            detail: Some("d".into()),
        });
        assert_eq!(
            shared
                .notice(Duration::from_secs(60))
                .unwrap()
                .detail
                .as_deref(),
            Some("d")
        );
        assert!(shared.notice(Duration::ZERO).is_none());
    }

    #[test]
    fn end_and_transfer_failures() {
        use siple_sip::{CallEnd, EndCause, EndedBy, TransferError, TransferNotify};
        let end = |cause, by, code, reason: &str| CallEnd {
            code,
            reason: reason.into(),
            by,
            missed: false,
            q850_cause: None,
            cause,
        };
        let busy =
            end_failure(&end(EndCause::Rejected, EndedBy::Remote, 486, "Busy Here")).unwrap();
        assert_eq!(busy.problem, Problem::Busy);
        assert_eq!(busy.detail, "486 Busy Here");
        assert!(end_failure(&end(EndCause::Hangup, EndedBy::Local, 200, "OK")).is_none());
        assert!(end_failure(&end(EndCause::Rejected, EndedBy::Local, 486, "Busy Here")).is_none());

        let at = Utc::now();
        let mut t = crate::visual_tests::session(siple_sip::TransferPhase::ConsultConnected);
        assert!(transfer_failure(&t).is_none());
        t.last_error = Some(TransferError {
            stage: "refer".into(),
            code: 403,
            reason: "Forbidden".into(),
        });
        assert_eq!(transfer_failure(&t).unwrap().problem, Problem::Forbidden);
        // A superseded session has no problem; a failed NOTIFY stands in.
        t.last_error = Some(TransferError {
            stage: "superseded".into(),
            code: 0,
            reason: "replaced".into(),
        });
        assert!(transfer_failure(&t).is_none());
        t.last_notify = Some(TransferNotify {
            code: 486,
            reason: "Busy Here".into(),
            at,
        });
        assert_eq!(transfer_failure(&t).unwrap().problem, Problem::Busy);
        t.last_notify = Some(TransferNotify {
            code: 180,
            reason: "Ringing".into(),
            at,
        });
        assert!(transfer_failure(&t).is_none());
    }

    #[test]
    fn device_choices_lists_default_first() {
        let devs = [dev("Built-in Mic", true), dev("USB Headset", false)];
        let c = device_choices(&devs, "", "System default");
        assert_eq!(c[0].value, "");
        assert_eq!(c[0].label, "System default");
        assert_eq!(c[1].label, "Built-in Mic (default)");
        assert_eq!(c[1].value, "Built-in Mic");
        assert_eq!(c.len(), 3);
        assert_eq!(selected_label(&c, ""), "System default");
        assert_eq!(selected_label(&c, "USB Headset"), "USB Headset");
    }

    #[test]
    fn missing_selected_device_is_kept() {
        let devs = [dev("A", true)];
        let c = device_choices(&devs, "Gone", "Same as speaker");
        assert_eq!(c.len(), 3);
        assert_eq!(selected_label(&c, "Gone"), "Gone (not found)");
        assert_eq!(c[0].label, "Same as speaker");
    }

    #[test]
    fn mwi_and_blf_labels() {
        use siple_sip::{BlfDialog, BlfState, BlfStatus, DialogDirection, DialogPhase, MwiSummary};
        let m = |new, old| MwiSummary {
            waiting: new > 0,
            new,
            old,
        };
        assert_eq!(mwi_label(&m(2, 5)), "2 new / 5 old");
        assert_eq!(mwi_label(&m(0, 0)), "no messages");
        assert_eq!(mwi_label(&m(0, 3)), "0 new / 3 old");
        let d = BlfDialog {
            id: "d".into(),
            call_id: None,
            local_tag: None,
            remote_tag: None,
            direction: Some(DialogDirection::Recipient),
            state: DialogPhase::Early,
            remote_identity: Some("sip:1019@x".into()),
            remote_display: Some("Dana".into()),
            local_identity: None,
            local_target: None,
            remote_target: None,
            rendering: None,
        };
        let mut b = BlfStatus {
            account: "a".into(),
            uri: "1018".into(),
            label: String::new(),
            state: BlfState::Ringing,
            dialogs: vec![d],
            updated_at: Utc::now(),
            group: String::new(),
            pickup: siple_sip::PickupMode::Off,
            subscription: siple_sip::BlfSubscription {
                phase: siple_sip::SubscriptionPhase::Active,
                last_code: None,
                last_reason: None,
                expires_at: None,
                subscribed_at: None,
                last_refreshed_at: None,
                retry_at: None,
            },
            stale: false,
        };
        assert_eq!(blf_title(&b), "1018");
        b.label = "Bob".into();
        assert_eq!(blf_title(&b), "Bob");
        assert_eq!(blf_state_label(b.state), "ringing");
        assert_eq!(blf_state_text(&b), "Ringing");
        assert!(blf_health_lines(&b).is_empty());
        b.state = BlfState::Failed;
        b.subscription.phase = siple_sip::SubscriptionPhase::Failed;
        assert_eq!(blf_state_text(&b), "Failed");
        b.subscription.last_code = Some(404);
        b.subscription.last_reason = Some("Not Found".into());
        // The key reads plain "Failed"; the problem and the server's answer are in the tooltip.
        assert_eq!(blf_state_text(&b), "Failed");
        assert_eq!(
            blf_health_lines(&b),
            [
                "Extension not found",
                Problem::ExtensionNotFound.hint(),
                "Details: 404 Not Found"
            ]
        );
        b.stale = true;
        assert_eq!(blf_health_lines(&b).len(), 4);
        // Retrying keeps the last known state and says when it tries again.
        b.state = BlfState::Ringing;
        b.subscription.phase = siple_sip::SubscriptionPhase::Retrying;
        b.subscription.last_code = Some(503);
        b.subscription.last_reason = Some("Service Unavailable".into());
        b.subscription.retry_at = Some(Utc::now() + chrono::Duration::seconds(30));
        assert_eq!(blf_state_text(&b), "Ringing");
        let l = blf_health_lines(&b);
        assert_eq!(l[0], "The server had a problem");
        assert_eq!(l[1], Problem::ServerError.hint());
        assert!(
            l[2].starts_with("Details: 503 Service Unavailable, next attempt "),
            "{l:?}"
        );
        assert_eq!(l[3], "Stale: this state may be out of date");
        b.state = BlfState::Ringing;
        b.stale = false;
        assert_eq!(blf_dialog_lines(&b), ["early from Dana <sip:1019@x>"]);
        b.dialogs[0].state = DialogPhase::Terminated;
        assert!(blf_dialog_lines(&b).is_empty());
    }

    #[test]
    fn call_state_labels() {
        assert_eq!(call_state_label(CallState::Held), "On hold");
        assert_eq!(call_state_label(CallState::Ringing), "Ringing");
    }

    fn blf_fixture() -> siple_sip::BlfStatus {
        siple_sip::BlfStatus {
            account: "a".into(),
            uri: "1018".into(),
            label: String::new(),
            state: siple_sip::BlfState::Idle,
            dialogs: Vec::new(),
            updated_at: Utc::now(),
            group: String::new(),
            pickup: siple_sip::PickupMode::Off,
            subscription: siple_sip::BlfSubscription {
                phase: siple_sip::SubscriptionPhase::Active,
                last_code: None,
                last_reason: None,
                expires_at: None,
                subscribed_at: None,
                last_refreshed_at: None,
                retry_at: None,
            },
            stale: false,
        }
    }

    fn local_utc(y: i32, m: u32, d: u32, h: u32, mi: u32, sec: u32) -> DateTime<Utc> {
        chrono::Local
            .with_ymd_and_hms(y, m, d, h, mi, sec)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn reg_time_lines_format_and_date_prefix() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        let st = status(
            true,
            RegState::Registered {
                registered_at: local_utc(2026, 11, 19, 14, 2, 11),
                last_renewed_at: local_utc(2026, 11, 19, 14, 32, 5),
                expires_at: local_utc(2026, 11, 20, 0, 2, 5),
                granted_secs: 3600,
            },
        );
        assert_eq!(
            reg_time_lines_on(&st, today),
            [
                "Registered 14:02:11",
                "Last renewal 14:32:05",
                "Expires 2026-11-20 00:02:05 (3600 s granted)"
            ]
        );
        assert!(reg_time_lines_on(&status(true, RegState::Registering), today).is_empty());
        // Disabled while the unREGISTER is still in flight: no times.
        assert!(reg_time_lines_on(&status(false, st.registration.clone()), today).is_empty());
        // Developer tools only.
        assert!(reg_time_lines(&st, false).is_empty());
        assert_eq!(reg_time_lines(&st, true).len(), 3);
    }

    #[test]
    fn blf_time_lines_only_known() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        let mut b = blf_fixture();
        assert!(blf_time_lines_on(&b, today).is_empty());
        b.subscription.subscribed_at = Some(local_utc(2026, 11, 19, 14, 2, 11));
        b.subscription.expires_at = Some(local_utc(2026, 11, 19, 15, 32, 5));
        assert_eq!(
            blf_time_lines_on(&b, today),
            ["Subscribed 14:02:11", "Expires 15:32:05"]
        );
        assert!(blf_time_lines(&b, false).is_empty(), "developer tools only");
        assert_eq!(blf_time_lines(&b, true).len(), 2);
        b.subscription.last_refreshed_at = Some(local_utc(2026, 11, 18, 23, 0, 0));
        assert_eq!(
            blf_time_lines_on(&b, today)[1],
            "Last refresh 2026-11-18 23:00:00"
        );
    }
}
