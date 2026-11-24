//! In-place install of a verified release (direct builds only; compiled out of store builds).
//!
//! [`Installer`] is the state machine: `idle -> downloading -> verifying -> ready|waiting ->
//! committing -> installed -> restarting`, any failure before the swap ends in `failed` with
//! nothing changed. It runs as a task of the app (never inside an API call), is cancellable until
//! the swap, never swaps or restarts while a call is active ([`IdleProbe`]) and never elevates.
//!
//! The per-OS parts are behind [`Platform`]: `macos`, `windows`, `linux` and `appimage`. The checks that
//! make a download trustworthy are in [`super::sums`] (signed checksum list) and
//! [`super::download`] (the transfer); the platform checks (OS code signature, archive layout)
//! are in `stage`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;
use siple_config::UpdatesConfig;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::checker::Updater;
use super::download::{AssetFetcher, Expected, MAX_ASSET};
use super::releases::Release;
use super::sums::{AssetPlatform, MAX_SIG_FILE, RELEASE_KEYS, SIG_NAME, SUMS_NAME, verify_sums};
use super::version::Version;
use super::{InstallPhase, InstallStatus, SourceName, UpdateAction, UpdateStatus};
use crate::control::ApiError;
use crate::events::EventLog;
use crate::state::{InstalledUpdate, StateStore};
use crate::ui_host::RelaunchSpec;

#[cfg(unix)]
pub mod appimage;
pub mod env;
pub mod journal;
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod windows;

/// How long the calls must have been over before the swap and the restart (`restart:
/// when_idle`).
pub const QUIET: Duration = Duration::from_secs(5);
/// How often the idle state is looked at while waiting.
pub const POLL: Duration = Duration::from_millis(250);
/// How long after a start the new version waits before removing what the update left behind.
pub const CLEANUP_DELAY: Duration = Duration::from_secs(60);
/// Progress events per second at most.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// What went wrong, as a stable category (`app.update_progress` `reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Canceled by the user, an agent or a setting.
    Cancelled,
    /// The download failed (connect, TLS, HTTP, a refused redirect, a wrong size, time-outs).
    Network,
    /// Something did not verify: hash, signature, code signature, layout.
    Untrusted,
    /// The file system refused.
    Io,
    /// Not possible here (the swap is not supported, a call is active, ...).
    Unsupported,
}

/// An install step failed. The message is for people (it is shown and logged); `kind` is for
/// programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallError {
    /// The category.
    pub kind: ErrorKind,
    /// What happened.
    pub message: String,
}

impl InstallError {
    /// A new error.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// [`ErrorKind::Network`].
    pub fn network(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, message)
    }

    /// [`ErrorKind::Untrusted`].
    pub fn untrusted(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Untrusted, message)
    }

    /// [`ErrorKind::Io`].
    pub fn io(what: &str, e: &std::io::Error) -> Self {
        Self::new(ErrorKind::Io, format!("{what}: {e}"))
    }

    /// [`ErrorKind::Cancelled`].
    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "cancelled")
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InstallError {}

/// Why an install is not possible here: a stable code and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotPossible {
    /// `disabled`, `headless`, `release_not_signed`, `no_asset`, `unsupported_platform`,
    /// `unsigned_build`, `not_bundle`, `not_writable`, `package_manager`, `container`,
    /// `translocated`, `read_only_volume`, `not_owner`, `root`, `system_install`, `sandboxed`,
    /// `in_use`.
    pub reason: &'static str,
    /// The reason in words.
    pub detail: String,
}

impl NotPossible {
    /// A new reason.
    pub fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

/// Where the running copy lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// macOS: the `.app` bundle. Windows and Linux: the directory that holds the files the swap
    /// replaces (the exe's directory; the install root). Linux AppImage: the directory of the
    /// `.AppImage`.
    pub root: PathBuf,
    /// The running executable (Linux AppImage: the `.AppImage` file, not the binary in its mount).
    pub exe: PathBuf,
}

/// What a staged archive must be.
#[derive(Debug, Clone)]
pub struct Expect {
    /// The release being installed.
    pub version: Version,
}

/// An archive that was unpacked and verified, ready to swap in.
#[derive(Debug, Clone)]
pub struct Staged {
    /// The staging directory (everything of this install is below it).
    pub dir: PathBuf,
    /// The verified payload (macOS: the new `.app`; Windows and Linux: the unpacked tree).
    pub payload: PathBuf,
}

/// What a swap changed and what is left to start and clean up.
#[derive(Debug, Clone)]
pub struct Committed {
    /// What the GUI starts after it quit.
    pub relaunch: RelaunchSpec,
    /// Paths to remove after the new version started (the replaced copy, the staging
    /// directory).
    pub cleanup: Vec<PathBuf>,
}

/// The per-OS half of an install. Implementations: `macos::MacPlatform`,
/// `windows::WindowsPlatform`, `linux::LinuxPlatform`, `appimage::AppImagePlatform`; tests use fakes. All methods are blocking
/// (the installer runs them on a blocking thread) except [`Platform::asset`] and
/// [`Platform::probe`], which are quick.
pub trait Platform: Send + Sync {
    /// The release archive this platform installs from; `None` when there is none.
    fn asset(&self) -> Option<AssetPlatform>;

