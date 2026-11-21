//! Desktop shell around the main window: tray / menu-bar icon,
//! close-to-tray, raise and attention on calls, notifications, link hand-off, launch at login
//! and the macOS menu.
//!
//! [`ShellApp`] wraps [`PhoneUi`] as the `eframe::App`. Everything platform-specific lives in
//! submodules (`tray_native`, `tray_linux`, `macos`, `autostart`, `links`, `notify`); the
//! decisions are pure functions in [`policy`] and [`tray_model`].
//!
//! The shell runs from `App::logic`, which eframe calls even while the window is hidden whenever
//! a repaint is requested. Tray clicks, API requests and engine events all request one, so a
//! hidden app stays responsive; hidden, nothing is painted.

pub mod autostart;
pub mod links;
pub mod notify;
pub mod policy;
pub mod tray_model;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_notify;
#[cfg(target_os = "linux")]
mod tray_linux;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod tray_native;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use serde_json::{Value, json};
use siple_core::control::{ApiError, ErrorKind};
use siple_core::ui_host::{UiHost, UiRequest};
use siple_core::uri::{IncomingLink, LinkRateLimit, parse_any};

use crate::actions::Actions;
use crate::snapshot::Shared;
use crate::ui::PhoneUi;
use notify::Notifier;
use policy::{CallEvent, CloseAction, EventTracker, HideStyle, Toast};
use tray_model::{TrayCmd, TrayModel};

/// Set by every real quit path (tray Quit, menu Quit, SIGTERM): the next close request is let
/// through instead of hiding the window.
pub static QUITTING: AtomicBool = AtomicBool::new(false);

/// Icon theme name of the installed app icon (the packaged `.desktop` file's `Icon=`).
pub const ICON_NAME: &str = siple_core::config::APP_ID;

/// Whether the window can be hidden (`Visible(false)`) in this session. winit's Wayland backend
/// cannot unmap a window, so there closing minimizes instead of hiding (the app stays
/// reachable through the tray either way).
pub fn session_can_hide(
    linux: bool,
    wayland_display: Option<&str>,
    wayland_socket: Option<&str>,
) -> bool {
    // winit 0.30 picks Wayland whenever either variable is set (it has no backend override
    // variable any more); `env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET siple` forces X11.
    let set = |v: Option<&str>| v.is_some_and(|d| !d.is_empty());
    !(linux && (set(wayland_display) || set(wayland_socket)))
}

fn hide_supported() -> bool {
    session_can_hide(
        cfg!(target_os = "linux"),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_SOCKET").ok().as_deref(),
    )
}

/// Set once the event loop has returned: the normal shutdown path is running, so the quit
/// fallback must not cut it short.
pub static LOOP_EXITED: AtomicBool = AtomicBool::new(false);

/// macOS: installs the launch-time Apple Event handling. Call from `main()` before the GUI.
#[cfg(target_os = "macos")]
pub fn early_init() {
    macos::early_init();
}

/// Whether the process runs inside a macOS app bundle (`Siple.app/Contents/MacOS/siple`).
#[allow(dead_code)]
pub(crate) fn platform_bundled() -> bool {
    std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}

/// Messages from other threads (tray, OS callbacks, API) to the shell.
pub enum ShellMsg {
    Tray(TrayCmd),
    Ui(UiRequest),
    /// A raw link from the OS (macOS Apple Event); validated by the shell.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Url(String),
    /// macOS: the Dock icon of the running app was clicked (reopen Apple Event).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Reopen,
    /// Linux: the StatusNotifierItem service started or failed.
    #[cfg(target_os = "linux")]
    LinuxTray(Result<tray_linux::LinuxTray, String>),
    /// Only wakes the UI thread (the tray host came or went).
    #[allow(dead_code)]
    Wake,
}

/// Sender side; every send wakes the UI thread (a hidden window does not repaint otherwise).
#[derive(Clone)]
pub struct ShellTx {
    tx: Sender<ShellMsg>,
    ctx: egui::Context,
}

