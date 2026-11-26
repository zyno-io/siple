//! App-owned `state.toml`: runtime state that is not a setting, and the effective audio
//! devices.
//!
//! Audio devices are settings, so they live in `[audio]` of `config.toml` and nowhere else: the
//! GUI's pickers and `audio.set_devices` write that file through the config actor
//! ([`crate::reconfig`]), and [`resolve_devices`] just reads it. Older builds kept a device choice
//! in `state.toml` that overrode `[audio]` ("most recent wins"). That override is gone: the first
//! start of this build moves such a choice into `[audio]` once, slot by slot, exactly where it was in
//! effect under that rule, and deletes the legacy data from `state.toml`
//! ([`StateStore::legacy_devices`] / [`StateStore::clear_legacy`]).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::audio::DeviceSelection;
use crate::config::AudioConfig;

/// File name inside the config directory.
pub const STATE_FILE: &str = "state.toml";

/// The three device slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Microphone.
    Input,
    /// Speaker.
    Output,
    /// Ringer.
    Ringer,
}

impl Slot {
    /// All slots in a fixed order.
    pub const ALL: [Slot; 3] = [Slot::Input, Slot::Output, Slot::Ringer];

    /// Lower-case name (`input`, `output`, `ringer`), also the `[audio]` key.
    pub fn name(self) -> &'static str {
        match self {
            Slot::Input => "input",
            Slot::Output => "output",
            Slot::Ringer => "ringer",
        }
    }
}

/// A device choice (or a recorded config value) of an older build, with its time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LegacyChoice {
    name: String,
    #[serde(default)]
    at: Option<DateTime<Utc>>,
}

/// The `[audio]` / `[config_seen]` tables an older build wrote to `state.toml` (read for the
/// one-time migration).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
struct LegacySlots {
    input: Option<LegacyChoice>,
    output: Option<LegacyChoice>,
    ringer: Option<LegacyChoice>,
}

impl LegacySlots {
    fn is_empty(&self) -> bool {
        self.input.is_none() && self.output.is_none() && self.ringer.is_none()
    }

    fn slot(&self, slot: Slot) -> Option<&LegacyChoice> {
        match slot {
            Slot::Input => self.input.as_ref(),
            Slot::Output => self.output.as_ref(),
            Slot::Ringer => self.ringer.as_ref(),
        }
    }
}

/// Everything `state.toml` holds. Unknown tables (future keys) are ignored on read and dropped on
/// the next write.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StateFile {
    /// The first-run wizard was finished or skipped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    first_run_completed: bool,
    #[serde(default, skip_serializing_if = "LegacySlots::is_empty")]
    audio: LegacySlots,
    #[serde(default, skip_serializing_if = "LegacySlots::is_empty")]
    config_seen: LegacySlots,
    /// The one-time "still running in the tray" notice was shown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    tray_hint_shown: bool,
    /// GUI: the Lines rail was collapsed (`true`) or expanded (`false`) by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gui_lines_collapsed: Option<bool>,
    /// GUI: the extensions panel was opened or closed by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gui_extensions_open: Option<bool>,
    /// The release the user chose to skip in the update pill ("Skip this version").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    skipped_update: Option<String>,
    /// Wall-clock time of the last update check that found nothing newer (shown as `checked_at`
    /// after a restart; every start still checks again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_check: Option<DateTime<Utc>>,
    /// The release version `app.update_available` was last emitted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    announced_update: Option<String>,
    /// An in-place update was installed and its restart is (or was) pending: the new process
    /// reads this to confirm the version, announce it and remove what the swap left behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    update_installed: Option<InstalledUpdate>,
}

/// The record of an installed update (`[update_installed]` in `state.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledUpdate {
    /// The version that was running when the update was installed.
    pub from: String,
    /// The version that was installed.
    pub to: String,
    /// When the swap finished.
    pub at: DateTime<Utc>,
    /// Paths the swap left behind (the replaced copy, the staging directory); the new version
    /// removes them after it started, each only if the platform's rules accept the path.
    #[serde(default)]
    pub cleanup: Vec<String>,
}

/// Panel choices the GUI remembers across launches. `None` = never chosen (the GUI's default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuiPanels {
    /// Lines rail collapsed.
    pub lines_collapsed: Option<bool>,
    /// Extensions panel open.
    pub extensions_open: Option<bool>,
}

/// A partial device change (`audio.set_devices` params): `None` leaves the slot alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialSelection {
    /// New microphone.
    pub input: Option<String>,
    /// New speaker.
    pub output: Option<String>,
    /// New ringer.
    pub ringer: Option<String>,
}

