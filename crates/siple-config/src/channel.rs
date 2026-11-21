//! Distribution channel: how this process was installed (direct download, Mac App Store,
//! Microsoft Store MSIX, Flatpak) and the file locations that follow from it.
//!
//! Detection looks at the process, not at the build: macOS sandbox containers set
//! `APP_SANDBOX_CONTAINER_ID`, an MSIX process has a package identity, and a Linux Flatpak sandbox
//! has `/.flatpak-info`. Gating code copies the
//! [`Channel`] (it is `Copy`), so tests can inject any channel.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// App group shared by the sandboxed app and its bundled CLI (team prefix + bundle id).
pub const MAS_APP_GROUP: &str = "VUDU4KEQ99.io.zyno.siple";

/// Name of the Siple directory inside a channel's data root.
pub const SIPLE_DIR: &str = "Siple";

/// How the running process was distributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    /// Developer ID / zip / installer / package manager: the platform defaults apply.
    Direct,
    /// Mac App Store: app sandbox, config in the app group container.
    MacAppStore,
    /// Microsoft Store (or sideloaded) MSIX package: config in the package's LocalState.
    Msix,
    /// Flatpak sandbox (Flathub): config in the per-app `$XDG_CONFIG_HOME`, no external programs.
    Flatpak,
}

impl ChannelKind {
    /// Stable name used in `status.build.channel`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::MacAppStore => "mas",
            Self::Msix => "msix",
            Self::Flatpak => "flatpak",
        }
    }
}

/// The detected (or injected) distribution channel and what it allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    /// Which channel.
    pub kind: ChannelKind,
    /// Running inside an OS sandbox (macOS app sandbox, Flatpak).
    pub sandboxed: bool,
    /// May spawn arbitrary external programs (`[speech]` hooks, a headless engine, ...).
    pub can_exec: bool,
    /// MSIX package family name.
    pub package_family: Option<&'static str>,
    /// MSIX application user model id (toast identity).
    pub aumid: Option<&'static str>,
}

impl Channel {
    /// An unpackaged, unsandboxed process.
    pub const DIRECT: Channel = Channel {
        kind: ChannelKind::Direct,
        sandboxed: false,
        can_exec: true,
        package_family: None,
        aumid: None,
    };
    /// A Mac App Store sandboxed process.
    pub const MAC_APP_STORE: Channel = Channel {
        kind: ChannelKind::MacAppStore,
        sandboxed: true,
        can_exec: false,
        package_family: None,
        aumid: None,
    };

    /// A Flatpak (Flathub) process.
    pub const FLATPAK: Channel = Channel {
        kind: ChannelKind::Flatpak,
        sandboxed: true,
        can_exec: false,
        package_family: None,
        aumid: None,
    };

    /// An MSIX-packaged process.
    pub fn msix(package_family: &'static str, aumid: Option<&'static str>) -> Channel {
        Channel {
            kind: ChannelKind::Msix,
            sandboxed: false,
            can_exec: true,
            package_family: Some(package_family),
            aumid,
        }
    }

    /// Installed through a store (OS-managed install and updates, no self-update, no OS
    /// integration written by the app itself).
    pub fn is_packaged(&self) -> bool {
        self.kind != ChannelKind::Direct
    }
}

/// The channel of this process, detected once.
pub fn current() -> &'static Channel {
    static C: OnceLock<Channel> = OnceLock::new();
    C.get_or_init(detect)
}

#[cfg(target_os = "macos")]
fn detect() -> Channel {
    if std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some_and(|v| !v.is_empty()) {
        Channel::MAC_APP_STORE
    } else {
        Channel::DIRECT
    }
}

#[cfg(windows)]
fn detect() -> Channel {
    use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows_sys::Win32::Storage::Packaging::Appx::{
        GetCurrentApplicationUserModelId, GetCurrentPackageFamilyName,
    };

    type Getter = unsafe extern "system" fn(*mut u32, *mut u16) -> u32;

    // The standard two-call pattern; 15700 (APPMODEL_ERROR_NO_PACKAGE) means "not packaged".
    fn query(f: Getter) -> Option<String> {
        let mut len = 0u32;
        // SAFETY: a null buffer with len 0 only asks for the required length.
        let rc = unsafe { f(&mut len, std::ptr::null_mut()) };
        if rc != ERROR_INSUFFICIENT_BUFFER || len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        // SAFETY: `buf` holds `len` u16 as announced.
        let rc = unsafe { f(&mut len, buf.as_mut_ptr()) };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }

    match query(GetCurrentPackageFamilyName) {
        Some(pfn) => {
            let aumid = query(GetCurrentApplicationUserModelId);
            // Leaked once: the channel lives for the whole process.
            let pfn: &'static str = Box::leak(pfn.into_boxed_str());
            let aumid = aumid.map(|a| &*Box::leak(a.into_boxed_str()));
            Channel::msix(pfn, aumid)
        }
        None => Channel::DIRECT,
    }
}