impl ShellTx {
    pub fn send(&self, m: ShellMsg) {
        let _ = self.tx.send(m);
        self.ctx.request_repaint();
    }
}

/// Window facts the API can read from any thread.
#[derive(Debug, Clone, Copy, Default)]
struct WindowStatus {
    visible: bool,
    focused: bool,
    tray: bool,
    close_to_tray: bool,
}

/// The control-API face of the GUI.
struct GuiHost {
    tx: ShellTx,
    shared: Arc<Shared>,
    status: Arc<Mutex<WindowStatus>>,
    limiter: Arc<Mutex<LinkRateLimit>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl UiHost for GuiHost {
    fn request(&self, req: UiRequest) -> Result<Value, ApiError> {
        let st = *lock(&self.status);
        match req {
            UiRequest::Show => {
                self.tx.send(ShellMsg::Ui(UiRequest::Show));
                Ok(json!({"visible": true}))
            }
            UiRequest::Hide => {
                if !st.tray {
                    return Err(ApiError::new(
                        ErrorKind::InvalidState,
                        "hiding needs the tray: set `[ui] tray = true`",
                    ));
                }
                self.tx.send(ShellMsg::Ui(UiRequest::Hide));
                Ok(json!({"visible": false}))
            }
            UiRequest::State => Ok(json!({
                "visible": st.visible,
                "focused": st.focused,
                "tray": st.tray,
                "close_to_tray": st.close_to_tray,
                "lines_collapsed": self.shared.panel_link().lines_collapsed,
                "extensions_open": self.shared.panel_link().extensions_open,
            })),
            UiRequest::SetPanels {
                lines_collapsed,
                extensions_open,
            } => {
                let p = self.shared.request_panels(lines_collapsed, extensions_open);
                self.tx.send(ShellMsg::Wake);
                Ok(json!({
                    "lines_collapsed": p.lines_collapsed,
                    "extensions_open": p.extensions_open,
                }))
            }
            UiRequest::Attention => {
                self.tx.send(ShellMsg::Ui(UiRequest::Attention));
                Ok(json!({"ok": true}))
            }
            #[cfg(not(feature = "store"))]
            UiRequest::RelaunchForUpdate(spec) => {
                // The updater already waited for the calls to be over; this is the last guard.
                if self.shared.snapshot().call_active() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidState,
                        "a call is active; Siple does not restart during a call",
                    )
                    .with_extra(json!({"reason": "call_active"})));
                }
                self.tx
                    .send(ShellMsg::Ui(UiRequest::RelaunchForUpdate(spec)));
                Ok(json!({"ok": true}))
            }
            UiRequest::OpenLink(l) => {
                self.allow_link()?;
                let out =
                    json!({"ok": true, "kind": "dial", "target": l.target, "scheme": l.scheme});
                self.tx.send(ShellMsg::Ui(UiRequest::OpenLink(l)));
                Ok(out)
            }
            UiRequest::OpenSetup(l) => {
                self.allow_link()?;
                let out = json!({
                    "ok": true,
                    "kind": "provision",
                    "scheme": "siple",
                    "next_hop": l.account.next_hop(),
                });
                self.tx.send(ShellMsg::Ui(UiRequest::OpenSetup(l)));
                Ok(out)
            }
            // The engine refused a link: say why, unless links come in too fast.
            UiRequest::LinkRefused(m) => {
                if lock(&self.limiter).allow(Instant::now()) {
                    self.tx.send(ShellMsg::Ui(UiRequest::LinkRefused(m)));
                }
                Ok(json!({"ok": true}))
            }
        }
    }
}