impl PartialSelection {
    /// Whether no slot is set.
    pub fn is_empty(&self) -> bool {
        self.input.is_none() && self.output.is_none() && self.ringer.is_none()
    }

    /// The named slot.
    pub fn slot(&self, slot: Slot) -> Option<&String> {
        match slot {
            Slot::Input => self.input.as_ref(),
            Slot::Output => self.output.as_ref(),
            Slot::Ringer => self.ringer.as_ref(),
        }
    }
}

/// Where an effective device name comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSource {
    /// The `[audio]` section of the config file.
    Config,
    /// Nobody chose: the system default.
    Default,
}

/// Effective selection of one slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveSlot {
    /// Device name; `""` = system default.
    pub name: String,
    /// Who decided.
    pub source: DeviceSource,
}

/// Effective selection of all slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveDevices {
    /// Microphone.
    pub input: EffectiveSlot,
    /// Speaker.
    pub output: EffectiveSlot,
    /// Ringer.
    pub ringer: EffectiveSlot,
}

impl EffectiveDevices {
    /// The names, as the audio layer wants them.
    pub fn selection(&self) -> DeviceSelection {
        DeviceSelection {
            input: self.input.name.clone(),
            output: self.output.name.clone(),
            ringer: self.ringer.name.clone(),
        }
    }

    /// The slot.
    pub fn slot(&self, slot: Slot) -> &EffectiveSlot {
        match slot {
            Slot::Input => &self.input,
            Slot::Output => &self.output,
            Slot::Ringer => &self.ringer,
        }
    }

