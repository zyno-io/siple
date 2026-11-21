//! Distribution channel: how this process was installed (direct download, Mac App Store,
//! Microsoft Store MSIX, Flatpak, Linux AppImage) and the file locations that follow from it.
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
    /// Linux AppImage (direct download, one self-mounting file): platform defaults apply; the
    /// running executable lives in a temporary mount, `Channel::appimage` is the file.
    AppImage,
}

impl ChannelKind {
    /// Stable name used in `status.build.channel`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::MacAppStore => "mas",
            Self::Msix => "msix",
            Self::Flatpak => "flatpak",
            Self::AppImage => "appimage",
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
    /// AppImage: the `.AppImage` file this process runs from (`$APPIMAGE`, canonical).
    pub appimage: Option<&'static Path>,
}

impl Channel {
    /// An unpackaged, unsandboxed process.
    pub const DIRECT: Channel = Channel {
        kind: ChannelKind::Direct,
        sandboxed: false,
        can_exec: true,
        package_family: None,
        aumid: None,
        appimage: None,
    };
    /// A Mac App Store sandboxed process.
    pub const MAC_APP_STORE: Channel = Channel {
        kind: ChannelKind::MacAppStore,
        sandboxed: true,
        can_exec: false,
        package_family: None,
        aumid: None,
        appimage: None,
    };

    /// A Flatpak (Flathub) process.
    pub const FLATPAK: Channel = Channel {
        kind: ChannelKind::Flatpak,
        sandboxed: true,
        can_exec: false,
        package_family: None,
        aumid: None,
        appimage: None,
    };

    /// An MSIX-packaged process.
    pub fn msix(package_family: &'static str, aumid: Option<&'static str>) -> Channel {
        Channel {
            kind: ChannelKind::Msix,
            sandboxed: false,
            can_exec: true,
            package_family: Some(package_family),
            aumid,
            appimage: None,
        }
    }

    /// A Linux AppImage process running from `file` (the canonical `$APPIMAGE`).
    pub fn appimage(file: &'static Path) -> Channel {
        Channel {
            kind: ChannelKind::AppImage,
            sandboxed: false,
            can_exec: true,
            package_family: None,
            aumid: None,
            appimage: Some(file),
        }
    }

    /// Store or sandbox managed (OS-managed install and updates, no self-update, no OS
    /// integration written by the app itself).
    pub fn is_packaged(&self) -> bool {
        matches!(
            self.kind,
            ChannelKind::MacAppStore | ChannelKind::Msix | ChannelKind::Flatpak
        )
    }

    /// What a login item or a relaunch must start: the AppImage file, never its temporary mount;
    /// otherwise the running executable.
    pub fn launch_target(&self) -> std::io::Result<PathBuf> {
        match self.appimage {
            Some(f) => Ok(f.to_path_buf()),
            None => std::env::current_exe(),
        }
    }
}

/// Whether `exe` lies inside `appdir` (both absolute; `exe` is not `appdir` itself).
pub fn runs_inside(exe: &Path, appdir: &Path) -> bool {
    exe.is_absolute() && appdir.is_absolute() && exe != appdir && exe.starts_with(appdir)
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
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| std::fs::canonicalize(e).ok());
    detect_in(Path::new("/"), &|k| std::env::var_os(k), exe.as_deref())
}

/// Flatpak writes `/.flatpak-info` into every sandbox; it never exists on the host. It must name
/// this app: the tarball run inside another app's sandbox (or a stray file) is not the Flatpak edition.
/// Otherwise the AppImage runtime's `APPIMAGE`/`APPDIR` (see [`appimage_file`]) make it the AppImage.
#[cfg(any(target_os = "linux", test))]
fn detect_in(
    root: &Path,
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    exe: Option<&Path>,
) -> Channel {
    match std::fs::read_to_string(root.join(".flatpak-info")) {
        Ok(text) if flatpak_info_app(&text).as_deref() == Some(FLATPAK_APP_ID) => Channel::FLATPAK,
        _ => match appimage_file(var, exe) {
            // Leaked once: the channel lives for the whole process.
            Some(f) => Channel::appimage(Box::leak(f.into_boxed_path())),
            None => Channel::DIRECT,
        },
    }
}

