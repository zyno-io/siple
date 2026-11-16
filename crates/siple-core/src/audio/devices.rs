//! Device enumeration and configured-name resolution.

use cpal::traits::{DeviceTrait, HostTrait};
use serde::Serialize;

/// Direction of an audio device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Capture (microphone).
    Input,
    /// Playback (speaker / ringer).
    Output,
}

/// One enumerated audio device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    /// Human-readable device name; this is the identifier used in config.
    pub name: String,
    /// Whether this is the system default for its direction.
    pub is_default: bool,
    /// Sample rate of the device's default config (Hz).
    pub default_sample_rate: u32,
    /// Channel count of the device's default config.
    pub channels: u16,
}

/// Result of resolving a configured device name against the available list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    /// Index into the list passed to [`resolve_device`]; `None` if the list has
    /// no usable device (no devices, or empty name and no default).
    pub index: Option<usize>,
    /// The configured name was non-empty but not found, so the system default
    /// was chosen instead (callers should log a warning).
    pub fallback_to_default: bool,
}

/// Pure name resolution. `""` selects the system default. A name that is not
/// present selects the default with `fallback_to_default = true`. When there is
/// no flagged default the first device is used as the default; on Linux ALSA
/// pseudo-PCMs such as `null` are never chosen that way.
pub fn resolve_device(requested: &str, available: &[DeviceInfo]) -> Resolution {
    resolve_device_with(requested, available, cfg!(target_os = "linux"))
}

fn resolve_device_with(requested: &str, available: &[DeviceInfo], skip_pseudo: bool) -> Resolution {
    let default_idx = available.iter().position(|d| d.is_default).or_else(|| {
        available
            .iter()
            .position(|d| !(skip_pseudo && is_pseudo_pcm(&d.name)))
    });
    if requested.is_empty() {
        return Resolution {
            index: default_idx,
            fallback_to_default: false,
        };
    }
    match available.iter().position(|d| d.name == requested) {
        Some(i) => Resolution {
            index: Some(i),
            fallback_to_default: false,
        },
        None => Resolution {
            index: default_idx,
            fallback_to_default: true,
        },
    }
}

/// ALSA PCM names (the part before any `:`) that are plugins, mixers or pass-through
/// aliases rather than something a person would pick as a microphone or speaker:
/// `null` (not paced in real time), `dmix`/`dsnoop` (software sharing), `surround*`,
/// `iec958`/`spdif`/`a52` (digital pass-through), `front` (raw hardware alias; `plughw:`
/// and `sysdefault:` cover it), rate-conversion and routing plugins, `jack`, `oss`.
/// Kept: `default`, `pipewire`, `pulse`, `sysdefault:*`, `hw:*`, `plughw:*`, `hdmi:*`, and
/// anything not listed.
const PSEUDO_PCM_PREFIXES: &[&str] = &[
    "null",
    "dmix",
    "dsnoop",
    "surround",
    "iec958",
    "spdif",
    "a52",
    "front",
    "lavrate",
    "samplerate",
    "speexrate",
    "speex",
    "upmix",
    "vdownmix",
    "usbstream",
    "jack",
    "oss",
    "dsp",
];

/// Whether `name` (an ALSA PCM id such as `dmix:CARD=PCH,DEV=0`) is a pseudo-PCM that must
/// not be listed or chosen as a fallback. Also recognises the description of `null`.
pub(crate) fn is_pseudo_pcm(name: &str) -> bool {
    let id = name.split(':').next().unwrap_or(name);
    PSEUDO_PCM_PREFIXES.iter().any(|p| id.starts_with(p)) || name.starts_with("Discard all samples")
}

pub(crate) fn device_name(dev: &cpal::Device) -> Option<String> {
    dev.description().ok().map(|d| d.name().to_string())
}

/// ALSA PCM id of a cpal device (`default`, `hw:CARD=...`); cpal's `name` is the pretty text.
#[cfg(target_os = "linux")]
fn pcm_id(dev: &cpal::Device) -> Option<String> {
    dev.description()
        .ok()
        .and_then(|d| d.driver().map(str::to_string))
}

fn default_config(dev: &cpal::Device, dir: Direction) -> Option<cpal::SupportedStreamConfig> {
    match dir {
        Direction::Input => dev.default_input_config().ok(),
        Direction::Output => dev.default_output_config().ok(),
    }
}