#[cfg(target_os = "linux")]
fn detect() -> Channel {
    detect_in(Path::new("/"))
}

/// Flatpak writes `/.flatpak-info` into every sandbox; it never exists on the host. It must name
/// this app: the tarball run inside another app's sandbox (or a stray file) is not the Flatpak edition.
#[cfg(any(target_os = "linux", test))]
fn detect_in(root: &Path) -> Channel {
    match std::fs::read_to_string(root.join(".flatpak-info")) {
        Ok(text) if flatpak_info_app(&text).as_deref() == Some(FLATPAK_APP_ID) => Channel::FLATPAK,
        _ => Channel::DIRECT,
    }
}

/// The Flatpak application id.
#[cfg(any(target_os = "linux", test))]
const FLATPAK_APP_ID: &str = "io.zyno.siple";

/// `name` of the `[Application]` group of a `.flatpak-info` key file (whitespace around the
/// header, key and value is ignored).
#[cfg(any(target_os = "linux", test))]
fn flatpak_info_app(text: &str) -> Option<String> {
    let mut in_app = false;
    for line in text.lines().map(str::trim) {
        if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_app = group.trim() == "Application";
        } else if in_app
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == "name"
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn detect() -> Channel {
    Channel::DIRECT
}

/// The account's real home directory from the password database, never `$HOME`: inside the macOS
/// sandbox `$HOME` points into the app container.
#[cfg(unix)]
pub fn real_home() -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: a zeroed passwd is a valid out-parameter for getpwuid_r.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: all pointers are valid for the lengths passed.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < (1 << 20) {
            let n = buf.len() * 2;
            buf.resize(n, 0);
            continue;
        }
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        // SAFETY: pw_dir points into `buf`, NUL-terminated, alive here.
        let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
        let p = PathBuf::from(OsStr::from_bytes(dir.to_bytes()));
        return p.is_absolute().then_some(p);
    }
}

/// Non-unix platforms have no passwd home.
#[cfg(not(unix))]
pub fn real_home() -> Option<PathBuf> {
    None
}

/// `<home>/Library/Group Containers/<app group>`: the directory the sandboxed app and CLI share.
pub fn mas_group_in(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Group Containers")
        .join(MAS_APP_GROUP)
}

/// Mac App Store config directory under `home`.
pub fn mas_dir_in(home: &Path) -> PathBuf {
    mas_group_in(home).join(SIPLE_DIR)
}

/// MSIX config directory: the package's real (non-virtualized) LocalState under
/// `local_appdata`.
pub fn msix_dir_in(local_appdata: &Path, package_family: &str) -> PathBuf {
    local_appdata
        .join("Packages")
        .join(package_family)
        .join("LocalState")
        .join(SIPLE_DIR)
}

/// The app group container directory (macOS), from the passwd home.
pub fn mas_group_dir() -> Option<PathBuf> {
    real_home().map(|h| mas_group_in(&h))
}

/// Config directory for `channel`, or `None` when it cannot be determined.
/// [`ChannelKind::Direct`] uses the platform default.
pub fn config_dir_for_channel(channel: &Channel) -> Option<PathBuf> {
    match channel.kind {
        ChannelKind::Direct => crate::paths::direct_config_dir(),
        ChannelKind::MacAppStore => real_home().map(|h| mas_dir_in(&h)),
        ChannelKind::Msix => {
            let pfn = channel.package_family?;
            dirs::data_local_dir().map(|d| msix_dir_in(&d, pfn))
        }
        // Flatpak sets `$XDG_CONFIG_HOME` to `~/.var/app/<app id>/config`, the same path on the host.
        ChannelKind::Flatpak => crate::paths::direct_config_dir(),
    }
}

