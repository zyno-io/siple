//! What a platform probe looks at: the running executable, environment variables and a few file
//! system questions. Everything goes through [`Env`] so the probes are pure functions of their
//! input and tests can fake any machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// File system questions a probe asks. Nothing here changes anything that outlives the call
/// (`can_create_in` creates and removes one probe file).
pub trait Fs: Send + Sync {
    /// The path exists (symlinks are followed).
    fn exists(&self, p: &Path) -> bool;
    /// The path is a directory.
    fn is_dir(&self, p: &Path) -> bool;
    /// A small text file, when it can be read.
    fn read_to_string(&self, p: &Path) -> Option<String>;
    /// A file can be created in (and removed from) this directory by this process.
    fn can_create_in(&self, dir: &Path) -> bool;
    /// The owning user id (Unix).
    fn owner(&self, p: &Path) -> Option<u32>;
    /// The volume holding `p` is mounted read-only.
    fn read_only_volume(&self, p: &Path) -> bool;
    /// This process may delete `p` (Windows: `CreateFileW` with `DELETE` access; elsewhere:
    /// the same as the parent directory being writable).
    fn can_delete(&self, p: &Path) -> bool;
}

/// The machine Siple runs on.
pub struct Env {
    /// The running executable (symlinks resolved).
    pub exe: PathBuf,
    /// Environment variables the probes look at.
    pub vars: BTreeMap<String, String>,
    /// The effective user id (Unix).
    pub uid: Option<u32>,
    /// The file system.
    pub fs: Arc<dyn Fs>,
}

/// The environment variables the probes read.
pub const PROBED_VARS: &[&str] = &[
    "container",
    "FLATPAK_ID",
    "SNAP",
    "APPIMAGE",
    "APPDIR",
    "SIPLE_NO_SELF_UPDATE",
];

impl Env {
    /// The real machine.
    pub fn real() -> Option<Env> {
        let exe = std::env::current_exe().ok()?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        Some(Env {
            exe,
            vars: PROBED_VARS
                .iter()
                .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
                .collect(),
            #[cfg(unix)]
            uid: Some(unsafe { libc::geteuid() }),
            #[cfg(not(unix))]
            uid: None,
            fs: Arc::new(RealFs),
        })
    }

    /// An environment variable that is set (and not empty).
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// The real file system.
pub struct RealFs;

impl Fs for RealFs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }

    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir()
    }

    fn read_to_string(&self, p: &Path) -> Option<String> {
        // Small files only (a plist, /proc/1/cgroup): never slurp something large.
        let meta = std::fs::metadata(p).ok()?;
        if meta.len() > 1024 * 1024 {
            return None;
        }
        std::fs::read_to_string(p).ok()
    }

    fn can_create_in(&self, dir: &Path) -> bool {
        let name = format!(".siple-probe-{}-{}", std::process::id(), nanos());
        let probe = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(f) => {
                drop(f);
                std::fs::remove_file(&probe).is_ok()
            }
            Err(_) => false,
        }
    }

    fn owner(&self, p: &Path) -> Option<u32> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(p).ok().map(|m| m.uid())
        }
        #[cfg(not(unix))]
        {
            let _ = p;
            None
        }
    }

    fn read_only_volume(&self, p: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
                return false;
            };
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
            if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
                return false;
            }
            #[allow(clippy::unnecessary_cast)] // the flag types differ per OS
            let ro = (st.f_flag as u64 & libc::ST_RDONLY as u64) != 0;
            ro
        }
        #[cfg(not(unix))]
        {
            let _ = p;
            false
        }
    }

    fn can_delete(&self, p: &Path) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // DELETE access (0x10000) with every share mode: the same right the rename of a
            // running image needs.
            std::fs::OpenOptions::new()
                .access_mode(0x0001_0000)
                .share_mode(7)
                .open(p)
                .is_ok()
        }
        #[cfg(not(windows))]
        {
            p.parent().is_some_and(|d| self.can_create_in(d))
        }
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod fake {
    //! A scripted machine for probe tests.

    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// Paths and answers a test sets up.
    #[derive(Default)]
    pub struct FakeFs {
        pub files: BTreeMap<PathBuf, String>,
        pub dirs: BTreeSet<PathBuf>,
        pub writable: BTreeSet<PathBuf>,
        pub owners: BTreeMap<PathBuf, u32>,
        pub read_only: BTreeSet<PathBuf>,
        pub undeletable: BTreeSet<PathBuf>,
    }

    impl FakeFs {
        pub fn file(mut self, p: &str, text: &str) -> Self {
            self.files.insert(p.into(), text.into());
            self
        }
        pub fn dir(mut self, p: &str) -> Self {
            self.dirs.insert(p.into());
            self
        }
        pub fn writable(mut self, p: &str) -> Self {
            self.writable.insert(p.into());
            self
        }
        pub fn owner(mut self, p: &str, uid: u32) -> Self {
            self.owners.insert(p.into(), uid);
            self
        }
        pub fn read_only(mut self, p: &str) -> Self {
            self.read_only.insert(p.into());
            self
        }
        pub fn undeletable(mut self, p: &str) -> Self {
            self.undeletable.insert(p.into());
            self
        }
        pub fn env(self, exe: &str, uid: Option<u32>, vars: &[(&str, &str)]) -> Env {
            Env {
                exe: exe.into(),
                vars: vars
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                uid,
                fs: Arc::new(self),
            }
        }
    }

    impl Fs for FakeFs {
        fn exists(&self, p: &Path) -> bool {
            self.files.contains_key(p) || self.dirs.contains(p)
        }
        fn is_dir(&self, p: &Path) -> bool {
            self.dirs.contains(p)
        }
        fn read_to_string(&self, p: &Path) -> Option<String> {
            self.files.get(p).cloned()
        }
        fn can_create_in(&self, dir: &Path) -> bool {
            self.writable.contains(dir)
        }
        fn owner(&self, p: &Path) -> Option<u32> {
            self.owners.get(p).copied()
        }
        fn read_only_volume(&self, p: &Path) -> bool {
            self.read_only.iter().any(|r| p.starts_with(r))
        }
        fn can_delete(&self, p: &Path) -> bool {
            self.exists(p) && !self.undeletable.contains(p)
        }
    }
}