    /// Whether the running copy can be replaced in place. Looks only; changes nothing.
    fn probe(&self) -> Result<Target, NotPossible>;

    /// Creates the private staging directory for one install (on the same volume as the target;
    /// the archive is downloaded into it too) and returns it. It must be empty and new.
    fn stage_dir(&self, target: &Target) -> Result<PathBuf, InstallError>;

    /// Unpacks `archive` (already hash-verified) inside `stage_dir` and runs the platform checks
    /// (code signature and pins, layout, version). Nothing outside `stage_dir` changes.
    fn stage(
        &self,
        archive: &Path,
        stage_dir: &Path,
        target: &Target,
        expect: &Expect,
    ) -> Result<Staged, InstallError>;

    /// Swaps the staged files in. Either everything is replaced or, on an error, everything is
    /// back as it was (the implementation rolls back itself).
    fn commit(&self, staged: &Staged, target: &Target) -> Result<Committed, InstallError>;

    /// At every start, before anything else: finishes or undoes a swap that was interrupted, and
    /// removes leftovers that are safe to remove.
    fn recover(&self);

    /// After the new version started: removes what the update left behind (`record.cleanup`),
    /// each path only when it is inside a staging area of ours.
    fn cleanup(&self, record: &InstalledUpdate);

    /// Removes a staging directory of ours (after a failure).
    fn discard(&self, stage_dir: &Path) {
        if is_staging_path(stage_dir) {
            let _ = std::fs::remove_dir_all(stage_dir);
        }
    }
}

/// Whether `p` is inside a staging area of ours: a component named `.siple-update` or one that
/// starts with `NSIRD_` (macOS replacement directories). Nothing else may be deleted by cleanup.
pub fn is_staging_path(p: &Path) -> bool {
    p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        && p.components().any(|c| {
            c.as_os_str() == ".siple-update"
                || c.as_os_str()
                    .to_str()
                    .is_some_and(|s| s.starts_with("NSIRD_"))
        })
}

/// A platform that can install nothing (hosts that did not opt in, unsupported systems).
pub struct NoPlatform(pub NotPossible);

impl NoPlatform {
    /// The platform of an app whose host did not enable installs.
    pub fn disabled() -> Self {
        Self(NotPossible::new(
            "unsupported_platform",
            "this build does not install updates",
        ))
    }
}

impl Platform for NoPlatform {
    fn asset(&self) -> Option<AssetPlatform> {
        None
    }
    fn probe(&self) -> Result<Target, NotPossible> {
        Err(self.0.clone())
    }
    fn stage_dir(&self, _: &Target) -> Result<PathBuf, InstallError> {
        Err(InstallError::new(ErrorKind::Unsupported, &self.0.detail))
    }
    fn stage(&self, _: &Path, _: &Path, _: &Target, _: &Expect) -> Result<Staged, InstallError> {
        Err(InstallError::new(ErrorKind::Unsupported, &self.0.detail))
    }
    fn commit(&self, _: &Staged, _: &Target) -> Result<Committed, InstallError> {
        Err(InstallError::new(ErrorKind::Unsupported, &self.0.detail))
    }
    fn recover(&self) {}
    fn cleanup(&self, _: &InstalledUpdate) {}
}

/// The platform of the running OS, probing the real machine.
pub fn native_platform() -> Arc<dyn Platform> {
    let Some(env) = env::Env::real() else {
        return Arc::new(NoPlatform::disabled());
    };
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacPlatform::new(env))
    }
    #[cfg(windows)]
    {
        Arc::new(windows::WindowsPlatform::new(env))
    }
    #[cfg(target_os = "linux")]
    {
        linux_platform_for(siple_config::channel::current(), env)
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        let _ = env;
        Arc::new(NoPlatform::disabled())
    }
}

/// The Linux platform of `channel`: the AppImage replaces its own file, the tarball its tree.
#[cfg(target_os = "linux")]
fn linux_platform_for(
    channel: &siple_config::channel::Channel,
    env: env::Env,
) -> Arc<dyn Platform> {
    if channel.kind == siple_config::channel::ChannelKind::AppImage {
        Arc::new(appimage::AppImagePlatform::new(env))
    } else {
        Arc::new(linux::LinuxPlatform::new(env))
    }
}

/// Whether the app is idle: no call (not ended), no transfer in progress, no echo test.
pub trait IdleProbe: Send + Sync {
    /// `true` when nothing is using the audio devices or a call.
    fn idle(&self) -> bool;
}

/// The real idle probe: no call that has not ended, no transfer session in progress, no echo
/// test recording or playing.
pub struct AppIdle {
    /// The SIP stack (calls and transfer sessions).
    pub stack: siple_sip::SipStack,
    /// The media engine (echo test).
    pub media: Arc<crate::media::MediaEngine>,
}