/// An enumerated device together with the cpal handle.
pub(crate) struct Enumerated {
    pub info: DeviceInfo,
    pub device: cpal::Device,
}

/// Enumerate usable devices of `dir` on `host` (devices without a readable
/// name or default config are skipped).
pub(crate) fn enumerate(host: &cpal::Host, dir: Direction) -> Vec<Enumerated> {
    let default_name = match dir {
        Direction::Input => host.default_input_device(),
        Direction::Output => host.default_output_device(),
    }
    .and_then(|d| device_name(&d));
    let devices: Vec<cpal::Device> = match dir {
        Direction::Input => host.input_devices().map(|i| i.collect()),
        Direction::Output => host.output_devices().map(|i| i.collect()),
    }
    .unwrap_or_default();
    let mut out: Vec<Enumerated> = Vec::new();
    for device in devices {
        #[cfg(target_os = "linux")]
        if pcm_id(&device).is_some_and(|id| is_pseudo_pcm(&id)) {
            continue;
        }
        let Some(name) = device_name(&device) else {
            continue;
        };
        let Some(cfg) = default_config(&device, dir) else {
            continue;
        };
        if out.iter().any(|e| e.info.name == name) {
            continue; // duplicate names: first wins, matches name-based resolution
        }
        out.push(Enumerated {
            info: DeviceInfo {
                is_default: default_name.as_deref() == Some(name.as_str()),
                name,
                default_sample_rate: cfg.sample_rate(),
                channels: cfg.channels(),
            },
            device,
        });
    }
    // The default device sometimes has no entry in the enumerated list.
    if default_name.is_some()
        && !out.iter().any(|e| e.info.is_default)
        && let Some(e) = default_device(host, dir)
    {
        out.insert(0, e);
    }
    out
}

/// The host's default device of `dir` with its default config, without enumerating the
/// others (enumeration opens every ALSA hint PCM).
pub(crate) fn default_device(host: &cpal::Host, dir: Direction) -> Option<Enumerated> {
    let device = match dir {
        Direction::Input => host.default_input_device(),
        Direction::Output => host.default_output_device(),
    }?;
    let name = device_name(&device)?;
    let cfg = default_config(&device, dir)?;
    Some(Enumerated {
        info: DeviceInfo {
            name,
            is_default: true,
            default_sample_rate: cfg.sample_rate(),
            channels: cfg.channels(),
        },
        device,
    })
}

/// List devices of one direction on the default host.
pub fn list_devices(dir: Direction) -> Vec<DeviceInfo> {
    #[cfg(target_os = "linux")]
    let mut alsa_log = AlsaLog::install();
    let host = cpal::default_host();
    let out = enumerate(&host, dir).into_iter().map(|e| e.info).collect();
    #[cfg(target_os = "linux")]
    alsa_log.drain();
    out
}

/// Routes alsa-lib error messages (thread-local handler) to tracing at DEBUG instead of
/// stderr. Install on the thread that touches ALSA, then call [`AlsaLog::drain`] after work.
#[cfg(target_os = "linux")]
pub(crate) struct AlsaLog(Option<std::rc::Rc<std::cell::RefCell<alsa::Output>>>);

#[cfg(target_os = "linux")]
impl AlsaLog {
    pub(crate) fn install() -> Self {
        Self(alsa::Output::local_error_handler().ok())
    }

    /// Emits and clears whatever alsa-lib printed since the last call.
    pub(crate) fn drain(&mut self) {
        let Some(buf) = &self.0 else { return };
        let text = buf.borrow().to_string();
        if text.trim().is_empty() {
            return;
        }
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            tracing::debug!(target: "alsa", "{line}");
        }
        // A fresh handler/buffer replaces (and so empties) the old one.
        *self = Self::install();
    }
}

/// Environment inputs of the Linux audio probe.
#[derive(Debug, Default, Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct AudioEnv<'a> {
    pub xdg_runtime_dir: Option<&'a std::path::Path>,
    pub pulse_runtime_path: Option<&'a std::path::Path>,
    pub pipewire_runtime_dir: Option<&'a std::path::Path>,
    pub pulse_server: Option<&'a str>,
    pub pipewire_remote: Option<&'a str>,
}