    /// One-line description, e.g. `input=Mic (config), output=<default> (default), ...`.
    pub fn describe(&self) -> String {
        Slot::ALL
            .iter()
            .map(|s| {
                let e = self.slot(*s);
                let src = match e.source {
                    DeviceSource::Config => "config",
                    DeviceSource::Default => "default",
                };
                let name = if e.name.is_empty() {
                    "<default>"
                } else {
                    &e.name
                };
                format!("{}={name} ({src})", s.name())
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The devices implied by `[audio]`: a non-empty name is the config's choice, `""` is the system
/// default.
pub fn resolve_devices(audio: &AudioConfig) -> EffectiveDevices {
    let slot = |name: &str| EffectiveSlot {
        name: name.to_string(),
        source: if name.is_empty() {
            DeviceSource::Default
        } else {
            DeviceSource::Config
        },
    };
    EffectiveDevices {
        input: slot(&audio.input),
        output: slot(&audio.output),
        ringer: slot(&audio.ringer),
    }
}

/// `<config dir>/state.toml`, loaded at start and rewritten atomically on every change.
pub struct StateStore {
    path: PathBuf,
    data: Mutex<StateFile>,
}

impl StateStore {
    /// Loads `<dir>/state.toml`. A missing file is an empty state; an unreadable or corrupt one
    /// is logged, moved aside to `state.toml.bad` and treated as empty.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(STATE_FILE);
        let data = match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<StateFile>(&text) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "state.toml is corrupt; starting with an empty state");
                    let _ = fs::rename(&path, dir.join(format!("{STATE_FILE}.bad")));
                    StateFile::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => StateFile::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read state.toml");
                StateFile::default()
            }
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    /// Path of the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The device choices that were *in effect* under the old "most recent change wins" rule,
    /// for the one-time move into `[audio]`; each slot of the result is to be written (a `""`
    /// writes the system default over an older config value). Slots where the config value was
    /// in effect, or that have no choice, are `None`. `audio` is the config as read now and
    /// `file_mtime` its modification time.
    ///
    /// Per slot the user choice won when it is not older than when the config value last changed:
    /// the time recorded in `config_seen` while the value was unchanged; the file's mtime when it
    /// was edited while stopped (a different value, or cleared); the epoch when nothing was
    /// recorded and the config is empty (no opinion); the mtime when nothing was recorded and it
    /// is set.
    pub fn legacy_devices(
        &self,
        audio: &AudioConfig,
        file_mtime: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> PartialSelection {
        let d = self.lock();
        let mtime = file_mtime.min(now);
        let pick = |slot: Slot, current: &str| {
            let choice = d.audio.slot(slot)?;
            let at = choice.at.map_or(DateTime::UNIX_EPOCH, |t| t.min(now));
            let config_at = match d.config_seen.slot(slot) {
                Some(c) if c.name == current => c.at.map_or(mtime, |t| t.min(now)),
                Some(c) if current.is_empty() && !c.name.is_empty() => mtime,
                None if current.is_empty() => DateTime::UNIX_EPOCH,
                _ => mtime,
            };
            (at >= config_at).then(|| choice.name.clone())
        };
        PartialSelection {
            input: pick(Slot::Input, &audio.input),
            output: pick(Slot::Output, &audio.output),
            ringer: pick(Slot::Ringer, &audio.ringer),
        }
    }

    /// Whether the first-run wizard was finished or skipped.
    pub fn first_run_completed(&self) -> bool {
        self.lock().first_run_completed
    }

    /// Records that the first-run wizard was finished or skipped.
    pub fn set_first_run_completed(&self) -> io::Result<()> {
        let snapshot = {
            let mut d = self.lock();
            d.first_run_completed = true;
            d.clone()
        };
        self.write(&snapshot)
    }

    /// The remembered GUI panel choices.
    pub fn gui_panels(&self) -> GuiPanels {
        let d = self.lock();
        GuiPanels {
            lines_collapsed: d.gui_lines_collapsed,
            extensions_open: d.gui_extensions_open,
        }
    }

    /// Remembers the GUI panel choices (`None` leaves a slot as it is).
    pub fn set_gui_panels(&self, panels: GuiPanels) -> io::Result<()> {
        let snapshot = {
            let mut d = self.lock();
            if let Some(v) = panels.lines_collapsed {
                d.gui_lines_collapsed = Some(v);
            }
            if let Some(v) = panels.extensions_open {
                d.gui_extensions_open = Some(v);
            }
            d.clone()
        };
        self.write(&snapshot)
    }

    /// Whether the first-close tray notice was already shown.
    pub fn tray_hint_shown(&self) -> bool {
        self.lock().tray_hint_shown
    }

    /// Records that the first-close tray notice was shown. Returns `true` when this call set the
    /// flag (the notice is due), `false` when it was already set or the file cannot be written
    /// (then the notice is skipped rather than repeated on every close).
    pub fn mark_tray_hint_shown(&self) -> bool {
        let snapshot = {
            let mut d = self.lock();
            if d.tray_hint_shown {
                return false;
            }
            d.tray_hint_shown = true;
            d.clone()
        };
        match self.write(&snapshot) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("cannot update {}: {e}", self.path.display());
                false
            }
        }
    }

    /// Whether `state.toml` still holds legacy device data.
    pub fn has_legacy_devices(&self) -> bool {
        let d = self.lock();
        !d.audio.is_empty() || !d.config_seen.is_empty()
    }

    /// Removes the legacy device data and rewrites (or, when nothing else is left, deletes) the
    /// file.
    pub fn clear_legacy(&self) -> io::Result<()> {
        let snapshot = {
            let mut d = self.lock();
            d.audio = LegacySlots::default();
            d.config_seen = LegacySlots::default();
            d.clone()
        };
        self.write(&snapshot)
    }

    /// The release version the user skipped in the update pill, if any.
    pub fn skipped_update(&self) -> Option<String> {
        self.lock().skipped_update.clone()
    }

    /// Remembers (or, with `None`, forgets) the skipped release version.
    pub fn set_skipped_update(&self, version: Option<&str>) -> io::Result<()> {
        let snapshot = {
            let mut d = self.lock();
            d.skipped_update = version.map(str::to_string);
            d.clone()
        };
        self.write(&snapshot)
    }

    /// When the update check last succeeded without finding a newer release.
    pub fn last_update_check(&self) -> Option<DateTime<Utc>> {
        self.lock().last_update_check
    }

    /// Records (or clears) [`StateStore::last_update_check`].
    pub fn set_last_update_check(&self, at: Option<DateTime<Utc>>) -> io::Result<()> {
        self.update(|d| d.last_update_check = at)
    }

    /// The release version `app.update_available` was last emitted for.
    pub fn announced_update(&self) -> Option<String> {
        self.lock().announced_update.clone()
    }

    /// Records the release version `app.update_available` was emitted for.
    pub fn set_announced_update(&self, version: &str) -> io::Result<()> {
        self.update(|d| d.announced_update = Some(version.to_string()))
    }

    /// The installed-update record, if one is pending.
    pub fn installed_update(&self) -> Option<InstalledUpdate> {
        self.lock().update_installed.clone()
    }

    /// Records (or clears) [`StateStore::installed_update`].
    pub fn set_installed_update(&self, rec: Option<InstalledUpdate>) -> io::Result<()> {
        self.update(|d| d.update_installed = rec)
    }

    /// Changes the state and rewrites the file when something differs.
    fn update(&self, f: impl FnOnce(&mut StateFile)) -> io::Result<()> {
        let snapshot = {
            let mut d = self.lock();
            let before = toml::to_string(&*d).ok();
            f(&mut d);
            if toml::to_string(&*d).ok() == before {
                return Ok(());
            }
            d.clone()
        };
        self.write(&snapshot)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StateFile> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self, data: &StateFile) -> io::Result<()> {
        let body = toml::to_string_pretty(data).map_err(io::Error::other)?;
        if body.trim().is_empty() {
            return match fs::remove_file(&self.path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let text = format!(
            "# Written by Siple; runtime state that is not a setting. Safe to delete.\n\
             # Settings (including audio devices) live in config.toml.\n\
             # Last written {}\n{body}",
            chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
        );
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let mut rnd = [0u8; 4];
        getrandom::fill(&mut rnd).map_err(io::Error::other)?;
        let tmp = dir.join(format!(
            "{STATE_FILE}.{}.{:08x}.tmp",
            std::process::id(),
            u32::from_le_bytes(rnd)
        ));
        let result = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(i: &str, o: &str, r: &str) -> AudioConfig {
        AudioConfig {
            input: i.into(),
            output: o.into(),
            ringer: r.into(),
            ..Default::default()
        }
    }

    #[test]
    fn tray_hint_is_reported_once_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::open(dir.path());
        assert!(s.mark_tray_hint_shown());
        assert!(!s.mark_tray_hint_shown());
        // Survives a restart.
        let s = StateStore::open(dir.path());
        assert!(!s.mark_tray_hint_shown());
        let text = std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(text.contains("tray_hint_shown = true"), "{text}");
    }

    #[test]
    fn skipped_update_persists_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::open(dir.path());
        assert_eq!(s.skipped_update(), None);
        s.set_skipped_update(Some("0.2.0")).unwrap();
        assert_eq!(
            StateStore::open(dir.path()).skipped_update().as_deref(),
            Some("0.2.0")
        );
        s.set_skipped_update(None).unwrap();
        assert_eq!(StateStore::open(dir.path()).skipped_update(), None);
    }

    #[test]
    fn installed_update_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::open(dir.path());
        assert_eq!(s.installed_update(), None);
        let rec = InstalledUpdate {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            at: Utc::now(),
            cleanup: vec!["/a/b".into()],
        };
        s.set_installed_update(Some(rec.clone())).unwrap();
        let again = StateStore::open(dir.path()).installed_update().unwrap();
        assert_eq!(
            (again.from, again.to, again.cleanup),
            ("0.1.0".into(), "0.2.0".into(), rec.cleanup)
        );
        s.set_installed_update(None).unwrap();
        assert_eq!(StateStore::open(dir.path()).installed_update(), None);
    }

    #[test]
    fn update_check_state_persists() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::open(dir.path());
        let t = Utc::now();
        s.set_last_update_check(Some(t)).unwrap();
        s.set_announced_update("0.2.0").unwrap();
        let s = StateStore::open(dir.path());
        assert_eq!(s.announced_update().as_deref(), Some("0.2.0"));
        assert_eq!(
            s.last_update_check().map(|x| x.timestamp()),
            Some(t.timestamp())
        );
        s.set_last_update_check(None).unwrap();
        assert_eq!(StateStore::open(dir.path()).last_update_check(), None);
    }

