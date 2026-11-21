//! Application wiring: owns the SIP stack, media engine, audio devices, event log,
//! control server, state store and config watcher, and keeps them consistent when the config
//! changes (see [`crate::reconfig`]).
//!
//! ```text
//! App::start ─ lock ─ logging ─ state.toml ─ audio ─ media engine ─ SipStack(+trace sink)
//!            ─ accounts ─ control server + runtime.json ─ tasks:
//!                 sip→events bridge · media→events bridge · audio poll (1 s)
//!                 reload actor (serialises every config apply) · config file watcher
//! ```

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use siple_sip::{SipEvent, SipStack, SipStackOptions};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentState;
use crate::audio::{AudioDevices, AudioHandle, DeviceSelection};
use crate::backend::AppBackend;
use crate::call_media::{EngineMediaProvider, MediaRegistry};
use crate::config::edit::ConfigEdit;
use crate::config::{
    self, AudioBackend, AudioConfig, Config, ConfigError, ConfigIssue, MediaConfig, Secret,
};
use crate::config_watch::{self, ConfigWatcher};
use crate::control::{ControlContext, ControlServer, ServerError};
use crate::events::EventLog;
use crate::instance::{
    ControlEndpoints, InstanceError, InstanceLock, RuntimeFileGuard, RuntimeInfo,
};
use crate::logging::{self, LoggingError, LoggingHandle, LoggingOptions};
use crate::media::{MediaEngine, MediaEvent, MediaEventKind};
use crate::reconfig;
use crate::reconfig::{AppliedFile, EditFailure};
use crate::redact::Redactor;
use crate::sip_trace::SipTraceWriter;
use crate::state::{EffectiveDevices, PartialSelection, Slot, StateStore, resolve_devices};
use crate::tones::ToneController;

/// How logging is set up by [`App::start`].
pub enum LoggingMode {
    /// Build the pipeline and install it as the global subscriber (the normal case).
    Install,
    /// Build the pipeline but do not install it (tests; the subscriber is kept alive so
    /// `set_filter` still works).
    Detached,
    /// Use an already-initialised handle (the GUI sets logging up before the app starts).
    Provided(Arc<LoggingHandle>),
}

/// Options of [`App::start`].
pub struct AppOptions {
    /// Config file to run with (created from the example if missing).
    pub config_path: PathBuf,
    /// No GUI in this process.
    pub headless: bool,
    /// Logging setup.
    pub logging: LoggingMode,
    /// Also log to stderr (console runs).
    pub stderr_log: bool,
    /// Open real audio devices. `false` runs without an audio thread (tests, CI).
    pub enable_audio: bool,
    /// Overrides `[audio] backend` (the `--no-audio` flag sets `Some(None)`).
    pub audio_backend: Option<AudioBackend>,
    /// `config_path` is the platform default: migrate the legacy (pre-rename) config dir first.
    pub migrate_legacy_config: bool,
    /// Notices (e.g. deprecated env vars) to log as warnings once logging is up.
    pub startup_warnings: Vec<String>,
    /// Watch the config file for changes.
    pub watch_config: bool,
    /// Test hook: the distribution channel to behave as (default: the detected one).
    #[doc(hidden)]
    pub channel: Option<config::channel::Channel>,
    /// Debounce of the config watcher.
    pub watch_debounce: Duration,
    /// Upper bound for the SIP unregister phase of [`App::shutdown`].
    pub shutdown_timeout: Duration,
    /// Test hook: fake network monitor / probe / clock for the SIP stack (default: the OS).
    #[doc(hidden)]
    pub net: Option<siple_sip::net::NetSetup>,
    /// Where the update check gets its answers. [`AppOptions::new`] starts with providers that
    /// never touch the network (tests and embedders stay offline); a real host calls
    /// [`AppOptions::use_network_updates`]. The distribution channel picks which one is used.
    #[doc(hidden)]
    pub update_providers: crate::update::source::Providers,
}

impl AppOptions {
    /// Defaults for a headless console run.
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            headless: true,
            logging: LoggingMode::Install,
            stderr_log: true,
            enable_audio: true,
            audio_backend: None,
            migrate_legacy_config: false,
            startup_warnings: Vec::new(),
            watch_config: true,
            channel: None,
            watch_debounce: config_watch::DEBOUNCE,
            shutdown_timeout: Duration::from_secs(8),
            net: None,
            update_providers: crate::update::source::Providers::offline(),
        }
    }

    /// Lets the update check use the network (`[updates] check` still decides whether it runs):
    /// GitHub for direct builds, Apple's lookup on the Mac App Store, Windows' Store service in
    /// the Microsoft Store edition. The `siple` and `siple-cli` binaries call this; nothing else
    /// does.
    pub fn use_network_updates(&mut self) {
        #[cfg(not(feature = "store"))]
        {
            self.update_providers.github = Arc::new(crate::update::GithubFetcher);
            self.update_providers.install =
                crate::update::install::InstallProviders::native(crate::update::full_version());
        }
        self.update_providers.lookup = Arc::new(crate::update::appstore::ItunesFetcher::new());
        #[cfg(all(windows, feature = "store"))]
        {
            self.update_providers.store_query = Arc::new(crate::update::msstore::WinStoreQuery);
        }
    }
}

