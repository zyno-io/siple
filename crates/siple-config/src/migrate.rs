// siple-rename: keep (this file holds the legacy ZynoPhone names on purpose)
//! One-time migration of the legacy `ZynoPhone` config directory to the `Siple` one.
//!
//! The caller (`siple_core::App::start`) takes the new instance lock first, then calls
//! [`migrate`]. Nothing here ever deletes or modifies the legacy directory except for adding
//! the `MOVED_TO_SIPLE.txt` note.

use std::fs::{self, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use super::paths::CONFIG_FILE;

/// Legacy config directory name on macOS and Windows.
#[cfg(any(target_os = "macos", target_os = "windows"))]
const LEGACY_APP_DIR: &str = "ZynoPhone";
/// Legacy config directory name elsewhere.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const LEGACY_APP_DIR: &str = "zynophone";

/// Legacy single-instance lock file.
pub const LEGACY_LOCK_FILE: &str = "zynophone.lock";
/// Top-level entries that are never copied.
const EXCLUDED: &[&str] = &[LEGACY_LOCK_FILE, "runtime.json", "logs"];
/// Written into the new dir with the legacy path.
pub const MIGRATED_FROM_FILE: &str = "MIGRATED_FROM";
/// Message of the error raised when the pre-rename build holds its lock in the same directory.
pub const LEGACY_RUNNING_MESSAGE: &str =
    "ZynoPhone (the previous version) is running; quit it to start Siple";
/// Written into the legacy dir.
pub const MOVED_TO_FILE: &str = "MOVED_TO_SIPLE.txt";

/// Why a migration did not happen.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// The old app is running.
    #[error("Quit ZynoPhone to migrate settings (it holds {})", .0.display())]
    LegacyRunning(PathBuf),
    /// Filesystem failure.
    #[error("config migration: {}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> MigrateError + '_ {
    move |source| MigrateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// `<platform config>/ZynoPhone` (Linux `zynophone`).
pub fn legacy_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(LEGACY_APP_DIR))
}

/// Deprecated alias of `SIPLE_CONFIG`; honoured through 0.3.x.
const LEGACY_CONFIG_ENV: &str = "ZYNOPHONE_CONFIG";

/// Result of [`resolve_config_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    /// Config file to run with.
    pub path: PathBuf,
    /// `true` only for the platform default, the one case where migration applies.
    pub is_default: bool,
    /// Deprecation notices to log once logging is up (already printed to stderr).
    pub warnings: Vec<String>,
}

/// Pure core of [`resolve_config_path`]; `legacy_env` is the value of `ZYNOPHONE_CONFIG`.
/// Returns `(explicit path, warnings)`.
pub fn resolve_explicit(
    parsed: Option<PathBuf>,
    legacy_env: Option<std::ffi::OsString>,
) -> (Option<PathBuf>, Vec<String>) {
    let legacy = legacy_env.filter(|v| !v.is_empty()).map(PathBuf::from);
    match (parsed, legacy) {
        (Some(p), Some(l)) if l != p => (
            Some(p),
            vec![
                "ZYNOPHONE_CONFIG is deprecated and ignored because --config/SIPLE_CONFIG is set"
                    .into(),
            ],
        ),
        (Some(p), _) => (Some(p), Vec::new()),
        (None, Some(l)) => (
            Some(l),
            vec!["ZYNOPHONE_CONFIG is deprecated; use SIPLE_CONFIG".into()],
        ),
        (None, None) => (None, Vec::new()),
    }
}

/// Resolves the config path from a parsed `--config`/`SIPLE_CONFIG` value, the deprecated
/// `ZYNOPHONE_CONFIG`, or the platform default. Deprecation warnings are printed to stderr
/// here and returned so the caller can log them once logging is up.
pub fn resolve_config_path(parsed: Option<PathBuf>) -> Option<ResolvedConfig> {
    let (explicit, warnings) = resolve_explicit(parsed, std::env::var_os(LEGACY_CONFIG_ENV));
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    match explicit {
        Some(path) => Some(ResolvedConfig {
            path,
            is_default: false,
            warnings,
        }),
        None => crate::paths::default_config_path().map(|path| ResolvedConfig {
            path,
            is_default: true,
            warnings,
        }),
    }
}

/// True when the new config does not exist and the legacy one does.
pub fn needs_migration(new_dir: &Path, legacy_dir: &Path) -> bool {
    !new_dir.join(CONFIG_FILE).exists() && legacy_dir.join(CONFIG_FILE).is_file()
}