impl GuiHost {
    /// Counts a link against the shared limiter (a web page can fire links in a loop).
    fn allow_link(&self) -> Result<(), ApiError> {
        if lock(&self.limiter).allow(Instant::now()) {
            return Ok(());
        }
        Err(ApiError::new(
            ErrorKind::InvalidState,
            "too many links in a short time; try again in a few seconds",
        )
        .with_extra(json!({"reason": "rate_limited"})))
    }
}

enum TrayBackend {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Native(tray_native::NativeTray),
    #[cfg(target_os = "linux")]
    Linux(tray_linux::LinuxTray),
}

/// Login-item / link-registration work, run in order by one thread so quick toggles cannot
/// interleave (the last request always lands last).
enum OsJob {
    LaunchAtLogin(bool, Option<std::path::PathBuf>),
    Links(bool),
}

fn os_worker() -> Sender<OsJob> {
    let (tx, rx) = channel::<OsJob>();
    let _ = std::thread::Builder::new()
        .name("siple-os-entries".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                match job {
                    OsJob::LaunchAtLogin(on, config) => {
                        match autostart::apply(on, config.as_deref()) {
                            Ok(()) => tracing::info!(enabled = on, "launch at login updated"),
                            Err(e) => tracing::warn!("cannot update launch at login: {e}"),
                        }
                    }
                    OsJob::Links(on) => {
                        if let Err(e) = links::apply(on) {
                            tracing::warn!("cannot update the link handler registration: {e}");
                        }
                    }
                }
            }
        });
    tx
}

/// The shell's state (UI thread only).
pub struct Shell {
    rx: Receiver<ShellMsg>,
    tx: ShellTx,
    shared: Arc<Shared>,
    actions: Actions,
    #[allow(dead_code)] // Linux tray service
    rt: tokio::runtime::Handle,
    config_dir: std::path::PathBuf,
    status: Arc<Mutex<WindowStatus>>,
    /// Same limiter as the API's `app.open_uri` (OS links arrive here directly on macOS).
    limiter: Arc<Mutex<LinkRateLimit>>,
    /// `--config` of a non-default profile, written into the login item.
    config_arg: Option<std::path::PathBuf>,
    os_jobs: Sender<OsJob>,
    notifier: Notifier,
    tracker: EventTracker,

    /// We believe the window is on screen.
    visible: bool,
    /// macOS: the Dock icon is currently dropped.
    dock_dropped: bool,
    hidden_at: Option<Instant>,
    /// We unmapped the window (`Visible(false)`), as opposed to minimizing it. Nobody can bring
    /// an unmapped window back but us (tray, `ui.show`, a link, a call), so a focus report is
    /// stale then: X11 can keep reporting the focus it had before the unmap.
    unmapped: bool,
    /// Last accepted tray click (double-click suppression).
    last_toggle: Option<Instant>,
    /// The window had keyboard focus at the last frame.
    focused: bool,
    /// Frames left in which `start_hidden` re-asserts the hidden state (eframe shows the window
    /// after its first painted frame).
    start_hidden_frames: u8,
    start_hidden_pending: bool,

    tray: Option<TrayBackend>,
    /// A tray could not be created (config toggles retry).
    tray_failed: bool,
    /// Linux: the service is being started.
    #[allow(dead_code)]
    tray_starting: bool,
    last_model: Option<TrayModel>,
    base_icon: Option<(Vec<u8>, usize)>,

    applied_login: Option<bool>,
    applied_links: Option<bool>,
    /// "Settings…" was requested (menu or tray); see [`Shell::take_settings_request`].
    settings_requested: bool,
    #[cfg(target_os = "macos")]
    _menu: Option<macos::AppMenu>,
}