/// Why [`App::start`] failed.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The config file cannot be read or parsed.
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// The config parses but fails validation.
    #[error("{}: {} configuration error(s):{}", path.display(), issues.len(),
        issues.iter().map(|i| format!("\n  {i}")).collect::<String>())]
    Invalid {
        /// Config file.
        path: PathBuf,
        /// Every problem found.
        issues: Vec<ConfigIssue>,
    },
    /// Single-instance lock or `runtime.json`.
    #[error("{0}")]
    Instance(#[from] InstanceError),
    /// Logging could not be set up.
    #[error("{0}")]
    Logging(#[from] LoggingError),
    /// A control listener could not be started.
    #[error("{0}")]
    Control(#[from] ServerError),
    /// Anything else (filesystem, watcher).
    #[error("{0}")]
    Other(String),
}

/// The last rejected config, for `status`.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigErrorInfo {
    /// When it was rejected (RFC 3339).
    pub at: String,
    /// What was wrong.
    pub errors: Vec<String>,
    /// The error came from reading / validating the file (not from applying a valid one).
    #[serde(skip)]
    pub(crate) from_load: bool,
}

/// Outcome of a reload.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReloadReport {
    /// The file was accepted (parsed and valid). `false` means the previous config stays.
    pub applied: bool,
    /// Something effective changed.
    pub changed: bool,
    /// Human-readable list of what changed.
    pub summary: Vec<String>,
    /// Load/validation errors, or failures while applying individual changes.
    pub errors: Vec<String>,
    /// Hash of the config file text now in effect (the `base_hash` for the next `config.edit`);
    /// absent when the file was rejected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

pub(crate) struct ReloadRequest {
    /// Typed edits to write into the config file before reloading (empty = plain reload).
    pub edits: Vec<ConfigEdit>,
    /// Refuse the edits unless the file on disk still has this hash (hex).
    pub base_hash: Option<String>,
    /// Re-run even if the file content hash is unchanged.
    pub force: bool,
    pub reason: &'static str,
    pub reply: Option<oneshot::Sender<Result<ReloadReport, EditFailure>>>,
}

pub(crate) struct AudioPolicy {
    /// What was last handed to the audio layer.
    pub applied: DeviceSelection,
}

pub(crate) struct ControlSlot {
    pub server: Option<ControlServer>,
    /// Endpoints asked for (before the server shortened paths / resolved ports).
    pub requested: ControlEndpoints,
    /// Contents of `runtime.json` (token and pid are fixed for the run).
    pub info: RuntimeInfo,
    /// Deletes `runtime.json` when dropped.
    pub guard: Option<RuntimeFileGuard>,
}

/// `audio.disabled_reason` when `backend = "auto"` finds nothing to play through.
pub(crate) const NO_AUDIO_HOST_REASON: &str = "no sound hardware or sound server found";

/// Whether real audio is plausible on this host. Only Linux can tell (headless servers and
/// containers); elsewhere devices are always assumed present.
fn host_audio_plausible() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::audio::devices::host_audio_plausible()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Whether the audio devices are in use, fixed at startup (`[audio] backend` needs a restart).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioMode {
    /// `[audio] backend` as configured.
    pub backend: AudioBackend,
    /// Why no devices are opened; `None` while audio runs.
    pub disabled_reason: Option<&'static str>,
}

impl AudioMode {
    /// `host_has_audio` is the platform probe (only used for `Auto`; `Always` skips it; always `true`
    /// except on Linux without a sound card or sound server).
    pub(crate) fn decide(enable_audio: bool, backend: AudioBackend, host_has_audio: bool) -> Self {
        let disabled_reason = if !enable_audio {
            Some("disabled by the host application")
        } else if backend == AudioBackend::None {
            Some("[audio] backend = \"none\"")
        } else if backend == AudioBackend::Auto && !host_has_audio {
            Some(NO_AUDIO_HOST_REASON)
        } else {
            None
        };
        Self {
            backend,
            disabled_reason,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

pub(crate) struct Inner {
    pub config_path: PathBuf,
    pub config_dir: PathBuf,
    /// The effective (last applied) config.
    pub config: Arc<ArcSwap<Config>>,
    pub applied_at: Mutex<DateTime<Utc>>,
    pub last_error: Mutex<Option<ConfigErrorInfo>>,
    pub logging: Option<Arc<LoggingHandle>>,
    pub events: Arc<EventLog>,
    pub trace: Arc<SipTraceWriter>,
    /// Masks credentials / SRTP keys in logs and traces (`[logging]`).
    pub redactor: Arc<Redactor>,
    /// The config file text and hash last accepted (for `config.view`).
    pub applied_file: Mutex<Arc<AppliedFile>>,
    pub stack: SipStack,
    pub audio: Option<AudioHandle>,
    /// Why audio is on or off (reported by `status`).
    pub audio_mode: AudioMode,
    /// Agent audio state (streams, ...).
    #[allow(dead_code)] // read by call.stream (Wave 2)
    pub agent: AgentState,
    pub media: Arc<MediaEngine>,
    /// `cN` -> media handle of every call.
    pub call_media: Arc<MediaRegistry>,
    pub tones: ToneController,
    pub audio_policy: Mutex<AudioPolicy>,
    pub control: tokio::sync::Mutex<ControlSlot>,
    pub token: Secret,
    pub reload_tx: UnboundedSender<ReloadRequest>,
    pub cancel: CancellationToken,
    pub started_at: DateTime<Utc>,
    pub headless: bool,
    /// The distribution channel gating reads (detected, or injected by tests).
    pub channel: config::channel::Channel,
    /// The GUI, when there is one (`ui.*` methods).
    pub ui_host: Arc<crate::ui_host::UiHostSlot>,
    /// The update checker.
    pub updater: Arc<crate::update::Updater>,
    /// Downloads, verifies and installs a release in place (absent from store builds).
    #[cfg(not(feature = "store"))]
    pub installer: Arc<crate::update::install::Installer>,
}

/// Accounts of the applied config (for the redactor at startup).
fn applied_cfg_accounts(cell: &ArcSwap<Config>) -> Vec<config::AccountConfig> {
    cell.load().accounts.clone()
}

/// One-time move of device choices an older build kept in `state.toml` into `[audio]` of the
/// config file: exactly the slots where the state choice was in effect under the old "most recent
/// change wins" rule (even over an older `[audio]` value), written through the same typed-edit path
/// as the GUI, and removed from `state.toml` afterwards. Returns whether the file was changed.
/// On failure the state file keeps the choices so the next start tries again.
fn migrate_legacy_devices(
    state: &StateStore,
    config_path: &Path,
    config_dir: &Path,
    cfg: &Config,
    file_mtime: DateTime<Utc>,
) -> bool {
    let legacy = state.legacy_devices(&cfg.audio, file_mtime, Utc::now());
    let edits: Vec<ConfigEdit> = Slot::ALL
        .into_iter()
        .filter_map(|slot| {
            let name = legacy.slot(slot)?;
            Some(ConfigEdit::SetSection {
                section: "audio".into(),
                key: slot.name().into(),
                value: config::edit::EditValue::Str(name.clone()),
            })
        })
        .collect();
    let changed = if edits.is_empty() {
        false
    } else {
        match reconfig::write_edits(config_path, config_dir, &edits, None) {
            Ok(w) => {
                tracing::info!(
                    "moved the audio device choice from {} into [audio] of {}",
                    state.path().display(),
                    config_path.display()
                );
                w.changed
            }
            Err(e) => {
                tracing::warn!(
                    "cannot move the audio device choice from {} into {}: {e}; it stays there until the next start",
                    state.path().display(),
                    config_path.display()
                );
                return false;
            }
        }
    };
    if let Err(e) = state.clear_legacy() {
        tracing::warn!("cannot update {}: {e}", state.path().display());
    }
    changed
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    /// Queues a reload with the reload actor and waits for its report.
    pub(crate) async fn request_reload(&self, force: bool, reason: &'static str) -> ReloadReport {
        self.request_edit(Vec::new(), None, force, reason)
            .await
            .unwrap_or_else(|f| ReloadReport {
                errors: vec![f.to_string()],
                ..Default::default()
            })
    }

    /// Queues typed edits (and a forced reload) with the ConfigActor and waits for the result:
    /// the reload report, or why the edit was refused (the file is untouched then).
    pub(crate) async fn request_edit(
        &self,
        edits: Vec<ConfigEdit>,
        base_hash: Option<String>,
        force: bool,
        reason: &'static str,
    ) -> Result<ReloadReport, EditFailure> {
        let (tx, rx) = oneshot::channel();
        let req = ReloadRequest {
            edits,
            base_hash,
            force,
            reason,
            reply: Some(tx),
        };
        if self.reload_tx.send(req).is_err() {
            return Err(EditFailure::Stopping);
        }
        rx.await.unwrap_or(Err(EditFailure::Stopping))
    }

    /// Devices implied by the effective `[audio]`.
    pub(crate) fn effective_devices(&self) -> EffectiveDevices {
        resolve_devices(&self.config.load().audio)
    }

    /// Pushes the devices `audio` names to the audio layer if they differ from what it has.
    /// Returns them and whether that changed anything.
    pub(crate) fn sync_audio(&self, audio: &AudioConfig) -> (EffectiveDevices, bool) {
        let mut p = lock(&self.audio_policy);
        let eff = resolve_devices(audio);
        let sel = eff.selection();
        let changed = sel != p.applied;
        if changed {
            if let Some(a) = &self.audio {
                a.set_devices(sel.clone());
            }
            p.applied = sel;
        }
        (eff, changed)
    }

    /// Writes a device choice into `[audio]` of the config file through the ConfigActor, which
    /// reloads and applies it (the config is the only home of settings).
    pub(crate) async fn set_user_devices(
        &self,
        sel: &PartialSelection,
    ) -> Result<EffectiveDevices, EditFailure> {
        let edits: Vec<ConfigEdit> = Slot::ALL
            .into_iter()
            .filter_map(|slot| {
                sel.slot(slot).map(|name| ConfigEdit::SetSection {
                    section: "audio".into(),
                    key: slot.name().into(),
                    value: config::edit::EditValue::Str(name.clone()),
                })
            })
            .collect();
        self.request_edit(edits, None, true, "audio.set_devices")
            .await?;
        let eff = self.effective_devices();
        tracing::info!(devices = %eff.describe(), "audio devices set");
        Ok(eff)
    }

    /// Records (or, with `None`, clears) the last config problem. `from_load` marks errors
    /// that came from reading / validating the file rather than from applying it.
    pub(crate) fn set_last_error(&self, errors: Option<Vec<String>>, from_load: bool) {
        *lock(&self.last_error) = errors.map(|errors| ConfigErrorInfo {
            at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            errors,
            from_load,
        });
    }

    /// Clears the last config error if it was a load error (the file is readable and valid
    /// again).
    pub(crate) fn clear_load_error(&self) {
        let mut g = lock(&self.last_error);
        if g.as_ref().is_some_and(|e| e.from_load) {
            *g = None;
        }
    }

    /// Starts the control server on `requested` and writes `runtime.json`. The slot must have no
    /// running server. On failure nothing stays bound.
    pub(crate) async fn start_control(
        self: &Arc<Self>,
        slot: &mut ControlSlot,
        requested: ControlEndpoints,
    ) -> Result<(), ServerError> {
        let ctx = ControlContext::new(
            self.events.clone(),
            Arc::new(AppBackend::new(self.clone())),
            self.logging.clone(),
            self.token.clone(),
        )
        .with_channel(&self.channel);
        let server = ControlServer::start(&requested, ctx, self.cancel.child_token()).await?;
        slot.server = Some(server);
        if let Err(e) = self.publish_runtime(slot) {
            // Dropping the server releases its sockets and removes the socket files.
            slot.server = None;
            return Err(e);
        }
        slot.requested = requested;
        Ok(())
    }

    /// Rewrites `runtime.json` with the endpoints the slot's server is bound to.
    pub(crate) fn publish_runtime(&self, slot: &mut ControlSlot) -> Result<(), ServerError> {
        let Some(server) = &slot.server else {
            return Err(ServerError::Config("no control server".into()));
        };
        let mut info = slot.info.clone();
        info.control = server.endpoints().clone();
        let guard = info
            .write(&self.config_dir)
            .map_err(|e| ServerError::Config(e.to_string()))?;
        // The new file replaced the old one atomically; the old guard must not delete it.
        if let Some(old) = slot.guard.replace(guard) {
            old.disarm();
        }
        slot.info = info;
        Ok(())
    }
}

/// The running application. See the module docs.
pub struct App {
    inner: Arc<Inner>,
    tasks: Vec<JoinHandle<()>>,
    watcher: Option<ConfigWatcher>,
    _subscriber: Option<Box<dyn Any + Send + Sync>>,
    /// Held until the app is dropped (after `runtime.json` is removed).
    _instance_lock: InstanceLock,
    created_config: bool,
    shutdown_timeout: Duration,
}

impl App {
    /// Starts everything. Must run inside a tokio (multi-thread) runtime.
    ///
    /// Order: ensure + load + validate the config (nothing else is touched if it is bad), take
    /// the single-instance lock, set up logging, state, audio, media and the SIP stack, start
    /// the accounts, then bind the control endpoints and write `runtime.json`.
    pub async fn start(mut opts: AppOptions) -> Result<App, AppError> {
        let config_path = config::absolute_path(&opts.config_path);
        let config_dir = config::config_dir_for(&config_path);

        // Legacy-dir migration: new lock first, then migrate, all before `ensure_config` could
        // write an example config into the new dir.
        let mut early_lock = None;
        if opts.migrate_legacy_config
            && let Some(legacy) = config::legacy_config_dir()
            && config::needs_migration(&config_dir, &legacy)
        {
            early_lock = Some(InstanceLock::acquire(&config_dir)?);
            // Leftovers of an interrupted attempt must not pile up if this attempt fails too.
            config::cleanup_stale(&config_dir);
            config::migrate(&config_dir, &legacy).map_err(|e| AppError::Other(e.to_string()))?;
        }

        let created_config = config::ensure_config(&config_path).map_err(|e| {
            AppError::Other(format!("cannot create {}: {e}", config_path.display()))
        })?;

        let mut loaded = reconfig::load_config(&config_path, &config_dir)
            .map_err(|e| e.into_app_error(&config_path))?;
        let mut cfg = loaded.config.clone();

        // Lock before touching log files so a second instance fails without side effects.
        let instance_lock = match early_lock {
            Some(l) => l,
            None => InstanceLock::acquire(&config_dir)?,
        };
        if opts.migrate_legacy_config {
            config::cleanup_stale(&config_dir);
        }

        let log_opts = LoggingOptions {
            filter: logging::effective_filter(&cfg.general.log_level),
            log_dir: config::resolve_log_dir(&cfg.general, &config_dir),
            retention_days: cfg.general.log_retention_days,
            stderr: opts.stderr_log,
        };
        let mut subscriber: Option<Box<dyn Any + Send + Sync>> = None;
        let logging_handle = match opts.logging {
            LoggingMode::Install => Arc::new(logging::init(&log_opts)?),
            LoggingMode::Detached => {
                let (h, s) = logging::build(&log_opts)?;
                subscriber = Some(Box::new(s));
                Arc::new(h)
            }
            LoggingMode::Provided(h) => h,
        };
        let log_dir = logging_handle.log_dir().to_path_buf();

        for w in &opts.startup_warnings {
            tracing::warn!("{w}");
        }
        for w in &loaded.warnings {
            tracing::warn!("config warning: {w}");
        }
        tracing::info!(
            version = crate::update::full_version(),
            pid = std::process::id(),
            config = %config_path.display(),
            log_dir = %log_dir.display(),
            accounts = cfg.accounts.len(),
            headless = opts.headless,
            "Siple starting"
        );

        let events = Arc::new(EventLog::default());
        let state = StateStore::open(&config_dir);
        if state.has_legacy_devices()
            && migrate_legacy_devices(&state, &config_path, &config_dir, &cfg, loaded.mtime)
        {
            // The file now carries the devices: load what is really there.
            match reconfig::load_config(&config_path, &config_dir) {
                Ok(l) => {
                    cfg = l.config.clone();
                    loaded = l;
                }
                Err(e) => tracing::warn!(
                    "cannot reload {} after the device migration: {}",
                    config_path.display(),
                    e.messages().join("; ")
                ),
            }
        }
        let effective = resolve_devices(&cfg.audio);
        tracing::info!(devices = %effective.describe(), "effective audio devices");
        let backend = opts.audio_backend.unwrap_or(cfg.audio.backend);
        let host_has_audio =
            !opts.enable_audio || backend != AudioBackend::Auto || host_audio_plausible();
        let audio_mode = AudioMode::decide(opts.enable_audio, backend, host_has_audio);
        if let Some(reason) = audio_mode.disabled_reason
            && opts.enable_audio
        {
            tracing::info!("audio disabled: {reason}");
        }
        let audio = audio_mode
            .enabled()
            .then(|| AudioDevices::start(effective.selection()));

        let handle = tokio::runtime::Handle::current();
        let (media_tx, media_rx) = unbounded_channel::<MediaEvent>();
        let media = Arc::new(MediaEngine::start(audio.clone(), handle, media_tx));

        let redactor = Arc::clone(logging_handle.redactor());
        // Before the first account starts: the first REGISTER retry carries credentials. The
        // control token is added once it exists.
        redactor.update(&cfg.logging, &cfg.accounts, &Secret::default());
        let trace = Arc::new(
            SipTraceWriter::new(log_dir, cfg.general.log_retention_days)
                .with_redactor(Arc::clone(&redactor)),
        );
        let (sip_tx, sip_rx) = unbounded_channel::<SipEvent>();
        let stack_opts = SipStackOptions {
            user_agent_default: cfg.general.user_agent.clone(),
            events: sip_tx,
            wire_sink: trace.clone(),
        };
        let stack = match opts.net.take() {
            Some(net) => SipStack::with_net(stack_opts, net),
            None => SipStack::new(stack_opts),
        };

        stack.set_network_config(&cfg.network);

        // Calls: the media engine behind every SIP call. The config cell is shared with `Inner`
        // so `[media]` changes apply to the next call.
        let channel = opts.channel.unwrap_or(*config::channel::current());
        crate::agent::speech::warn_if_unsupported(&cfg.speech, &channel);
        let config_cell = Arc::new(ArcSwap::from_pointee(cfg.clone()));
        let provider = EngineMediaProvider::new(media.clone(), config_cell.clone());
        let call_media = provider.registry.clone();
        stack.set_media_provider(provider);
        stack.set_hold_scope(cfg.calls.hold_scope);
        stack.set_resume_after_failed_transfer(cfg.calls.resume_after_failed_transfer);
        let tones = ToneController::new(media.clone());

        // Accounts. A failure is logged and the account left out of the applied config, so the
        // next reload retries it.
        let mut applied_cfg = cfg.clone();
        applied_cfg.accounts.clear();
        stack.set_account_order(
            &cfg.accounts
                .iter()
                .map(|a| a.id.clone())
                .collect::<Vec<_>>(),
        );
        for a in &cfg.accounts {
            match stack.add_account(a.clone(), &cfg.general, &cfg.media).await {
                Ok(()) => {
                    tracing::info!(
                        account = %a.id, enabled = a.enabled, transport = ?a.transport,
                        registrar = a.effective_registrar(), srtp = ?a.srtp, blf = a.blf.len(),
                        "account configured"
                    );
                    applied_cfg.accounts.push(a.clone());
                }
                Err(e) => tracing::error!(account = %a.id, "cannot start account: {e}"),
            }
        }

        config_cell.store(Arc::new(applied_cfg));

        let requested = ControlEndpoints::from_config(&cfg.control, &config_dir);
        let info = RuntimeInfo::new(requested.clone())?;
        redactor.update(
            &cfg.logging,
            &applied_cfg_accounts(&config_cell),
            &info.token,
        );
        let (reload_tx, reload_rx) = unbounded_channel::<ReloadRequest>();
        let cancel = CancellationToken::new();
        let updater = {
            let cell = config_cell.clone();
            let u = crate::update::Updater::new(crate::update::UpdaterOptions {
                current: crate::update::current_version(),
                current_text: crate::update::full_version().to_string(),
                source: opts.update_providers.source(&channel),
                package_family: channel.package_family.map(str::to_string),
                events: events.clone(),
                settings: Arc::new(move || cell.load().updates.clone()),
                clock: Arc::new(Utc::now),
                state_dir: Some(config_dir.clone()),
                initial_delay: crate::update::INITIAL_DELAY,
                interval: crate::update::CHECK_INTERVAL,
                min_manual_interval: crate::update::MIN_MANUAL_INTERVAL,
            });
            u.set_skipped(state.skipped_update());
            u
        };
        let ui_host: Arc<crate::ui_host::UiHostSlot> = Arc::new(Mutex::new(None));
        #[cfg(not(feature = "store"))]
        let installer = {
            use crate::update::install::{AppIdle, Installer, InstallerOptions, UiRelauncher};
            let cell = config_cell.clone();
            let p = &opts.update_providers.install;
            let mut io = InstallerOptions::new(
                updater.clone(),
                p.platform.clone(),
                p.assets.clone(),
                Arc::new(AppIdle {
                    stack: stack.clone(),
                    media: media.clone(),
                }),
                Arc::new(UiRelauncher(ui_host.clone())),
                events.clone(),
                Arc::new(move || cell.load().updates.clone()),
                crate::update::full_version().to_string(),
                Some(config_dir.clone()),
                cancel.clone(),
            );
            io.lock_dir = dirs::cache_dir().map(|d| d.join("Siple"));
            io.keys = p.keys.clone();
            io.quiet = p.quiet;
            io.poll = p.poll;
            io.cleanup_delay = p.cleanup_delay;
            Installer::new(io)
        };
        let inner = Arc::new(Inner {
            config_path: config_path.clone(),
            config_dir: config_dir.clone(),
            config: config_cell.clone(),
            applied_at: Mutex::new(Utc::now()),
            last_error: Mutex::new(None),
            logging: Some(logging_handle),
            events: events.clone(),
            trace,
            redactor: redactor.clone(),
            applied_file: Mutex::new(Arc::new(AppliedFile {
                hash: loaded.hash,
                text: loaded.text.clone(),
            })),
            stack: stack.clone(),
            audio,
            audio_mode,
            agent: AgentState::default(),
            media,
            call_media,
            tones,
            audio_policy: Mutex::new(AudioPolicy {
                applied: effective.selection(),
            }),
            control: tokio::sync::Mutex::new(ControlSlot {
                server: None,
                requested: ControlEndpoints::default(),
                info: info.clone(),
                guard: None,
            }),
            token: info.token.clone(),
            reload_tx,
            cancel: cancel.clone(),
            started_at: events.started_at(),
            headless: opts.headless,
            channel,
            ui_host,
            updater,
            #[cfg(not(feature = "store"))]
            installer,
        });
        if !loaded.secret_failures.is_empty() {
            // Accounts whose keyring password could not be read start disabled; say so loudly.
            let errors: Vec<String> = loaded
                .secret_failures
                .iter()
                .map(ToString::to_string)
                .collect();
            for e in &errors {
                tracing::error!("config: {e}");
            }
            inner.set_last_error(Some(errors), false);
        }
        {
            let mut slot = inner.control.lock().await;
            if let Err(e) = inner.start_control(&mut slot, requested).await {
                drop(slot);
                stack.shutdown().await;
                return Err(e.into());
            }
        }

        let mut tasks = vec![
            tokio::spawn(bridge_sip_events(inner.clone(), sip_rx)),
            tokio::spawn(bridge_media_events(inner.clone(), media_rx)),
            tokio::spawn(reconfig::run_reload_actor(
                inner.clone(),
                reload_rx,
                loaded.hash,
            )),
        ];
        if inner.audio.is_some() {
            tasks.push(tokio::spawn(audio_poll(inner.clone())));
        }
        #[cfg(not(feature = "store"))]
        inner.installer.startup();
        // Last in the list: `shutdown` removes the reload task by index.
        tasks.push(crate::update::spawn_updater(
            inner.updater.clone(),
            inner.cancel.clone(),
        ));

        let watcher = if opts.watch_config {
            let tx = inner.reload_tx.clone();
            match ConfigWatcher::start(&config_path, opts.watch_debounce, move || {
                let _ = tx.send(ReloadRequest {
                    edits: Vec::new(),
                    base_hash: None,
                    force: false,
                    reason: "file change",
                    reply: None,
                });
            }) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!(
                        "cannot watch {}: {e}; use config.reload or SIGHUP",
                        config_path.display()
                    );
                    None
                }
            }
        } else {
            None
        };

        {
            let slot = inner.control.lock().await;
            events.emit(
                "app.started",
                None,
                None,
                json!({
                    "version": crate::update::full_version(),
                    "pid": std::process::id(),
                    "control": slot.info.control,
                }),
            );
        }

        Ok(App {
            inner,
            tasks,
            watcher,
            _subscriber: subscriber,
            _instance_lock: instance_lock,
            created_config,
            shutdown_timeout: opts.shutdown_timeout,
        })
    }

    /// Registers the GUI so `ui.*` and `app.open_uri` reach it.
    pub fn set_ui_host(&self, host: Arc<dyn crate::ui_host::UiHost>) {
        *self
            .inner
            .ui_host
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(host);
    }

    /// Whether this start wrote the example config because none existed.
    pub fn created_config(&self) -> bool {
        self.created_config
    }

    /// The config file in use (absolute).
    pub fn config_path(&self) -> &Path {
        &self.inner.config_path
    }

    /// The effective (last applied) config, with resolved paths and real passwords.
    pub fn config(&self) -> Arc<Config> {
        self.inner.config.load_full()
    }

    /// Current `[media]` settings; read this when a call starts ("next call" semantics).
    pub fn media_config(&self) -> MediaConfig {
        self.inner.config.load().media.clone()
    }

    /// The event bus.
    pub fn events(&self) -> &Arc<EventLog> {
        &self.inner.events
    }

    /// The SIP stack.
    pub fn stack(&self) -> &SipStack {
        &self.inner.stack
    }

    /// The media engine.
    pub fn media(&self) -> &Arc<MediaEngine> {
        &self.inner.media
    }

    /// The audio device layer (`None` when started with `enable_audio = false`).
    pub fn audio(&self) -> Option<&AudioHandle> {
        self.inner.audio.as_ref()
    }

    /// Why the audio devices are not in use (`None` while audio runs); fixed at startup.
    pub fn audio_disabled_reason(&self) -> Option<&'static str> {
        self.inner.audio_mode.disabled_reason
    }

    /// The SIP wire trace store.
    pub fn sip_trace(&self) -> &Arc<SipTraceWriter> {
        &self.inner.trace
    }

    /// Logging control.
    pub fn logging(&self) -> Option<&Arc<LoggingHandle>> {
        self.inner.logging.as_ref()
    }

    /// Effective audio devices and where each choice came from.
    pub fn effective_devices(&self) -> EffectiveDevices {
        self.inner.effective_devices()
    }

    /// Writes a device choice into `[audio]` of `config.toml` (through the ConfigActor, which
    /// reloads and applies it) and returns the new effective devices. Fails, with the file
    /// untouched, when the config file is invalid or changed underneath.
    pub async fn set_devices(&self, sel: &PartialSelection) -> Result<EffectiveDevices, String> {
        self.inner
            .set_user_devices(sel)
            .await
            .map_err(|e| e.to_string())
    }

    /// Where the control API actually listens (after path shortening / port resolution).
    pub async fn control_endpoints(&self) -> ControlEndpoints {
        let slot = self.inner.control.lock().await;
        slot.info.control.clone()
    }

    /// The per-run control token (also in `runtime.json`).
    pub fn token(&self) -> &Secret {
        &self.inner.token
    }

    /// Reloads the config file now (also used for SIGHUP and `config.reload`). Runs even if the
    /// file content is unchanged.
    pub async fn reload(&self) -> ReloadReport {
        self.inner.request_reload(true, "reload request").await
    }

    /// Rejects a ringing inbound call the way the GUI's Reject button does: with `[calls]
    /// decline_code` (default 486) when another call is live (a call-waiting decline), else with
    /// the plain default 486. `call.reject` takes an explicit `code` for everything else.
    pub async fn reject_call(&self, call_id: &str) -> Result<(), siple_sip::SipError> {
        // Call waiting: any other live call, on any account. The far end of this very call (the
        // same SIP Call-ID; a self-call between two local accounts) is not another call.
        let calls = self.inner.stack.calls();
        let sip_id = calls
            .iter()
            .find(|c| c.id == call_id)
            .map(|c| c.sip_call_id.clone());
        let other_live = calls.iter().any(|c| {
            c.id != call_id
                && c.state != siple_sip::CallState::Ended
                && Some(&c.sip_call_id) != sip_id.as_ref()
        });
        let code = other_live.then(|| self.inner.config.load().calls.decline_code);
        self.inner.stack.reject(call_id, code, None).await
    }

    /// Applies typed edits to `config.toml` (comments preserved, validated, written atomically)
    /// and reloads. `base_hash` (from [`App::config_view`]'s `hash`) makes the edit fail with
    /// [`EditFailure::Conflict`] when the file changed since it was read.
    pub async fn edit_config(
        &self,
        edits: Vec<ConfigEdit>,
        base_hash: Option<String>,
    ) -> Result<ReloadReport, EditFailure> {
        self.inner
            .request_edit(edits, base_hash, true, "config.edit")
            .await
    }

    /// Whether the first-run wizard was finished or skipped (`state.toml`).
    pub fn first_run_completed(&self) -> bool {
        StateStore::open(&self.inner.config_dir).first_run_completed()
    }

    /// Records that the first-run wizard was finished or skipped (`state.toml`).
    pub fn set_first_run_completed(&self) -> std::io::Result<()> {
        StateStore::open(&self.inner.config_dir).set_first_run_completed()
    }

    /// The remembered GUI panel choices (`state.toml`).
    pub fn gui_panels(&self) -> crate::state::GuiPanels {
        StateStore::open(&self.inner.config_dir).gui_panels()
    }

    /// Remembers GUI panel choices (`state.toml`); `None` slots are left alone.
    pub fn set_gui_panels(&self, panels: crate::state::GuiPanels) -> std::io::Result<()> {
        StateStore::open(&self.inner.config_dir).set_gui_panels(panels)
    }

    /// The update check's result as of the last check (`app.update_status`; no network access).
    pub fn update_status(&self) -> crate::update::UpdateStatus {
        self.inner.update_status()
    }

    /// Checks for a newer release now (`app.check_updates`; limited to one real check per minute).
    pub async fn check_updates(&self) -> crate::update::UpdateStatus {
        self.inner.check_updates().await
    }

    /// The release version the user chose to skip (`state.toml`), if any.
    pub fn skipped_update(&self) -> Option<String> {
        self.inner.updater.skipped()
    }

    /// Records that the user skipped release `version` (`state.toml`): the GUI stops offering that
    /// one, and offers the next newer one again. Not part of the control API: agents see
    /// `app.update_status` unfiltered.
    pub fn skip_update(&self, version: &str) -> std::io::Result<()> {
        self.inner.updater.set_skipped(Some(version.to_string()));
        StateStore::open(&self.inner.config_dir).set_skipped_update(Some(version))
    }

    /// Calls a control-API method in process, exactly as a control client would (`account.test`,
    /// `account.set_password`, `audio.echo_test`, ...): the GUI uses this so it has no
    /// capability an agent lacks.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, crate::control::ApiError> {
        use crate::control::ControlBackend;
        crate::backend::AppBackend::new(self.inner.clone())
            .call(method, params)
            .await
    }

    /// `config.view`: the effective config with per-field provenance, the file hash and path;
    /// secrets are masked.
    pub fn config_view(&self) -> Value {
        self.inner.config_view()
    }

    /// Persists an account enablement change and applies it through the ConfigActor.
    pub async fn set_account_enabled(&self, account: &str, enabled: bool) -> ReloadReport {
        let edit = ConfigEdit::SetAccount {
            id: account.to_string(),
            field: config::edit::AccountField::Enabled,
            value: config::edit::EditValue::Bool(enabled),
        };
        self.inner
            .request_edit(vec![edit], None, true, "account toggle")
            .await
            .unwrap_or_else(|f| ReloadReport {
                errors: vec![f.to_string()],
                ..Default::default()
            })
    }

    /// Reloads only if the file content differs from what was last seen (what the file watcher
    /// does).
    pub async fn reload_if_changed(&self) -> ReloadReport {
        self.inner.request_reload(false, "check").await
    }

    /// The last rejected config, if the most recent reload failed.
    pub fn last_config_error(&self) -> Option<ConfigErrorInfo> {
        lock(&self.inner.last_error).clone()
    }

    /// Orderly stop: unregister accounts (bounded by `shutdown_timeout`), stop the control
    /// server, then media and audio. `runtime.json` and the instance lock are released when the
    /// returned future completes.
    pub async fn shutdown(mut self) {
        let inner = self.inner.clone();
        tracing::info!("shutting down");
        inner.events.emit("app.stopping", None, None, Value::Null);
        drop(self.watcher.take());
        inner.tones.stop_all();
        // Stop accepting reloads before tearing the SIP side down.
        let reload_task = self.tasks.remove(2);
        reload_task.abort();
        if tokio::time::timeout(self.shutdown_timeout, inner.stack.shutdown())
            .await
            .is_err()
        {
            tracing::warn!("SIP shutdown timed out; continuing");
        }
        // Let the bridge deliver the final registration events.
        tokio::time::sleep(Duration::from_millis(50)).await;
        inner.cancel.cancel();
        {
            let mut slot = inner.control.lock().await;
            if let Some(server) = slot.server.take() {
                server.shutdown().await;
            }
            // Dropping the guard removes runtime.json.
            slot.guard = None;
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
        tracing::info!("stopped");
    }
}

impl Drop for App {
    /// Dropping without [`App::shutdown`] still stops the control listeners and background
    /// tasks (no orderly SIP unregister).
    fn drop(&mut self) {
        self.inner.cancel.cancel();
        for t in &self.tasks {
            t.abort();
        }
    }
}

// ---- event bridges ---------------------------------------------------------------------

/// Maps a SIP event to `(kind, account, call_id, data)`; `None` for events that are only logged.
/// Call events without an account (`call.media`, `call.dtmf`) get it from the bridge.
pub fn sip_event_to_wire(
    ev: &SipEvent,
) -> Option<(&'static str, Option<String>, Option<String>, Value)> {
    match ev {
        SipEvent::Registration { account, state } => Some((
            "account.registration",
            Some(account.clone()),
            None,
            serde_json::to_value(state).unwrap_or(Value::Null),
        )),
        SipEvent::MessageReceived {
            account,
            from,
            content_type,
            body,
        } => Some((
            "message.received",
            Some(account.clone()),
            None,
            json!({ "from": from, "content_type": content_type, "body": body }),
        )),
        SipEvent::UnhandledNotify {
            account,
            event,
            from,
            body,
        } => Some((
            "subscription.notify",
            Some(account.clone()),
            None,
            json!({ "event": event, "from": from, "body": body, "matched": false }),
        )),
        SipEvent::BlfState {
            account,
            uri,
            label,
            state,
            dialogs,
        } => Some((
            "blf.state",
            Some(account.clone()),
            None,
            json!({ "uri": uri, "label": label, "state": state, "dialogs": dialogs }),
        )),
        SipEvent::Mwi { account, status } => Some((
            "account.mwi",
            Some(account.clone()),
            None,
            serde_json::to_value(status).unwrap_or(Value::Null),
        )),
        SipEvent::SubscriptionNotify {
            account,
            sub_id,
            event,
            content_type,
            body,
            subscription_state,
        } => Some((
            "subscription.notify",
            Some(account.clone()),
            None,
            json!({
                "sub_id": sub_id, "event": event, "content_type": content_type,
                "body": body, "subscription_state": subscription_state, "matched": true,
            }),
        )),
        SipEvent::SubscriptionState {
            account,
            sub_id,
            uri,
            event,
            kind,
            state,
        } => {
            // {"state": "failed", "code": .., "reason": ..} merged into the event data.
            let mut data = json!({ "sub_id": sub_id, "uri": uri, "event": event, "kind": kind });
            if let (Some(d), Ok(Value::Object(st))) =
                (data.as_object_mut(), serde_json::to_value(state))
            {
                d.extend(st);
            }
            Some(("subscription.state", Some(account.clone()), None, data))
        }
        SipEvent::Dnd { account, enabled } => Some((
            "account.dnd",
            Some(account.clone()),
            None,
            json!({ "enabled": enabled }),
        )),
        SipEvent::NetworkChanged {
            generation,
            reasons,
            old_local_ip,
            new_local_ip,
        } => Some((
            "network.changed",
            None,
            None,
            json!({
                "generation": generation,
                "reasons": reasons,
                "old_local_ip": old_local_ip,
                "new_local_ip": new_local_ip,
            }),
        )),
        SipEvent::AccountReconnecting { account, reason } => Some((
            "account.reconnecting",
            Some(account.clone()),
            None,
            json!({ "reason": reason }),
        )),
        SipEvent::CallReconnecting { call_id, account } => Some((
            "call.reconnecting",
            Some(account.clone()),
            Some(call_id.clone()),
            json!({ "call_id": call_id, "account": account }),
        )),
        SipEvent::CallReconnected { call_id, account } => Some((
            "call.reconnected",
            Some(account.clone()),
            Some(call_id.clone()),
            json!({ "call_id": call_id }),
        )),
        SipEvent::TransportClosed { account, remote } => {
            tracing::info!(%account, %remote, "transport closed");
            None
        }
        SipEvent::OptionsReceived { account, from } => {
            tracing::debug!(%account, %from, "OPTIONS received");
            None
        }
        SipEvent::CallIncoming { call } => Some((
            "call.incoming",
            Some(call.account.clone()),
            Some(call.id.clone()),
            serde_json::to_value(call).unwrap_or(Value::Null),
        )),
        SipEvent::CallState { call } => Some((
            "call.state",
            Some(call.account.clone()),
            Some(call.id.clone()),
            serde_json::to_value(call).unwrap_or(Value::Null),
        )),
        SipEvent::CallUpdated { call } => Some((
            "call.updated",
            Some(call.account.clone()),
            Some(call.id.clone()),
            serde_json::to_value(call).unwrap_or(Value::Null),
        )),
        // The account is filled in by the bridge from the call.
        SipEvent::CallMedia {
            call_id,
            negotiated,
        } => Some((
            "call.media",
            None,
            Some(call_id.clone()),
            serde_json::to_value(negotiated).unwrap_or(Value::Null),
        )),
        SipEvent::CallDtmf {
            call_id,
            digit,
            duration_ms,
            via,
        } => Some((
            "call.dtmf",
            None,
            Some(call_id.clone()),
            json!({ "digit": digit, "duration_ms": duration_ms, "via": via }),
        )),
        SipEvent::CallRejected {
            account,
            from,
            code,
            reason,
            call_id,
        } => Some((
            "call.rejected",
            Some(account.clone()),
            call_id.clone(),
            json!({ "from": from, "code": code, "reason": reason }),
        )),
        SipEvent::TransferProgress {
            call_id,
            code,
            reason,
            is_final,
        } => Some((
            "call.transfer_progress",
            None,
            Some(call_id.clone()),
            json!({ "code": code, "reason": reason, "final": is_final }),
        )),
        SipEvent::TransferSession { session } => Some((
            "transfer.session",
            Some(session.account.clone()),
            Some(session.original.clone()),
            json!({ "session": session }),
        )),
        SipEvent::ReferReceived {
            call_id,
            refer_to,
            referred_by,
            replaces,
        } => Some((
            "call.refer_received",
            None,
            Some(call_id.clone()),
            json!({ "refer_to": refer_to, "referred_by": referred_by, "replaces": replaces }),
        )),
    }
}

async fn bridge_sip_events(
    inner: Arc<Inner>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SipEvent>,
) {
    while let Some(ev) = rx.recv().await {
        // Tones first: they only depend on the snapshot in the event.
        if let SipEvent::CallIncoming { call }
        | SipEvent::CallState { call }
        | SipEvent::CallUpdated { call } = &ev
        {
            inner
                .tones
                .on_call(call, inner.call_media.device_bound(&call.id));
        }
        // The call is confirmed: media clips may now be transmitted (never reset).
        if let SipEvent::CallState { call } = &ev
            && matches!(
                call.state,
                siple_sip::CallState::Active
                    | siple_sip::CallState::Held
                    | siple_sip::CallState::RemoteHeld
            )
            && let Some(e) = inner.call_media.get(&call.id)
        {
            e.handle.mark_answered();
        }
        if let Some((kind, account, call_id, data)) = sip_event_to_wire(&ev) {
            let account = account.or_else(|| call_account(&inner, call_id.as_deref()));
            inner
                .events
                .emit(kind, account.as_deref(), call_id.as_deref(), data);
        }
    }
}

/// Account of a call, for events that only carry the call id.
fn call_account(inner: &Inner, call_id: Option<&str>) -> Option<String> {
    inner.stack.call(call_id?).map(|c| c.account)
}

/// Maps a media event to `(kind, call_id, data)`.
pub fn media_event_to_wire(ev: &MediaEvent) -> Option<(&'static str, Option<String>, Value)> {
    let call = (!ev.call_id.is_empty()).then(|| ev.call_id.clone());
    match &ev.kind {
        MediaEventKind::DtmfReceived {
            digit, duration_ms, ..
        } => Some((
            "call.dtmf",
            call,
            json!({ "digit": digit.to_string(), "duration_ms": duration_ms, "via": "rfc4733" }),
        )),
        MediaEventKind::MediaTimeout => Some(("call.media_timeout", call, json!({}))),
        MediaEventKind::RecordFinished { path } => Some((
            "call.record_finished",
            call,
            json!({ "path": path.display().to_string() }),
        )),
        MediaEventKind::PlayFinished {
            play_id,
            reason,
            played_ms,
            rx_ms,
            barge_in_rx_ms,
        } => {
            let mut data = json!({
                "play_id": play_id,
                "reason": reason.as_str(),
                "played_ms": played_ms,
                "rx_ms": rx_ms,
            });
            if let Some(b) = barge_in_rx_ms {
                data["barge_in_rx_ms"] = (*b).into();
            }
            Some(("call.play_finished", call, data))
        }
        MediaEventKind::RxActive(active) => {
            tracing::debug!(call = ?call, active, "remote audio");
            None
        }
        MediaEventKind::SrtpAuthFailures(n) => {
            tracing::warn!(call = ?call, failures = n, "SRTP authentication failures");
            None
        }
        MediaEventKind::Error(msg) => {
            tracing::warn!(call = ?call, "media error: {msg}");
            Some(("call.media_error", call, json!({ "message": msg })))
        }
    }
}

async fn bridge_media_events(
    inner: Arc<Inner>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<MediaEvent>,
) {
    while let Some(ev) = rx.recv().await {
        if let Some((kind, call, data)) = media_event_to_wire(&ev) {
            let account = call_account(&inner, call.as_deref());
            inner
                .events
                .emit(kind, account.as_deref(), call.as_deref(), data);
        }
    }
}

/// Polls the audio layer every second and emits `audio.devices_changed` when the device lists
/// or the active/effective devices differ from the previous poll.
async fn audio_poll(inner: Arc<Inner>) {
    let Some(audio) = inner.audio.clone() else {
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous: Option<Value> = None;
    loop {
        tokio::select! {
            _ = inner.cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let a = audio.clone();
        let Ok((status, list)) = tokio::task::spawn_blocking(move || (a.status(), a.list())).await
        else {
            continue;
        };
        let active = |d: &Option<crate::audio::ActiveDevice>| {
            d.as_ref()
                .map(|d| json!({ "name": d.name, "fallback_to_default": d.fallback_to_default }))
        };
        let names = |l: &[crate::audio::DeviceInfo]| -> Vec<Value> {
            l.iter()
                .map(|d| json!({ "name": d.name, "is_default": d.is_default }))
                .collect()
        };
        let now = json!({
            "inputs": names(&list.inputs),
            "outputs": names(&list.outputs),
            "active": {
                "input": active(&status.input),
                "output": active(&status.output),
                "ringer": active(&status.ringer),
            },
            "effective": inner.effective_devices(),
        });
        if let Some(prev) = &previous
            && *prev != now
        {
            tracing::info!(
                input = ?status.input.as_ref().map(|d| &d.name),
                output = ?status.output.as_ref().map(|d| &d.name),
                "audio devices changed"
            );
            inner
                .events
                .emit("audio.devices_changed", None, None, now.clone());
        }
        previous = Some(now);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn audio_mode_decisions() {
        use AudioBackend::{Auto, None as NoAudio};
        assert!(AudioMode::decide(true, Auto, true).enabled());
        assert_eq!(
            AudioMode::decide(true, Auto, false).disabled_reason,
            Some(NO_AUDIO_HOST_REASON)
        );
        assert_eq!(
            AudioMode::decide(true, NoAudio, true).disabled_reason,
            Some("[audio] backend = \"none\"")
        );
        assert!(!AudioMode::decide(false, Auto, true).enabled());
        assert!(AudioMode::decide(true, AudioBackend::Always, false).enabled());
        assert!(!AudioMode::decide(false, AudioBackend::Always, true).enabled());
    }

    use super::*;
    use siple_sip::RegState;

    #[test]
    fn sip_events_map_to_plan_kinds() {
        let (kind, account, _, data) = sip_event_to_wire(&SipEvent::Registration {
            account: "a".into(),
            state: RegState::Unregistered,
        })
        .unwrap();
        assert_eq!(
            (kind, account.as_deref()),
            ("account.registration", Some("a"))
        );
        assert_eq!(data["state"], "unregistered");

        let (kind, _, _, data) = sip_event_to_wire(&SipEvent::MessageReceived {
            account: "a".into(),
            from: "sip:x@y".into(),
            content_type: "text/plain".into(),
            body: "hi".into(),
        })
        .unwrap();
        assert_eq!(
            (kind, data["body"].as_str()),
            ("message.received", Some("hi"))
        );

        let (kind, ..) = sip_event_to_wire(&SipEvent::UnhandledNotify {
            account: "a".into(),
            event: "message-summary".into(),
            from: "sip:x@y".into(),
            body: String::new(),
        })
        .unwrap();
        assert_eq!(kind, "subscription.notify");
        assert!(
            sip_event_to_wire(&SipEvent::OptionsReceived {
                account: "a".into(),
                from: "f".into()
            })
            .is_none()
        );
    }

    #[test]
    fn media_events_map() {
        let ev = MediaEvent {
            call_id: "c1".into(),
            kind: MediaEventKind::MediaTimeout,
        };
        let (kind, call, _) = media_event_to_wire(&ev).unwrap();
        assert_eq!((kind, call.as_deref()), ("call.media_timeout", Some("c1")));
        let ev = MediaEvent {
            call_id: String::new(),
            kind: MediaEventKind::Error("x".into()),
        };
        let (kind, call, data) = media_event_to_wire(&ev).unwrap();
        assert_eq!((kind, call), ("call.media_error", None));
        assert_eq!(data["message"], "x");
    }
}