impl IdleProbe for AppIdle {
    fn idle(&self) -> bool {
        use crate::media::EchoPhase;
        self.stack.dials_in_progress() == 0
            && self
                .stack
                .calls()
                .iter()
                .all(|c| c.state == siple_sip::CallState::Ended)
            && self.stack.transfers().iter().all(|t| t.phase.is_terminal())
            && !matches!(
                self.media.echo_test_status().phase,
                EchoPhase::Recording | EchoPhase::Playing
            )
    }
}

/// The real relauncher: asks the GUI host to quit and start the new version.
pub struct UiRelauncher(pub Arc<crate::ui_host::UiHostSlot>);

impl Relauncher for UiRelauncher {
    fn available(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    fn relaunch(&self, spec: RelaunchSpec) -> Result<(), String> {
        let host = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or("there is no GUI to restart")?;
        host.request(crate::ui_host::UiRequest::RelaunchForUpdate(spec))
            .map(|_| ())
            .map_err(|e| e.message)
    }
}

/// What a host provides for installs (see [`super::source::Providers`]).
#[derive(Clone)]
pub struct InstallProviders {
    /// The downloader.
    pub assets: Arc<dyn AssetFetcher>,
    /// The per-OS half.
    pub platform: Arc<dyn Platform>,
    /// The release keys the signature must match.
    pub keys: Vec<String>,
    /// [`QUIET`].
    pub quiet: Duration,
    /// [`POLL`].
    pub poll: Duration,
    /// [`CLEANUP_DELAY`].
    pub cleanup_delay: Duration,
}

impl InstallProviders {
    /// Installs are impossible and the network is never touched (tests, embedders).
    pub fn offline() -> Self {
        Self {
            assets: Arc::new(super::download::OfflineAssets),
            platform: Arc::new(NoPlatform::disabled()),
            keys: RELEASE_KEYS.iter().map(|k| (*k).to_string()).collect(),
            quiet: QUIET,
            poll: POLL,
            cleanup_delay: CLEANUP_DELAY,
        }
    }

    /// The real downloader and the platform of the running OS.
    pub fn native(version_text: &str) -> Self {
        Self {
            assets: Arc::new(super::download::HttpsFetcher::new(version_text)),
            platform: native_platform(),
            ..Self::offline()
        }
    }
}

/// Quits Siple and starts the new version.
pub trait Relauncher: Send + Sync {
    /// There is a GUI to do it (a headless instance cannot restart itself).
    fn available(&self) -> bool;
    /// Starts the quit-and-relaunch; the process exits shortly after.
    fn relaunch(&self, spec: RelaunchSpec) -> Result<(), String>;
}

/// When the restart happens (`app.install_update` `restart`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// As soon as no call has been active for [`QUIET`] (the default).
    WhenIdle,
    /// Immediately (still never during a call).
    Now,
    /// Download and verify, then wait: nothing is swapped until the restart is requested.
    Manual,
}

impl Restart {
    /// The name used in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WhenIdle => "when_idle",
            Self::Now => "now",
            Self::Manual => "manual",
        }
    }

    /// Parses the API name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "when_idle" => Some(Self::WhenIdle),
            "now" => Some(Self::Now),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// Everything an [`Installer`] needs.
pub struct InstallerOptions {
    /// The checker (its list names the release to install).
    pub updater: Arc<Updater>,
    /// The per-OS half.
    pub platform: Arc<dyn Platform>,
    /// The downloader.
    pub assets: Arc<dyn AssetFetcher>,
    /// Whether calls are going on.
    pub idle: Arc<dyn IdleProbe>,
    /// Restarts into the new version.
    pub relauncher: Arc<dyn Relauncher>,
    /// Receives `app.update_progress` and `app.updated`.
    pub events: Arc<EventLog>,
    /// The `[updates]` section in effect right now.
    pub settings: Arc<dyn Fn() -> UpdatesConfig + Send + Sync>,
    /// The running version, as shown.
    pub current_text: String,
    /// Where `state.toml` lives.
    pub state_dir: Option<PathBuf>,
    /// Where the per-target install lock lives. The app sets a per-user directory that every
    /// profile shares, so two instances never swap or recover the same install at once; without
    /// one the lock goes next to `state.toml`.
    pub lock_dir: Option<PathBuf>,
    /// Release keys (the embedded ones; tests inject their own).
    pub keys: Vec<String>,
    /// [`QUIET`].
    pub quiet: Duration,
    /// [`POLL`].
    pub poll: Duration,
    /// [`CLEANUP_DELAY`].
    pub cleanup_delay: Duration,
    /// Canceled when the app shuts down.
    pub shutdown: CancellationToken,
}

impl InstallerOptions {
    /// The defaults of the real app around the given collaborators.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        updater: Arc<Updater>,
        platform: Arc<dyn Platform>,
        assets: Arc<dyn AssetFetcher>,
        idle: Arc<dyn IdleProbe>,
        relauncher: Arc<dyn Relauncher>,
        events: Arc<EventLog>,
        settings: Arc<dyn Fn() -> UpdatesConfig + Send + Sync>,
        current_text: String,
        state_dir: Option<PathBuf>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            updater,
            platform,
            assets,
            idle,
            relauncher,
            events,
            settings,
            current_text,
            state_dir,
            lock_dir: None,
            keys: RELEASE_KEYS.iter().map(|k| (*k).to_string()).collect(),
            quiet: QUIET,
            poll: POLL,
            cleanup_delay: CLEANUP_DELAY,
            shutdown,
        }
    }
}