fn temp_prefix(new_dir: &Path) -> Option<(PathBuf, String)> {
    let parent = new_dir.parent()?.to_path_buf();
    let name = new_dir.file_name()?.to_string_lossy().into_owned();
    Some((parent, format!("{name}.migrating-")))
}

/// Like `remove_dir_all`, but first makes every directory owner-writable so read-only
/// (e.g. 0500) subdirectories copied from the legacy dir cannot block the removal.
fn force_remove(path: &Path) {
    fn unlock(p: &Path) {
        let Ok(md) = fs::symlink_metadata(p) else {
            return;
        };
        if md.is_dir() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = md.permissions();
                perms.set_mode(perms.mode() | 0o700);
                let _ = fs::set_permissions(p, perms);
            }
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    unlock(&e.path());
                }
            }
        }
    }
    unlock(path);
    let _ = fs::remove_dir_all(path);
}

/// Deletes `<new dir name>.migrating-*` directories left behind by an interrupted migration.
/// Call only while holding the new instance lock. Errors are ignored (best effort).
pub fn cleanup_stale(new_dir: &Path) {
    let Some((parent, prefix)) = temp_prefix(new_dir) else {
        return;
    };
    let Ok(rd) = fs::read_dir(&parent) else {
        return;
    };
    for e in rd.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) && e.path().is_dir() {
            force_remove(&e.path());
        }
    }
}

/// True when `base` and every component of `rel` below it is a real directory (no symlinks).
fn real_dirs_all_the_way(base: &Path, rel: &Path) -> bool {
    let mut cur = base.to_path_buf();
    if !fs::symlink_metadata(&cur).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    for c in rel.components() {
        cur.push(c);
        if !fs::symlink_metadata(&cur).is_ok_and(|m| m.is_dir()) {
            return false;
        }
    }
    true
}

/// A directory's final permissions, applied after everything is in place.
type DirModes = Vec<(PathBuf, fs::Permissions)>;

/// Copies `src` into `dst` (created), preserving file permissions. Directories are created
/// with owner rwx added (so a read-only source dir cannot block filling it or moving it) and
/// their original permissions are recorded in `modes` (relative to `rel`). Skips sockets,
/// fifos and, at the top level, [`EXCLUDED`] entries.
fn copy_tree(src: &Path, dst: &Path, rel: &Path, modes: &mut DirModes) -> Result<(), MigrateError> {
    fs::create_dir_all(dst).map_err(io_err(dst))?;
    for entry in fs::read_dir(src).map_err(io_err(src))? {
        let entry = entry.map_err(io_err(src))?;
        let name = entry.file_name();
        if rel.as_os_str().is_empty() && EXCLUDED.iter().any(|x| name == *x) {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(&name));
        let ft = entry.file_type().map_err(io_err(&from))?;
        #[cfg(unix)]
        if ft.is_symlink() {
            let target = fs::read_link(&from).map_err(io_err(&from))?;
            std::os::unix::fs::symlink(target, &to).map_err(io_err(&to))?;
            continue;
        }
        // Elsewhere only symlinks to files are followed (copied by content); directory symlinks
        // and junctions are skipped so the copy cannot escape the legacy dir.
        let md = if ft.is_symlink() {
            match fs::metadata(&from) {
                Ok(m) if m.is_file() => m,
                Ok(_) => {
                    tracing::warn!(path = %from.display(), "config migration: skipping directory link");
                    continue;
                }
                Err(_) => continue, // dangling
            }
        } else {
            entry.metadata().map_err(io_err(&from))?
        };
        if md.is_dir() {
            copy_tree(&from, &to, &rel.join(&name), modes)?;
        } else if md.is_file() {
            fs::copy(&from, &to).map_err(io_err(&from))?; // copies the mode bits too
        }
        // sockets, fifos, devices: skipped
    }
    let orig = fs::metadata(src).map_err(io_err(src))?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut open = orig.clone();
        open.set_mode(open.mode() | 0o700);
        fs::set_permissions(dst, open).map_err(io_err(dst))?;
    }
    modes.push((rel.to_path_buf(), orig));
    Ok(())
}

/// Migrates `legacy_dir` into `new_dir`. The caller must hold the new instance lock (so
/// `new_dir` exists and contains the lock file). Returns `Ok(true)` when settings were
/// migrated, `Ok(false)` when there was nothing to do or the new config appeared meanwhile.
///
/// The copy is staged in `<new dir>.migrating-<pid>` next to `new_dir`. Because `new_dir`
/// already exists (it holds the lock file) the staged entries are moved in one by one with
/// `config.toml` last; the config file appearing is the commit point. Directory permissions
/// (including `new_dir` itself) are applied from the legacy ones after the commit. The staging
/// dir is always removed, on success and on failure.
pub fn migrate(new_dir: &Path, legacy_dir: &Path) -> Result<bool, MigrateError> {
    migrate_with(new_dir, legacy_dir, || {})
}