impl Shell {
    /// Builds the shell inside the eframe creator (main thread, event loop running). Registers
    /// the [`UiHost`] with the engine.
    pub fn new(
        ctx: &egui::Context,
        app: &Arc<siple_core::App>,
        rt: &tokio::runtime::Handle,
        shared: Arc<Shared>,
        actions: Actions,
        start_hidden: bool,
        config_arg: Option<std::path::PathBuf>,
    ) -> Self {
        let (tx, rx) = channel();
        let tx = ShellTx {
            tx,
            ctx: ctx.clone(),
        };
        let status = Arc::new(Mutex::new(WindowStatus::default()));
        let limiter = Arc::new(Mutex::new(LinkRateLimit::new(8, Duration::from_secs(10))));
        app.set_ui_host(Arc::new(GuiHost {
            tx: tx.clone(),
            shared: shared.clone(),
            status: status.clone(),
            limiter: limiter.clone(),
        }));
        let base_icon = eframe::icon_data::from_png_bytes(crate::TRAY_ICON_PNG)
            .ok()
            .map(|i| (i.rgba, i.width as usize));
        let config_dir = siple_core::config::config_dir_for(app.config_path());
        let mut shell = Self {
            rx,
            notifier: Notifier::new(tx.clone()),
            tx,
            shared,
            actions,
            rt: rt.clone(),
            config_dir,
            status,
            limiter,
            config_arg,
            os_jobs: os_worker(),
            tracker: EventTracker::default(),
            visible: true,
            dock_dropped: false,
            hidden_at: None,
            unmapped: false,
            last_toggle: None,
            focused: false,
            start_hidden_frames: if start_hidden { 4 } else { 0 },
            start_hidden_pending: start_hidden,
            tray: None,
            tray_failed: false,
            tray_starting: false,
            last_model: None,
            base_icon,
            applied_login: None,
            applied_links: None,
            settings_requested: false,
            #[cfg(target_os = "macos")]
            _menu: None,
        };
        shell.platform_init();
        shell
    }

    fn platform_init(&mut self) {
        #[cfg(target_os = "macos")]
        {
            match macos::install_app_menu() {
                Ok(m) => self._menu = Some(m),
                Err(e) => tracing::warn!("cannot install the app menu: {e}"),
            }
            tray_native::install_handlers(&self.tx);
            macos::attach(self.tx.clone());
        }
        #[cfg(target_os = "windows")]
        tray_native::install_handlers(&self.tx);
    }

    /// Whether "Settings…" (tray menu, Cmd+,) was chosen since the last call; clears the flag.
    /// The window has already been raised. The Settings page polls this to open itself.
    pub fn take_settings_request(&mut self) -> bool {
        std::mem::take(&mut self.settings_requested)
    }