#[derive(Default)]
struct St {
    phase: Option<InstallPhase>,
    version: Option<Version>,
    bytes: u64,
    total: u64,
    restart: Option<Restart>,
    error: Option<InstallError>,
    token: Option<CancellationToken>,
    last_progress: Option<Instant>,
    /// An install requested while a canceled run winds down: it starts once that run has stopped.
    again: Option<(Release, Restart)>,
}

impl St {
    fn phase(&self) -> InstallPhase {
        self.phase.unwrap_or(InstallPhase::Idle)
    }

    /// The run was canceled before the swap and has not stopped yet (it still holds the lock and
    /// its staging area).
    fn winding_down(&self) -> bool {
        matches!(
            self.phase(),
            InstallPhase::Downloading
                | InstallPhase::Verifying
                | InstallPhase::Ready
                | InstallPhase::Waiting
        ) && self
            .token
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}

/// How long a probe result is reused.
const PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(20);

/// The installer. See the module docs.
pub struct Installer {
    o: InstallerOptions,
    st: Mutex<St>,
    wake: Notify,
    probe: Mutex<Option<(Instant, Result<Target, NotPossible>)>>,
    runs: AtomicU64,
}

fn blocking_err(e: tokio::task::JoinError) -> InstallError {
    InstallError::new(
        ErrorKind::Io,
        if e.is_panic() {
            "internal error while installing the update"
        } else {
            "the install was interrupted"
        },
    )
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, InstallError> {
    tokio::task::spawn_blocking(f).await.map_err(blocking_err)
}

/// Removes a staging directory unless disarmed.
struct Staging {
    platform: Arc<dyn Platform>,
    dir: Option<PathBuf>,
}

impl Staging {
    fn disarm(&mut self) {
        self.dir = None;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Some(d) = self.dir.take() {
            self.platform.discard(&d);
        }
    }
}

/// The per-target install lock, held until dropped.
struct InstallLock(std::fs::File);

impl Drop for InstallLock {
    fn drop(&mut self) {
        // Unlock before the close. A child process that another thread is starting holds a copy of
        // every open file until its exec closes it, and a lock left to the close lasts as long as
        // any copy: the next install would find it still held for a moment.
        let _ = self.0.unlock();
    }
}

impl Installer {
    /// The installer of an app. It adds its part to the updater's status.
    pub fn new(o: InstallerOptions) -> Arc<Self> {
        let this = Arc::new(Self {
            o,
            st: Mutex::new(St::default()),
            wake: Notify::new(),
            probe: Mutex::new(None),
            runs: AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&this);
        this.o.updater.set_overlay(Arc::new(move |s| {
            if let Some(i) = weak.upgrade() {
                i.overlay(s);
            }
        }));
        this
    }

    fn st(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// At start: finish or undo an interrupted swap, then confirm and clean up after an
    /// installed update. Never blocks the caller.
    pub fn startup(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let t = this.clone();
            let _ = tokio::task::spawn_blocking(move || {
                // Never recover a journal that another instance is still writing: recovery runs
                // under the same per-target lock as an install.
                let _lock = match t.o.platform.probe() {
                    Ok(target) => match t.lock_target(&target) {
                        Ok(l) => Some(l),
                        Err(_) => {
                            tracing::info!(
                                "another Siple is installing an update; recovery skipped"
                            );
                            return;
                        }
                    },
                    // Not installable in place: nothing can have been swapped here.
                    Err(_) => None,
                };
                t.o.platform.recover();
            })
            .await;
            let Some(dir) = this.o.state_dir.clone() else {
                return;
            };
            let store = StateStore::open(&dir);
            let Some(rec) = store.installed_update() else {
                return;
            };
            if rec.to == this.o.current_text {
                tracing::info!(from = %rec.from, to = %rec.to, "the update was installed");
                this.o.events.emit(
                    "app.updated",
                    None,
                    None,
                    json!({"from": rec.from, "to": rec.to}),
                );
                // The previous copy stays a while, in case the new one does not stay up.
                tokio::select! {
                    () = this.o.shutdown.cancelled() => return,
                    () = tokio::time::sleep(this.o.cleanup_delay) => {}
                }
                let t = this.clone();
                let r = rec.clone();
                let _ = tokio::task::spawn_blocking(move || t.o.platform.cleanup(&r)).await;
                // A fresh handle: the one opened at start does not know what was written since.
                if let Err(e) = StateStore::open(&dir).set_installed_update(None) {
                    tracing::warn!("cannot clear the installed-update record: {e}");
                }
            } else if rec.from != this.o.current_text
                && (chrono::Utc::now() - rec.at) > chrono::TimeDelta::days(30)
            {
                // Neither the old nor the new version: a record nobody will act on.
                let _ = store.set_installed_update(None);
            }
        });
    }

    /// The `[updates]` settings changed: an install that is no longer wanted stops (before the
    /// swap; after it there is nothing to stop).
    pub fn config_changed(&self) {
        let cfg = (self.o.settings)();
        if cfg.check && cfg.install {
            return;
        }
        let mut st = self.st();
        if matches!(
            st.phase(),
            InstallPhase::Downloading
                | InstallPhase::Verifying
                | InstallPhase::Ready
                | InstallPhase::Waiting
        ) && let Some(t) = &st.token
        {
            t.cancel();
            st.again = None;
        }
    }

    fn allowed(&self) -> Result<(), InstallError> {
        let cfg = (self.o.settings)();
        if cfg.check && cfg.install {
            Ok(())
        } else {
            Err(InstallError::cancelled())
        }
    }

    fn probe(&self) -> Result<Target, NotPossible> {
        let mut g = self.probe.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, r)) = g.as_ref()
            && at.elapsed() < PROBE_TTL
        {
            return r.clone();
        }
        let r = self.o.platform.probe();
        *g = Some((Instant::now(), r.clone()));
        r
    }