    #[test]
    fn effective_devices_come_from_config_only() {
        let e = resolve_devices(&audio("Mic", "", "Ring"));
        assert_eq!(
            (e.input.name.as_str(), e.input.source),
            ("Mic", DeviceSource::Config)
        );
        assert_eq!(
            (e.output.name.as_str(), e.output.source),
            ("", DeviceSource::Default)
        );
        assert_eq!(e.ringer.source, DeviceSource::Config);
        assert_eq!(e.selection().ringer, "Ring");
        assert_eq!(
            e.describe(),
            "input=Mic (config), output=<default> (default), ringer=Ring (config)"
        );
    }

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    /// State with `audio.<slot>` chosen at `choice_at`; `seen` = what `config_seen` recorded.
    fn store(dir: &Path, choices: &[(&str, &str, i64)], seen: &[(&str, &str, i64)]) -> StateStore {
        let mut text = String::new();
        for (table, list) in [("audio", choices), ("config_seen", seen)] {
            for (slot, name, at) in list.iter() {
                text.push_str(&format!(
                    "[{table}.{slot}]\nname = \"{name}\"\nat = \"{}\"\n",
                    t(*at).to_rfc3339_opts(SecondsFormat::Secs, true)
                ));
            }
        }
        fs::write(dir.join(STATE_FILE), text).unwrap();
        StateStore::open(dir)
    }