    /// Runs the shell for one frame. Returns links to prefill into the dial field.
    pub fn logic(&mut self, ctx: &egui::Context) -> Vec<IncomingLink> {
        let snap = self.shared.snapshot();
        let ui_cfg = &snap.ui;
        let mut links = Vec::new();

        self.sync_tray(ui_cfg);
        self.sync_os_entries(ui_cfg);

        while let Ok(msg) = self.rx.try_recv() {
            self.handle(ctx, msg, ui_cfg, &snap, &mut links);
        }

        if self.start_hidden_pending {
            self.drive_start_hidden(ctx, ui_cfg);
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            self.on_close_request(ctx, ui_cfg);
        }

        let os_focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
        // The user brought a minimized window back themselves (taskbar, Dock, compositor).
        if policy::user_restored(
            self.visible,
            self.unmapped,
            os_focused,
            self.hidden_at.map(|t| t.elapsed()),
        ) {
            self.visible = true;
        }
        let focused = self.visible && os_focused;
        self.focused = focused;
        // Ctrl+Q quits on Windows / Linux (macOS has Cmd+Q in the app menu).
        #[cfg(not(target_os = "macos"))]
        if focused && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Q)) {
            self.quit(ctx);
        }
        for ev in self.tracker.update(&snap.calls, &snap.transfers) {
            self.on_event(ctx, ev, ui_cfg, &snap, focused);
        }

        // Tray content follows the snapshot (even when the window is hidden).
        let model = TrayModel::from_snapshot(&snap, self.visible, chrono::Utc::now());
        if self.last_model.as_ref() != Some(&model) {
            self.update_tray(&model);
            self.last_model = Some(model);
        }

        *lock(&self.status) = WindowStatus {
            visible: self.visible,
            focused,
            tray: self.tray_ready(),
            close_to_tray: ui_cfg.tray && ui_cfg.close_to_tray,
        };
        links
    }

    // ---- tray ----------------------------------------------------------------------------

    fn tray_ready(&self) -> bool {
        match &self.tray {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Some(TrayBackend::Native(_)) => true,
            #[cfg(target_os = "linux")]
            Some(TrayBackend::Linux(t)) => !t.is_closed() && t.is_online(),
            None => false,
        }
    }

    /// Creates or removes the tray icon to match `[ui] tray` (hot reload).
    fn sync_tray(&mut self, ui: &siple_core::config::UiConfig) {
        if ui.tray && self.tray.is_none() && !self.tray_failed && !self.tray_starting {
            self.create_tray();
        } else if !ui.tray {
            self.tray_failed = false;
            if self.tray.is_some() {
                self.destroy_tray();
            }
            // Nothing to come back to: make sure the window is not left hidden.
            if !self.visible {
                self.visible = true;
                self.unmapped = false;
                self.restore_dock();
                self.tx
                    .ctx
                    .send_viewport_cmd(egui::ViewportCommand::Visible(true));
                self.tx.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        #[cfg(target_os = "linux")]
        {
            let lost = match &self.tray {
                Some(TrayBackend::Linux(t)) => {
                    if t.is_closed() {
                        self.tray = None;
                        self.tray_failed = true;
                        self.last_model = None;
                        true
                    } else {
                        !t.is_online()
                    }
                }
                None => false,
            };
            // The icon is gone (host quit): a hidden window would be unreachable.
            if lost && !self.visible {
                let ctx = self.tx.ctx.clone();
                self.show_window(&ctx);
            }
        }
    }

    fn model_now(&self) -> TrayModel {
        TrayModel::from_snapshot(&self.shared.snapshot(), self.visible, chrono::Utc::now())
    }

    #[allow(unused_variables, clippy::unused_self)]
    fn create_tray(&mut self) {
        let model = self.model_now();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        match tray_native::NativeTray::new(self.base_icon.clone(), &model) {
            Ok(t) => {
                self.tray = Some(TrayBackend::Native(t));
                self.last_model = Some(model);
            }
            Err(e) => {
                tracing::warn!("cannot create the tray icon: {e}");
                self.tray_failed = true;
            }
        }
        #[cfg(target_os = "linux")]
        {
            self.tray_starting = true;
            tray_linux::LinuxTray::spawn(&self.rt, self.tx.clone(), self.base_icon.clone(), model);
        }
    }

    fn destroy_tray(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(TrayBackend::Linux(t)) = &self.tray {
            t.shutdown();
        }
        self.tray = None;
        self.last_model = None;
    }

    fn update_tray(&mut self, model: &TrayModel) {
        match &mut self.tray {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Some(TrayBackend::Native(t)) => t.update(model),
            #[cfg(target_os = "linux")]
            Some(TrayBackend::Linux(t)) => t.update(model),
            None => {}
        }
    }

    // ---- login item / link registration --------------------------------------------------

    fn sync_os_entries(&mut self, ui: &siple_core::config::UiConfig) {
        if self.applied_login != Some(ui.launch_at_login) {
            self.applied_login = Some(ui.launch_at_login);
            let _ = self.os_jobs.send(OsJob::LaunchAtLogin(
                ui.launch_at_login,
                self.config_arg.clone(),
            ));
        }
        if self.applied_links != Some(ui.handle_links) {
            self.applied_links = Some(ui.handle_links);
            let _ = self.os_jobs.send(OsJob::Links(ui.handle_links));
        }
    }

    // ---- window ----------------------------------------------------------------------------

    fn restore_dock(&mut self) {
        #[cfg(target_os = "macos")]
        if self.dock_dropped {
            macos::set_dock_icon(true);
        }
        self.dock_dropped = false;
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.restore_dock();
        self.visible = true;
        self.unmapped = false;
        self.start_hidden_pending = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &egui::Context, ui: &siple_core::config::UiConfig) {
        if !self.tray_ready() || !hide_supported() {
            // Nothing to find the app by (or the session cannot unmap windows): minimize.
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            self.visible = false;
            self.hidden_at = Some(Instant::now());
            return;
        }
        let plan = policy::hide_plan(ui, cfg!(target_os = "macos"));
        match plan.style {
            HideStyle::Hide => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                self.unmapped = true;
            }
            HideStyle::Minimize => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
        }
        #[cfg(target_os = "macos")]
        if plan.drop_dock_icon && plan.style == HideStyle::Hide && !self.dock_dropped {
            macos::set_dock_icon(false);
            self.dock_dropped = true;
        }
        // "Not on screen for the user": hidden or minimized. The tray offers Show either way.
        self.visible = false;
        self.hidden_at = Some(Instant::now());
    }

    fn drive_start_hidden(&mut self, ctx: &egui::Context, ui: &siple_core::config::UiConfig) {
        if !ui.tray || self.tray_failed || !hide_supported() {
            // No way back to the window: show it.
            self.start_hidden_pending = false;
            return;
        }
        if self.start_hidden_frames == 0 {
            self.start_hidden_pending = false;
            return;
        }
        self.start_hidden_frames -= 1;
        // Linux provisional hide before the tray service answered: `hide_window` would minimize,
        // so use the plain hide until the tray is known.
        if self.tray_ready() || self.tray_starting {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.unmapped = true;
            #[cfg(target_os = "macos")]
            if ui.hide_from_taskbar && !self.dock_dropped {
                macos::set_dock_icon(false);
                self.dock_dropped = true;
            }
            self.visible = false;
            self.hidden_at = Some(Instant::now());
            ctx.request_repaint();
        }
    }

    fn on_close_request(&mut self, ctx: &egui::Context, ui: &siple_core::config::UiConfig) {
        let quitting = QUITTING.load(Ordering::SeqCst);
        match policy::close_action(ui, self.tray_ready() && hide_supported(), quitting) {
            CloseAction::Quit => {}
            CloseAction::HideToTray => {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_window(ctx, ui);
                self.first_close_hint(ui);
            }
            CloseAction::Minimize => {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }
    }

    /// The one-time "still running" notice (flag in `state.toml`).
    fn first_close_hint(&self, ui: &siple_core::config::UiConfig) {
        if !ui.notifications {
            return;
        }
        let dir = self.config_dir.clone();
        if siple_core::state::StateStore::open(&dir).tray_hint_shown() {
            return;
        }
        // Remember it only once the OS took the notification: a failed post (no permission, no
        // bundle, no server) must not use up the one-time notice.
        let (title, body) = notify::tray_hint();
        self.notifier.info(
            &title,
            &body,
            Some(Box::new(move || {
                siple_core::state::StateStore::open(&dir).mark_tray_hint_shown();
            })),
        );
    }

    fn quit(&mut self, ctx: &egui::Context) {
        QUITTING.store(true, Ordering::SeqCst);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        // A hidden window may not process the close; fall back to the shutdown hook.
        let _ = std::thread::Builder::new()
            .name("siple-quit".into())
            .spawn(|| {
                std::thread::sleep(Duration::from_secs(4));
                if LOOP_EXITED.load(Ordering::SeqCst) {
                    return; // the normal shutdown is underway; let it finish
                }
                tracing::warn!("window did not close; shutting down directly");
                crate::run_shutdown_hook();
                std::process::exit(0);
            });
    }

    // ---- messages and events ---------------------------------------------------------------

    fn handle(
        &mut self,
        ctx: &egui::Context,
        msg: ShellMsg,
        ui: &siple_core::config::UiConfig,
        snap: &crate::snapshot::Snapshot,
        links: &mut Vec<IncomingLink>,
    ) {
        match msg {
            ShellMsg::Tray(cmd) => self.on_cmd(ctx, cmd, ui, snap),
            ShellMsg::Ui(UiRequest::Show) => self.show_window(ctx),
            ShellMsg::Ui(UiRequest::Hide) => self.hide_window(ctx, ui),
            ShellMsg::Ui(UiRequest::Attention) => request_attention(ctx),
            ShellMsg::Ui(UiRequest::OpenLink(l)) => {
                self.show_window(ctx);
                links.push(IncomingLink::Dial(l));
            }
            ShellMsg::Ui(UiRequest::OpenSetup(l)) => {
                self.show_window(ctx);
                links.push(IncomingLink::Setup(l));
            }
            ShellMsg::Ui(UiRequest::LinkRefused(m)) => {
                self.show_window(ctx);
                self.shared.set_notice(m);
            }
            #[cfg(not(feature = "store"))]
            ShellMsg::Ui(UiRequest::RelaunchForUpdate(spec)) => {
                crate::relaunch::request(spec);
                self.quit(ctx);
            }
            ShellMsg::Ui(UiRequest::State | UiRequest::SetPanels { .. }) => {}
            ShellMsg::Url(raw) if !lock(&self.limiter).allow(Instant::now()) => {
                tracing::warn!(
                    "ignoring link: too many in a short time ({} bytes)",
                    raw.len()
                );
            }
            ShellMsg::Url(raw) => match parse_any(&raw) {
                Ok(l) => {
                    self.show_window(ctx);
                    links.push(l);
                }
                Err(e) => {
                    // Never log what the link contained: its kind of error and its size only.
                    tracing::warn!(code = e.code(), len = raw.len(), "ignoring link");
                    self.show_window(ctx);
                    self.shared.set_notice(format!("Ignored link: {e}"));
                }
            },
            ShellMsg::Wake => {}
            ShellMsg::Reopen => {
                // Dock click with the window hidden (Dock icon kept): show it.
                if !self.visible && !self.start_hidden_pending {
                    self.show_window(ctx);
                }
            }
            #[cfg(target_os = "linux")]
            ShellMsg::LinuxTray(r) => {
                self.tray_starting = false;
                match r {
                    Ok(t) => {
                        let online = t.is_online();
                        self.tray = Some(TrayBackend::Linux(t));
                        self.last_model = None;
                        if !online && !self.visible {
                            // start_hidden without a tray host (yet): keep the window.
                            self.show_window(ctx);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "no tray available ({e}); closing the window will minimize it"
                        );
                        self.tray_failed = true;
                        if !self.visible {
                            // start_hidden without a tray: bring the window back.
                            self.show_window(ctx);
                        }
                    }
                }
            }
        }
    }

    fn on_cmd(
        &mut self,
        ctx: &egui::Context,
        cmd: TrayCmd,
        ui: &siple_core::config::UiConfig,
        snap: &crate::snapshot::Snapshot,
    ) {
        match cmd {
            TrayCmd::Show => self.show_window(ctx),
            TrayCmd::Hide => self.hide_window(ctx, ui),
            TrayCmd::Toggle => {
                let since = self.last_toggle.map(|t| t.elapsed());
                match policy::tray_click(self.visible, self.focused, since) {
                    policy::ClickAction::Ignore => return,
                    policy::ClickAction::Show => self.show_window(ctx),
                    policy::ClickAction::Hide => self.hide_window(ctx, ui),
                }
                self.last_toggle = Some(Instant::now());
            }
            // A button on an old notification or menu can outlive its call: do nothing then.
            TrayCmd::Answer(id) | TrayCmd::Decline(id)
                if !policy::ring_is_actionable(&snap.calls, &id) => {}
            TrayCmd::Answer(id) => self.actions.answer(&id),
            TrayCmd::Decline(id) => self.actions.reject(&id),
            TrayCmd::Mute(id, on) => self.actions.mute(&id, on),
            TrayCmd::Hold(id, on) => self.actions.hold(&id, on),
            TrayCmd::Hangup(id) => self.actions.hangup(&id),
            TrayCmd::SetDnd(on) => {
                for a in snap.accounts.iter().filter(|a| a.enabled && a.dnd != on) {
                    self.actions.set_dnd(&a.id, on);
                }
            }
            TrayCmd::Settings => {
                self.settings_requested = true;
                self.show_window(ctx);
            }
            TrayCmd::Quit => self.quit(ctx),
        }
    }

    fn on_event(
        &mut self,
        ctx: &egui::Context,
        ev: CallEvent,
        ui: &siple_core::config::UiConfig,
        snap: &crate::snapshot::Snapshot,
        focused: bool,
    ) {
        let who = |id: &str| {
            snap.calls
                .iter()
                .find(|c| c.id == id)
                .map(|c| crate::ui::display_name(c).to_string())
                .unwrap_or_else(|| "Unknown caller".into())
        };
        match ev {
            CallEvent::Ringing(id) => {
                // A second call while one is live is call waiting: same effects.
                let fx = policy::incoming_effects(ui, focused);
                if fx.raise {
                    self.show_window(ctx);
                }
                if fx.attention {
                    request_attention(ctx);
                }
                if fx.notify {
                    self.notifier.toast(&Toast::Incoming(id.clone(), who(&id)));
                }
            }
            CallEvent::RingStopped(id) => self.notifier.withdraw(&id),
            CallEvent::Answered(_) => {
                if policy::raise_on_answer(ui, self.visible) {
                    self.show_window(ctx);
                }
            }
            CallEvent::Missed(id) => {
                if ui.notifications && !focused {
                    self.notifier.toast(&Toast::Missed(who(&id)));
                }
            }
            CallEvent::TransferDone(id, phase) => {
                if ui.notifications
                    && !focused
                    && let Some(s) = snap.transfers.iter().find(|s| s.id == id)
                    && let Some((head, detail)) = policy::transfer_toast(phase, &s.target_uri)
                {
                    self.notifier.toast(&Toast::Transfer(head, detail));
                }
            }
        }
    }
}

fn request_attention(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
        egui::UserAttentionType::Critical,
    ));
}