    /// Whether an install of the release in `s` can be started.
    fn possible(&self, s: &UpdateStatus) -> Result<(), NotPossible> {
        let cfg = (self.o.settings)();
        if !cfg.install {
            return Err(NotPossible::new(
                "disabled",
                "installing updates is turned off ([updates] install = false)",
            ));
        }
        if !self.o.relauncher.available() {
            return Err(NotPossible::new(
                "headless",
                "this instance has no window to restart into",
            ));
        }
        // The install location is only looked at while there is something to install (the probe
        // touches the disk), and while an install is under way.
        if !s.available && self.st().phase() == InstallPhase::Idle {
            return Ok(());
        }
        self.probe()?;
        let Some(platform) = self.o.platform.asset() else {
            return Err(NotPossible::new(
                "unsupported_platform",
                "there is no release file for this system",
            ));
        };
        if s.available
            && let Some(latest) = &s.latest
            && let Some(rel) = self.o.updater.release(latest)
        {
            if rel.asset(SUMS_NAME).is_none() || rel.asset(SIG_NAME).is_none() {
                return Err(NotPossible::new(
                    "release_not_signed",
                    "this release has no signed checksum list",
                ));
            }
            let names = platform.names(&rel.version);
            if rel
                .assets
                .iter()
                .filter(|a| names.contains(&a.name))
                .count()
                != 1
            {
                return Err(NotPossible::new(
                    "no_asset",
                    "this release has no file for this system",
                ));
            }
        }
        Ok(())
    }

