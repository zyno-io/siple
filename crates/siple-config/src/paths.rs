//! Default file locations and first-run config creation.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::{EXAMPLE_CONFIG, GeneralConfig};

/// Reverse-DNS application id used for everything the OS registers on our behalf: notification
/// app id, LaunchAgent label, URL-protocol and autostart entries, tray ids, keyring service,
/// window/desktop id.
pub const APP_ID: &str = "io.zyno.siple";
/// Name of the config file inside the config directory.
pub const CONFIG_FILE: &str = "config.toml";
/// Name of the single-instance lock file.
pub const LOCK_FILE: &str = "siple.lock";
/// Name of the runtime discovery file.
pub const RUNTIME_FILE: &str = "runtime.json";
/// Base name of the application log; rolled files get a `.<date>` suffix.
pub const LOG_FILE: &str = "siple.log";

#[cfg(any(target_os = "macos", target_os = "windows"))]
const APP_DIR: &str = "Siple";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const APP_DIR: &str = "siple";

/// Config directory of this process's distribution channel (see [`crate::channel`]): the
/// platform default for direct installs (`~/Library/Application Support/Siple`,
/// `%APPDATA%\Siple`, or `$XDG_CONFIG_HOME/siple` elsewhere), the app group container for the Mac
/// App Store build, the package's LocalState for MSIX, and the per-app `$XDG_CONFIG_HOME/siple`
/// (`~/.var/app/<app id>/config/siple`) in a Flatpak.
pub fn default_config_dir() -> Option<PathBuf> {
    crate::channel::config_dir()
}

/// The platform default config directory of unpackaged installs.
pub(crate) fn direct_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APP_DIR))
}

/// `<default config dir>/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    default_config_dir().map(|d| d.join(CONFIG_FILE))
}

/// `path` made absolute (without resolving symlinks or requiring it to exist). Falls back to
/// the input if the working directory is unavailable.
pub fn absolute_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Absolute directory holding `config_path`; lock, runtime file, control socket and logs live
/// relative to it. Always absolute so `runtime.json` never advertises relative socket paths.
pub fn config_dir_for(config_path: &Path) -> PathBuf {
    let abs = absolute_path(config_path);
    match abs.parent() {
        Some(p) => p.to_path_buf(),
        None => abs,
    }
}

/// Effective log directory: `[general].log_dir`, or `<config dir>/logs` when empty.
/// Relative values are resolved against the config directory.
pub fn resolve_log_dir(general: &GeneralConfig, config_dir: &Path) -> PathBuf {
    if general.log_dir.is_empty() {
        config_dir.join("logs")
    } else {
        config_dir.join(&general.log_dir) // absolute paths replace the base
    }
}

/// Writes the embedded example config to `path` if it does not exist yet (mode 0600 on unix,
/// since it will hold passwords). Returns whether a file was created.
pub fn ensure_config(path: &Path) -> io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(mut f) => {
            f.write_all(EXAMPLE_CONFIG.as_bytes())?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_config_creates_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CONFIG_FILE);
        assert!(ensure_config(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), EXAMPLE_CONFIG);
        fs::write(&path, "# mine\n").unwrap();
        assert!(!ensure_config(&path).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "# mine\n");
    }

    #[test]
    fn log_dir_resolution() {
        let base = Path::new("/cfg");
        let mut g = GeneralConfig::default();
        assert_eq!(resolve_log_dir(&g, base), Path::new("/cfg/logs"));
        g.log_dir = "out".into();
        assert_eq!(resolve_log_dir(&g, base), Path::new("/cfg/out"));
        g.log_dir = "/var/log/zp".into();
        assert_eq!(resolve_log_dir(&g, base), Path::new("/var/log/zp"));
    }

    #[test]
    fn config_dir_of_bare_filename() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(config_dir_for(Path::new("config.toml")), cwd);
        assert_eq!(
            config_dir_for(Path::new("sub/config.toml")),
            cwd.join("sub")
        );
        assert!(config_dir_for(Path::new("x/../c.toml")).is_absolute());
        // An absolute path (with a drive letter on Windows) is kept as is.
        let abs = std::env::temp_dir().join("a").join("b");
        assert_eq!(config_dir_for(&abs.join("config.toml")), abs);
    }

    #[test]
    fn default_dir_name() {
        let dir = default_config_dir().unwrap();
        assert!(dir.ends_with(APP_DIR));
    }
}
