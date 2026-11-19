#![cfg_attr(windows, windows_subsystem = "windows")]
//! Siple GUI. `siple [--config PATH] [--headless]`.
//!
//! Main thread: eframe/egui (wgpu). Worker threads: a multi-thread tokio runtime running the
//! engine ([`siple_core::App`]), a snapshot poller and an event-log watcher.

mod actions;
#[cfg(not(feature = "store"))]
mod relaunch;
mod shell;
mod snapshot;
mod theme;
mod ui;
#[cfg(test)]
mod ui_tests;
#[cfg(test)]
mod visual_tests;
mod wait_pid;
mod widgets;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use eframe::egui;
use siple_core::events::EventFilter;
use siple_core::instance::InstanceError;
use siple_core::{App, AppError, AppOptions, config};

#[derive(Parser)]
#[command(name = "siple", version = siple_core::update::full_version(), about = "Siple (SIP test softphone)")]
struct Cli {
    /// Config file; defaults to the platform config directory.
    #[arg(long, env = "SIPLE_CONFIG")]
    config: Option<PathBuf>,
    /// Run without a window (same as `siple-cli run --headless`).
    #[arg(long)]
    headless: bool,
    /// Never open sound devices (same as `[audio] backend = "none"`).
    #[arg(long)]
    no_audio: bool,
    /// Wait for this process to exit before starting (at most 30 s), so a restarted instance finds
    /// the instance lock and control socket free. A permanent part of the command line.
    #[arg(long, hide = true, value_name = "PID")]
    wait_pid: Option<u32>,
    /// A `sip:`, `sips:`, `tel:` or `callto:` link passed by the OS URL handler. Validated, then
    /// put into the dial field of the running (or this) instance; it never dials by itself.
    #[arg(hide = true)]
    uri: Option<String>,
}

/// Linux without X11 or Wayland: the GUI cannot open a window, and eframe would only fail after
/// the engine and instance lock were up.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn missing_display(
    display: Option<&str>,
    wayland: Option<&str>,
    wayland_socket: Option<&str>,
) -> bool {
    let unset = |v: Option<&str>| v.is_none_or(|s| s.is_empty());
    unset(display) && unset(wayland) && unset(wayland_socket)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(pid) = cli.wait_pid
        && !wait_pid::wait_for_exit(pid, wait_pid::WAIT_LIMIT)
    {
        eprintln!("process {pid} is still running after 30 s; starting anyway");
    }
    let Some(resolved) = config::resolve_config_path(cli.config) else {
        eprintln!("cannot determine the default config directory; pass --config");
        return ExitCode::from(2);
    };
    let (config_path, is_default_path, startup_warnings) =
        (resolved.path, resolved.is_default, resolved.warnings);
    #[cfg(target_os = "linux")]
    if !cli.headless
        && missing_display(
            std::env::var("DISPLAY").ok().as_deref(),
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            std::env::var("WAYLAND_SOCKET").ok().as_deref(),
        )
    {
        eprintln!(
            "no display found (none of DISPLAY, WAYLAND_DISPLAY, WAYLAND_SOCKET is set); run with \
             --headless, or use `siple-cli run --headless`, to run without a window"
        );
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("siple-rt")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return ExitCode::from(1);
        }
    };
    if cli.headless {
        return rt.block_on(headless(
            config_path,
            is_default_path,
            startup_warnings,
            cli.no_audio,
        ));
    }

    // A second launch (or a link click) forwards to the running instance instead of failing.
    let config_dir = config::config_dir_for(&config_path);
    let link = cli
        .uri
        .as_deref()
        .and_then(|u| match siple_core::uri::parse_link(u) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("ignoring link argument: {e}");
                None
            }
        });
    let link_arg = cli.uri.clone().filter(|_| link.is_some());
    // Ask a running instance first: before the config is parsed (a broken file must not stop a
    // link click) and before keyring reads.
    if !matches!(
        hand_off_to_running(&rt, &config_dir, link_arg.as_deref()),
        HandOffResult::Continue
    ) {
        return ExitCode::SUCCESS;
    }
    let config_path_for_login = siple_core::config::absolute_path(&config_path);
    #[cfg(not(feature = "store"))]
    relaunch::set_carried(
        (!is_default_path).then(|| config_path_for_login.clone()),
        cli.no_audio,
    );
    #[cfg(target_os = "macos")]
    shell::early_init();
    let mut opts = AppOptions::new(config_path);
    opts.use_network_updates();
    opts.migrate_legacy_config = is_default_path;
    opts.startup_warnings = startup_warnings;
    opts.headless = false;
    if cli.no_audio {
        opts.audio_backend = Some(config::AudioBackend::None);
    }
    if !cli.no_audio {
        request_mic_access();
    }
    opts.stderr_log = false; // file log only (also the only option on Windows)
    let app = {
        let _enter = rt.enter();
        match rt.block_on(App::start(opts)) {
            Ok(app) => Arc::new(app),
            Err(e) => return show_start_error(&e),
        }
    };
    run_gui(
        rt,
        app,
        link,
        (!is_default_path).then_some(config_path_for_login),
    )
}