    fn overlay(&self, s: &mut UpdateStatus) {
        if s.source != SourceName::Github || !s.enabled {
            return;
        }
        let possible = self.possible(s);
        let st = self.st();
        let phase = st.phase();
        let mut i = InstallStatus {
            possible: possible.is_ok(),
            reason: None,
            detail: None,
            state: phase,
            version: st.version.as_ref().map(ToString::to_string),
            bytes: (phase == InstallPhase::Downloading).then_some(st.bytes),
            total: (phase == InstallPhase::Downloading).then_some(st.total),
            restart: st.restart.map(|r| r.as_str().to_string()),
            error: st.error.as_ref().map(|e| e.message.clone()),
            error_kind: st.error.as_ref().map(|e| {
                serde_json::to_value(e.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            }),
        };
        if let Err(n) = &possible {
            i.reason = Some(n.reason.to_string());
            i.detail = Some(n.detail.clone());
        }
        let in_progress = !matches!(phase, InstallPhase::Idle | InstallPhase::Failed);
        if s.available && (possible.is_ok() || in_progress) {
            s.action = UpdateAction::Install;
        }
        s.install = Some(i);
    }

    /// The `install` part of the status as of now.
    pub fn status(&self) -> Option<InstallStatus> {
        self.o.updater.status().install
    }

    fn event(&self, st: &St) {
        let Some(v) = &st.version else { return };
        let phase = serde_json::to_value(st.phase()).unwrap_or_default();
        let mut data = json!({"version": v.to_string(), "state": phase});
        if st.phase() == InstallPhase::Downloading {
            data["bytes"] = json!(st.bytes);
            data["total"] = json!(st.total);
        }
        if let Some(e) = &st.error {
            data["error"] = json!(e.message);
            data["reason"] = serde_json::to_value(e.kind).unwrap_or_default();
        }
        self.o.events.emit("app.update_progress", None, None, data);
    }

    fn set_phase(&self, phase: InstallPhase) {
        let mut st = self.st();
        if st.phase() == phase {
            return;
        }
        st.phase = Some(phase);
        self.event(&st);
    }

    fn progress(&self, bytes: u64) {
        let mut st = self.st();
        st.bytes = bytes;
        let now = Instant::now();
        let due = st
            .last_progress
            .is_none_or(|t| now.duration_since(t) >= PROGRESS_INTERVAL);
        if due || bytes == st.total {
            st.last_progress = Some(now);
            self.event(&st);
        }
    }

    fn api_err(reason: &str, message: impl Into<String>, extra: serde_json::Value) -> ApiError {
        let mut data = json!({"reason": reason});
        if let (Some(d), Some(e)) = (data.as_object_mut(), extra.as_object()) {
            d.extend(e.clone());
        }
        ApiError::invalid_state(message).with_extra(data)
    }

    /// `app.install_update`. Starts the install of `version` (or, when it is already running,
    /// updates its restart mode; right after a cancel, once the canceled run has stopped); the
    /// work runs in the background.
    pub fn install(self: &Arc<Self>, version: &str, restart: Restart) -> Result<(), ApiError> {
        let cfg = (self.o.settings)();
        if !cfg.check || !cfg.install {
            return Err(Self::api_err(
                "disabled",
                "installing updates is turned off ([updates] check / install)",
                json!({}),
            ));
        }
        let s = self.o.updater.status();
        let busy_check = |st: &St| -> Option<Result<(), ApiError>> {
            // A canceled run that is still stopping counts as over: this request starts anew.
            if matches!(st.phase(), InstallPhase::Idle | InstallPhase::Failed) || st.winding_down()
            {
                return None;
            }
            if st.version.as_ref().map(ToString::to_string).as_deref() != Some(version) {
                return Some(Err(Self::api_err(
                    "busy",
                    "another update is being installed",
                    json!({"version": st.version.as_ref().map(ToString::to_string)}),
                )));
            }
            if restart == Restart::Now && !self.o.idle.idle() {
                return Some(Err(Self::api_err(
                    "call_active",
                    "a call is active; the restart waits until it is over",
                    json!({}),
                )));
            }
            Some(Ok(()))
        };
        {
            let mut st = self.st();
            if let Some(r) = busy_check(&st) {
                if r.is_ok() {
                    st.restart = Some(restart);
                    self.wake.notify_waiters();
                }
                return r;
            }
        }
        if !s.available || s.latest.as_deref() != Some(version) {
            return Err(Self::api_err(
                "not_offered",
                format!("{version} is not the offered update"),
                json!({"latest": s.latest}),
            ));
        }
        if let Err(n) = self.possible(&s) {
            return Err(Self::api_err(
                "install_not_possible",
                format!("cannot install here: {}", n.detail),
                json!({"why": n.reason, "detail": n.detail, "release_url": s.release_url}),
            ));
        }
        if restart == Restart::Now && !self.o.idle.idle() {
            return Err(Self::api_err(
                "call_active",
                "a call is active; the restart waits until it is over",
                json!({}),
            ));
        }
        let Some(rel) = self.o.updater.release(version) else {
            return Err(Self::api_err(
                "not_offered",
                format!("{version} is not the offered update"),
                json!({"latest": s.latest}),
            ));
        };
        let mut st = self.st();
        if let Some(r) = busy_check(&st) {
            // Lost a race with another caller.
            if r.is_ok() {
                st.restart = Some(restart);
                self.wake.notify_waiters();
            }
            return r;
        }
        if st.winding_down() {
            // The canceled run still holds the lock and its staging area: it starts this one
            // when it has stopped (see `run`).
            st.again = Some((rel, restart));
            return Ok(());
        }
        self.start(&mut st, rel, restart);
        Ok(())
    }

    /// Starts a run of `rel` in the background.
    fn start(self: &Arc<Self>, st: &mut St, rel: Release, restart: Restart) {
        let token = self.o.shutdown.child_token();
        *st = St {
            phase: Some(InstallPhase::Downloading),
            version: Some(rel.version.clone()),
            total: 0,
            restart: Some(restart),
            token: Some(token.clone()),
            ..St::default()
        };
        self.event(st);
        self.runs.fetch_add(1, Ordering::SeqCst);
        let this = self.clone();
        tokio::spawn(async move { this.run(rel, token).await });
    }

    /// `app.cancel_update`: stops an install that did not reach the swap. Returns whether
    /// something was canceled.
    pub fn cancel(&self) -> Result<bool, ApiError> {
        let mut st = self.st();
        match st.phase() {
            InstallPhase::Idle | InstallPhase::Failed => Ok(false),
            InstallPhase::Downloading
            | InstallPhase::Verifying
            | InstallPhase::Ready
            | InstallPhase::Waiting => {
                if let Some(t) = &st.token {
                    t.cancel();
                }
                // Also an install requested again since an earlier cancel.
                st.again = None;
                Ok(true)
            }
            _ => Err(Self::api_err(
                "too_late",
                "the new version is already in place; it starts at the next restart",
                json!({}),
            )),
        }
    }

    fn restart_mode(&self) -> Restart {
        self.st().restart.unwrap_or(Restart::WhenIdle)
    }

    async fn run(self: Arc<Self>, rel: Release, token: CancellationToken) {
        let r = self.run_inner(&rel, &token).await;
        let mut st = self.st();
        match r {
            Ok(()) => {}
            Err(e) if e.kind == ErrorKind::Cancelled || token.is_cancelled() => {
                tracing::info!("the update install was canceled");
                // Requested again while this run was stopping: its staging area is gone and the
                // lock is free, so the new run starts from scratch.
                if let Some((rel, restart)) = st.again.take()
                    && !self.o.shutdown.is_cancelled()
                {
                    tracing::info!("the update install starts again");
                    self.start(&mut st, rel, restart);
                    return;
                }
                st.phase = Some(InstallPhase::Idle);
                st.error = None;
                st.token = None;
                self.event(&st);
            }
            Err(e) => {
                tracing::warn!(error = %e, kind = ?e.kind, "the update install failed");
                st.phase = Some(InstallPhase::Failed);
                st.error = Some(e);
                st.token = None;
                self.event(&st);
            }
        }
    }

    /// Waits until the calls have been over for `quiet` (zero for `restart: now`), or the restart
    /// mode became `manual` (`Ok(false)`), or the install was canceled.
    async fn wait_idle(
        &self,
        token: &CancellationToken,
        pre_commit: bool,
    ) -> Result<bool, InstallError> {
        let mut since: Option<Instant> = None;
        loop {
            if token.is_cancelled() {
                return Err(InstallError::cancelled());
            }
            if pre_commit {
                self.allowed()?;
            }
            let mode = self.restart_mode();
            if mode == Restart::Manual {
                return Ok(false);
            }
            if self.o.idle.idle() {
                let start = *since.get_or_insert_with(Instant::now);
                let need = if mode == Restart::Now {
                    Duration::ZERO
                } else {
                    self.o.quiet
                };
                if start.elapsed() >= need {
                    return Ok(true);
                }
            } else {
                since = None;
            }
            tokio::select! {
                () = token.cancelled() => {}
                () = self.wake.notified() => {}
                () = tokio::time::sleep(self.o.poll) => {}
            }
        }
    }

    fn lock_target(&self, target: &Target) -> Result<InstallLock, InstallError> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        target.root.hash(&mut h);
        let uid = {
            #[cfg(unix)]
            {
                unsafe { libc::geteuid() }
            }
            #[cfg(not(unix))]
            {
                0u32
            }
        };
        // The shared per-user lock directory, else the config directory (private to the user),
        // else the temp dir.
        let dir = self
            .o
            .lock_dir
            .clone()
            .or_else(|| self.o.state_dir.clone())
            .unwrap_or_else(std::env::temp_dir);
        if self.o.lock_dir.is_some() {
            std::fs::create_dir_all(&dir)
                .map_err(|e| InstallError::io("cannot create the update lock directory", &e))?;
        }
        let path = dir.join(format!(".siple-update-{uid}-{:016x}.lock", h.finish()));
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| InstallError::io("cannot open the update lock", &e))?;
        f.try_lock().map_err(|_| {
            InstallError::new(
                ErrorKind::Unsupported,
                "another Siple is installing an update",
            )
        })?;
        Ok(InstallLock(f))
    }

    async fn run_inner(
        self: &Arc<Self>,
        rel: &Release,
        token: &CancellationToken,
    ) -> Result<(), InstallError> {
        let platform = self.o.platform.clone();
        let target = self
            .probe()
            .map_err(|n| InstallError::new(ErrorKind::Unsupported, n.detail))?;
        let asset_platform = platform.asset().ok_or_else(|| {
            InstallError::new(ErrorKind::Unsupported, "no release file for this system")
        })?;
        let _lock = self.lock_target(&target)?;
        let size_of = |name: &str| {
            rel.asset(name)
                .map(|a| a.size)
                .ok_or_else(|| InstallError::untrusted(format!("the release has no {name}")))
        };
        let sums_size = size_of(SUMS_NAME)?;
        let sig_size = size_of(SIG_NAME)?;
        if sums_size > MAX_SIG_FILE || sig_size > MAX_SIG_FILE {
            return Err(InstallError::untrusted("the signature files are too large"));
        }
        self.allowed()?;

        let dir = {
            let (p, t) = (platform.clone(), target.clone());
            blocking(move || p.stage_dir(&t)).await??
        };
        let mut staging = Staging {
            platform: platform.clone(),
            dir: Some(dir.clone()),
        };

        // The signed checksum list.
        let sums_path = dir.join(SUMS_NAME);
        let sig_path = dir.join(SIG_NAME);
        for (name, path, size) in [
            (SUMS_NAME, &sums_path, sums_size),
            (SIG_NAME, &sig_path, sig_size),
        ] {
            let exp = Expected {
                size,
                sha256: None,
                max: MAX_SIG_FILE,
            };
            self.o
                .assets
                .fetch(name, &rel.version, path, &exp, &|_| {}, token)
                .await?;
        }
        let sums_bytes = std::fs::read(&sums_path)
            .map_err(|e| InstallError::io("cannot read the checksum list", &e))?;
        let sig_bytes = std::fs::read(&sig_path)
            .map_err(|e| InstallError::io("cannot read the signature", &e))?;
        let keys: Vec<&str> = self.o.keys.iter().map(String::as_str).collect();
        let sums = verify_sums(&sums_bytes, &sig_bytes, &rel.version, &keys)
            .map_err(|e| InstallError::untrusted(e.to_string()))?;
        let (name, digest) = sums
            .select(asset_platform, &rel.version)
            .map_err(|e| InstallError::untrusted(e.to_string()))?;
        let api = rel.asset(&name).ok_or_else(|| {
            InstallError::untrusted(format!("{name} is not among the release's files"))
        })?;
        self.allowed()?;

        // The archive.
        {
            let mut st = self.st();
            st.total = api.size;
            st.bytes = 0;
            self.event(&st);
        }
        let archive = dir.join(&name);
        let exp = Expected {
            size: api.size,
            sha256: Some(digest),
            max: MAX_ASSET,
        };
        self.o
            .assets
            .fetch(
                &name,
                &rel.version,
                &archive,
                &exp,
                &|n| self.progress(n),
                token,
            )
            .await?;
        self.allowed()?;

        // Unpack and check.
        self.set_phase(InstallPhase::Verifying);
        let staged = {
            let (p, t, d, a) = (
                platform.clone(),
                target.clone(),
                dir.clone(),
                archive.clone(),
            );
            let expect = Expect {
                version: rel.version.clone(),
            };
            blocking(move || p.stage(&a, &d, &t, &expect)).await??
        };
        let _ = std::fs::remove_file(&archive);

        // Wait for the restart to be allowed, then the calls to be over.
        loop {
            if token.is_cancelled() {
                return Err(InstallError::cancelled());
            }
            self.allowed()?;
            if self.restart_mode() == Restart::Manual {
                self.set_phase(InstallPhase::Ready);
            } else {
                self.set_phase(InstallPhase::Waiting);
            }
            if !self.wait_idle(token, true).await? {
                // Manual: wait for the restart mode to change.
                tokio::select! {
                    () = token.cancelled() => {}
                    () = self.wake.notified() => {}
                    () = tokio::time::sleep(self.o.poll) => {}
                }
                continue;
            }
            // The last check before the swap: under the lock, so that canceling and
            // committing cannot both win.
            let mut st = self.st();
            if token.is_cancelled() {
                return Err(InstallError::cancelled());
            }
            if !self.o.idle.idle() || st.restart == Some(Restart::Manual) {
                continue;
            }
            st.phase = Some(InstallPhase::Committing);
            self.event(&st);
            break;
        }

        // The swap.
        let committed = {
            let (p, t, s) = (platform.clone(), target.clone(), staged.clone());
            match blocking(move || p.commit(&s, &t)).await? {
                Ok(c) => c,
                Err(e) => {
                    // A swap whose rollback failed leaves its journal: keep the staging area (and
                    // the backups in it or next to the files) for the next start to finish.
                    if dir.join(journal::JOURNAL_NAME).exists() {
                        staging.disarm();
                    }
                    return Err(e);
                }
            }
        };
        staging.disarm();
        self.record_installed(rel, &committed);
        self.set_phase(InstallPhase::Installed);

        // Restart: only once no call is active; the files are in place, the running image is
        // unaffected, so waiting costs nothing.
        loop {
            match self.wait_idle(token, false).await {
                Ok(true) => {}
                Ok(false) => {
                    tokio::select! {
                        () = self.o.shutdown.cancelled() => return Ok(()),
                        () = self.wake.notified() => {}
                        () = tokio::time::sleep(self.o.poll) => {}
                    }
                    continue;
                }
                Err(_) => return Ok(()), // shutting down: the restart will be the user's
            }
            match self.o.relauncher.relaunch(committed.relaunch.clone()) {
                Ok(()) => {
                    self.set_phase(InstallPhase::Restarting);
                }
                Err(e) => {
                    tracing::warn!("cannot restart into the new version: {e}");
                    let mut st = self.st();
                    st.error = Some(InstallError::new(
                        ErrorKind::Unsupported,
                        format!("the new version is installed; restart Siple to use it ({e})"),
                    ));
                    self.event(&st);
                }
            }
            return Ok(());
        }
    }

    fn record_installed(&self, rel: &Release, c: &Committed) {
        let Some(dir) = &self.o.state_dir else { return };
        let rec = InstalledUpdate {
            from: self.o.current_text.clone(),
            to: rel.name.clone(),
            at: chrono::Utc::now(),
            cleanup: c.cleanup.iter().map(|p| p.display().to_string()).collect(),
        };
        if let Err(e) = StateStore::open(dir).set_installed_update(Some(rec)) {
            tracing::warn!("cannot record the installed update: {e}");
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux"))]
mod platform_choice_tests {
    use super::*;
    use siple_config::channel::Channel;

    #[test]
    fn the_appimage_channel_gets_the_appimage_platform() {
        let env = || env::fake::FakeFs::default().env("/x/siple", Some(1000), &[]);
        let ai = linux_platform_for(&Channel::appimage(Path::new("/x/Siple.AppImage")), env());
        let arch = std::env::consts::ARCH;
        assert_eq!(ai.asset(), AssetPlatform::linux_appimage(arch));
        let tar = linux_platform_for(&Channel::DIRECT, env());
        assert_eq!(tar.asset(), AssetPlatform::linux_tarball(arch));
    }
}
