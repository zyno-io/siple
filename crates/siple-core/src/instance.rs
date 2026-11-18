//! Single-instance lock and `runtime.json` discovery file.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{
    ControlConfig, LEGACY_LOCK_FILE, LEGACY_RUNNING_MESSAGE, LOCK_FILE, RUNTIME_FILE, Secret,
};

/// Failure in lock or runtime-file handling.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    /// Another process holds the lock for this config directory.
    #[error(
        "another Siple instance is already running for this config directory ({}){}",
        lock_path.display(),
        pid.map(|p| format!(", pid {p}")).unwrap_or_default()
    )]
    AlreadyRunning {
        /// The lock file that is held.
        lock_path: PathBuf,
        /// PID of the holder, if `runtime.json` or the lock file says so.
        pid: Option<u32>,
    },
    /// The pre-rename build holds its lock in this directory.
    #[error("{LEGACY_RUNNING_MESSAGE} ({})", lock_path.display())]
    LegacyRunning {
        /// The pre-rename lock file that is held.
        lock_path: PathBuf,
    },
    /// Filesystem failure.
    #[error("{}: {source}", path.display())]
    Io {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// `runtime.json` is malformed.
    #[error("invalid {}: {source}", path.display())]
    Json {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        source: serde_json::Error,
    },
    /// The OS random source failed.
    #[error("cannot generate token: {0}")]
    Random(String),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> InstanceError + '_ {
    move |source| InstanceError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Exclusive lock on `<config dir>/siple.lock`, held until dropped (or the process exits,
/// including crashes). The lock file itself is left in place.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
    /// The pre-rename lock file, held too (through 0.3.x) so an old build running in the same
    /// directory is detected, and cannot start while we run.
    _legacy: Option<File>,
    path: PathBuf,
}

impl InstanceLock {
    /// Takes the lock for `dir`, creating the directory if needed.
    pub fn acquire(dir: &Path) -> Result<Self, InstanceError> {
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        let path = dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err(&path))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let pid = RuntimeInfo::read(dir).ok().map(|r| r.pid);
                return Err(InstanceError::AlreadyRunning {
                    lock_path: path,
                    pid,
                });
            }
            Err(TryLockError::Error(e)) => return Err(io_err(&path)(e)),
        }
        let legacy_path = dir.join(LEGACY_LOCK_FILE);
        let legacy = match OpenOptions::new().read(true).write(true).open(&legacy_path) {
            Ok(f) => match f.try_lock() {
                Ok(()) => Some(f),
                Err(TryLockError::WouldBlock) => {
                    return Err(InstanceError::LegacyRunning {
                        lock_path: legacy_path,
                    });
                }
                Err(TryLockError::Error(e)) => return Err(io_err(&legacy_path)(e)),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_err(&legacy_path)(e)),
        };
        file.set_len(0).map_err(io_err(&path))?;
        write!(file, "{}", std::process::id()).map_err(io_err(&path))?;
        Ok(Self {
            _file: file,
            _legacy: legacy,
            path,
        })
    }

    /// Path of the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Where a running instance's control API listens. Absent endpoints are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ControlEndpoints {
    /// Unix domain socket path (macOS/Linux).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_socket: Option<String>,
    /// Windows named pipe name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named_pipe: Option<String>,
    /// Loopback TCP address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<String>,
}

/// Longest unix socket path we accept; `sun_path` is 104 bytes on macOS (108 on Linux).
const MAX_SOCKET_PATH: usize = 100;

impl ControlEndpoints {
    /// Resolves the endpoints implied by `[control]`: platform defaults for the empty values,
    /// and `tcp_listen` when set. Only the endpoint native to this platform is filled in.
    pub fn from_config(control: &ControlConfig, config_dir: &Path) -> Self {
        let mut out = Self::default();
        if cfg!(windows) {
            out.named_pipe = Some(if control.named_pipe.is_empty() {
                let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
                format!(r"\\.\pipe\siple-{user}-{}", dir_hash8(config_dir))
            } else {
                control.named_pipe.clone()
            });
        } else {
            let path = if control.unix_socket.is_empty() {
                let channel = siple_config::channel::current();
                // Only the Mac App Store build needs the group container (a passwd lookup).
                let group = (channel.kind == siple_config::channel::ChannelKind::MacAppStore)
                    .then(siple_config::channel::mas_group_dir)
                    .flatten();
                default_unix_socket(config_dir, channel, group.as_deref())
            } else {
                PathBuf::from(&control.unix_socket)
            };
            out.unix_socket = Some(path.to_string_lossy().into_owned());
        }
        if let Ok(addr) = control.tcp_listen.parse::<SocketAddr>() {
            out.tcp = Some(addr.to_string());
        }
        out
    }
}