/// [`migrate`] with a hook that runs after the copy and before the commit (tests use it to
/// simulate a concurrent creator).
pub fn migrate_with(
    new_dir: &Path,
    legacy_dir: &Path,
    before_commit: impl FnOnce(),
) -> Result<bool, MigrateError> {
    if !needs_migration(new_dir, legacy_dir) {
        return Ok(false);
    }
    let (parent, prefix) = temp_prefix(new_dir).ok_or_else(|| MigrateError::Io {
        path: new_dir.to_path_buf(),
        source: io::Error::other("config dir has no parent"),
    })?;
    // Hold the legacy lock for the whole copy so the old app cannot start mid-way.
    let legacy_lock_path = legacy_dir.join(LEGACY_LOCK_FILE);
    let _legacy_lock = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&legacy_lock_path)
    {
        Ok(f) => {
            match f.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    return Err(MigrateError::LegacyRunning(legacy_lock_path));
                }
                Err(TryLockError::Error(e)) => return Err(io_err(&legacy_lock_path)(e)),
            }
            Some(f)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err(&legacy_lock_path)(e)),
    };

    let tmp = parent.join(format!("{prefix}{}", std::process::id()));
    force_remove(&tmp);
    let result = (|| {
        let mut modes = DirModes::new();
        copy_tree(legacy_dir, &tmp, Path::new(""), &mut modes)?;
        before_commit();
        if new_dir.join(CONFIG_FILE).exists() {
            return Ok(false); // someone else created it; keep theirs
        }
        if fs::symlink_metadata(tmp.join(CONFIG_FILE)).is_err() {
            return Err(MigrateError::Io {
                path: legacy_dir.join(CONFIG_FILE),
                source: io::Error::other("config.toml was not copied"),
            });
        }
        fs::create_dir_all(new_dir).map_err(io_err(new_dir))?;
        // Until the final modes are restored, never be looser than the legacy dir (plus owner rwx).
        #[cfg(unix)]
        if let Some((_, perms)) = modes.iter().find(|(r, _)| r.as_os_str().is_empty()) {
            use std::os::unix::fs::PermissionsExt;
            let mut p = perms.clone();
            p.set_mode(p.mode() | 0o700);
            let _ = fs::set_permissions(new_dir, p);
        }
        let mut moved: Vec<std::ffi::OsString> = Vec::new();
        let mut names: Vec<_> = fs::read_dir(&tmp)
            .map_err(io_err(&tmp))?
            .flatten()
            .map(|e| e.file_name())
            .collect();
        names.sort_by_key(|n| n == CONFIG_FILE); // config.toml last
        let mut committed = false;
        for n in names {
            let to = new_dir.join(&n);
            if to.exists() {
                continue;
            }
            fs::rename(tmp.join(&n), &to).map_err(io_err(&to))?;
            committed |= n == CONFIG_FILE;
            moved.push(n);
        }
        if !committed {
            return Err(MigrateError::Io {
                path: new_dir.join(CONFIG_FILE),
                source: io::Error::other("config.toml could not be moved into place"),
            });
        }
        let from_file = new_dir.join(MIGRATED_FROM_FILE);
        let _ = fs::write(&from_file, format!("{}\n", legacy_dir.display()));
        let note = legacy_dir.join(MOVED_TO_FILE);
        let _ = fs::write(
            &note,
            format!(
                "Settings were copied to {} when Siple (formerly ZynoPhone) first started.\n\
                 This directory is no longer used by Siple and was left untouched.\n",
                new_dir.display()
            ),
        );
        // Restore the legacy permissions, deepest directories first, new_dir last.
        modes.sort_by_key(|(r, _)| std::cmp::Reverse(r.components().count()));
        // Only directories we moved in, and never through a symlink.
        for (rel, perms) in modes {
            let ours = rel
                .components()
                .next()
                .is_none_or(|c| moved.iter().any(|m| m.as_os_str() == c.as_os_str()));
            if ours && real_dirs_all_the_way(new_dir, &rel) {
                let _ = fs::set_permissions(new_dir.join(&rel), perms);
            }
        }
        Ok(true)
    })();
    force_remove(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _root: tempfile::TempDir,
        new: PathBuf,
        old: PathBuf,
    }

    fn fixture() -> Fx {
        let root = tempfile::tempdir().unwrap();
        let new = root.path().join("Siple");
        let old = root.path().join("ZynoPhone");
        fs::create_dir_all(old.join("logs")).unwrap();
        fs::create_dir_all(old.join("certs")).unwrap();
        fs::write(old.join(CONFIG_FILE), "user_agent = \"x\"\n").unwrap();
        fs::write(old.join("certs/ca.pem"), "pem").unwrap();
        fs::write(old.join("state.toml"), "s=1\n").unwrap();
        fs::write(old.join("logs/zynophone.log"), "log").unwrap();
        fs::write(old.join("runtime.json"), "{}").unwrap();
        fs::write(old.join(LEGACY_LOCK_FILE), "1").unwrap();
        fs::create_dir_all(&new).unwrap(); // the caller created it to take the lock
        fs::write(new.join("siple.lock"), "9").unwrap();
        Fx {
            _root: root,
            new,
            old,
        }
    }

    fn temps(fx: &Fx) -> Vec<String> {
        fs::read_dir(fx.new.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".migrating-"))
            .collect()
    }

    #[test]
    fn happy_path() {
        let fx = fixture();
        assert!(migrate(&fx.new, &fx.old).unwrap());
        assert_eq!(
            fs::read_to_string(fx.new.join(CONFIG_FILE)).unwrap(),
            "user_agent = \"x\"\n"
        );
        assert_eq!(
            fs::read_to_string(fx.new.join("certs/ca.pem")).unwrap(),
            "pem"
        );
        assert!(fx.new.join("state.toml").exists());
        let from = fs::read_to_string(fx.new.join(MIGRATED_FROM_FILE)).unwrap();
        assert_eq!(from.trim(), fx.old.display().to_string());
        assert!(fx.old.join(MOVED_TO_FILE).exists());
        // legacy dir untouched
        assert!(fx.old.join(CONFIG_FILE).exists());
        assert!(fx.old.join("logs/zynophone.log").exists());
        assert!(temps(&fx).is_empty());
        assert_eq!(fs::read_to_string(fx.new.join("siple.lock")).unwrap(), "9");
    }

    #[test]
    fn excluded_files_absent() {
        let fx = fixture();
        migrate(&fx.new, &fx.old).unwrap();
        assert!(!fx.new.join("logs").exists());
        assert!(!fx.new.join("runtime.json").exists());
        assert!(!fx.new.join(LEGACY_LOCK_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn permissions_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        fs::set_permissions(fx.old.join(CONFIG_FILE), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(fx.old.join("certs"), fs::Permissions::from_mode(0o700)).unwrap();
        migrate(&fx.new, &fx.old).unwrap();
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(fx.new.join(CONFIG_FILE)), 0o600);
        assert_eq!(mode(fx.new.join("certs")), 0o700);
    }

    #[test]
    fn legacy_lock_held_errors() {
        let fx = fixture();
        let held = fs::File::options()
            .read(true)
            .write(true)
            .open(fx.old.join(LEGACY_LOCK_FILE))
            .unwrap();
        held.try_lock().unwrap();
        let err = migrate(&fx.new, &fx.old).unwrap_err();
        assert!(matches!(err, MigrateError::LegacyRunning(_)));
        assert!(
            err.to_string()
                .contains("Quit ZynoPhone to migrate settings")
        );
        assert!(!fx.new.join(CONFIG_FILE).exists());
        assert!(temps(&fx).is_empty());
    }

    #[test]
    fn final_config_appears_midway_discards_temp() {
        let fx = fixture();
        let new = fx.new.clone();
        let done = migrate_with(&fx.new, &fx.old, || {
            fs::write(new.join(CONFIG_FILE), "# theirs\n").unwrap();
        })
        .unwrap();
        assert!(!done);
        assert_eq!(
            fs::read_to_string(fx.new.join(CONFIG_FILE)).unwrap(),
            "# theirs\n"
        );
        assert!(!fx.new.join(MIGRATED_FROM_FILE).exists());
        assert!(temps(&fx).is_empty());
    }

    #[test]
    fn stale_temp_cleanup() {
        let fx = fixture();
        let parent = fx.new.parent().unwrap();
        fs::create_dir_all(parent.join("Siple.migrating-1/x")).unwrap();
        fs::create_dir_all(parent.join("Siple.migrating-77")).unwrap();
        fs::create_dir_all(parent.join("Other.migrating-1")).unwrap();
        cleanup_stale(&fx.new);
        assert!(temps(&fx).iter().all(|n| n.starts_with("Other")));
        assert!(fx.new.exists() && fx.old.exists());
    }

    #[test]
    fn noop_when_new_config_exists_or_no_legacy() {
        let fx = fixture();
        fs::write(fx.new.join(CONFIG_FILE), "# mine\n").unwrap();
        assert!(!migrate(&fx.new, &fx.old).unwrap());
        assert!(!fx.new.join(MIGRATED_FROM_FILE).exists());
        assert!(!fx.old.join(MOVED_TO_FILE).exists());

        let fx = fixture();
        fs::remove_file(fx.old.join(CONFIG_FILE)).unwrap();
        assert!(!migrate(&fx.new, &fx.old).unwrap());
    }

    #[cfg(unix)]
    fn unlock_all(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = fs::symlink_metadata(p)
            && md.is_dir()
        {
            let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
            for e in fs::read_dir(p).into_iter().flatten().flatten() {
                unlock_all(&e.path());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_only_subdir_migrates_and_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        fs::write(fx.old.join("certs/client.key"), "KEY").unwrap();
        fs::set_permissions(fx.old.join("certs"), fs::Permissions::from_mode(0o500)).unwrap();
        let r = migrate(&fx.new, &fx.old);
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let ok = matches!(r, Ok(true));
        let (m, key, t) = (
            mode(fx.new.join("certs")),
            fs::read_to_string(fx.new.join("certs/client.key")).ok(),
            temps(&fx),
        );
        unlock_all(&fx.old);
        unlock_all(&fx.new);
        assert!(ok, "{r:?}");
        assert_eq!(m, 0o500);
        assert_eq!(key.as_deref(), Some("KEY"));
        assert!(t.is_empty(), "{t:?}");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_removes_read_only_stale_temp() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        let stale = fx.new.parent().unwrap().join("Siple.migrating-5");
        fs::create_dir_all(stale.join("certs")).unwrap();
        fs::write(stale.join("certs/k"), "x").unwrap();
        fs::set_permissions(stale.join("certs"), fs::Permissions::from_mode(0o500)).unwrap();
        cleanup_stale(&fx.new);
        assert!(!stale.exists());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_dir_mode_carried_to_new_dir() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        fs::set_permissions(&fx.old, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&fx.new, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(migrate(&fx.new, &fx.old).unwrap());
        let m = fs::metadata(&fx.new).unwrap().permissions().mode() & 0o777;
        assert_eq!(m, 0o700);
    }

    #[test]
    fn dangling_config_symlink_is_not_migrated() {
        // config.toml is a dangling symlink: nothing usable to commit.
        #[cfg(unix)]
        {
            let fx = fixture();
            fs::remove_file(fx.old.join(CONFIG_FILE)).unwrap();
            std::os::unix::fs::symlink("/nonexistent/x", fx.old.join(CONFIG_FILE)).unwrap();
            // needs_migration (is_file) is false for a dangling link: no-op, no markers
            assert!(!migrate(&fx.new, &fx.old).unwrap());
            assert!(!fx.new.join(MIGRATED_FROM_FILE).exists());
            assert!(!fx.old.join(MOVED_TO_FILE).exists());
        }
    }

    #[test]
    fn env_alias_resolution() {
        use std::ffi::OsString;
        let p = |s: &str| Some(PathBuf::from(s));
        let o = |s: &str| Some(OsString::from(s));
        let (path, w) = resolve_explicit(None, o("/old.toml"));
        assert_eq!(path, p("/old.toml"));
        assert_eq!(w, ["ZYNOPHONE_CONFIG is deprecated; use SIPLE_CONFIG"]);
        let (path, w) = resolve_explicit(p("/new.toml"), o("/old.toml"));
        assert_eq!(path, p("/new.toml"));
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("ignored"));
        assert_eq!(
            resolve_explicit(p("/same"), o("/same")),
            (p("/same"), vec![])
        );
        assert_eq!(resolve_explicit(p("/new"), None), (p("/new"), vec![]));
        assert_eq!(resolve_explicit(None, o("")), (None, vec![]));
    }

    #[cfg(unix)]
    #[test]
    fn mode_restore_never_follows_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        let outside = fx.new.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("../outside", fx.new.join("certs")).unwrap();
        fs::set_permissions(fx.old.join("certs"), fs::Permissions::from_mode(0o500)).unwrap();
        let r = migrate(&fx.new, &fx.old);
        let m = fs::metadata(&outside).unwrap().permissions().mode() & 0o777;
        unlock_all(&fx.old);
        assert!(matches!(r, Ok(true)), "{r:?}");
        assert_eq!(m, 0o755);
    }
}