/// Config directory for this process's channel.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_for_channel(current())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mas_dir_is_inside_the_group_container() {
        let d = mas_dir_in(Path::new("/Users/ann"));
        assert_eq!(
            d,
            Path::new("/Users/ann/Library/Group Containers/VUDU4KEQ99.io.zyno.siple/Siple")
        );
        assert_eq!(d.parent().unwrap(), mas_group_in(Path::new("/Users/ann")));
    }

    #[test]
    fn msix_dir_is_real_localstate() {
        let d = msix_dir_in(
            Path::new(r"C:\Users\ann\AppData\Local"),
            "Zyno.Siple_abc123",
        );
        assert_eq!(
            d,
            Path::new(r"C:\Users\ann\AppData\Local")
                .join("Packages")
                .join("Zyno.Siple_abc123")
                .join("LocalState")
                .join("Siple")
        );
    }

    #[test]
    fn direct_uses_platform_default() {
        assert_eq!(
            config_dir_for_channel(&Channel::DIRECT),
            crate::paths::direct_config_dir()
        );
    }

    #[test]
    fn msix_without_family_has_no_dir() {
        let mut c = Channel::msix("x_y", None);
        c.package_family = None;
        assert_eq!(config_dir_for_channel(&c), None);
    }

    #[test]
    fn channel_flags() {
        assert!(!Channel::DIRECT.is_packaged() && std::hint::black_box(Channel::DIRECT).can_exec);
        assert!(
            Channel::MAC_APP_STORE.is_packaged()
                && !std::hint::black_box(Channel::MAC_APP_STORE).can_exec
        );
        assert!(std::hint::black_box(Channel::MAC_APP_STORE).sandboxed);
        let m = Channel::msix("a_b", Some("a_b!App"));
        assert!(m.is_packaged() && m.can_exec && !m.sandboxed);
        assert_eq!(ChannelKind::MacAppStore.as_str(), "mas");
        let f = std::hint::black_box(Channel::FLATPAK);
        assert!(f.is_packaged() && f.sandboxed && !f.can_exec);
        assert_eq!(ChannelKind::Flatpak.as_str(), "flatpak");
    }

    #[test]
    fn flatpak_is_detected_by_flatpak_info() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(detect_in(d.path()), Channel::DIRECT);
        std::fs::create_dir(d.path().join(".flatpak-info")).unwrap();
        assert_eq!(detect_in(d.path()), Channel::DIRECT);
        std::fs::remove_dir(d.path().join(".flatpak-info")).unwrap();
        std::fs::write(d.path().join(".flatpak-info"), "[Application]\n").unwrap();
        assert_eq!(detect_in(d.path()), Channel::DIRECT, "no name");
        let info = d.path().join(".flatpak-info");
        std::fs::write(&info, "[Application]\nname=io.zyno.siple\nruntime=x\n").unwrap();
        assert_eq!(detect_in(d.path()), Channel::FLATPAK);
        std::fs::write(
            &info,
            "[Instance]\nx=1\n\n [ Application ] \n name = io.zyno.siple \n",
        )
        .unwrap();
        assert_eq!(
            detect_in(d.path()),
            Channel::FLATPAK,
            "whitespace is tolerated"
        );
    }

    #[test]
    fn another_apps_flatpak_info_is_not_this_flatpak() {
        let d = tempfile::tempdir().unwrap();
        let info = d.path().join(".flatpak-info");
        for text in [
            "[Application]\nname=org.example.Other\n",
            "[Application]\nname=io.zyno.siple.Other\n",
            "[Instance]\nname=io.zyno.siple\n",
            "[Application]\nruntime=io.zyno.siple\n",
            "[Context]\nname=io.zyno.siple\n[Application]\nname=org.example.Other\n",
            "",
        ] {
            std::fs::write(&info, text).unwrap();
            assert_eq!(detect_in(d.path()), Channel::DIRECT, "{text:?}");
        }
    }

    #[test]
    fn flatpak_uses_the_per_app_xdg_config_dir() {
        assert_eq!(
            config_dir_for_channel(&Channel::FLATPAK),
            crate::paths::direct_config_dir()
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_home_is_absolute() {
        if let Some(h) = real_home() {
            assert!(h.is_absolute());
        }
    }
}