/// Default control socket for `config_dir`: `<config_dir>/control.sock`, or a short fallback when
/// that exceeds `sun_path`.
///
/// Mac App Store: the fallback is `<group container>/s.sock` for the default config dir and
/// `<group container>/s-<hash>.sock` for any other (inside the container, reachable from the
/// unsandboxed world; the sandbox's own `$TMPDIR` is not), never `temp_dir()`. When even that is
/// too long the long path is returned after logging how to avoid it; binding reports the failure.
fn default_unix_socket(
    config_dir: &Path,
    channel: &siple_config::channel::Channel,
    mas_group: Option<&Path>,
) -> PathBuf {
    let p = config_dir.join("control.sock");
    if p.as_os_str().len() <= MAX_SOCKET_PATH {
        return p;
    }
    if channel.kind == siple_config::channel::ChannelKind::MacAppStore {
        if let Some(s) = mas_group.map(|g| {
            if config_dir == g.join(siple_config::channel::SIPLE_DIR) {
                g.join("s.sock")
            } else {
                g.join(format!("s-{}.sock", dir_hash8(config_dir)))
            }
        }) && s.as_os_str().len() <= MAX_SOCKET_PATH
        {
            return s;
        }
        tracing::error!("control socket path is too long; set [control] tcp_listen");
        return p;
    }
    // On failure keep the long path; binding it reports the problem.
    short_socket_path(&p.to_string_lossy()).unwrap_or_else(|e| {
        tracing::error!("cannot use a short control socket path: {e}");
        p
    })
}

/// First 8 hex chars of sha256 over the absolute config directory: makes default endpoint
/// names unique per config directory (two instances of one user can coexist).
fn dir_hash8(config_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let abs = crate::config::absolute_path(config_dir);
    let digest = Sha256::digest(abs.to_string_lossy().as_bytes());
    hex(&digest[..4])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Directory for control sockets whose configured path is too long for `sun_path`.
///
/// Linux: `$XDG_RUNTIME_DIR` when it is a real directory owned by this user (not a symlink), else a private `/tmp/siple-<euid>/` (created 0700
/// and verified: owned by this user, a real directory, not a symlink; anything else is
/// refused). macOS and other platforms: `$TMPDIR` (per-user already).
pub fn short_socket_dir() -> io::Result<PathBuf> {
    // The sandbox's `$TMPDIR` is inside the app container: unreachable for the CLI. Stay in the
    // group container instead.
    if siple_config::channel::current().kind == siple_config::channel::ChannelKind::MacAppStore {
        return siple_config::channel::mas_group_dir().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "cannot determine the home directory",
            )
        });
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let xdg = std::env::var_os("XDG_RUNTIME_DIR");
        short_socket_dir_in(xdg.as_deref(), euid, Path::new("/tmp"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(std::env::temp_dir())
    }
}

/// Pure core of the Linux [`short_socket_dir`]: `xdg` (the `XDG_RUNTIME_DIR` value) when it is a
/// trusted directory of `euid`, else a private `<tmp_root>/siple-<euid>/`.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn short_socket_dir_in(
    xdg: Option<&std::ffi::OsStr>,
    euid: u32,
    tmp_root: &Path,
) -> io::Result<PathBuf> {
    if let Some(d) = xdg.filter(|d| !d.is_empty()) {
        let d = PathBuf::from(d);
        if usable_runtime_dir(&d, euid) {
            return Ok(d);
        }
    }
    let dir = tmp_root.join(format!("siple-{euid}"));
    ensure_private_dir(&dir, euid)?;
    Ok(dir)
}

/// Short, stable socket path (`<short_socket_dir>/siple-<hash of key>.sock`).
pub fn short_socket_path(key: &str) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(key.as_bytes());
    Ok(short_socket_dir()?.join(format!("siple-{}.sock", hex(&d[..6]))))
}

/// Whether `$XDG_RUNTIME_DIR` can be trusted: a real directory (not a symlink) owned by `euid`.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn usable_runtime_dir(dir: &Path, euid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(dir).is_ok_and(|md| md.is_dir() && md.uid() == euid)
}