// ---- GUI -----------------------------------------------------------------------------------

/// Asks for microphone access through AVFoundation before the engine opens the input device,
/// on a background thread (the answer can take as long as the user likes). A prompt raised by
/// opening the device instead leaves the in-process status "not determined" after the user
/// answers, so the GUI would keep warning about a microphone that works. Only inside the app
/// bundle: a bare binary has no `NSMicrophoneUsageDescription` and macOS would kill it.
fn request_mic_access() {
    #[cfg(target_os = "macos")]
    {
        let bundled = std::env::current_exe()
            .is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
        if bundled
            && siple_core::audio::mic_permission()
                == siple_core::audio::MicPermission::NotDetermined
        {
            let _ = std::thread::Builder::new()
                .name("mic-permission".into())
                .spawn(|| {
                    let r =
                        siple_core::audio::request_mic_permission(Duration::from_secs(24 * 3600));
                    tracing::info!(result = ?r, "microphone permission request answered");
                });
        }
    }
}

const APP_TITLE: &str = "Siple";

/// Window / Dock / taskbar icon (the packaged app icon).
static APP_ICON_PNG: &[u8] = include_bytes!("../../../packaging/icons/siple-1024.png");
/// The simplified mark (tile only): the brand mark in the header and the first-run wizard.
static BRAND_MARK_PNG: &[u8] = include_bytes!("../../../packaging/icons/siple-mark-256.png");
/// Tray / menu-bar base image: the simplified mark on the app icon's grid (no lamp: the tray
/// draws its own status badge).
static TRAY_ICON_PNG: &[u8] = include_bytes!("../../../packaging/icons/siple-small-128.png");

fn window_options(size: [f32; 2], min: [f32; 2]) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_TITLE)
        .with_app_id(siple_core::config::APP_ID)
        .with_inner_size(size)
        .with_min_inner_size(min);
    match eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(Arc::new(icon)),
        Err(e) => tracing::warn!("cannot decode the window icon: {e}"),
    }
    eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    }
}

enum HandOffResult {
    /// The running instance took the launch (or refused it politely): this process is done.
    Handled,
    /// Nothing to hand to (no instance, or a headless one): carry on starting.
    Continue,
}

/// Sends this launch's link (or just "show") to the instance that holds the lock, if one
/// answers.
fn hand_off_to_running(
    rt: &tokio::runtime::Runtime,
    config_dir: &std::path::Path,
    link: Option<&str>,
) -> HandOffResult {
    use siple_core::uri::HandOff;
    match rt.block_on(siple_core::uri::hand_off(config_dir, link)) {
        HandOff::Done => HandOffResult::Handled,
        HandOff::Refused(why) => {
            eprintln!("the running instance did not take the link: {why}");
            HandOffResult::Handled
        }
        // Headless instance: start normally, which reports "already running".
        HandOff::NoGui | HandOff::Unreachable(_) => HandOffResult::Continue,
    }
}