/// Whether real audio is plausible on a Linux host: a sound card node under
/// `<root>/dev/snd/pcmC*`, a PulseAudio socket (`$XDG_RUNTIME_DIR/pulse/native`,
/// `$PULSE_RUNTIME_PATH/native`), a PipeWire socket (`$XDG_RUNTIME_DIR/pipewire-0`,
/// `$PIPEWIRE_RUNTIME_DIR/pipewire-0`), or `PULSE_SERVER` / `PIPEWIRE_REMOTE` set. Pure
/// (apart from reading the filesystem) so it is testable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_audio_plausible(root: &std::path::Path, env: AudioEnv<'_>) -> bool {
    if let Ok(rd) = std::fs::read_dir(root.join("dev/snd"))
        && rd
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("pcmC"))
    {
        return true;
    }
    fn set(d: Option<&std::path::Path>) -> Option<&std::path::Path> {
        d.filter(|d| !d.as_os_str().is_empty())
    }
    if let Some(d) = set(env.xdg_runtime_dir)
        && (d.join("pulse/native").exists() || d.join("pipewire-0").exists())
    {
        return true;
    }
    if set(env.pulse_runtime_path).is_some_and(|d| d.join("native").exists()) {
        return true;
    }
    if set(env.pipewire_runtime_dir).is_some_and(|d| d.join("pipewire-0").exists()) {
        return true;
    }
    let nonblank = |s: Option<&str>| s.is_some_and(|s| !s.trim().is_empty());
    nonblank(env.pulse_server) || nonblank(env.pipewire_remote)
}