/// `eframe::App` that adds the shell to the phone UI.
pub struct ShellApp {
    inner: PhoneUi,
    shell: Shell,
}

impl ShellApp {
    pub fn new(inner: PhoneUi, shell: Shell) -> Self {
        Self { inner, shell }
    }

    /// Hands a link given on the command line (cold start by URL) to the UI.
    pub fn open_link(&mut self, l: &IncomingLink) {
        self.inner.open_incoming(l);
    }
}

impl eframe::App for ShellApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Panel requests from the control API apply (and save) even while hidden.
        self.inner.sync_panels();
        for l in self.shell.logic(ctx) {
            self.inner.open_incoming(&l);
        }
        if self.shell.take_settings_request() {
            self.inner.open_settings();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.inner.ui(ui, frame);
    }
}

#[cfg(test)]
mod tests {
    use super::session_can_hide;

    #[test]
    fn hiding_is_unavailable_only_on_wayland() {
        assert!(!session_can_hide(true, Some("wayland-0"), None));
        assert!(!session_can_hide(true, None, Some("3")));
        assert!(!session_can_hide(true, Some("wayland-0"), Some("3")));
        assert!(session_can_hide(true, Some(""), Some("")));
        assert!(session_can_hide(true, None, None));
        assert!(session_can_hide(false, Some("wayland-0"), Some("3")));
    }
}