fn run_gui(
    rt: tokio::runtime::Runtime,
    app: Arc<App>,
    link: Option<siple_core::uri::LinkTarget>,
    login_config: Option<PathBuf>,
) -> ExitCode {
    if app.created_config() {
        tracing::info!("created example config at {}", app.config_path().display());
    }
    // Rescan on GUI open, then take the first snapshot so the first frame is complete.
    if let Some(a) = app.audio() {
        a.rescan();
    }
    let initial = rt.block_on(async {
        let devices = snapshot::list_devices(&app).await;
        snapshot::capture(&app, devices, false)
    });
    let shared = Arc::new(snapshot::Shared::new(
        initial,
        app.config_path().to_path_buf(),
        app.logging().map(|l| l.log_dir().to_path_buf()),
    ));
    let tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();

    let handle = rt.handle().clone();
    // Every exit path funnels into `run_shutdown_hook`: eframe returning normally, and (on macOS)
    // Cmd-Q / Dock Quit / `osascript quit`, where AppKit's `terminate:` calls `exit()` without
    // ever returning from the event loop, which runs the `atexit` handler below.
    let creator: eframe::AppCreator<'_> = Box::new({
        let (app, shared, tasks) = (app.clone(), shared.clone(), tasks.clone());
        move |cc| {
            theme::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let mut t = tasks.lock().unwrap_or_else(|e| e.into_inner());
            {
                let (app, shared, ctx) = (app.clone(), shared.clone(), ctx.clone());
                t.push(
                    handle
                        .spawn(async move {
                            let repaint = ctx.clone();
                            snapshot::poll_loop(app, shared, move || repaint.request_repaint())
                                .await
                        })
                        .abort_handle(),
                );
            }
            {
                let mut sub = app.events().subscribe(EventFilter::any());
                let shared = shared.clone();
                t.push(
                    handle
                        .spawn(async move {
                            while sub.next().await.is_some() {
                                shared.wake.notify_one();
                            }
                        })
                        .abort_handle(),
                );
            }
            // SIGTERM / Ctrl-C close the window, which runs the normal clean shutdown. If the
            // event loop does not react (e.g. the app is hidden), shut down from here.
            #[cfg(unix)]
            {
                let ctx = ctx.clone();
                t.push(
                    handle
                        .spawn(async move {
                            use tokio::signal::unix::{SignalKind, signal};
                            let Ok(mut term) = signal(SignalKind::terminate()) else {
                                return;
                            };
                            tokio::select! {
                                _ = term.recv() => {}
                                _ = tokio::signal::ctrl_c() => {}
                            }
                            shell::QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            if shell::LOOP_EXITED.load(std::sync::atomic::Ordering::SeqCst) {
                                return; // the normal shutdown is underway
                            }
                            tracing::warn!("window did not close; shutting down directly");
                            let _ =
                                std::thread::Builder::new()
                                    .name("siple-exit".into())
                                    .spawn(|| {
                                        run_shutdown_hook();
                                        std::process::exit(0);
                                    });
                        })
                        .abort_handle(),
                );
            }
            let actions = actions::Actions::new(app.clone(), handle.clone(), shared.clone());
            let ui_cfg = app.config().ui.clone();
            let phone = ui::PhoneUi::new(shared.clone(), actions.clone())
                .with_audio_disabled_reason(app.audio_disabled_reason());
            let sh = shell::Shell::new(
                &cc.egui_ctx,
                &app,
                &handle,
                shared.clone(),
                actions,
                ui_cfg.start_hidden && ui_cfg.tray && link.is_none(),
                login_config,
            );
            let mut shell_app = shell::ShellApp::new(phone, sh);
            if let Some(l) = &link {
                shell_app.open_link(l);
            }
            Ok(Box::new(shell_app))
        }
    });
    // The hook takes over the only strong `Arc<App>` this frame holds (the UI keeps weak refs),
    // so shutdown can own the engine even while the event loop is still running.
    install_shutdown_hook(rt.handle(), app, tasks.clone());
    // winit can also panic while loading libX11 / libwayland: treat that like an error.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        eframe::run_native(
            APP_TITLE,
            window_options([900.0, 760.0], [420.0, 480.0]),
            creator,
        )
    }))
    .unwrap_or_else(|p| {
        let m = p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "the GUI panicked".into());
        Err(eframe::Error::AppCreation(m.into()))
    });

    // Event loop returned (window closed): run the same shutdown the exit paths use.
    shell::LOOP_EXITED.store(true, std::sync::atomic::Ordering::SeqCst);
    run_shutdown_hook();
    rt.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("GUI error: {e}");
            #[cfg(target_os = "linux")]
            eprintln!(
                "hint: the GUI needs its runtime libraries (X11 / Wayland, xkbcommon, a Vulkan or \
                 GL driver); see the GUI runtime package list in the README (\"Linux\" packaging \
                 section), or run headless with `siple-cli run --headless`"
            );
            ExitCode::from(1)
        }
    }
}

