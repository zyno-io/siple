//! Dynamic reconfiguration: loading and validating the config file, diffing the
//! effective config against the applied one, and applying the difference.
//!
//! Every apply runs inside the single reload actor task ([`run_reload_actor`]), so file-watcher,
//! SIGHUP and `config.reload` triggers are serialized and an apply is never cut short by a
//! disconnecting API client.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use siple_sip::{SipError, UpdateOutcome};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{AppError, Inner, ReloadReport, ReloadRequest};
use crate::config::edit::{self, ConfigEdit};
use crate::config::secrets::{self, SecretFailure, Vault};
use crate::config::{AccountConfig, Config, ConfigIssue, PasswordStore, Profile};
use crate::config_watch::{ContentHash, content_hash, hash_hex};
use crate::instance::ControlEndpoints;
use crate::logging::effective_filter;

// ---- loading ----------------------------------------------------------------------------

/// A config that parsed and validated.
pub struct LoadedConfig {
    /// The config, paths resolved against the config directory.
    pub config: Config,
    /// Hash of the file contents it came from.
    pub hash: ContentHash,
    /// The file contents it came from.
    pub text: String,
    /// Non-fatal problems (missing TLS files).
    pub warnings: Vec<ConfigIssue>,
    /// File modification time.
    pub mtime: DateTime<Utc>,
    /// Accounts whose keyring password could not be read: they are switched off in `config`
    /// (never started with an empty password) and reported by the reload.
    pub secret_failures: Vec<SecretFailure>,
}

/// Why a config could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// Unreadable or not valid TOML / schema.
    Read(crate::config::ConfigError),
    /// Semantic validation failed.
    Invalid(Vec<ConfigIssue>),
}

impl LoadError {
    /// One message per problem.
    pub fn messages(&self) -> Vec<String> {
        match self {
            LoadError::Read(e) => vec![e.to_string()],
            LoadError::Invalid(v) => v.iter().map(ToString::to_string).collect(),
        }
    }

    pub(crate) fn into_app_error(self, path: &Path) -> AppError {
        match self {
            LoadError::Read(e) => AppError::Config(e),
            LoadError::Invalid(issues) => AppError::Invalid {
                path: path.to_path_buf(),
                issues,
            },
        }
    }
}

/// Reads, parses, path-resolves and validates the config file.
pub fn load_config(path: &Path, config_dir: &Path) -> Result<LoadedConfig, LoadError> {
    let io = |source| {
        LoadError::Read(crate::config::ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    };
    let bytes = std::fs::read(path).map_err(io)?;
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());
    let text = String::from_utf8_lossy(&bytes);
    let mut config = Config::parse(&text).map_err(|e| {
        LoadError::Read(match e {
            crate::config::ConfigError::Parse {
                position,
                message,
                snippet,
                ..
            } => crate::config::ConfigError::Parse {
                path: Some(path.to_path_buf()),
                position,
                message,
                snippet,
            },
            other => other,
        })
    })?;
    config.resolve_paths(config_dir);
    config.validate().map_err(LoadError::Invalid)?;
    let warnings = config.warnings();
    // Once per load (never per REGISTER: a Keychain access can prompt). Only touches the vault
    // when an enabled account asks for it.
    let secret_failures = if config
        .accounts
        .iter()
        .any(|a| a.enabled && a.password_store == PasswordStore::Keyring)
    {
        secrets::resolve_passwords(&mut config, secrets::vault().as_ref())
    } else {
        Vec::new()
    };
    Ok(LoadedConfig {
        config,
        hash: content_hash(&bytes),
        text: text.into_owned(),
        warnings,
        mtime,
        secret_failures,
    })
}

// ---- diff -------------------------------------------------------------------------------

/// What differs between two configs, by area (pure; see [`diff`]).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ConfigDiff {
    /// Accounts only in the new config, in file order.
    pub accounts_added: Vec<AccountConfig>,
    /// Ids only in the old config.
    pub accounts_removed: Vec<String>,
    /// Accounts in both whose settings differ: new config + names of the changed options.
    pub accounts_changed: Vec<(AccountConfig, Vec<String>)>,
    /// `[general].log_level` (old, new).
    pub log_level: Option<(String, String)>,
    /// `user_agent` / `reconfig_force` changed.
    pub general_stack: bool,
    /// Options that only take effect at restart (`[general]` log settings, `audio.backend`).
    pub restart_required: Vec<&'static str>,
    /// `[control]` changed.
    pub control: bool,
    /// `[media]` changed (applies to new calls).
    pub media: bool,
    /// `[logging]` changed (hot; applies to new log lines and trace records).
    pub logging: bool,
    /// `[network]` changed (hot; applied to the running network monitor, no re-register).
    pub network: bool,
    /// `[audio]` device names changed (`backend` is reported via `restart_required`).
    pub audio: bool,
    /// `[speech]` changed (hot; jobs snapshot the config when they start).
    pub speech: bool,
    /// `[calls]` changed (hot; pushed to the SIP stack).
    pub calls: bool,
    /// `[ui]` changed (hot; nothing to push, the GUI reads the effective config).
    pub ui: bool,
    /// `[updates]` changed (hot; the update checker re-reads it).
    pub updates: bool,
}

impl ConfigDiff {
    /// Nothing to do.
    pub fn is_empty(&self) -> bool {
        *self == ConfigDiff::default()
    }
}