/// Creates `dir` with mode 0700 if missing and checks it is a real directory (not a symlink)
/// owned by `euid`; group/other permission bits on an existing directory of ours are removed.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn ensure_private_dir(dir: &Path, euid: u32) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let bad = |why: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to use {} for the control socket: {why}; \
                 set XDG_RUNTIME_DIR or remove it",
                dir.display()
            ),
        )
    };
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = fs::symlink_metadata(dir)?;
    if md.file_type().is_symlink() {
        return Err(bad("it is a symlink"));
    }
    if !md.is_dir() {
        return Err(bad("it is not a directory"));
    }
    if md.uid() != euid {
        return Err(bad("it is owned by another user"));
    }
    if md.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Contents of `runtime.json`: how clients find and authenticate to a running instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// Process id of the instance.
    pub pid: u32,
    /// Siple version.
    pub version: String,
    /// When this instance started.
    pub started_at: DateTime<Utc>,
    /// Control API endpoints.
    pub control: ControlEndpoints,
    /// Random per-run token (64 hex chars); required on TCP, sent in `hello`.
    pub token: Secret,
}

impl RuntimeInfo {
    /// Info for the current process with a fresh random token.
    pub fn new(control: ControlEndpoints) -> Result<Self, InstanceError> {
        Ok(Self {
            pid: std::process::id(),
            version: crate::update::full_version().to_string(),
            started_at: DateTime::from_timestamp(Utc::now().timestamp(), 0)
                .unwrap_or_else(Utc::now),
            control,
            token: Secret::new(random_token()?),
        })
    }

    /// Reads `<dir>/runtime.json`.
    pub fn read(dir: &Path) -> Result<Self, InstanceError> {
        let path = dir.join(RUNTIME_FILE);
        let text = fs::read_to_string(&path).map_err(io_err(&path))?;
        serde_json::from_str(&text).map_err(|source| InstanceError::Json { path, source })
    }

    /// Writes `<dir>/runtime.json` (0600 on unix) atomically. The returned guard removes the
    /// file when dropped, i.e. on clean shutdown.
    ///
    /// The caller must hold the [`InstanceLock`]. A `runtime.json` left behind by a crashed
    /// instance is harmless (the lock is free, so nothing is running) and is simply replaced.
    pub fn write(&self, dir: &Path) -> Result<RuntimeFileGuard, InstanceError> {
        let path = dir.join(RUNTIME_FILE);
        remove_stale_tmps(dir);
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).map_err(|e| InstanceError::Random(e.to_string()))?;
        let tmp = dir.join(format!(
            "{RUNTIME_FILE}.{}.{}.tmp",
            std::process::id(),
            hex(&rnd)
        ));
        let json = serde_json::to_string_pretty(self).expect("RuntimeInfo always serializes");
        // create_new fails on any existing entry, including a symlink planted at `tmp`.
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let result = (|| {
            let mut f = opts.open(&tmp).map_err(io_err(&tmp))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(io_err(&tmp))?;
            }
            f.write_all(json.as_bytes()).map_err(io_err(&tmp))?;
            f.sync_all().map_err(io_err(&tmp))?;
            drop(f);
            fs::rename(&tmp, &path).map_err(io_err(&path))
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(RuntimeFileGuard {
            path,
            token: self.token.clone(),
            armed: true,
        })
    }
}