/// Overall bound for the graceful shutdown (waiting for clones to drop + `App::shutdown`).
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

struct ShutdownHook {
    handle: tokio::runtime::Handle,
    app: Arc<App>,
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

static SHUTDOWN_HOOK: Mutex<Option<ShutdownHook>> = Mutex::new(None);

fn install_shutdown_hook(
    handle: &tokio::runtime::Handle,
    app: Arc<App>,
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
) {
    *SHUTDOWN_HOOK.lock().unwrap_or_else(|e| e.into_inner()) = Some(ShutdownHook {
        handle: handle.clone(),
        app,
        tasks,
    });
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn atexit(cb: extern "C" fn()) -> std::os::raw::c_int;
        }
        extern "C" fn at_exit() {
            run_shutdown_hook();
        }
        // SAFETY: plain libc call with a valid `extern "C"` callback.
        unsafe {
            atexit(at_exit);
        }
    }
}

/// Idempotent: the first caller runs the shutdown, later callers (the `atexit` handler after a
/// normal return, a second signal) find nothing to do. Never called from a runtime thread.
fn run_shutdown_hook() {
    let Some(hook) = SHUTDOWN_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    else {
        return;
    };
    for t in hook
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
    {
        t.abort();
    }
    // Run on a fresh thread: from an `atexit` handler the calling thread's TLS (which tokio's
    // `block_on` needs) may already be torn down.
    let r = std::thread::Builder::new()
        .name("siple-shutdown".into())
        .spawn(move || shutdown(&hook.handle, hook.app))
        .map(|h| h.join());
    if !matches!(r, Ok(Ok(()))) {
        eprintln!("shutdown thread failed");
    }
    // An installed update: start the new version now that the engine has let go of the instance
    // lock and the control socket (it waits for this pid anyway).
    #[cfg(not(feature = "store"))]
    relaunch::spawn_pending();
}

/// `App::shutdown` consumes the app, so wait briefly for the last clones (aborted tasks, queued
/// actions) to drop, then run it, all within [`SHUTDOWN_BUDGET`].
fn shutdown(handle: &tokio::runtime::Handle, mut app: Arc<App>) {
    let deadline = std::time::Instant::now() + SHUTDOWN_BUDGET;
    let r = handle.block_on(async {
        let owned = loop {
            match Arc::try_unwrap(app) {
                Ok(a) => break a,
                Err(a) => app = a,
            }
            if std::time::Instant::now() >= deadline {
                return Err("shutdown skipped: engine still referenced");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        tokio::time::timeout(left, owned.shutdown())
            .await
            .map_err(|_| "shutdown timed out")
    });
    if let Err(e) = r {
        eprintln!("{e}");
    }
}

/// Small native window explaining why the app did not start; also printed to stderr.
fn show_start_error(e: &AppError) -> ExitCode {
    let (title, detail) = match e {
        AppError::Instance(InstanceError::AlreadyRunning { .. }) => {
            ("Siple is already running".to_string(), e.to_string())
        }
        AppError::Instance(InstanceError::LegacyRunning { .. }) => (
            "The previous version is still running".to_string(),
            e.to_string(),
        ),
        _ => ("Siple could not start".to_string(), e.to_string()),
    };
    eprintln!("{title}: {detail}");
    let ui = ui::ErrorUi { title, detail };
    let _ = eframe::run_native(
        APP_TITLE,
        window_options([480.0, 220.0], [360.0, 180.0]),
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(ui))
        }),
    );
    ExitCode::from(1)
}