    #[test]
    fn migration_picks_what_was_in_effect_per_slot() {
        let dir = tempfile::tempdir().unwrap();
        let now = t(10_000);
        // Config says "Cfg" since t=10 (recorded), user chose "Usr" at t=50: the user's choice was
        // in effect, even over an older non-empty config value.
        let s = store(dir.path(), &[("input", "Usr", 50)], &[("input", "Cfg", 10)]);
        let l = s.legacy_devices(&audio("Cfg", "", ""), t(500), now);
        assert_eq!(l.input.as_deref(), Some("Usr"));
        // Config changed after the user's choice (seen at t=100): the config value was in effect.
        let s = store(
            dir.path(),
            &[("input", "Usr", 50)],
            &[("input", "Cfg", 100)],
        );
        assert_eq!(
            s.legacy_devices(&audio("Cfg", "", ""), t(500), now).input,
            None
        );
        // Edited while stopped (different from what was seen): dated the file's mtime.
        let s = store(dir.path(), &[("input", "Usr", 50)], &[("input", "Old", 10)]);
        assert_eq!(
            s.legacy_devices(&audio("New", "", ""), t(500), now).input,
            None
        );
        assert_eq!(
            s.legacy_devices(&audio("New", "", ""), t(20), now)
                .input
                .as_deref(),
            Some("Usr")
        );
        // Nothing recorded and an empty config value: no opinion, the user's choice is in effect.
        let s = store(dir.path(), &[("output", "Spk", 50)], &[]);
        assert_eq!(
            s.legacy_devices(&audio("", "", ""), t(500), now)
                .output
                .as_deref(),
            Some("Spk")
        );
        // Nothing recorded and a config value set, written after the choice: the config wins.
        assert_eq!(
            s.legacy_devices(&audio("", "Cfg", ""), t(500), now).output,
            None
        );
        // An explicit system-default choice is carried (it overwrites an older value).
        let s = store(dir.path(), &[("ringer", "", 50)], &[("ringer", "Cfg", 10)]);
        assert_eq!(
            s.legacy_devices(&audio("", "", "Cfg"), t(500), now)
                .ringer
                .as_deref(),
            Some("")
        );
        // Future times are clamped to now.
        let s = store(
            dir.path(),
            &[("input", "Usr", 90_000)],
            &[("input", "Cfg", 90_000)],
        );
        assert_eq!(
            s.legacy_devices(&audio("Cfg", "", ""), t(500), now)
                .input
                .as_deref(),
            Some("Usr")
        );
    }

    #[test]
    fn legacy_data_is_cleared_and_the_empty_file_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(
            dir.path(),
            &[("input", "Old Mic", 10)],
            &[("input", "x", 5)],
        );
        assert!(s.has_legacy_devices());
        s.clear_legacy().unwrap();
        assert!(!s.has_legacy_devices());
        assert!(!dir.path().join(STATE_FILE).exists());
        assert!(!StateStore::open(dir.path()).has_legacy_devices());
        s.clear_legacy().unwrap();
    }

    #[test]
    fn missing_and_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateStore::open(dir.path());
        assert!(!s.has_legacy_devices());
        assert!(s.legacy_devices(&audio("", "", ""), t(0), t(1)).is_empty());
        fs::write(dir.path().join(STATE_FILE), "audio = [[[").unwrap();
        let s = StateStore::open(dir.path());
        assert!(!s.has_legacy_devices());
        assert!(dir.path().join("state.toml.bad").exists());
    }

    #[test]
    fn gui_panels_persist_and_merge() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path());
        assert_eq!(store.gui_panels(), GuiPanels::default());
        store
            .set_gui_panels(GuiPanels {
                lines_collapsed: Some(false),
                extensions_open: None,
            })
            .unwrap();
        let store = StateStore::open(dir.path());
        store
            .set_gui_panels(GuiPanels {
                lines_collapsed: None,
                extensions_open: Some(true),
            })
            .unwrap();
        let got = StateStore::open(dir.path()).gui_panels();
        assert_eq!(got.lines_collapsed, Some(false));
        assert_eq!(got.extensions_open, Some(true));
    }

    #[test]
    fn first_run_flag_persists_and_empty_state_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path());
        assert!(!store.first_run_completed());
        assert!(!dir.path().join(STATE_FILE).exists());
        store.set_first_run_completed().unwrap();
        assert!(StateStore::open(dir.path()).first_run_completed());
    }
}