/// Deletes leftover `runtime.json.*.tmp` files (and the legacy `runtime.json.tmp`) from
/// crashed writers. Only the lock holder writes, so none can be in use.
fn remove_stale_tmps(dir: &Path) {
    let prefix = format!("{RUNTIME_FILE}.");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Removes `runtime.json` on drop, unless another instance has replaced it meanwhile.
/// Debug output never shows the token.
#[derive(Debug)]
pub struct RuntimeFileGuard {
    path: PathBuf,
    token: Secret,
    armed: bool,
}

impl RuntimeFileGuard {
    /// Gives up the duty to delete the file. Used when `runtime.json` is rewritten: the new
    /// guard (same token) takes over, and dropping the old one afterwards must not delete the
    /// fresh file.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for RuntimeFileGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let ours = fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str::<RuntimeInfo>(&t).ok())
            .is_some_and(|r| r.token.ct_eq(self.token.expose()));
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn random_token() -> Result<String, InstanceError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| InstanceError::Random(e.to_string()))?;
    Ok(hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_build_lock_in_the_same_dir_blocks_and_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join(LEGACY_LOCK_FILE);
        // A held pre-rename lock blocks us.
        let old = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&legacy_path)
            .unwrap();
        old.try_lock().unwrap();
        let err = InstanceLock::acquire(dir.path()).unwrap_err();
        assert!(
            matches!(&err, InstanceError::LegacyRunning { lock_path } if *lock_path == legacy_path),
            "{err}"
        );
        assert!(err.to_string().contains("previous version"), "{err}");
        drop(old);
        // Free again: we take it and hold it for our lifetime.
        let ours = InstanceLock::acquire(dir.path()).unwrap();
        let probe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&legacy_path)
            .unwrap();
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(ours);
        assert!(probe.try_lock().is_ok());
    }

    #[test]
    fn second_lock_fails_then_succeeds_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let first = InstanceLock::acquire(dir.path()).unwrap();
        let err = InstanceLock::acquire(dir.path()).unwrap_err();
        assert!(matches!(err, InstanceError::AlreadyRunning { .. }), "{err}");
        assert!(err.to_string().contains("already running"));
        // Windows locks are mandatory: the locked file can't be read through another handle.
        #[cfg(unix)]
        assert_eq!(
            fs::read_to_string(first.path()).unwrap(),
            std::process::id().to_string()
        );
        drop(first);
        InstanceLock::acquire(dir.path()).unwrap();
    }

    #[test]
    fn already_running_reports_pid_from_runtime_json() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = InstanceLock::acquire(dir.path()).unwrap();
        let info = RuntimeInfo::new(ControlEndpoints::default()).unwrap();
        let _guard = info.write(dir.path()).unwrap();
        let msg = InstanceLock::acquire(dir.path()).unwrap_err().to_string();
        assert!(msg.contains(&format!("pid {}", info.pid)), "{msg}");
    }

    #[test]
    fn runtime_roundtrip_and_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = ControlEndpoints {
            unix_socket: Some("/tmp/x.sock".into()),
            tcp: Some("127.0.0.1:5079".into()),
            ..Default::default()
        };
        let info = RuntimeInfo::new(ctl.clone()).unwrap();
        assert_eq!(info.token.expose().len(), 64);
        assert!(info.token.expose().chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        let guard = info.write(dir.path()).unwrap();
        let read = RuntimeInfo::read(dir.path()).unwrap();
        assert_eq!(read.pid, std::process::id());
        assert_eq!(read.token, info.token);
        assert_eq!(read.control, ctl);
        let raw = fs::read_to_string(dir.path().join(RUNTIME_FILE)).unwrap();
        assert!(raw.contains("\"unix_socket\"") && !raw.contains("named_pipe"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(RUNTIME_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(guard);
        assert!(!dir.path().join(RUNTIME_FILE).exists());
        assert!(RuntimeInfo::read(dir.path()).is_err());
    }

    #[test]
    fn guard_does_not_remove_a_newer_instances_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = RuntimeInfo::new(ControlEndpoints::default()).unwrap();
        let old_guard = old.write(dir.path()).unwrap();
        let newer = RuntimeInfo::new(ControlEndpoints::default()).unwrap();
        let _new_guard = newer.write(dir.path()).unwrap();
        drop(old_guard);
        assert_eq!(RuntimeInfo::read(dir.path()).unwrap().token, newer.token);
    }

    #[test]
    fn guard_debug_hides_token_and_stale_tmps_are_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        let info = RuntimeInfo::new(ControlEndpoints::default()).unwrap();
        fs::write(dir.path().join("runtime.json.tmp"), "stale").unwrap();
        fs::write(dir.path().join("runtime.json.1.abcd.tmp"), "stale").unwrap();
        fs::write(dir.path().join("runtime.json"), "{ crashed").unwrap(); // overwritten
        let guard = info.write(dir.path()).unwrap();
        let dbg = format!("{guard:?} {info:?}");
        assert!(!dbg.contains(info.token.expose()), "{dbg}");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["runtime.json"]);
        assert_eq!(RuntimeInfo::read(dir.path()).unwrap().token, info.token);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_file_is_private_even_with_permissive_umask_and_symlink_tmp() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        // A planted symlink at the legacy tmp name must not be followed or written through.
        symlink(&victim, dir.path().join("runtime.json.tmp")).unwrap();
        let info = RuntimeInfo::new(ControlEndpoints::default()).unwrap();
        let _g = info.write(dir.path()).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        let mode = fs::metadata(dir.path().join(RUNTIME_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn secret_constant_time_eq() {
        let s = Secret::new("abc123");
        assert!(s.ct_eq("abc123"));
        assert!(!s.ct_eq("abc124") && !s.ct_eq("abc12") && !s.ct_eq(""));
        assert_eq!(s, Secret::new("abc123"));
        assert_ne!(s, Secret::new("abc124"));
    }

    #[test]
    fn pipe_name_hash_is_per_directory() {
        assert_ne!(dir_hash8(Path::new("/a")), dir_hash8(Path::new("/b")));
        assert_eq!(dir_hash8(Path::new("/a")).len(), 8);
    }

    #[test]
    fn mas_default_socket_fits_for_normal_users() {
        use siple_config::channel::{Channel, mas_dir_in, mas_group_in};
        let home = Path::new("/Users/someone");
        let cfg = mas_dir_in(home);
        let p = default_unix_socket(&cfg, &Channel::MAC_APP_STORE, Some(&mas_group_in(home)));
        assert_eq!(
            p,
            Path::new(
                "/Users/someone/Library/Group Containers/VUDU4KEQ99.io.zyno.siple/Siple/control.sock"
            )
        );
        // 76 bytes plus the user name, as documented.
        assert_eq!(p.as_os_str().len(), 76 + "someone".len());
    }

    #[test]
    fn mas_long_home_falls_back_inside_the_group_container() {
        use siple_config::channel::{Channel, mas_dir_in, mas_group_in};
        let user = "u".repeat(30);
        let home = PathBuf::from(format!("/Users/{user}"));
        let group = mas_group_in(&home);
        let p = default_unix_socket(&mas_dir_in(&home), &Channel::MAC_APP_STORE, Some(&group));
        assert_eq!(p, group.join("s.sock"));
        assert!(p.as_os_str().len() <= MAX_SOCKET_PATH);
        assert!(!p.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn mas_other_profile_gets_its_own_short_socket() {
        use siple_config::channel::{Channel, mas_group_in};
        let home = PathBuf::from(format!("/Users/{}", "u".repeat(20)));
        let group = mas_group_in(&home);
        let p2 = group.join("second-profile");
        let p = default_unix_socket(&p2, &Channel::MAC_APP_STORE, Some(&group));
        assert_eq!(p, group.join(format!("s-{}.sock", dir_hash8(&p2))));
        assert!(p.as_os_str().len() <= MAX_SOCKET_PATH);
    }

    #[test]
    fn mas_too_long_even_for_the_short_name_keeps_the_long_path() {
        use siple_config::channel::{Channel, mas_dir_in, mas_group_in};
        let home = PathBuf::from(format!("/Users/{}", "u".repeat(60)));
        let cfg = mas_dir_in(&home);
        let p = default_unix_socket(&cfg, &Channel::MAC_APP_STORE, Some(&mas_group_in(&home)));
        // Never a temp_dir() socket: the caller gets the long path and a bind error.
        assert_eq!(p, cfg.join("control.sock"));
        let p = default_unix_socket(&cfg, &Channel::MAC_APP_STORE, None);
        assert_eq!(p, cfg.join("control.sock"));
    }

    #[test]
    fn direct_long_path_uses_short_hashed_socket() {
        use siple_config::channel::Channel;
        let long = PathBuf::from(format!("/{}", "d/".repeat(60)));
        let p = default_unix_socket(&long, &Channel::DIRECT, None);
        assert!(p.as_os_str().len() <= MAX_SOCKET_PATH);
        assert!(p.to_string_lossy().contains("siple-"));
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(random_token().unwrap(), random_token().unwrap());
    }

    #[test]
    fn endpoints_from_config() {
        let dir = Path::new("/cfg");
        let mut c = ControlConfig::default();
        let e = ControlEndpoints::from_config(&c, dir);
        if cfg!(windows) {
            assert!(e.named_pipe.unwrap().starts_with(r"\\.\pipe\siple-"));
        } else {
            assert_eq!(e.unix_socket.as_deref(), Some("/cfg/control.sock"));
        }
        assert!(e.tcp.is_none());
        c.tcp_listen = "127.0.0.1:5079".into();
        assert_eq!(
            ControlEndpoints::from_config(&c, dir).tcp.as_deref(),
            Some("127.0.0.1:5079")
        );

        if !cfg!(windows) {
            let long = Path::new(
                "/a/very/long/directory/name/that/goes/on/and/on/and/on/and/on/and/on/and/on/and/on/and/on",
            );
            let p = ControlEndpoints::from_config(&ControlConfig::default(), long)
                .unix_socket
                .unwrap();
            assert!(p.len() <= MAX_SOCKET_PATH && p.ends_with(".sock"), "{p}");
        }
    }

    #[cfg(unix)]
    mod private_dir {
        use super::super::{ensure_private_dir, usable_runtime_dir};
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        fn euid() -> u32 {
            // SAFETY: geteuid cannot fail.
            unsafe { libc::geteuid() }
        }

        #[test]
        fn runtime_dir_trust() {
            let t = tempfile::tempdir().unwrap();
            let real = t.path().join("real");
            std::fs::create_dir(&real).unwrap();
            assert!(usable_runtime_dir(&real, euid()));
            assert!(!usable_runtime_dir(&real, euid().wrapping_add(1)));
            let link = t.path().join("link");
            symlink(&real, &link).unwrap();
            assert!(!usable_runtime_dir(&link, euid()));
            let file = t.path().join("file");
            std::fs::write(&file, "").unwrap();
            assert!(!usable_runtime_dir(&file, euid()));
            assert!(!usable_runtime_dir(&t.path().join("missing"), euid()));
        }

        #[test]
        fn short_socket_dir_in_prefers_trusted_xdg_then_private_fallback() {
            use super::super::short_socket_dir_in;
            let t = tempfile::tempdir().unwrap();
            let xdg = t.path().join("xdg");
            std::fs::create_dir(&xdg).unwrap();
            // Trusted XDG dir wins.
            let got = short_socket_dir_in(Some(xdg.as_os_str()), euid(), t.path()).unwrap();
            assert_eq!(got, xdg);
            // Missing, empty or absent XDG: private fallback under the temp root.
            let missing = t.path().join("missing");
            for x in [Some(missing.as_os_str()), Some("".as_ref()), None] {
                let dir = short_socket_dir_in(x, euid(), t.path()).unwrap();
                assert_eq!(dir, t.path().join(format!("siple-{}", euid())));
                let md = std::fs::metadata(&dir).unwrap();
                assert!(md.is_dir());
                assert_eq!(md.permissions().mode() & 0o777, 0o700);
                assert_eq!(md.uid(), euid());
            }
            // A foreign-owned XDG dir is not trusted.
            let dir = short_socket_dir_in(Some(xdg.as_os_str()), euid().wrapping_add(1), t.path());
            assert!(
                dir.is_err(),
                "fallback dir is owned by us, not euid+1: {dir:?}"
            );
            // The real function yields a usable, short directory.
            let real = super::super::short_socket_dir().expect("socket dir");
            assert!(real.is_dir(), "{real:?}");
            let p = real.join("siple-0123456789ab.sock");
            assert!(p.as_os_str().len() < 104, "{p:?}");
        }

        #[test]
        fn creates_0700() {
            let t = tempfile::tempdir().unwrap();
            let d = t.path().join("zp");
            ensure_private_dir(&d, euid()).unwrap();
            let md = std::fs::metadata(&d).unwrap();
            assert!(md.is_dir());
            assert_eq!(md.permissions().mode() & 0o777, 0o700);
            assert_eq!(md.uid(), euid());
            ensure_private_dir(&d, euid()).unwrap(); // idempotent
        }

        #[test]
        fn tightens_loose_mode() {
            let t = tempfile::tempdir().unwrap();
            let d = t.path().join("zp");
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
            ensure_private_dir(&d, euid()).unwrap();
            assert_eq!(
                std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        #[test]
        fn rejects_symlink_file_and_foreign_owner() {
            let t = tempfile::tempdir().unwrap();
            let real = t.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let link = t.path().join("link");
            symlink(&real, &link).unwrap();
            let e = ensure_private_dir(&link, euid()).unwrap_err();
            assert!(e.to_string().contains("symlink"), "{e}");

            let file = t.path().join("file");
            std::fs::write(&file, "").unwrap();
            assert!(ensure_private_dir(&file, euid()).is_err());

            // Owner mismatch: pretend to be a different user.
            let e = ensure_private_dir(&real, euid().wrapping_add(1)).unwrap_err();
            assert!(e.to_string().contains("another user"), "{e}");
        }
    }

    #[test]
    fn short_socket_path_is_stable_and_short() {
        let a = short_socket_path("/some/long/path").unwrap();
        assert_eq!(a, short_socket_path("/some/long/path").unwrap());
        assert_ne!(a, short_socket_path("/other").unwrap());
        assert!(a.as_os_str().len() <= MAX_SOCKET_PATH, "{a:?}");
    }
}