// ---- headless (same behaviour as `siple-cli run --headless`) -----------------------------

async fn headless(
    config_path: PathBuf,
    is_default_path: bool,
    startup_warnings: Vec<String>,
    no_audio: bool,
) -> ExitCode {
    let mut opts = AppOptions::new(config_path);
    opts.use_network_updates();
    opts.migrate_legacy_config = is_default_path;
    opts.startup_warnings = startup_warnings;
    opts.headless = true;
    if no_audio {
        opts.audio_backend = Some(config::AudioBackend::None);
    }
    let app = match App::start(opts).await {
        Ok(app) => app,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    if app.created_config() {
        eprintln!("created example config at {}", app.config_path().display());
    }
    if let Err(e) = wait_for_shutdown(&app).await {
        eprintln!("signal handling failed: {e}");
    }
    app.shutdown().await;
    ExitCode::SUCCESS
}

/// Resolves on the first termination request. SIGHUP reloads the config instead of stopping.
async fn wait_for_shutdown(app: &App) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut hup = signal(SignalKind::hangup())?;
        loop {
            tokio::select! {
                r = tokio::signal::ctrl_c() => { r?; return Ok(()); }
                _ = term.recv() => return Ok(()),
                _ = hup.recv() => {
                    tracing::info!("SIGHUP: reloading config");
                    let report = app.reload().await;
                    if !report.errors.is_empty() {
                        tracing::warn!("reload finished with errors: {}", report.errors.join("; "));
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let _ = app;
        let mut close = windows::ctrl_close()?;
        let mut shutdown = windows::ctrl_shutdown()?;
        let mut logoff = windows::ctrl_logoff()?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = close.recv() => {}
            _ = shutdown.recv() => {}
            _ = logoff.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = app;
        tokio::signal::ctrl_c().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn display_detection() {
        assert!(missing_display(None, None, None));
        assert!(missing_display(Some(""), Some(""), Some("")));
        assert!(!missing_display(Some(":0"), None, None));
        assert!(!missing_display(None, Some("wayland-0"), None));
        assert!(!missing_display(None, None, Some("3")));
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_flags() {
        let c = Cli::try_parse_from(["siple", "--headless", "--config", "/x/c.toml"]).unwrap();
        assert!(c.headless);
        assert_eq!(c.config.unwrap(), PathBuf::from("/x/c.toml"));
        assert!(!Cli::try_parse_from(["siple"]).unwrap().headless);
    }

    #[test]
    fn accepts_a_positional_uri_from_url_handlers() {
        let c = Cli::try_parse_from(["siple", "sip:1001@pbx.example.test"]).unwrap();
        assert_eq!(c.uri.as_deref(), Some("sip:1001@pbx.example.test"));
        let c = Cli::try_parse_from(["siple", "--headless", "tel:+15551234567"]).unwrap();
        assert!(c.headless && c.uri.is_some());
        assert!(Cli::try_parse_from(["siple"]).unwrap().uri.is_none());
    }

    #[test]
    fn parses_no_audio() {
        assert!(!Cli::try_parse_from(["siple"]).unwrap().no_audio);
        let c = Cli::try_parse_from(["siple", "--no-audio"]).unwrap();
        assert!(c.no_audio && !c.headless);
        let c = Cli::try_parse_from(["siple", "--headless", "--no-audio"]).unwrap();
        assert!(c.no_audio && c.headless);
    }
}