/// Names of the top-level options that differ between two accounts. Never contains values.
pub fn changed_fields(old: &AccountConfig, new: &AccountConfig) -> Vec<String> {
    let (Ok(Value::Object(a)), Ok(Value::Object(b))) =
        (serde_json::to_value(old), serde_json::to_value(new))
    else {
        return vec!["?".into()];
    };
    let mut out: Vec<String> = b
        .iter()
        .filter(|(k, v)| a.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    out.extend(a.keys().filter(|k| !b.contains_key(*k)).cloned());
    out
}

/// Computes the difference `old -> new`.
pub fn diff(old: &Config, new: &Config) -> ConfigDiff {
    let mut d = ConfigDiff::default();
    for a in &new.accounts {
        match old.accounts.iter().find(|o| o.id == a.id) {
            None => d.accounts_added.push(a.clone()),
            Some(o) if o != a => d.accounts_changed.push((a.clone(), changed_fields(o, a))),
            Some(_) => {}
        }
    }
    for o in &old.accounts {
        if !new.accounts.iter().any(|a| a.id == o.id) {
            d.accounts_removed.push(o.id.clone());
        }
    }
    if old.general.log_level != new.general.log_level {
        d.log_level = Some((old.general.log_level.clone(), new.general.log_level.clone()));
    }
    d.general_stack = old.general.user_agent != new.general.user_agent
        || old.general.reconfig_force != new.general.reconfig_force;
    if old.general.log_dir != new.general.log_dir {
        d.restart_required.push("general.log_dir");
    }
    if old.general.log_retention_days != new.general.log_retention_days {
        d.restart_required.push("general.log_retention_days");
    }
    d.control = old.control != new.control;
    d.network = old.network != new.network;
    d.media = old.media != new.media;
    d.logging = old.logging != new.logging;
    // `audio.backend` is fixed at startup; only the device names are hot.
    d.audio = (&old.audio.input, &old.audio.output, &old.audio.ringer)
        != (&new.audio.input, &new.audio.output, &new.audio.ringer);
    if old.audio.backend != new.audio.backend {
        d.restart_required.push("audio.backend");
    }
    d.speech = old.speech != new.speech;
    d.calls = old.calls != new.calls;
    d.ui = old.ui != new.ui;
    d.updates = old.updates != new.updates;
    d
}

// ---- applying ---------------------------------------------------------------------------

/// One applied (or failed) change.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Account started.
    AccountAdded(String),
    /// Account unregistered and stopped.
    AccountRemoved(String),
    /// Account settings updated.
    AccountUpdated {
        /// Account id.
        id: String,
        /// Names of the changed options.
        fields: Vec<String>,
        /// What the SIP layer did.
        outcome: UpdateOutcome,
    },
    /// The accounts (kept ones) are listed in a different order.
    AccountsReordered,
    /// Log filter replaced.
    LogLevel {
        /// Old value.
        from: String,
        /// New value.
        to: String,
    },
    /// `user_agent` / `reconfig_force` pushed to the SIP stack.
    General,
    /// Option that needs a restart.
    RestartRequired(&'static str),
    /// Control endpoints changed (only the changed ones were restarted); holds the endpoints now
    /// bound.
    Control(ControlEndpoints),
    /// `[media]` defaults replaced and pushed to the SIP stack.
    Media,
    /// `[logging]` redaction policy updated.
    Logging,
    /// `[network]` settings applied to the running network monitor.
    Network,
    /// Effective audio devices changed.
    Audio(String),
    /// `[speech]` commands/timeouts updated (running jobs keep their snapshot).
    Speech,
    /// `[calls]` updated and pushed to the SIP stack.
    Calls,
    /// `[ui]` updated (no account is touched).
    Ui,
    /// `[updates]` updated (the update checker follows it).
    Updates,
    /// A change could not be applied.
    Failed {
        /// What was attempted.
        what: String,
        /// Why not.
        error: String,
    },
}

impl Change {
    /// Account the change concerns.
    pub fn account(&self) -> Option<&str> {
        match self {
            Change::AccountAdded(id)
            | Change::AccountRemoved(id)
            | Change::AccountUpdated { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Whether it is a failure.
    pub fn is_failure(&self) -> bool {
        matches!(self, Change::Failed { .. })
    }
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::AccountAdded(id) => write!(f, "account added: {id}"),
            Change::AccountRemoved(id) => write!(f, "account removed: {id}"),
            Change::AccountUpdated {
                id,
                fields,
                outcome,
            } => {
                let how = match outcome {
                    UpdateOutcome::Hot => "applied in place",
                    UpdateOutcome::Reregistered => "re-registering",
                    UpdateOutcome::Rebuilt => "transport rebuilt",
                    UpdateOutcome::RebuildDeferred => "transport rebuild deferred until calls end",
                };
                write!(f, "account changed: {id} ({}): {how}", fields.join(", "))
            }
            Change::AccountsReordered => write!(f, "account order changed"),
            Change::LogLevel { from, to } => write!(f, "log level: {from:?} -> {to:?}"),
            Change::General => write!(f, "general: user_agent/reconfig_force updated"),
            Change::RestartRequired(o) => write!(f, "{o} changed; takes effect after restart"),
            Change::Control(e) => write!(
                f,
                "control endpoints changed (unchanged ones keep their connections): {}",
                [
                    e.unix_socket.as_deref().map(|s| format!("unix={s}")),
                    e.named_pipe.as_deref().map(|s| format!("pipe={s}")),
                    e.tcp.as_deref().map(|s| format!("tcp={s}")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ")
            ),
            Change::Media => write!(
                f,
                "media settings updated (new calls use the new codecs/SDP options; DTMF options also apply to running calls; running audio streams are not renegotiated)"
            ),
            Change::Network => write!(
                f,
                "network settings updated (applied to the running monitor; no re-register)"
            ),
            Change::Audio(d) => write!(f, "audio devices: {d}"),
            Change::Speech => write!(
                f,
                "speech settings updated (running jobs keep the old ones)"
            ),
            Change::Calls => write!(
                f,
                "calls settings updated (hold_scope applies to the next auto-hold, resume_after_failed_transfer to the next failed transfer)"
            ),
            Change::Logging => write!(
                f,
                "logging settings updated (redaction applies to new log lines and trace records)"
            ),
            Change::Ui => write!(f, "ui settings updated"),
            Change::Updates => write!(f, "update check settings updated"),
            Change::Failed { what, error } => write!(f, "FAILED {what}: {error}"),
        }
    }
}

impl Inner {
    /// A keyring read that fails while the account already runs with a keyring password (a
    /// locked keychain during an unrelated reload) must not switch it off and hang up its calls:
    /// the password in memory stays, with a warning.
    fn keep_running_keyring_passwords(&self, loaded: &mut LoadedConfig) {
        let old = self.config.load_full();
        let failures = std::mem::take(&mut loaded.secret_failures);
        for f in failures {
            // A missing entry is a decision (the password was cleared), not a failed read.
            let running = old.accounts.iter().find(|a| {
                !f.message.starts_with("no password in the keyring")
                    && a.id == f.account
                    && a.enabled
                    && a.password_store == PasswordStore::Keyring
                    && !a.password.is_empty()
            });
            let slot = loaded
                .config
                .accounts
                .iter_mut()
                .find(|a| a.id == f.account);
            match (running, slot) {
                (Some(r), Some(a)) => {
                    tracing::warn!(account = %f.account, "keyring read failed ({}); keeping the password in use", f.message);
                    a.password = r.password.clone();
                    a.enabled = true;
                }
                _ => loaded.secret_failures.push(f),
            }
        }
    }

    /// Applies `old -> new` and returns the config that is now in effect (accounts whose change
    /// failed keep their previous state so the next reload retries) with the list of changes.
    pub(crate) async fn apply_config(
        self: &Arc<Self>,
        old: &Config,
        new: Config,
    ) -> (Config, Vec<Change>) {
        let d = diff(old, &new);
        let mut effective = new.clone();
        let mut out: Vec<Change> = Vec::new();
        let fail = |what: String, e: &dyn std::fmt::Display| Change::Failed {
            what,
            error: e.to_string(),
        };

        // [general]
        if let Some((from, to)) = &d.log_level
            && let Some(l) = &self.logging
        {
            match l.set_filter(&effective_filter(to)) {
                Ok(()) => out.push(Change::LogLevel {
                    from: from.clone(),
                    to: to.clone(),
                }),
                Err(e) => {
                    effective.general.log_level = from.clone();
                    out.push(fail("log level".into(), &e));
                }
            }
        }
        if d.general_stack {
            self.stack.set_general(&new.general);
            out.push(Change::General);
        }
        out.extend(
            d.restart_required
                .iter()
                .map(|o| Change::RestartRequired(o)),
        );

        // [media]: pushed to every account (SDP capabilities of new calls, DTMF options of
        // running ones) and read by the call code via the config snapshot.
        if d.media {
            self.stack.set_media(&new.media);
            out.push(Change::Media);
        }

        // [network]: hot, applied to the running monitor.
        if d.network {
            self.stack.set_network_config(&new.network);
            out.push(Change::Network);
        }
        // [calls]: read by the stack when an auto-hold runs.
        if d.calls {
            self.stack.set_hold_scope(new.calls.hold_scope);
            self.stack
                .set_resume_after_failed_transfer(new.calls.resume_after_failed_transfer);
            out.push(Change::Calls);
        }

        if d.logging {
            out.push(Change::Logging);
        }
        // [ui]: hot, no side effects here.
        if d.ui {
            out.push(Change::Ui);
        }
        // [updates]: hot; the checker is woken once the new config is stored.
        if d.updates {
            out.push(Change::Updates);
        }
        // Log/trace masking follows [logging] and the configured passwords (cheap; always).
        self.redactor
            .update(&new.logging, &new.accounts, &self.token);

        // Account display order is the file order (a pure reorder changes nothing else).
        self.stack.set_account_order(
            &new.accounts
                .iter()
                .map(|a| a.id.clone())
                .collect::<Vec<_>>(),
        );
        let kept = |a: &Config, b: &Config| -> Vec<String> {
            a.accounts
                .iter()
                .filter(|x| b.accounts.iter().any(|y| y.id == x.id))
                .map(|x| x.id.clone())
                .collect()
        };
        if kept(old, &new) != kept(&new, old) {
            out.push(Change::AccountsReordered);
        }
        // [[account]]: removals first (frees ports), then changes, then additions.
        let mut keep_old: Vec<String> = Vec::new(); // failed removals / updates
        let mut drop_new: Vec<String> = Vec::new(); // failed additions
        for id in &d.accounts_removed {
            match self.stack.remove_account(id).await {
                Ok(()) | Err(SipError::UnknownAccount(_)) => {
                    out.push(Change::AccountRemoved(id.clone()))
                }
                Err(e) => {
                    keep_old.push(id.clone());
                    out.push(fail(format!("remove account {id}"), &e));
                }
            }
        }
        for (cfg, fields) in &d.accounts_changed {
            match self.stack.update_account(cfg.clone()).await {
                Ok(outcome) => out.push(Change::AccountUpdated {
                    id: cfg.id.clone(),
                    fields: fields.clone(),
                    outcome,
                }),
                Err(e) => {
                    keep_old.push(cfg.id.clone());
                    out.push(fail(format!("update account {}", cfg.id), &e));
                }
            }
        }
        for cfg in &d.accounts_added {
            match self
                .stack
                .add_account(cfg.clone(), &new.general, &new.media)
                .await
            {
                Ok(()) => out.push(Change::AccountAdded(cfg.id.clone())),
                Err(e) => {
                    drop_new.push(cfg.id.clone());
                    out.push(fail(format!("add account {}", cfg.id), &e));
                }
            }
        }
        if !keep_old.is_empty() || !drop_new.is_empty() {
            let mut accounts = Vec::new();
            for a in &new.accounts {
                if drop_new.contains(&a.id) {
                    continue;
                }
                if keep_old.contains(&a.id)
                    && let Some(o) = old.accounts.iter().find(|o| o.id == a.id)
                {
                    accounts.push(o.clone());
                    continue;
                }
                accounts.push(a.clone());
            }
            for o in &old.accounts {
                if keep_old.contains(&o.id) && !new.accounts.iter().any(|a| a.id == o.id) {
                    accounts.push(o.clone());
                }
            }
            effective.accounts = accounts;
        }

        // [control]
        if d.control {
            match self.apply_control(&new).await {
                Ok(Some(e)) => out.push(Change::Control(e)),
                Ok(None) => {}
                Err(e) => {
                    effective.control = old.control.clone();
                    out.push(fail("control endpoints".into(), &e));
                }
            }
        }

        // [speech]: read from the config snapshot when a job starts; nothing to push.
        if d.speech {
            crate::agent::speech::warn_if_unsupported(&new.speech, &self.channel);
            out.push(Change::Speech);
        }

        // [audio]: the config is the only source of devices.
        if d.audio {
            let (eff, changed) = self.sync_audio(&new.audio);
            if changed {
                out.push(Change::Audio(eff.describe()));
            }
        }
        (effective, out)
    }

    /// Moves the control server to the endpoints implied by `new`, restarting only the
    /// endpoints that changed (unchanged listeners keep their connections; replaced ones close
    /// theirs after a short grace so a pending reply still gets out). Returns the new actual
    /// endpoints, or `None` if nothing changed.
    async fn apply_control(
        self: &Arc<Self>,
        new: &Config,
    ) -> Result<Option<ControlEndpoints>, String> {
        let requested = ControlEndpoints::from_config(&new.control, &self.config_dir);
        let mut slot = self.control.lock().await;
        if slot.requested == requested {
            return Ok(None);
        }
        let previous = slot.requested.clone();
        let Some(server) = slot.server.as_mut() else {
            // No server (should not happen after a successful start): start one.
            return match self.start_control(&mut slot, requested).await {
                Ok(()) => Ok(Some(slot.info.control.clone())),
                Err(e) => Err(e.to_string()),
            };
        };
        // On failure `reconfigure` leaves the running endpoints untouched.
        server
            .reconfigure(&requested)
            .await
            .map_err(|e| e.to_string())?;
        if let Err(e) = self.publish_runtime(&mut slot) {
            tracing::error!("cannot write runtime.json: {e}; restoring the previous endpoints");
            if let Some(server) = slot.server.as_mut() {
                if let Err(e2) = server.reconfigure(&previous).await {
                    tracing::error!("cannot restore control endpoints: {e2}");
                }
                let _ = self.publish_runtime(&mut slot);
            }
            return Err(e.to_string());
        }
        slot.requested = requested;
        Ok(Some(slot.info.control.clone()))
    }

    /// One reload: read, validate, diff, apply, report. `last_hash` is the hash of the file
    /// content seen last (accepted or not).
    pub(crate) async fn reload_once(
        self: &Arc<Self>,
        last_hash: &mut ContentHash,
        force: bool,
        reason: &'static str,
    ) -> ReloadReport {
        let mut loaded = match load_config(&self.config_path, &self.config_dir) {
            Ok(l) => l,
            Err(e) => {
                // A hash of what is on disk now, so an unchanged bad file is not re-reported by
                // the watcher. (An unreadable file leaves the hash alone.)
                if let Ok(bytes) = std::fs::read(&self.config_path) {
                    let h = content_hash(&bytes);
                    if h == *last_hash && !force {
                        return ReloadReport::default();
                    }
                    *last_hash = h;
                }
                let errors = e.messages();
                tracing::warn!(
                    path = %self.config_path.display(),
                    reason,
                    "config rejected, keeping the previous configuration: {}",
                    errors.join("; ")
                );
                self.set_last_error(Some(errors.clone()), true);
                self.events.emit(
                    "config.error",
                    None,
                    None,
                    json!({ "path": self.config_path.display().to_string(), "stage": "load", "errors": errors }),
                );
                return ReloadReport {
                    errors,
                    ..Default::default()
                };
            }
        };
        if loaded.hash == *last_hash && !force {
            // Readable and valid again with the contents that are already applied (e.g. the
            // file was deleted and restored): the earlier load error no longer applies.
            self.clear_load_error();
            tracing::debug!("config file rewritten with identical contents; ignored");
            return ReloadReport::default();
        }
        *last_hash = loaded.hash;
        self.keep_running_keyring_passwords(&mut loaded);
        for w in &loaded.warnings {
            tracing::warn!("config warning: {w}");
        }
        self.set_last_error(None, false);

        let old = self.config.load_full();
        let (effective, changes) = self.apply_config(&old, loaded.config).await;
        self.config.store(Arc::new(effective));
        if changes.iter().any(|c| matches!(c, Change::Updates)) {
            self.updater.config_changed();
            #[cfg(not(feature = "store"))]
            self.installer.config_changed();
        }
        // After the config: a reader that sees the new hash also sees the new config (the other
        // way round it only sees a stale hash, which makes an edit fail safe with a conflict).
        *self
            .applied_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(AppliedFile {
            hash: loaded.hash,
            text: loaded.text.clone(),
        });
        *self
            .applied_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Utc::now();

        let mut report = ReloadReport {
            applied: true,
            ..Default::default()
        };
        for f in &loaded.secret_failures {
            tracing::error!(account = %f.account, "config: {f}");
            report.errors.push(f.to_string());
        }
        for c in &changes {
            if c.is_failure() {
                tracing::error!(account = c.account(), "config: {c}");
                report.errors.push(c.to_string());
            } else {
                tracing::info!(account = c.account(), "config: {c}");
                report.summary.push(c.to_string());
            }
        }
        report.changed = !report.summary.is_empty();
        if report.changed {
            self.events.emit(
                "config.applied",
                None,
                None,
                json!({ "summary": report.summary, "source": reason }),
            );
        } else if report.errors.is_empty() {
            tracing::debug!("config reloaded; no effective changes");
        }
        if !report.errors.is_empty() {
            self.set_last_error(Some(report.errors.clone()), false);
            self.events.emit(
                "config.error",
                None,
                None,
                json!({ "path": self.config_path.display().to_string(), "stage": "apply", "errors": report.errors }),
            );
        }
        report
    }
}

// ---- typed edits (ConfigActor) ----------------------------------------------------------------

/// The config file text and hash last accepted by a reload: the base of `config.view` and the
/// provenance it reports.
#[derive(Debug)]
pub(crate) struct AppliedFile {
    pub hash: ContentHash,
    pub text: String,
}

/// One problem with an edit or with the config the edit would produce.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FieldError {
    /// Dotted path of the offending key (`account[0].username`, `ui.theme`); empty when the
    /// problem is not tied to a key.
    pub path: String,
    /// What is wrong. Never contains a submitted value.
    pub message: String,
}

/// Why a typed edit was refused. The file is untouched in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditFailure {
    /// `base_hash` is not the hash of the file on disk (it changed since the caller read it).
    Conflict {
        /// Hash of the file as it is now.
        current_hash: String,
    },
    /// The file as it is now does not load, so it is not edited (fix it first).
    FileInvalid {
        /// What is wrong with the file.
        errors: Vec<String>,
    },
    /// An edit does not apply, or the edited file would not be valid.
    Invalid {
        /// One entry per problem, with the path of the key.
        errors: Vec<FieldError>,
    },
    /// The file could not be read or written.
    Io(String),
    /// The application is shutting down.
    Stopping,
}

impl std::fmt::Display for EditFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditFailure::Conflict { .. } => f.write_str(
                "the config file changed since it was read (base_hash mismatch); read it again and retry",
            ),
            EditFailure::FileInvalid { errors } => write!(
                f,
                "the config file is invalid, so it is not edited; fix it first: {}",
                errors.join("; ")
            ),
            EditFailure::Invalid { errors } => write!(
                f,
                "{}",
                errors
                    .iter()
                    .map(|e| if e.path.is_empty() {
                        e.message.clone()
                    } else {
                        format!("{}: {}", e.path, e.message)
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            EditFailure::Io(e) => f.write_str(e),
            EditFailure::Stopping => f.write_str("the application is shutting down"),
        }
    }
}

/// Parses, resolves and validates `text` as the loader would.
fn check_text(text: &str, config_dir: &Path) -> Result<(), Vec<FieldError>> {
    let mut config = Config::parse(text).map_err(|e| {
        vec![FieldError {
            path: String::new(),
            message: e.to_string(),
        }]
    })?;
    config.resolve_paths(config_dir);
    config.validate().map_err(|issues| {
        issues
            .into_iter()
            .map(|i| FieldError {
                path: i.path,
                message: i.message,
            })
            .collect()
    })
}

// ---- keyring side effects of edits -------------------------------------------------------------

/// What a batch of edits does to the credential store, besides the file.
#[derive(Default)]
struct KeyringPlan {
    /// Entries created or overwritten before the write; removed again if the write fails.
    created: Vec<(String, Option<String>)>,
    /// Entries to delete once the file was written (removed accounts, old ids of renames,
    /// passwords moved back into the file).
    obsolete: Vec<String>,
    /// Ids that are keyring accounts after the batch.
    live: Vec<String>,
}

fn keyring_failure(path: &str, e: &secrets::VaultError) -> EditFailure {
    EditFailure::Invalid {
        errors: vec![FieldError {
            path: path.to_string(),
            message: e.to_string(),
        }],
    }
}

impl KeyringPlan {
    /// Writes an entry, remembering what it replaces so a failed batch can restore it.
    fn put(&mut self, vault: &dyn Vault, id: &str, password: &str) -> Result<(), EditFailure> {
        let prev = vault
            .get(id)
            .map_err(|e| keyring_failure("password_store", &e))?;
        vault
            .set(id, password)
            .map_err(|e| keyring_failure("password_store", &e))?;
        self.created.push((id.to_string(), prev));
        Ok(())
    }

    fn rollback(&self, vault: &dyn Vault) {
        for (id, prev) in &self.created {
            let undone = match prev {
                Some(p) => vault.set(id, p),
                None => vault.delete(id),
            };
            if let Err(e) = undone {
                tracing::warn!(account = %id, "cannot undo a keyring change: {e}");
            }
        }
    }

    fn commit(&self, vault: &dyn Vault) {
        for id in &self.obsolete {
            // Re-created by the same batch (remove + add, swaps): the entry is live again.
            if self.live.iter().any(|l| l.eq_ignore_ascii_case(id)) {
                continue;
            }
            if let Err(e) = vault.delete(id) {
                tracing::warn!(account = %id, "cannot delete the keyring entry: {e}");
            }
        }
    }
}

/// Plans the credential-store half of `edits` against the file as it is now (`before`):
/// - `remove_account` of a keyring account deletes its entry; `rename_account` moves it;
/// - `password_store` file -> keyring moves the file password into the entry (and clears it in
///   the file); keyring -> file moves it back; `add_account` with `password_store = "keyring"`
///   and a `password` stores that password in the entry instead of the file.
///
/// Entries are written before the file (so a failure leaves the file alone) and deleted after.
/// Returns the edits to apply to the file, with the password moves folded in.
fn plan_keyring(
    before: &Config,
    edits: &[ConfigEdit],
    vault: &dyn Vault,
) -> Result<(Vec<ConfigEdit>, KeyringPlan), EditFailure> {
    use crate::config::edit::{AccountField, EditValue};
    use std::collections::HashMap;

    let mut plan = KeyringPlan::default();
    let mut out = Vec::with_capacity(edits.len());
    // id -> (store, password in the file), tracked through the batch.
    let mut state: HashMap<String, (PasswordStore, String)> = before
        .accounts
        .iter()
        .map(|a| {
            (
                a.id.clone(),
                (a.password_store, a.password.expose().to_string()),
            )
        })
        .collect();
    let sets_password = |id: &str| {
        edits.iter().any(|e| {
            matches!(e, ConfigEdit::SetAccount { id: i, field: AccountField::Password, .. }
                | ConfigEdit::ResetAccount { id: i, field: AccountField::Password } if i == id)
        })
    };
    let result = (|| -> Result<(), EditFailure> {
        for e in edits {
            match e {
                ConfigEdit::SetAccount {
                    id,
                    field: AccountField::PasswordStore,
                    value: EditValue::Str(v),
                } if matches!(v.as_str(), "keyring" | "file") => {
                    let cur = state
                        .get(id)
                        .cloned()
                        .unwrap_or((PasswordStore::File, String::new()));
                    out.push(e.clone());
                    if v == "keyring" && cur.0 == PasswordStore::File {
                        if !cur.1.is_empty() && !sets_password(id) {
                            plan.put(vault, id, &cur.1)?;
                            out.push(set_password_edit(id, ""));
                        }
                        state.insert(id.clone(), (PasswordStore::Keyring, String::new()));
                    } else if v == "file" && cur.0 == PasswordStore::Keyring {
                        if !sets_password(id)
                            && let Some(p) = vault
                                .get(id)
                                .map_err(|e| keyring_failure("password_store", &e))?
                        {
                            out.push(set_password_edit(id, &p));
                        }
                        plan.obsolete.push(id.clone());
                        state.insert(id.clone(), (PasswordStore::File, String::new()));
                    }
                }
                ConfigEdit::ResetAccount {
                    id,
                    field: AccountField::PasswordStore,
                } => {
                    // Back to the default, `file`.
                    let cur = state
                        .get(id)
                        .cloned()
                        .unwrap_or((PasswordStore::File, String::new()));
                    out.push(e.clone());
                    if cur.0 == PasswordStore::Keyring {
                        if !sets_password(id)
                            && let Some(p) = vault
                                .get(id)
                                .map_err(|e| keyring_failure("password_store", &e))?
                        {
                            out.push(set_password_edit(id, &p));
                        }
                        plan.obsolete.push(id.clone());
                        state.insert(id.clone(), (PasswordStore::File, String::new()));
                    }
                }
                ConfigEdit::RemoveAccount { id } => {
                    if state
                        .remove(id)
                        .is_some_and(|(s, _)| s == PasswordStore::Keyring)
                    {
                        plan.obsolete.push(id.clone());
                    }
                    out.push(e.clone());
                }
                ConfigEdit::RenameAccount { from, to } => {
                    // Same id, or an id another account has: `apply_edits` refuses or ignores it,
                    // and the credential store must not be touched.
                    let clash = from.eq_ignore_ascii_case(to)
                        || state.keys().any(|k| k.eq_ignore_ascii_case(to));
                    if !clash && let Some(cur) = state.remove(from) {
                        if cur.0 == PasswordStore::Keyring {
                            if let Some(p) = vault
                                .get(from)
                                .map_err(|e| keyring_failure("password_store", &e))?
                            {
                                plan.put(vault, to, &p)?;
                            }
                            plan.obsolete.push(from.clone());
                        }
                        state.insert(to.clone(), cur);
                    }
                    out.push(e.clone());
                }
                ConfigEdit::AddAccount {
                    draft,
                    replace_example,
                } => {
                    let mut draft = draft.clone();
                    let keyring = matches!(
                        draft.fields.get(&AccountField::PasswordStore),
                        Some(EditValue::Str(s)) if s == "keyring"
                    );
                    let taken = state.keys().any(|k| k.eq_ignore_ascii_case(&draft.id));
                    if keyring
                        && !taken
                        && let Some(EditValue::Str(p)) =
                            draft.fields.remove(&AccountField::Password)
                        && !p.is_empty()
                    {
                        if crate::config::edit::is_password_mask(&p) {
                            return Err(keyring_failure(
                                "password",
                                &secrets::VaultError::Failed(
                                    "that is the mask of a hidden password, not a password".into(),
                                ),
                            ));
                        }
                        plan.put(vault, &draft.id, &p)?;
                    }
                    state.insert(
                        draft.id.clone(),
                        (
                            if keyring {
                                PasswordStore::Keyring
                            } else {
                                PasswordStore::File
                            },
                            String::new(),
                        ),
                    );
                    out.push(ConfigEdit::AddAccount {
                        draft,
                        replace_example: *replace_example,
                    });
                }
                other => out.push(other.clone()),
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            plan.live = state
                .iter()
                .filter(|(_, (s, _))| *s == PasswordStore::Keyring)
                .map(|(id, _)| id.clone())
                .collect();
            Ok((out, plan))
        }
        Err(e) => {
            plan.rollback(vault);
            Err(e)
        }
    }
}

fn set_password_edit(id: &str, password: &str) -> ConfigEdit {
    ConfigEdit::SetAccount {
        id: id.to_string(),
        field: crate::config::edit::AccountField::Password,
        value: crate::config::edit::EditValue::Str(password.to_string()),
    }
}

/// What [`write_edits`] did.
pub(crate) struct Written {
    /// Whether the file text changed (an edit that changes nothing is not written).
    pub changed: bool,
}

/// The write half of the ConfigActor: checks `base_hash`, applies `edits` to the file text with
/// `toml_edit` (comments and layout preserved), re-validates the result like the loader would,
/// and replaces the file atomically (temp file with the same permissions, fsync, re-read compare,
/// rename, parent-directory fsync). Nothing is written unless every step succeeds.
pub(crate) fn write_edits(
    config_path: &Path,
    config_dir: &Path,
    edits: &[ConfigEdit],
    base_hash: Option<&str>,
) -> Result<Written, EditFailure> {
    let io = |what: &str, e: std::io::Error| EditFailure::Io(format!("{what}: {e}"));
    let path =
        std::fs::canonicalize(config_path).map_err(|e| io("cannot resolve the config path", e))?;
    let bytes = std::fs::read(&path).map_err(|e| io("cannot read the config file", e))?;
    let current = content_hash(&bytes);
    // A file that does not even parse is reported as such, before any hash comparison (a
    // conflict could never be cleared by re-reading it). Semantic problems do not block an edit:
    // the edited result is the gate, so an edit can repair them (e.g. disable the bad account).
    let text = String::from_utf8(bytes).map_err(|_| EditFailure::FileInvalid {
        errors: vec!["the config file is not valid UTF-8".into()],
    })?;
    let mut before = None;
    if !edits.is_empty() || base_hash.is_some() {
        before = Some(Config::parse(&text).map_err(|e| EditFailure::FileInvalid {
            errors: vec![e.to_string()],
        })?);
    }
    if let Some(base) = base_hash
        && !base.eq_ignore_ascii_case(&hash_hex(&current))
    {
        return Err(EditFailure::Conflict {
            current_hash: hash_hex(&current),
        });
    }
    if edits.is_empty() {
        return Ok(Written { changed: false });
    }
    // Credential-store half first (entries are written before the file and deleted after it).
    let vault = secrets::vault();
    let (edits, keyring) = plan_keyring(&before.unwrap_or_default(), edits, vault.as_ref())?;
    let result = write_planned(&path, config_dir, &edits, &text, &current);
    match &result {
        Ok(Written { changed: true }) => keyring.commit(vault.as_ref()),
        _ => keyring.rollback(vault.as_ref()),
    }
    result
}

/// The file half of [`write_edits`]: applies `edits` to `text` (the content whose hash is
/// `current`) and replaces the file at `path`.
fn write_planned(
    path: &Path,
    config_dir: &Path,
    edits: &[ConfigEdit],
    text: &str,
    current: &ContentHash,
) -> Result<Written, EditFailure> {
    use std::io::Write;

    let io = |what: &str, e: std::io::Error| EditFailure::Io(format!("{what}: {e}"));
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| EditFailure::FileInvalid {
            errors: vec!["the config file is not valid TOML".into()],
        })?;
    edit::apply_edits(&mut doc, edits).map_err(|e| EditFailure::Invalid {
        errors: vec![FieldError {
            path: e.path,
            message: e.message,
        }],
    })?;
    let updated = doc.to_string();
    check_text(&updated, config_dir).map_err(|errors| EditFailure::Invalid { errors })?;
    if updated == text {
        return Ok(Written { changed: false });
    }

    let dir = path.parent().unwrap_or(Path::new("."));
    let meta = std::fs::metadata(path).map_err(|e| io("cannot stat the config file", e))?;
    #[cfg(unix)]
    let read_only = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o200 == 0
    };
    #[cfg(not(unix))]
    let read_only = meta.permissions().readonly();
    if read_only {
        return Err(EditFailure::Io(format!(
            "{} is read-only; make it writable to change settings from here",
            path.display()
        )));
    }
    let mut tmp =
        tempfile::NamedTempFile::new_in(dir).map_err(|e| io("cannot create a temp file", e))?;
    let permissions = std::fs::metadata(path)
        .map_err(|e| io("cannot stat the config file", e))?
        .permissions();
    tmp.as_file()
        .set_permissions(permissions)
        .map_err(|e| io("cannot set permissions", e))?;
    tmp.write_all(updated.as_bytes())
        .map_err(|e| io("cannot write the config file", e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| io("cannot sync the config file", e))?;
    // Someone else wrote while we worked: do not overwrite their change.
    match std::fs::read(path) {
        Ok(now) if content_hash(&now) == *current => {}
        Ok(_) => {
            return Err(EditFailure::Conflict {
                current_hash: std::fs::read(path)
                    .map(|b| hash_hex(&content_hash(&b)))
                    .unwrap_or_default(),
            });
        }
        Err(e) => return Err(io("cannot re-read the config file", e)),
    }
    tmp.persist(path)
        .map_err(|e| io("cannot replace the config file", e.error))?;
    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        tracing::warn!("cannot sync {}: {e}", dir.display());
    }
    Ok(Written { changed: true })
}

// ---- config.view ------------------------------------------------------------------------------

/// Where an effective value comes from.
fn origin(in_file: bool, from_profile: bool) -> &'static str {
    if in_file {
        "file"
    } else if from_profile {
        "profile"
    } else {
        "default"
    }
}

/// The effective `config` (secrets masked) and the per-key `provenance` of every option: `file`
/// (the key is in the file), `profile` (an account key the file omits that the vendor profile
/// fills) or `default`. Accounts are keyed by id under `provenance.account`.
///
/// A password is never in the output: `{"set": true|false}` takes its place, and the type of an
/// edit value (a plain string, bool or integer) cannot hold such an object, so a masked value
/// can never be written back.
pub fn config_view(config: &Config, file_text: &str) -> (Value, Value) {
    let raw: toml::Table = toml::from_str(file_text).unwrap_or_default();
    let mut view = serde_json::to_value(config).unwrap_or(Value::Null);
    let mut provenance = serde_json::Map::new();

    if let Value::Object(root) = &view {
        for (section, value) in root {
            let Value::Object(keys) = value else { continue };
            let in_file = raw.get(section).and_then(toml::Value::as_table);
            let map: serde_json::Map<String, Value> = keys
                .keys()
                .map(|k| {
                    let present = in_file.is_some_and(|t| t.contains_key(k));
                    (k.clone(), Value::from(origin(present, false)))
                })
                .collect();
            provenance.insert(section.clone(), Value::Object(map));
        }
    }

    let file_accounts: &[toml::Value] = raw
        .get("account")
        .and_then(toml::Value::as_array)
        .map_or(&[], Vec::as_slice);
    let mut accounts = serde_json::Map::new();
    for a in &config.accounts {
        let file = file_accounts
            .iter()
            .filter_map(toml::Value::as_table)
            .find(|t| t.get("id").and_then(toml::Value::as_str) == Some(a.id.as_str()));
        let bundle: Vec<&str> = if a.profile == Profile::Generic {
            Vec::new()
        } else {
            a.profile.bundle().into_iter().map(|(k, _)| k).collect()
        };
        let Some(Value::Object(fields)) = serde_json::to_value(a).ok() else {
            continue;
        };
        let map: serde_json::Map<String, Value> = fields
            .keys()
            .map(|k| {
                let bundled = bundle.contains(&k.as_str());
                // Mirrors the profile resolver: an empty string for these two counts as unset.
                let unset = bundled
                    && matches!(k.as_str(), "user_agent" | "dtmf_mode")
                    && file.and_then(|t| t.get(k)).and_then(toml::Value::as_str) == Some("");
                let present = file.is_some_and(|t| t.contains_key(k)) && !unset;
                (k.clone(), Value::from(origin(present, bundled)))
            })
            .collect();
        accounts.insert(a.id.clone(), Value::Object(map));
    }
    provenance.insert("account".into(), Value::Object(accounts));

    if let Some(list) = view.get_mut("account").and_then(Value::as_array_mut) {
        for (v, a) in list.iter_mut().zip(&config.accounts) {
            v["password"] = json!({ "set": !a.password.is_empty() });
        }
    }
    (view, Value::Object(provenance))
}

impl Inner {
    /// `config.view`: see [`config_view`]. `hash` and the provenance describe the file text last
    /// accepted by a reload, the base to pass as `config.edit`'s `base_hash`.
    pub(crate) fn config_view(&self) -> Value {
        let applied = self
            .applied_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let config = self.config.load_full();
        let (view, provenance) = config_view(&config, &applied.text);
        json!({
            "path": self.config_path.display().to_string(),
            // The hash `config.edit` compares: the file as it is on disk now. When the file was
            // changed since the last accepted reload (a parses-but-invalid edit, say) it differs
            // from `accepted_hash`, and an edit based on it still goes through.
            "hash": std::fs::read(&self.config_path)
                .map_or_else(|_| hash_hex(&applied.hash), |b| hash_hex(&content_hash(&b))),
            "accepted_hash": hash_hex(&applied.hash),
            "config": view,
            "provenance": provenance,
            "secrets": { "control_token": { "set": !self.token.is_empty() } },
            "editable": {
                "account_fields": edit::AccountField::ALL.map(edit::AccountField::key),
                "sections": edit::editable_sections().into_iter()
                    .map(|s| (s, edit::editable_keys(s)))
                    .collect::<std::collections::BTreeMap<_, _>>(),
            },
        })
    }

    /// Hash (hex) of the config file text last accepted.
    pub(crate) fn applied_hash(&self) -> String {
        hash_hex(
            &self
                .applied_file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .hash,
        )
    }
}

/// The ConfigActor: serializes every reload request and typed edit.
pub(crate) async fn run_reload_actor(
    inner: Arc<Inner>,
    mut rx: UnboundedReceiver<ReloadRequest>,
    mut last_hash: ContentHash,
) {
    while let Some(req) = rx.recv().await {
        // A plain reload goes straight to the loader, which reports an unreadable file itself.
        let written = if req.edits.is_empty() && req.base_hash.is_none() {
            Ok(())
        } else {
            write_edits(
                &inner.config_path,
                &inner.config_dir,
                &req.edits,
                req.base_hash.as_deref(),
            )
            .map(|_| ())
        };
        let result = match written {
            Ok(()) => {
                let mut report = inner
                    .reload_once(&mut last_hash, req.force, req.reason)
                    .await;
                if report.applied {
                    report.hash = Some(inner.applied_hash());
                }
                Ok(report)
            }
            Err(failure) => Err(failure),
        };
        if let Some(reply) = req.reply {
            let _ = reply.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Secret, Transport};

    fn acct(id: &str) -> AccountConfig {
        AccountConfig {
            id: id.into(),
            username: "1001".into(),
            domain: "pbx.example".into(),
            password: Secret::new("pw"),
            register: false,
            transport: Transport::Udp,
            ..Default::default()
        }
    }

    fn cfg(ids: &[&str]) -> Config {
        Config {
            accounts: ids.iter().map(|i| acct(i)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn identical_configs_have_empty_diff() {
        assert!(diff(&cfg(&["a", "b"]), &cfg(&["a", "b"])).is_empty());
    }

    #[test]
    fn account_add_remove_change() {
        let old = cfg(&["a", "b"]);
        let mut new = cfg(&["b", "c"]);
        new.accounts[0].dnd = true;
        new.accounts[0].password = Secret::new("other");
        let d = diff(&old, &new);
        assert_eq!(d.accounts_removed, ["a"]);
        assert_eq!(d.accounts_added.len(), 1);
        assert_eq!(d.accounts_added[0].id, "c");
        assert_eq!(d.accounts_changed.len(), 1);
        let (c, fields) = &d.accounts_changed[0];
        assert_eq!(c.id, "b");
        let mut f = fields.clone();
        f.sort();
        assert_eq!(f, ["dnd", "password"]);
    }

    #[test]
    fn global_sections() {
        let old = Config::default();
        let mut new = old.clone();
        new.general.log_level = "debug".into();
        new.general.user_agent = "X/1".into();
        new.general.log_dir = "/elsewhere".into();
        new.control.tcp_listen = "127.0.0.1:5079".into();
        new.media.jitter_min_ms += 10;
        new.audio.input = "Mic".into();
        let d = diff(&old, &new);
        assert_eq!(d.log_level.as_ref().unwrap().1, "debug");
        assert!(d.general_stack && d.control && d.media && d.audio);
        assert_eq!(d.restart_required, ["general.log_dir"]);
        assert!(d.accounts_added.is_empty());
    }

    #[test]
    fn network_section_is_hot() {
        let old = Config::default();
        let mut new = old.clone();
        new.network.probe_secs = 0;
        new.network.debounce_ms = 500;
        new.network.watch = false;
        let d = diff(&old, &new);
        assert!(d.network);
        assert!(!d.control && !d.media && !d.general_stack && !d.audio);
        assert!(d.restart_required.is_empty() && d.accounts_changed.is_empty());
        assert!(!diff(&old, &old).network);
        // The new account key is hot: it changes the diff of the account but needs no re-register.
        let mut a = Config::default();
        a.accounts.push(acct("a"));
        let mut b = a.clone();
        b.accounts[0].keepalive_fail_threshold = 5;
        let d = diff(&a, &b);
        assert_eq!(d.accounts_changed.len(), 1);
        assert_eq!(d.accounts_changed[0].1, ["keepalive_fail_threshold"]);
    }

    #[test]
    fn audio_backend_needs_restart_and_speech_is_hot() {
        let old = Config::default();
        let mut new = old.clone();
        new.audio.backend = crate::config::AudioBackend::None;
        let d = diff(&old, &new);
        assert_eq!(d.restart_required, ["audio.backend"]);
        assert!(!d.audio && !d.speech && !d.media);
        let mut new = old.clone();
        new.speech.tts_command = vec!["say".into()];
        let d = diff(&old, &new);
        assert!(d.speech && !d.audio && d.restart_required.is_empty());
        assert!(!d.is_empty());
    }

    #[test]
    fn calls_hold_scope_is_hot() {
        let old = Config::default();
        let mut new = old.clone();
        new.calls.hold_scope = crate::config::HoldScope::Account;
        let d = diff(&old, &new);
        assert!(d.calls && !d.media && !d.speech && d.restart_required.is_empty());
        assert!(!d.is_empty());
        assert!(diff(&old, &old).is_empty());
        let mut new = old.clone();
        new.calls.resume_after_failed_transfer = false;
        let d = diff(&old, &new);
        assert!(d.calls && !d.media && d.restart_required.is_empty());
    }

    #[test]
    fn ui_section_is_hot_with_no_account_side_effects() {
        let mut a = Config::default();
        a.accounts.push(acct("a"));
        let mut b = a.clone();
        b.ui.theme = crate::config::Theme::Dark;
        b.ui.shortcuts.insert("answer".into(), "Mod+A".into());
        b.calls.decline_code = 603;
        let d = diff(&a, &b);
        assert!(d.ui && d.calls);
        assert!(d.accounts_changed.is_empty() && d.accounts_added.is_empty());
        assert!(d.accounts_removed.is_empty() && d.restart_required.is_empty());
        assert!(!d.media && !d.network && !d.audio && !d.control && !d.general_stack);
        assert!(!diff(&a, &a).ui);
        assert!(Change::Ui.to_string().contains("ui settings"));
    }

    const FILE: &str = "[[account]]\nid = \"y\"\nusername = \"1\"\npassword = \"pw-secret\"\n\
        domain = \"d\"\nprofile = \"yealink\"\ndnd_reject_code = 603\nuser_agent = \"\"\n\
        [[account]]\nid = \"g\"\nusername = \"2\"\ndomain = \"d\"\n[ui]\ntheme = \"dark\"\n";

    #[test]
    fn view_provenance_distinguishes_file_profile_and_default() {
        let cfg = Config::parse(FILE).unwrap();
        let (view, prov) = config_view(&cfg, FILE);
        let y = &prov["account"]["y"];
        assert_eq!(y["username"], "file");
        assert_eq!(y["dnd_reject_code"], "file");
        assert_eq!(y["hold_style"], "profile");
        assert_eq!(
            y["user_agent"], "profile",
            "an empty string counts as unset"
        );
        assert_eq!(y["dnd"], "default");
        // A generic account has no profile column.
        assert_eq!(prov["account"]["g"]["hold_style"], "default");
        assert_eq!(prov["ui"]["theme"], "file");
        assert_eq!(prov["ui"]["density"], "default");
        assert_eq!(prov["general"]["log_level"], "default");
        assert_eq!(view["account"][0]["dnd_reject_code"], 603);
        assert_eq!(view["account"][0]["hold_style"], "sendonly");
        // Secrets are masked everywhere.
        assert_eq!(view["account"][0]["password"], json!({"set": true}));
        assert_eq!(view["account"][1]["password"], json!({"set": false}));
        assert!(!view.to_string().contains("pw-secret"));
    }

    fn setup(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn set_ui(key: &str, value: &str) -> ConfigEdit {
        ConfigEdit::SetSection {
            section: "ui".into(),
            key: key.into(),
            value: crate::config::edit::EditValue::Str(value.into()),
        }
    }

    #[test]
    fn write_edits_is_atomic_checked_and_keeps_the_mode() {
        let (dir, path) = setup(FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let hash = hash_hex(&content_hash(FILE.as_bytes()));
        // Wrong base hash: conflict, nothing written.
        let e = write_edits(&path, dir.path(), &[set_ui("theme", "light")], Some("00")).err();
        assert_eq!(
            e,
            Some(EditFailure::Conflict {
                current_hash: hash.clone()
            })
        );
        // Bad value: invalid with the path.
        let e = write_edits(&path, dir.path(), &[set_ui("theme", "blue")], Some(&hash)).err();
        assert!(
            matches!(&e, Some(EditFailure::Invalid { errors }) if errors[0].path == "ui.theme"),
            "{e:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE);
        // Good edit.
        let w = write_edits(&path, dir.path(), &[set_ui("theme", "light")], Some(&hash))
            .ok()
            .unwrap();
        assert!(w.changed);
        let now = std::fs::read_to_string(&path).unwrap();
        assert_eq!(now, FILE.replace("\"dark\"", "\"light\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // No temp files left next to it.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // An edit that changes nothing writes nothing.
        let w = write_edits(&path, dir.path(), &[set_ui("theme", "light")], None)
            .ok()
            .unwrap();
        assert!(!w.changed);
        // A semantic problem in the result is reported by its config path.
        let bad = ConfigEdit::SetAccount {
            id: "g".into(),
            field: crate::config::edit::AccountField::Username,
            value: crate::config::edit::EditValue::Str("a b".into()),
        };
        let e = write_edits(&path, dir.path(), &[bad], None).err();
        assert!(
            matches!(&e, Some(EditFailure::Invalid { errors }) if errors[0].path == "account[1].username"),
            "{e:?}"
        );
        // While the file does not parse, edits are refused, and that is reported before a hash
        // conflict.
        std::fs::write(&path, "not [toml").unwrap();
        let e = write_edits(&path, dir.path(), &[set_ui("theme", "dark")], Some("00")).err();
        assert!(matches!(e, Some(EditFailure::FileInvalid { .. })), "{e:?}");
        // A read-only file is refused with a clear message, not replaced.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&path, FILE).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
            let e = write_edits(&path, dir.path(), &[set_ui("theme", "light")], None).err();
            assert!(
                matches!(&e, Some(EditFailure::Io(m)) if m.contains("read-only")),
                "{e:?}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE);
        }
    }

    #[test]
    fn disabling_the_bad_account_repairs_a_semantically_invalid_file() {
        // An enabled account without a username fails validation, but still parses.
        let broken = "[[account]]\nid = \"x\"\ndomain = \"d\"\n";
        let (dir, path) = setup(broken);
        assert!(Config::parse(broken).unwrap().validate().is_err());
        // An edit that leaves it invalid is refused with the path.
        let e = write_edits(&path, dir.path(), &[set_ui("theme", "dark")], None).err();
        assert!(
            matches!(&e, Some(EditFailure::Invalid { errors }) if errors.iter().any(|e| e.path == "account[0].username")),
            "{e:?}"
        );
        // Disabling it repairs the file.
        let off = ConfigEdit::SetAccount {
            id: "x".into(),
            field: crate::config::edit::AccountField::Enabled,
            value: crate::config::edit::EditValue::Bool(false),
        };
        assert!(
            write_edits(&path, dir.path(), &[off], None)
                .unwrap()
                .changed
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("enabled = false")
        );
        assert!(load_config(&path, dir.path()).is_ok());
    }

    #[test]
    fn edit_failures_read_clearly() {
        let f = EditFailure::Invalid {
            errors: vec![FieldError {
                path: "ui.theme".into(),
                message: "must be one of x".into(),
            }],
        };
        assert_eq!(f.to_string(), "ui.theme: must be one of x");
        assert!(
            EditFailure::Conflict {
                current_hash: "h".into()
            }
            .to_string()
            .contains("changed")
        );
        assert!(
            EditFailure::FileInvalid {
                errors: vec!["boom".into()]
            }
            .to_string()
            .contains("fix it first: boom")
        );
    }

    #[test]
    fn change_display_is_readable_and_value_free() {
        let c = Change::AccountUpdated {
            id: "a".into(),
            fields: vec!["password".into(), "expires".into()],
            outcome: UpdateOutcome::Reregistered,
        };
        let s = c.to_string();
        assert!(
            s.contains("password, expires") && s.contains("re-registering"),
            "{s}"
        );
    }
}