/// The canonical `.AppImage` file when this process runs from one: `APPIMAGE` (a regular file) and
/// `APPDIR` are absolute and the executable lies inside `APPDIR`. A terminal that is itself an
/// AppImage (WezTerm, VS Code...) leaks both variables to every shell; the executable check keeps
/// programs started there `Direct`.
#[cfg(any(target_os = "linux", test))]
fn appimage_file(
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    exe: Option<&Path>,
) -> Option<PathBuf> {
    let abs = |k: &str| var(k).map(PathBuf::from).filter(|p| p.is_absolute());
    let file = std::fs::canonicalize(abs("APPIMAGE")?).ok()?;
    let appdir = std::fs::canonicalize(abs("APPDIR")?).ok()?;
    if !file.is_file() {
        return None;
    }
    runs_inside(exe?, &appdir).then_some(file)
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

/// Marks every inherited descriptor above stderr close-on-exec.
///
/// The AppImage runtime (FUSE mode) leaves the read end of its keep-alive pipe (fd 3) and the
/// mount directory (fd 1023) open without `FD_CLOEXEC`. The FUSE daemon unmounts only when every
/// holder of the pipe has closed it, so a relaunched new version or a browser started by
/// `xdg-open` would keep the old mount (and the replaced, deleted file) alive and successive
/// updates would chain mounts.
#[cfg(target_os = "linux")]
pub fn cloexec_inherited_fds() {
    let fds: Vec<i32> = match std::fs::read_dir("/proc/self/fd") {
        Ok(rd) => rd
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect(),
        Err(_) => return,
    };
    for fd in fds.into_iter().filter(|&fd| fd > 2) {
        // SAFETY: fcntl on an arbitrary integer descriptor only reads or sets its flags; a
        // closed one (the directory handle itself) returns EBADF.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Config directory for `channel`, or `None` when it cannot be determined.
/// [`ChannelKind::Direct`] and [`ChannelKind::AppImage`] use the platform default.
pub fn config_dir_for_channel(channel: &Channel) -> Option<PathBuf> {
    match channel.kind {
        // The AppImage uses the same config as the tarball; the runtime's portable `<file>.config`
        // arrives as `XDG_CONFIG_HOME`, which `direct_config_dir()` honours.
        ChannelKind::Direct | ChannelKind::AppImage => crate::paths::direct_config_dir(),
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
        let a = Channel::appimage(Path::new("/x/Siple.AppImage"));
        assert!(!a.is_packaged() && a.can_exec && !a.sandboxed);
        assert_eq!(a.kind.as_str(), "appimage");
    }

    #[test]
    fn flatpak_is_detected_by_flatpak_info() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(detect_in(d.path(), &|_| None, None), Channel::DIRECT);
        std::fs::create_dir(d.path().join(".flatpak-info")).unwrap();
        assert_eq!(detect_in(d.path(), &|_| None, None), Channel::DIRECT);
        std::fs::remove_dir(d.path().join(".flatpak-info")).unwrap();
        std::fs::write(d.path().join(".flatpak-info"), "[Application]\n").unwrap();
        assert_eq!(
            detect_in(d.path(), &|_| None, None),
            Channel::DIRECT,
            "no name"
        );
        let info = d.path().join(".flatpak-info");
        std::fs::write(&info, "[Application]\nname=io.zyno.siple\nruntime=x\n").unwrap();
        assert_eq!(detect_in(d.path(), &|_| None, None), Channel::FLATPAK);
        std::fs::write(
            &info,
            "[Instance]\nx=1\n\n [ Application ] \n name = io.zyno.siple \n",
        )
        .unwrap();
        assert_eq!(
            detect_in(d.path(), &|_| None, None),
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
            assert_eq!(
                detect_in(d.path(), &|_| None, None),
                Channel::DIRECT,
                "{text:?}"
            );
        }
    }

    #[test]
    fn flatpak_uses_the_per_app_xdg_config_dir() {
        assert_eq!(
            config_dir_for_channel(&Channel::FLATPAK),
            crate::paths::direct_config_dir()
        );
    }

    /// A tempdir with `appdir/usr/bin/siple` and `x/Siple.AppImage`: (guard, appdir, exe, file).
    fn appimage_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let appdir = root.join("appdir");
        std::fs::create_dir_all(appdir.join("usr/bin")).unwrap();
        let exe = appdir.join("usr/bin/siple");
        std::fs::write(&exe, "x").unwrap();
        std::fs::create_dir(root.join("x")).unwrap();
        let file = root.join("x/Siple.AppImage");
        std::fs::write(&file, "x").unwrap();
        (d, appdir, exe, file)
    }

    fn vars<'a>(
        appimage: Option<&'a Path>,
        appdir: Option<&'a Path>,
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |k| match k {
            "APPIMAGE" => appimage.map(|p| p.as_os_str().to_owned()),
            "APPDIR" => appdir.map(|p| p.as_os_str().to_owned()),
            _ => None,
        }
    }

    #[test]
    fn appimage_is_detected_from_appimage_appdir_and_exe() {
        let (d, appdir, exe, file) = appimage_fixture();
        let none = tempfile::tempdir().unwrap();
        let c = detect_in(none.path(), &vars(Some(&file), Some(&appdir)), Some(&exe));
        assert_eq!(c.kind, ChannelKind::AppImage);
        assert_eq!(c.appimage, Some(file.as_path()));
        drop(d);
    }

    #[test]
    fn leaked_appimage_variables_do_not_make_an_appimage() {
        let (_d, appdir, _exe, file) = appimage_fixture();
        let none = tempfile::tempdir().unwrap();
        let outside = Path::new("/usr/bin/siple");
        let c = detect_in(
            none.path(),
            &vars(Some(&file), Some(&appdir)),
            Some(outside),
        );
        assert_eq!(c, Channel::DIRECT);
    }

    #[test]
    fn appimage_needs_absolute_variables_and_a_file() {
        let (d, appdir, exe, file) = appimage_fixture();
        let none = tempfile::tempdir().unwrap();
        let go = |a: Option<&Path>, b: Option<&Path>, e: Option<&Path>| {
            detect_in(none.path(), &vars(a, b), e)
        };
        assert_eq!(go(None, Some(&appdir), Some(&exe)), Channel::DIRECT);
        assert_eq!(go(Some(&file), None, Some(&exe)), Channel::DIRECT);
        assert_eq!(
            go(Some(Path::new("")), Some(&appdir), Some(&exe)),
            Channel::DIRECT
        );
        assert_eq!(
            go(
                Some(Path::new("x/Siple.AppImage")),
                Some(&appdir),
                Some(&exe)
            ),
            Channel::DIRECT,
            "relative"
        );
        assert_eq!(
            go(Some(Path::new("appdir")), Some(&appdir), Some(&exe)),
            Channel::DIRECT,
            "relative appdir"
        );
        let missing = d.path().join("nope.AppImage");
        assert_eq!(
            go(Some(&missing), Some(&appdir), Some(&exe)),
            Channel::DIRECT
        );
        assert_eq!(
            go(Some(&appdir), Some(&appdir), Some(&exe)),
            Channel::DIRECT,
            "APPIMAGE is a directory"
        );
        assert_eq!(
            go(Some(&file), Some(&d.path().join("gone")), Some(&exe)),
            Channel::DIRECT
        );
        assert_eq!(
            go(Some(&file), Some(&appdir), Some(&appdir)),
            Channel::DIRECT,
            "exe == appdir"
        );
        assert_eq!(go(Some(&file), Some(&appdir), None), Channel::DIRECT);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_appdir_is_canonicalized() {
        let (d, appdir, exe, file) = appimage_fixture();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&appdir, &link).unwrap();
        let flink = d.path().join("flink.AppImage");
        std::os::unix::fs::symlink(&file, &flink).unwrap();
        let none = tempfile::tempdir().unwrap();
        let c = detect_in(none.path(), &vars(Some(&flink), Some(&link)), Some(&exe));
        assert_eq!(c.kind, ChannelKind::AppImage);
        assert_eq!(c.appimage, Some(file.as_path()));
    }

    #[test]
    fn the_flatpak_wins_over_appimage_variables() {
        let (_d, appdir, exe, file) = appimage_fixture();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".flatpak-info"),
            "[Application]\nname=io.zyno.siple\n",
        )
        .unwrap();
        let c = detect_in(root.path(), &vars(Some(&file), Some(&appdir)), Some(&exe));
        assert_eq!(c, Channel::FLATPAK);
    }

    #[test]
    fn appimage_uses_the_direct_config_dir() {
        let c = Channel::appimage(Path::new("/x/Siple.AppImage"));
        assert_eq!(
            config_dir_for_channel(&c),
            crate::paths::direct_config_dir()
        );
    }

    #[test]
    fn launch_target_is_the_appimage_file() {
        let c = Channel::appimage(Path::new("/x/Siple.AppImage"));
        assert_eq!(c.launch_target().unwrap(), Path::new("/x/Siple.AppImage"));
        assert_eq!(
            Channel::DIRECT.launch_target().unwrap(),
            std::env::current_exe().unwrap()
        );
    }

    #[test]
    fn runs_inside_is_strict() {
        assert!(runs_inside(
            Path::new("/a/b/usr/bin/siple"),
            Path::new("/a/b")
        ));
        assert!(!runs_inside(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!runs_inside(Path::new("/a/bc/x"), Path::new("/a/b")));
        assert!(!runs_inside(Path::new("a/b/x"), Path::new("a/b")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cloexec_marks_inherited_descriptors() {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` has room for two descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let flags = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags(fds[0]) & libc::FD_CLOEXEC, 0);
        let std_before: Vec<i32> = (0..3).map(flags).collect();
        cloexec_inherited_fds();
        assert_ne!(flags(fds[0]) & libc::FD_CLOEXEC, 0);
        assert_ne!(flags(fds[1]) & libc::FD_CLOEXEC, 0);
        assert_eq!((0..3).map(flags).collect::<Vec<_>>(), std_before);
        // SAFETY: closing the pipe ends opened above.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_home_is_absolute() {
        if let Some(h) = real_home() {
            assert!(h.is_absolute());
        }
    }
}