/// [`linux_audio_plausible`] against the real system and environment.
#[cfg(target_os = "linux")]
pub(crate) fn host_audio_plausible() -> bool {
    let path = |k: &str| std::env::var_os(k).map(std::path::PathBuf::from);
    let (xdg, pulse_rt, pw_rt) = (
        path("XDG_RUNTIME_DIR"),
        path("PULSE_RUNTIME_PATH"),
        path("PIPEWIRE_RUNTIME_DIR"),
    );
    let (ps, pr) = (
        std::env::var("PULSE_SERVER").ok(),
        std::env::var("PIPEWIRE_REMOTE").ok(),
    );
    linux_audio_plausible(
        std::path::Path::new("/"),
        AudioEnv {
            xdg_runtime_dir: xdg.as_deref(),
            pulse_runtime_path: pulse_rt.as_deref(),
            pipewire_runtime_dir: pw_rt.as_deref(),
            pulse_server: ps.as_deref(),
            pipewire_remote: pr.as_deref(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, def: bool) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            is_default: def,
            default_sample_rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn empty_name_is_default() {
        let l = [dev("a", false), dev("b", true)];
        assert_eq!(
            resolve_device("", &l),
            Resolution {
                index: Some(1),
                fallback_to_default: false
            }
        );
    }

    #[test]
    fn named_found() {
        let l = [dev("a", false), dev("b", true)];
        assert_eq!(
            resolve_device("a", &l),
            Resolution {
                index: Some(0),
                fallback_to_default: false
            }
        );
    }

    #[test]
    fn missing_falls_back_with_flag() {
        let l = [dev("a", false), dev("b", true)];
        assert_eq!(
            resolve_device("zzz", &l),
            Resolution {
                index: Some(1),
                fallback_to_default: true
            }
        );
    }

    #[test]
    fn no_devices() {
        assert_eq!(
            resolve_device("", &[]),
            Resolution {
                index: None,
                fallback_to_default: false
            }
        );
        assert_eq!(
            resolve_device("x", &[]),
            Resolution {
                index: None,
                fallback_to_default: true
            }
        );
    }

    #[test]
    fn no_flagged_default_uses_first() {
        let l = [dev("a", false), dev("b", false)];
        assert_eq!(resolve_device("", &l).index, Some(0));
    }

    #[test]
    fn fallback_never_picks_null() {
        let l = [
            dev("null", false),
            dev("dmix:CARD=PCH", false),
            dev("hw:0", false),
        ];
        assert_eq!(resolve_device_with("", &l, true).index, Some(2));
        assert_eq!(resolve_device_with("zzz", &l, true).index, Some(2));
        assert_eq!(resolve_device_with("", &l[..1], true).index, None);
        // Not skipped when the platform has no pseudo-PCMs.
        assert_eq!(resolve_device_with("", &l, false).index, Some(0));
        // A flagged default wins regardless.
        let l = [dev("null", false), dev("default", true)];
        assert_eq!(resolve_device_with("", &l, true).index, Some(1));
    }

    #[test]
    fn pseudo_pcm_filter() {
        for n in [
            "null",
            "dmix",
            "dmix:CARD=PCH,DEV=0",
            "dsnoop:CARD=PCH,DEV=0",
            "surround51:CARD=PCH,DEV=0",
            "iec958:CARD=PCH,DEV=0",
            "front:CARD=PCH,DEV=0",
            "Discard all samples (playback) or generate zero samples (capture)",
        ] {
            assert!(is_pseudo_pcm(n), "{n}");
        }
        for n in [
            "default",
            "pipewire",
            "pulse",
            "sysdefault:CARD=PCH",
            "hw:CARD=PCH,DEV=0",
            "plughw:CARD=PCH,DEV=0",
            "hdmi:CARD=PCH,DEV=0",
            "Default Audio Device",
            "Built-in Audio Analog Stereo",
        ] {
            assert!(!is_pseudo_pcm(n), "{n}");
        }
    }

    #[test]
    fn audio_probe() {
        use std::fs;
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let run = root.join("run");
        fs::create_dir_all(&run).unwrap();
        fn xdg(d: &std::path::Path) -> AudioEnv<'_> {
            AudioEnv {
                xdg_runtime_dir: Some(d),
                ..Default::default()
            }
        }
        assert!(!linux_audio_plausible(root, xdg(&run)));
        fn ps(s: &str) -> AudioEnv<'_> {
            AudioEnv {
                pulse_server: Some(s),
                ..Default::default()
            }
        }
        assert!(!linux_audio_plausible(root, ps("  ")));
        assert!(linux_audio_plausible(root, ps("tcp:host")));
        fn pr(s: &str) -> AudioEnv<'_> {
            AudioEnv {
                pipewire_remote: Some(s),
                ..Default::default()
            }
        }
        assert!(!linux_audio_plausible(root, pr(" ")));
        assert!(linux_audio_plausible(root, pr("pipewire-1")));
        // Control nodes alone are not a card.
        fs::create_dir_all(root.join("dev/snd")).unwrap();
        fs::write(root.join("dev/snd/controlC0"), "").unwrap();
        fs::write(root.join("dev/snd/timer"), "").unwrap();
        assert!(!linux_audio_plausible(root, xdg(&run)));
        fs::write(root.join("dev/snd/pcmC0D0p"), "").unwrap();
        assert!(linux_audio_plausible(root, AudioEnv::default()));
        fs::remove_file(root.join("dev/snd/pcmC0D0p")).unwrap();
        fs::create_dir_all(run.join("pulse")).unwrap();
        fs::write(run.join("pulse/native"), "").unwrap();
        assert!(linux_audio_plausible(root, xdg(&run)));
        fs::remove_file(run.join("pulse/native")).unwrap();
        fs::write(run.join("pipewire-0"), "").unwrap();
        assert!(linux_audio_plausible(root, xdg(&run)));
        assert!(!linux_audio_plausible(root, AudioEnv::default()));
    }

    #[test]
    fn audio_probe_alternate_runtime_dirs() {
        use std::fs;
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let (p, w) = (root.join("pulse-rt"), root.join("pw-rt"));
        fs::create_dir_all(&p).unwrap();
        fs::create_dir_all(&w).unwrap();
        let env = AudioEnv {
            pulse_runtime_path: Some(&p),
            pipewire_runtime_dir: Some(&w),
            ..Default::default()
        };
        assert!(!linux_audio_plausible(root, env));
        fs::write(p.join("native"), "").unwrap();
        assert!(linux_audio_plausible(root, env));
        fs::remove_file(p.join("native")).unwrap();
        fs::write(w.join("pipewire-0"), "").unwrap();
        assert!(linux_audio_plausible(root, env));
        // Empty variable values are ignored.
        let empty = AudioEnv {
            pulse_runtime_path: Some(std::path::Path::new("")),
            ..Default::default()
        };
        assert!(!linux_audio_plausible(root, empty));
    }
}
