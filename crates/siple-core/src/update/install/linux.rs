//! Linux install: the direct tarball build, replaced in place.
//!
//! The install root is the unpacked tarball directory (`bin/siple`, `bin/siple-cli`,
//! `share/...`). An update unpacks the new tarball into a private staging directory below the
//! root (`<root>/.siple-update/<id>/`, same volume), checks it (entry types, paths, sizes, a run
//! of both executables), then replaces the files with the journaled swap: the old file is
//! hard-linked aside and the new one renamed over it, so the running executable keeps its inode
//! and a crash at any point is recovered at the next start. Only files the archive ships are
//! touched; nothing else is deleted. The top directory keeps the name of the version that was
//! unpacked first.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::env::Env;
use super::journal::{self, JOURNAL_NAME, StdOps};
use super::*;
use crate::ui_host::RelaunchSpec;

/// The staging area inside the install root.
const STAGING: &str = ".siple-update";
/// At most this many archive entries.
const MAX_ENTRIES: usize = 256;
/// At most this many bytes in one file.
const MAX_FILE: u64 = 128 * 1024 * 1024;
/// At most this many bytes in all files.
const MAX_TOTAL: u64 = 512 * 1024 * 1024;
/// How long each executable may take to print its version.
const EXEC_TIMEOUT: Duration = Duration::from_secs(5);
/// Staging directories without a journal older than this are removed at start.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// File names the archive ships (the only names whose backups are swept at start).
const SHIPPED: &[&str] = &[
    "LICENSE",
    "THIRD-PARTY-NOTICES.html",
    "siple",
    "siple-cli",
    "io.zyno.siple.desktop",
    "io.zyno.siple.metainfo.xml",
    "io.zyno.siple.png",
    "API.md",
    "README.md",
    "CONFIG.md",
];

/// Runs an executable with `--version`.
pub trait Exec: Send + Sync {
    /// The standard output of `exe --version`, or why it could not be had (not started, did not
    /// exit successfully, timed out).
    fn version_output(&self, exe: &Path) -> Result<String, String>;
}

impl<F> Exec for F
where
    F: Fn(&Path) -> Result<String, String> + Send + Sync,
{
    fn version_output(&self, exe: &Path) -> Result<String, String> {
        self(exe)
    }
}

/// Runs the real process.
pub struct RealExec;

impl Exec for RealExec {
    fn version_output(&self, exe: &Path) -> Result<String, String> {
        let mut cmd = Command::new(exe);
        cmd.arg("--version");
        run_capped(cmd, EXEC_TIMEOUT)
    }
}

/// Runs `cmd` (stdin closed) and returns its standard output. Each stream is capped at 64 KiB, the
/// child is killed after `timeout`, and a failed exit status is an error with the start of stderr.
pub(super) fn run_capped(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A freshly written file can be "text file busy" for a moment when another thread forks
    // while it is still open for writing.
    let mut child = None;
    for _ in 0..20 {
        match cmd.spawn() {
            Ok(c) => {
                child = Some(c);
                break;
            }
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("cannot start: {e}")),
        }
    }
    let Some(mut child) = child else {
        return Err("cannot start: text file busy".to_string());
    };
    let read = |s: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = s {
                let _ = s.take(64 * 1024).read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = read(child.stdout.take().map(|s| Box::new(s) as _));
    let err = read(child.stderr.take().map(|s| Box::new(s) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("cannot wait: {e}")),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "exited with {status}: {}",
            stderr.trim().chars().take(200).collect::<String>()
        ));
    }
    Ok(stdout)
}

/// Creates `<root>/.siple-update/<id>/` (0700, new and empty).
pub(super) fn new_stage_dir(root: &Path) -> Result<PathBuf, InstallError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = root.join(STAGING);
    make_dir(&base, true, 0o700).map_err(|e| InstallError::io("cannot create staging", &e))?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let id = format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let dir = base.join(id);
    make_dir(&dir, false, 0o700).map_err(|e| InstallError::io("cannot create staging", &e))?;
    Ok(dir)
}

/// Size limits of an unpacked archive.
#[derive(Debug, Clone, Copy)]
struct Limits {
    entries: usize,
    file: u64,
    total: u64,
}

impl Limits {
    const DEFAULT: Limits = Limits {
        entries: MAX_ENTRIES,
        file: MAX_FILE,
        total: MAX_TOTAL,
    };
}

/// The Linux tarball install.
pub struct LinuxPlatform {
    env: Env,
    exec: Arc<dyn Exec>,
    limits: Limits,
}

impl LinuxPlatform {
    /// The platform of this machine.
    pub fn new(env: Env) -> Self {
        Self::with_exec(env, Arc::new(RealExec))
    }

    /// With another way to run the staged executables (tests).
    pub fn with_exec(env: Env, exec: Arc<dyn Exec>) -> Self {
        Self {
            env,
            exec,
            limits: Limits::DEFAULT,
        }
    }

    /// `<root>` of `<root>/bin/siple`.
    fn root_of_exe(&self) -> Option<PathBuf> {
        let bin = self.env.exe.parent()?;
        (self.env.exe.file_name()? == "siple" && bin.file_name()? == "bin")
            .then(|| bin.parent().map(Path::to_path_buf))
            .flatten()
    }

    fn check_exec(&self, payload: &Path, version: &str) -> Result<(), InstallError> {
        for name in ["siple-cli", "siple"] {
            let exe = payload.join("bin").join(name);
            let out = self.exec.version_output(&exe).map_err(|e| {
                InstallError::untrusted(format!("the new {name} does not run here: {e}"))
            })?;
            if !reports_version(&out, version) {
                return Err(InstallError::untrusted(format!(
                    "the new {name} does not report version {version}"
                )));
            }
        }
        Ok(())
    }
}

/// Whether `out` (a `--version` output) names `version` as one of its words.
pub(super) fn reports_version(out: &str, version: &str) -> bool {
    out.split_whitespace().any(|t| {
        t.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-')) == version
    })
}

/// Whether `out` is the `--version` output of `program` (clap prints `<name> <version>`, so the
/// first word is the program name) and names `version`.
// Only the AppImage platform (Linux) uses it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn reports_program_version(out: &str, program: &str, version: &str) -> bool {
    out.split_whitespace().next() == Some(program) && reports_version(out, version)
}

/// A Flatpak or Snap sandbox: nothing is replaced from inside.
pub(super) fn sandbox_refusal(env: &Env) -> Option<NotPossible> {
    let no = NotPossible::new;
    if env.fs.exists(Path::new("/.flatpak-info")) || env.var("FLATPAK_ID").is_some() {
        return Some(no("sandboxed", "running inside a Flatpak sandbox"));
    }
    if env.var("SNAP").is_some() {
        return Some(no("sandboxed", "running inside a Snap"));
    }
    None
}

/// Running inside a container (update the image instead).
pub(super) fn in_container(env: &Env) -> bool {
    let fs = &env.fs;
    let cgroup = fs
        .read_to_string(Path::new("/proc/1/cgroup"))
        .unwrap_or_default();
    fs.exists(Path::new("/.dockerenv"))
        || fs.exists(Path::new("/run/.containerenv"))
        || env.var("container").is_some()
        || ["docker", "kubepods", "containerd", "libpod"]
            .iter()
            .any(|k| cgroup.contains(k))
}

impl Platform for LinuxPlatform {
    fn asset(&self) -> Option<AssetPlatform> {
        AssetPlatform::linux_tarball(std::env::consts::ARCH)
    }

    fn probe(&self) -> Result<Target, NotPossible> {
        let env = &self.env;
        let fs = &env.fs;
        let no = NotPossible::new;
        if env.var("SIPLE_NO_SELF_UPDATE").is_some() {
            return Err(no("disabled", "SIPLE_NO_SELF_UPDATE is set"));
        }
        if let Some(n) = sandbox_refusal(env) {
            return Err(n);
        }
        // Only a copy that runs from inside an AppImage is one. A terminal that is itself an
        // AppImage leaks `APPIMAGE`/`APPDIR` to the tarball started from it.
        if env.var("APPIMAGE").is_some()
            && let Some(appdir) = env.var("APPDIR")
            && siple_config::channel::runs_inside(&env.exe, &fs.canonical(Path::new(appdir)))
        {
            return Err(no("sandboxed", "running from an AppImage"));
        }
        if in_container(env) {
            return Err(no("container", "running inside a container"));
        }

        let not_bundle = || no("not_bundle", "this is not an unpacked Siple tarball");
        let root = self.root_of_exe().ok_or_else(not_bundle)?;
        if !fs.exists(&root.join("bin/siple-cli"))
            || !fs.exists(&root.join("share/applications/io.zyno.siple.desktop"))
        {
            return Err(not_bundle());
        }
        if root.starts_with("/nix/store") || root.starts_with("/gnu/store") {
            return Err(no("package_manager", "installed by Nix or Guix"));
        }
        if root.starts_with("/usr") {
            return Err(no("system_install", "installed under /usr"));
        }
        if env.uid == Some(0) {
            return Err(no(
                "root",
                "running as root; updates are not installed as root",
            ));
        }
        if fs.read_only_volume(&root) {
            return Err(no(
                "read_only_volume",
                "the install is on a read-only volume",
            ));
        }
        if !(fs.can_create_in(&root) && fs.can_create_in(&root.join("bin"))) {
            return Err(no("not_writable", "the install directory is not writable"));
        }
        if env.uid.is_none() || fs.owner(&root) != env.uid {
            return Err(no(
                "not_owner",
                "the install directory belongs to another user",
            ));
        }
        Ok(Target {
            root,
            exe: env.exe.clone(),
        })
    }

    fn stage_dir(&self, target: &Target) -> Result<PathBuf, InstallError> {
        new_stage_dir(&target.root)
    }

    fn stage(
        &self,
        archive: &Path,
        stage_dir: &Path,
        _target: &Target,
        expect: &Expect,
    ) -> Result<Staged, InstallError> {
        let top = format!("siple-{}", expect.version.core());
        let new = stage_dir.join("new");
        let result = (|| {
            let f = std::fs::File::open(archive)
                .map_err(|e| InstallError::io("cannot open the archive", &e))?;
            extract(f, &new, &top, self.limits)?;
            let payload = new.join(&top);
            self.check_exec(&payload, &expect.version.to_string())?;
            Ok(Staged {
                dir: stage_dir.to_path_buf(),
                payload,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&new);
        }
        result
    }

    fn commit(&self, staged: &Staged, target: &Target) -> Result<Committed, InstallError> {
        if !is_staging_path(&staged.dir) || !staged.payload.starts_with(&staged.dir) {
            return Err(InstallError::new(
                ErrorKind::Unsupported,
                "the staged files are not in a staging area",
            ));
        }
        let mut rels = Vec::new();
        list_files(&staged.payload, Path::new(""), &mut rels)
            .map_err(|e| InstallError::io("cannot list the staged files", &e))?;
        rels.sort();
        let files: Vec<(PathBuf, PathBuf)> = rels
            .iter()
            .map(|r| (staged.payload.join(r), target.root.join(r)))
            .collect();
        let backups = journal::swap(&files, &staged.dir.join(JOURNAL_NAME), &StdOps::unix())
            .map_err(|e| InstallError::io("cannot replace the installed files", &e))?;
        let mut cleanup = backups;
        cleanup.push(staged.dir.clone());
        Ok(Committed {
            relaunch: RelaunchSpec::Exe(target.exe.clone()),
            cleanup,
        })
    }

    fn recover(&self) {
        let Some(root) = self.root_of_exe() else {
            return;
        };
        let pending = recover_staging(&root.join(STAGING), &StdOps::unix());
        if !pending {
            sweep_backups(&root);
        }
    }

    fn cleanup(&self, record: &InstalledUpdate) {
        let Some(root) = self.root_of_exe() else {
            return;
        };
        let staging = root.join(STAGING);
        for p in record.cleanup.iter().map(PathBuf::from) {
            let clean =
                p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir));
            if !clean {
                continue;
            }
            if is_staging_path(&p) && p.starts_with(&staging) && p != staging {
                let _ = std::fs::remove_dir_all(&p);
            } else if p.starts_with(&root)
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(journal::is_backup_name)
                && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file())
            {
                let _ = std::fs::remove_file(&p);
            }
        }
        let _ = std::fs::remove_dir(&staging); // only when empty
    }
}

/// Finishes or undoes the interrupted swaps in `staging` (a `.siple-update` directory) and removes
/// stale journal-less staging directories. Returns whether a journal is still pending (its
/// backups are still needed).
pub(super) fn recover_staging(staging: &Path, ops: &dyn journal::Ops) -> bool {
    if let Ok(rd) = std::fs::read_dir(staging) {
        let dirs: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .collect();
        for d in &dirs {
            let j = d.join(JOURNAL_NAME);
            if j.exists() {
                journal::recover(&j, ops);
            }
        }
        for d in &dirs {
            let stale = std::fs::metadata(d)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_AFTER);
            if stale && !d.join(JOURNAL_NAME).exists() {
                let _ = std::fs::remove_dir_all(d);
            }
        }
        let _ = std::fs::remove_dir(staging); // only when empty
    }
    // A journal that could not be finished still needs the backups.
    std::fs::read_dir(staging)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.path().join(JOURNAL_NAME).exists())
}

/// Removes `*.old-<digits>` files of shipped names in the archive-owned places of `root`.
fn sweep_backups(root: &Path) {
    fn walk(dir: &Path, depth: usize) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.filter_map(Result::ok) {
            let Ok(t) = e.file_type() else { continue };
            let path = e.path();
            if t.is_dir() {
                walk(&path, depth + 1);
            } else if t.is_file()
                && let Some(name) = e.file_name().to_str()
                && journal::is_backup_name(name)
                && name
                    .rsplit_once(".old-")
                    .is_some_and(|(stem, _)| SHIPPED.contains(&stem))
            {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    // The root itself is not walked (that would reach user data); its shipped files only.
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.filter_map(Result::ok) {
            if e.file_type().is_ok_and(|t| t.is_file())
                && let Some(name) = e.file_name().to_str()
                && journal::is_backup_name(name)
                && name
                    .rsplit_once(".old-")
                    .is_some_and(|(stem, _)| matches!(stem, "LICENSE" | "THIRD-PARTY-NOTICES.html"))
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    walk(&root.join("bin"), 0);
    walk(&root.join("share"), 0);
}

/// Every regular file below `dir`, as paths relative to the first call's `dir`.
fn list_files(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for e in std::fs::read_dir(base.join(rel))? {
        let e = e?;
        let t = e.file_type()?;
        let r = rel.join(e.file_name());
        if t.is_dir() {
            list_files(base, &r, out)?;
        } else if t.is_file() {
            out.push(r);
        }
    }
    Ok(())
}

/// Creates a directory with `mode` (Unix).
pub(super) fn make_dir(p: &Path, recursive: bool, mode: u32) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    b.create(p)
}

#[cfg(unix)]
pub(super) fn set_mode(p: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(super) fn set_mode(_: &Path, _: u32) -> io::Result<()> {
    Ok(())
}

/// What an archive entry path is, relative to the top directory.
enum Kind {
    /// The top directory itself.
    Top,
    Dir(PathBuf),
    File(PathBuf),
}

/// Validates the raw entry path and classifies it. `is_dir`: the entry is a directory.
fn classify(raw: &[u8], is_dir: bool, top: &str) -> Result<Kind, InstallError> {
    let bad = |why: &str| {
        InstallError::untrusted(format!(
            "the archive has an unacceptable path ({why}): {}",
            String::from_utf8_lossy(raw)
                .chars()
                .take(80)
                .collect::<String>()
        ))
    };
    if raw.contains(&0) {
        return Err(bad("NUL"));
    }
    if raw.contains(&b'\\') {
        return Err(bad("backslash"));
    }
    let s = std::str::from_utf8(raw).map_err(|_| bad("not UTF-8"))?;
    let s = if is_dir {
        s.strip_suffix('/').unwrap_or(s)
    } else {
        s
    };
    let parts: Vec<&str> = s.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return Err(bad("empty, dot or absolute component"));
    }
    if parts[0] != top {
        return Err(bad("outside the top directory"));
    }
    let rel: PathBuf = parts[1..].iter().collect();
    if parts.len() == 1 {
        return if is_dir {
            Ok(Kind::Top)
        } else {
            Err(bad("top is a file"))
        };
    }
    let allowed = match parts[1] {
        "LICENSE" | "THIRD-PARTY-NOTICES.html" => parts.len() == 2 && !is_dir,
        "bin" | "share" => is_dir || parts.len() > 2,
        _ => false,
    };
    if !allowed {
        return Err(bad("not a location of the release"));
    }
    Ok(if is_dir {
        Kind::Dir(rel)
    } else {
        Kind::File(rel)
    })
}

/// Copies at most `cap` bytes; more is an error. Returns the bytes written.
fn copy_capped(r: &mut dyn Read, w: &mut dyn Write, cap: u64) -> Result<u64, InstallError> {
    let mut buf = [0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = r
            .read(&mut buf)
            .map_err(|e| InstallError::io("cannot read the archive", &e))?;
        if k == 0 {
            return Ok(n);
        }
        n += k as u64;
        if n > cap {
            return Err(InstallError::untrusted(
                "the archive holds more data than allowed",
            ));
        }
        w.write_all(&buf[..k])
            .map_err(|e| InstallError::io("cannot write a staged file", &e))?;
    }
}

/// Unpacks a `.tar.gz` stream into `dest` (which must not exist), checking each entry before
/// anything of it is written. Only regular files and directories of the release layout.
fn extract(src: impl Read, dest: &Path, top: &str, limits: Limits) -> Result<(), InstallError> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(src));
    let io_err = |e: io::Error| InstallError::untrusted(format!("the archive is unreadable: {e}"));
    make_dir(dest, false, 0o700).map_err(|e| InstallError::io("cannot create staging", &e))?;
    let top_dir = dest.join(top);
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    let (mut count, mut total) = (0usize, 0u64);
    for entry in ar.entries().map_err(io_err)? {
        let mut entry = entry.map_err(io_err)?;
        count += 1;
        if count > limits.entries {
            return Err(InstallError::untrusted("the archive has too many entries"));
        }
        let ty = entry.header().entry_type();
        let is_dir = match ty {
            tar::EntryType::Regular => false,
            tar::EntryType::Directory => true,
            _ => {
                return Err(InstallError::untrusted(
                    "the archive has an entry that is neither a file nor a directory",
                ));
            }
        };
        let raw = entry.path_bytes().into_owned();
        let kind = classify(&raw, is_dir, top)?;
        match kind {
            Kind::Top => {
                dirs.insert(top_dir.clone());
            }
            Kind::Dir(rel) => {
                dirs.insert(top_dir.join(rel));
            }
            Kind::File(rel) => {
                let size = entry.header().size().map_err(io_err)?;
                if size > limits.file {
                    return Err(InstallError::untrusted(
                        "a file in the archive is too large",
                    ));
                }
                if total.saturating_add(size) > limits.total {
                    return Err(InstallError::untrusted("the archive is too large"));
                }
                let path = top_dir.join(&rel);
                if let Some(parent) = path.parent() {
                    let mut p = parent.to_path_buf();
                    while p != dest {
                        dirs.insert(p.clone());
                        p.pop();
                    }
                    std::fs::create_dir_all(parent)
                        .map_err(|e| InstallError::io("cannot create a staged directory", &e))?;
                }
                let mode = if rel.starts_with("bin") { 0o755 } else { 0o644 };
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(mode);
                }
                let mut f = opts.open(&path).map_err(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists {
                        InstallError::untrusted("the archive lists a file twice")
                    } else {
                        InstallError::io("cannot create a staged file", &e)
                    }
                })?;
                let n = copy_capped(&mut entry, &mut f, limits.file.min(limits.total - total))?;
                total += n;
                f.flush()
                    .map_err(|e| InstallError::io("cannot write a staged file", &e))?;
                drop(f);
                set_mode(&path, mode)
                    .map_err(|e| InstallError::io("cannot set permissions", &e))?;
                files.insert(rel);
                continue;
            }
        }
    }
    for d in &dirs {
        std::fs::create_dir_all(d)
            .map_err(|e| InstallError::io("cannot create a staged directory", &e))?;
    }
    for d in dirs.iter().chain(std::iter::once(&top_dir)) {
        set_mode(d, 0o755).map_err(|e| InstallError::io("cannot set permissions", &e))?;
    }
    for required in ["bin/siple", "bin/siple-cli"] {
        if !files.contains(Path::new(required)) {
            return Err(InstallError::untrusted(format!(
                "the archive has no {required}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::install::env::fake::FakeFs;
    use crate::update::version::Version;

    #[test]
    fn program_version_distinguishes_the_cli_from_the_gui() {
        assert!(reports_program_version(
            "siple-cli 0.2.1\n",
            "siple-cli",
            "0.2.1"
        ));
        assert!(reports_program_version("siple 0.2.1\n", "siple", "0.2.1"));
        assert!(!reports_program_version(
            "siple 0.2.1\n",
            "siple-cli",
            "0.2.1"
        ));
        assert!(!reports_program_version(
            "siple-cli 0.2.1\n",
            "siple",
            "0.2.1"
        ));
        assert!(!reports_program_version(
            "siple-cli 0.2.0\n",
            "siple-cli",
            "0.2.1"
        ));
        assert!(!reports_program_version("", "siple", "0.2.1"));
    }

    // ---- probe ----

    const EXE: &str = "/home/u/siple-0.1.0/bin/siple";
    const ROOT: &str = "/home/u/siple-0.1.0";

    fn good_fs() -> FakeFs {
        FakeFs::default()
            .file(EXE, "")
            .file("/home/u/siple-0.1.0/bin/siple-cli", "")
            .file(
                "/home/u/siple-0.1.0/share/applications/io.zyno.siple.desktop",
                "",
            )
            .writable(ROOT)
            .writable("/home/u/siple-0.1.0/bin")
            .owner(ROOT, 1000)
    }

    fn probe(
        fs: FakeFs,
        exe: &str,
        uid: Option<u32>,
        vars: &[(&str, &str)],
    ) -> Result<Target, NotPossible> {
        LinuxPlatform::new(fs.env(exe, uid, vars)).probe()
    }

    #[test]
    fn probe_accepts_a_user_owned_tarball() {
        let t = probe(good_fs(), EXE, Some(1000), &[]).unwrap();
        assert_eq!(t.root, Path::new(ROOT));
        assert_eq!(t.exe, Path::new(EXE));
    }

    #[test]
    fn probe_accepts_a_tarball_started_from_an_appimage_terminal() {
        let vars = [
            ("APPIMAGE", "/home/u/Apps/Term.AppImage"),
            ("APPDIR", "/tmp/.mount_TermAB"),
        ];
        let t = probe(good_fs(), EXE, Some(1000), &vars).unwrap();
        assert_eq!(t.root, Path::new(ROOT));
    }

    #[test]
    fn probe_refusals() {
        type Case = (
            &'static str,
            FakeFs,
            &'static str,
            Option<u32>,
            Vec<(&'static str, &'static str)>,
            &'static str,
        );
        let cases: Vec<Case> = vec![
            (
                "disabled",
                good_fs(),
                EXE,
                Some(1000),
                vec![("SIPLE_NO_SELF_UPDATE", "1")],
                "disabled",
            ),
            (
                "flatpak file",
                good_fs().file("/.flatpak-info", ""),
                EXE,
                Some(1000),
                vec![],
                "sandboxed",
            ),
            (
                "flatpak var",
                good_fs(),
                EXE,
                Some(1000),
                vec![("FLATPAK_ID", "x.y")],
                "sandboxed",
            ),
            (
                "snap",
                good_fs(),
                EXE,
                Some(1000),
                vec![("SNAP", "/snap/x")],
                "sandboxed",
            ),
            (
                "appimage",
                good_fs(),
                EXE,
                Some(1000),
                vec![
                    ("APPIMAGE", "/a.AppImage"),
                    ("APPDIR", "/home/u/siple-0.1.0"),
                ],
                "sandboxed",
            ),
            (
                "dockerenv",
                good_fs().file("/.dockerenv", ""),
                EXE,
                Some(1000),
                vec![],
                "container",
            ),
            (
                "containerenv",
                good_fs().file("/run/.containerenv", ""),
                EXE,
                Some(1000),
                vec![],
                "container",
            ),
            (
                "container var",
                good_fs(),
                EXE,
                Some(1000),
                vec![("container", "podman")],
                "container",
            ),
            (
                "cgroup docker",
                good_fs().file("/proc/1/cgroup", "0::/docker/abc\n"),
                EXE,
                Some(1000),
                vec![],
                "container",
            ),
            (
                "cgroup kubepods",
                good_fs().file("/proc/1/cgroup", "0::/kubepods/x\n"),
                EXE,
                Some(1000),
                vec![],
                "container",
            ),
            (
                "cgroup containerd",
                good_fs().file("/proc/1/cgroup", "0::/containerd/x\n"),
                EXE,
                Some(1000),
                vec![],
                "container",
            ),
            (
                "cgroup libpod",
                good_fs().file("/proc/1/cgroup", "0::/libpod-1\n"),
                EXE,
                Some(1000),
                vec![],
                "container",
            ),
            (
                "not siple exe",
                good_fs(),
                "/home/u/siple-0.1.0/bin/other",
                Some(1000),
                vec![],
                "not_bundle",
            ),
            (
                "not in bin",
                good_fs(),
                "/home/u/siple-0.1.0/siple",
                Some(1000),
                vec![],
                "not_bundle",
            ),
            (
                "no cli",
                FakeFs::default()
                    .file(EXE, "")
                    .file(
                        "/home/u/siple-0.1.0/share/applications/io.zyno.siple.desktop",
                        "",
                    )
                    .writable(ROOT)
                    .writable("/home/u/siple-0.1.0/bin")
                    .owner(ROOT, 1000),
                EXE,
                Some(1000),
                vec![],
                "not_bundle",
            ),
            (
                "no desktop",
                FakeFs::default()
                    .file(EXE, "")
                    .file("/home/u/siple-0.1.0/bin/siple-cli", "")
                    .writable(ROOT)
                    .writable("/home/u/siple-0.1.0/bin")
                    .owner(ROOT, 1000),
                EXE,
                Some(1000),
                vec![],
                "not_bundle",
            ),
            (
                "root dir not writable",
                FakeFs::default()
                    .file(EXE, "")
                    .file("/home/u/siple-0.1.0/bin/siple-cli", "")
                    .file(
                        "/home/u/siple-0.1.0/share/applications/io.zyno.siple.desktop",
                        "",
                    )
                    .writable("/home/u/siple-0.1.0/bin")
                    .owner(ROOT, 1000),
                EXE,
                Some(1000),
                vec![],
                "not_writable",
            ),
            (
                "bin not writable",
                FakeFs::default()
                    .file(EXE, "")
                    .file("/home/u/siple-0.1.0/bin/siple-cli", "")
                    .file(
                        "/home/u/siple-0.1.0/share/applications/io.zyno.siple.desktop",
                        "",
                    )
                    .writable(ROOT)
                    .owner(ROOT, 1000),
                EXE,
                Some(1000),
                vec![],
                "not_writable",
            ),
            (
                "other owner",
                good_fs(),
                EXE,
                Some(1001),
                vec![],
                "not_owner",
            ),
            ("owner unknown", good_fs(), EXE, None, vec![], "not_owner"),
            ("root user", good_fs(), EXE, Some(0), vec![], "root"),
            (
                "read only",
                good_fs().read_only(ROOT),
                EXE,
                Some(1000),
                vec![],
                "read_only_volume",
            ),
        ];
        for (name, fs, exe, uid, vars, reason) in cases {
            let got = probe(fs, exe, uid, &vars).expect_err(name);
            assert_eq!(got.reason, reason, "{name}");
        }
    }

    #[test]
    fn probe_refuses_system_and_store_installs() {
        for (root, reason) in [
            ("/usr/local/siple", "system_install"),
            ("/usr/lib/siple", "system_install"),
            ("/nix/store/abc-siple", "package_manager"),
            ("/gnu/store/abc-siple", "package_manager"),
        ] {
            let fs = FakeFs::default()
                .file(&format!("{root}/bin/siple"), "")
                .file(&format!("{root}/bin/siple-cli"), "")
                .file(
                    &format!("{root}/share/applications/io.zyno.siple.desktop"),
                    "",
                )
                .writable(root)
                .writable(&format!("{root}/bin"))
                .owner(root, 1000);
            let got = probe(fs, &format!("{root}/bin/siple"), Some(1000), &[]).unwrap_err();
            assert_eq!(got.reason, reason, "{root}");
        }
    }

    #[test]
    fn probe_ignores_empty_variables() {
        assert!(
            probe(
                good_fs(),
                EXE,
                Some(1000),
                &[("SNAP", ""), ("container", "")]
            )
            .is_ok()
        );
    }

    // ---- tarballs ----

    fn raw_header(name: &[u8], ty: tar::EntryType, size: u64) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.as_old_mut().name[..name.len()].copy_from_slice(name);
        h.set_entry_type(ty);
        h.set_size(size);
        h.set_mode(0o777);
        h.set_cksum();
        h
    }

    struct Tb(tar::Builder<Vec<u8>>);

    impl Tb {
        fn new() -> Self {
            Tb(tar::Builder::new(Vec::new()))
        }
        fn file(mut self, name: &str, body: &[u8]) -> Self {
            let h = raw_header(name.as_bytes(), tar::EntryType::Regular, body.len() as u64);
            self.0.append(&h, body).unwrap();
            self
        }
        fn dir(mut self, name: &str) -> Self {
            let h = raw_header(name.as_bytes(), tar::EntryType::Directory, 0);
            self.0.append(&h, io::empty()).unwrap();
            self
        }
        fn link(mut self, name: &str, ty: tar::EntryType, to: &str) -> Self {
            let mut h = raw_header(name.as_bytes(), ty, 0);
            h.set_link_name(to).unwrap();
            h.set_cksum();
            self.0.append(&h, io::empty()).unwrap();
            self
        }
        fn special(mut self, name: &str, ty: tar::EntryType) -> Self {
            let h = raw_header(name.as_bytes(), ty, 0);
            self.0.append(&h, io::empty()).unwrap();
            self
        }
        /// A header claiming `claimed` bytes with `body` as the content.
        fn lying(mut self, name: &str, claimed: u64, body: &[u8]) -> Self {
            let h = raw_header(name.as_bytes(), tar::EntryType::Regular, claimed);
            self.0.append(&h, body).unwrap();
            self
        }
        /// A complete, valid release of `core`.
        fn release(self, core: &str, siple: &str, cli: &str) -> Self {
            let t = format!("siple-{core}");
            self.dir(&format!("{t}/"))
                .dir(&format!("{t}/bin/"))
                .file(&format!("{t}/bin/siple"), siple.as_bytes())
                .file(&format!("{t}/bin/siple-cli"), cli.as_bytes())
                .file(&format!("{t}/LICENSE"), b"license")
                .file(&format!("{t}/THIRD-PARTY-NOTICES.html"), b"<html>")
                .dir(&format!("{t}/share/"))
                .dir(&format!("{t}/share/applications/"))
                .file(
                    &format!("{t}/share/applications/io.zyno.siple.desktop"),
                    b"[Desktop Entry]",
                )
                .file(&format!("{t}/share/doc/siple/README.md"), b"readme")
        }
        fn gz(self) -> Vec<u8> {
            let raw = self.0.into_inner().unwrap();
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&raw).unwrap();
            e.finish().unwrap()
        }
    }

    fn unpack(tb: Tb, limits: Limits) -> (tempfile::TempDir, Result<(), InstallError>) {
        let tmp = tempfile::tempdir().unwrap();
        let r = extract(&tb.gz()[..], &tmp.path().join("new"), "siple-0.2.0", limits);
        (tmp, r)
    }

    fn rejects(tb: Tb, what: &str) {
        let (tmp, r) = unpack(tb, Limits::DEFAULT);
        let e = r.expect_err(what);
        assert_eq!(e.kind, ErrorKind::Untrusted, "{what}: {e}");
        // Nothing of the offending entry reaches the disk outside `new`.
        assert!(
            std::fs::read_dir(tmp.path())
                .unwrap()
                .all(|e| e.unwrap().file_name() == "new"),
            "{what}"
        );
    }

    #[test]
    fn extracts_a_valid_release() {
        let (tmp, r) = unpack(Tb::new().release("0.2.0", "a", "b"), Limits::DEFAULT);
        r.unwrap();
        let top = tmp.path().join("new/siple-0.2.0");
        assert_eq!(std::fs::read(top.join("bin/siple")).unwrap(), b"a");
        assert_eq!(
            std::fs::read(top.join("share/doc/siple/README.md")).unwrap(),
            b"readme"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &str| std::fs::metadata(top.join(p)).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode("bin/siple"), 0o755);
            assert_eq!(mode("bin/siple-cli"), 0o755);
            assert_eq!(mode("LICENSE"), 0o644);
            assert_eq!(mode("share/applications/io.zyno.siple.desktop"), 0o644);
            assert_eq!(mode("share/doc/siple"), 0o755);
            assert_eq!(mode("bin"), 0o755);
        }
    }

    #[test]
    fn rejects_symlinks_hardlinks_and_devices() {
        let base = || Tb::new().release("0.2.0", "a", "b");
        rejects(
            base().link(
                "siple-0.2.0/share/evil",
                tar::EntryType::Symlink,
                "/etc/passwd",
            ),
            "symlink",
        );
        rejects(
            base().link(
                "siple-0.2.0/share/evil",
                tar::EntryType::Link,
                "siple-0.2.0/bin/siple",
            ),
            "hardlink",
        );
        rejects(
            base().special("siple-0.2.0/share/dev", tar::EntryType::Char),
            "char device",
        );
        rejects(
            base().special("siple-0.2.0/share/dev", tar::EntryType::Block),
            "block device",
        );
        rejects(
            base().special("siple-0.2.0/share/fifo", tar::EntryType::Fifo),
            "fifo",
        );
    }

    #[test]
    fn rejects_bad_paths() {
        let base = || Tb::new().release("0.2.0", "a", "b");
        for (name, p) in [
            ("traversal", "siple-0.2.0/../evil"),
            ("traversal mid", "siple-0.2.0/share/../../evil"),
            ("absolute", "/etc/evil"),
            ("wrong top", "siple-0.3.0/bin/siple"),
            ("other top", "other/bin/siple"),
            ("dot", "siple-0.2.0/./share/x"),
            ("empty component", "siple-0.2.0/share//x"),
            ("backslash", "siple-0.2.0/share/a\\b"),
            ("outside allowlist", "siple-0.2.0/etc/passwd"),
            ("nested license", "siple-0.2.0/LICENSE/x"),
            ("top level file", "siple-0.2.0"),
        ] {
            rejects(base().file(p, b"x"), name);
        }
    }

    #[test]
    fn classify_rejects_nul() {
        assert!(classify(b"siple-0.2.0/share/a\0b", false, "siple-0.2.0").is_err());
        assert!(classify(b"siple-0.2.0/share/ab", false, "siple-0.2.0").is_ok());
    }

    #[test]
    fn rejects_a_directory_outside_the_allowlist() {
        rejects(
            Tb::new().release("0.2.0", "a", "b").dir("siple-0.2.0/etc/"),
            "dir",
        );
    }

    #[test]
    fn rejects_duplicates() {
        rejects(
            Tb::new()
                .release("0.2.0", "a", "b")
                .file("siple-0.2.0/bin/siple", b"again"),
            "duplicate",
        );
    }

    #[test]
    fn rejects_missing_binaries() {
        let t = "siple-0.2.0";
        rejects(
            Tb::new()
                .file(&format!("{t}/bin/siple-cli"), b"x")
                .file(&format!("{t}/LICENSE"), b"l"),
            "no siple",
        );
        rejects(
            Tb::new().file(&format!("{t}/bin/siple"), b"x"),
            "no siple-cli",
        );
    }

    #[test]
    fn enforces_the_limits() {
        let tiny = Limits {
            entries: 5,
            file: 100,
            total: 150,
        };
        // too many entries
        let (_t, r) = unpack(Tb::new().release("0.2.0", "a", "b"), tiny);
        assert!(r.unwrap_err().message.contains("too many"));
        // oversize by header
        let l = Limits {
            entries: 100,
            ..tiny
        };
        let (_t, r) = unpack(
            Tb::new()
                .release("0.2.0", "a", "b")
                .file("siple-0.2.0/share/big", &[0u8; 101]),
            l,
        );
        assert!(r.unwrap_err().message.contains("too large"));
        // total, with every file under the per-file limit
        let (_t, r) = unpack(
            Tb::new()
                .release("0.2.0", "a", "b")
                .file("siple-0.2.0/share/b1", &[0u8; 90])
                .file("siple-0.2.0/share/b2", &[0u8; 90]),
            l,
        );
        assert!(r.unwrap_err().message.contains("too large"));
        // a header that claims more than the default cap
        let (_t, r) = unpack(
            Tb::new()
                .release("0.2.0", "a", "b")
                .lying("siple-0.2.0/share/big", MAX_FILE + 1, b""),
            Limits::DEFAULT,
        );
        assert!(r.unwrap_err().message.contains("too large"));
    }

    #[test]
    fn copy_is_capped_by_the_actual_content() {
        let data = vec![7u8; 1000];
        let mut out = Vec::new();
        let e = copy_capped(&mut &data[..], &mut out, 999).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Untrusted);
        out.clear();
        assert_eq!(copy_capped(&mut &data[..], &mut out, 1000).unwrap(), 1000);
    }

    // ---- stage / exec check / commit ----

    fn version(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn platform(root: &Path, exec: Arc<dyn Exec>) -> (LinuxPlatform, Target) {
        let exe = root.join("bin/siple");
        let env = FakeFs::default().env(exe.to_str().unwrap(), Some(1000), &[]);
        (
            LinuxPlatform::with_exec(env, exec),
            Target {
                root: root.to_path_buf(),
                exe,
            },
        )
    }

    fn installed(root: &Path, marker: &str) {
        for f in [
            "bin/siple",
            "bin/siple-cli",
            "LICENSE",
            "share/applications/io.zyno.siple.desktop",
        ] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("{marker} {f}")).unwrap();
        }
        std::fs::write(root.join("user-notes.txt"), "mine").unwrap();
    }

    fn archive(dir: &Path, tb: Tb) -> PathBuf {
        let p = dir.join("a.tar.gz");
        std::fs::write(&p, tb.gz()).unwrap();
        p
    }

    fn says(v: &'static str) -> Arc<dyn Exec> {
        Arc::new(move |_: &Path| -> Result<String, String> { Ok(format!("siple {v}\n")) })
    }

    #[test]
    fn stage_dir_is_private_and_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, t) = platform(tmp.path(), says("0.2.0"));
        let a = p.stage_dir(&t).unwrap();
        let b = p.stage_dir(&t).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(tmp.path().join(STAGING)));
        assert!(is_staging_path(&a));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(m(&tmp.path().join(STAGING)), 0o700);
            assert_eq!(m(&a), 0o700);
        }
    }

    #[test]
    fn stage_checks_the_version_of_both_executables() {
        let tmp = tempfile::tempdir().unwrap();
        installed(tmp.path(), "old");
        let ar = archive(tmp.path(), Tb::new().release("0.2.0", "a", "b"));
        let expect = Expect {
            version: version("0.2.0-rc.1"),
        };
        // Both report the expected full version: staged.
        let (p, t) = platform(tmp.path(), says("0.2.0-rc.1"));
        let sd = p.stage_dir(&t).unwrap();
        let staged = p.stage(&ar, &sd, &t, &expect).unwrap();
        assert_eq!(staged.dir, sd);
        assert_eq!(staged.payload, sd.join("new/siple-0.2.0"));
        // A wrong version, a failing run and a timeout are all refused.
        for exec in [
            says("0.2.0"),
            says("0.1.9"),
            Arc::new(|_: &Path| -> Result<String, String> { Err("no answer within 5 s".into()) })
                as Arc<dyn Exec>,
            Arc::new(|e: &Path| -> Result<String, String> {
                if e.ends_with("siple") {
                    Ok("siple 0.9.9".into())
                } else {
                    Ok("0.2.0-rc.1".into())
                }
            }) as Arc<dyn Exec>,
        ] {
            let (p, t) = platform(tmp.path(), exec);
            let sd = p.stage_dir(&t).unwrap();
            let e = p.stage(&ar, &sd, &t, &expect).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Untrusted, "{e}");
            assert!(!sd.join("new").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_exec_runs_scripts_and_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let w = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let p = tmp.path().join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let ok = w("ok", "#!/bin/sh\necho siple 1.2.3\n");
        assert_eq!(RealExec.version_output(&ok).unwrap().trim(), "siple 1.2.3");
        let bad = w("bad", "#!/bin/sh\necho boom >&2\nexit 3\n");
        assert!(RealExec.version_output(&bad).unwrap_err().contains("boom"));
        assert!(
            RealExec
                .version_output(&tmp.path().join("missing"))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_and_commit_with_real_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("siple-0.1.0");
        installed(&root, "old");
        let script = |v: &str| format!("#!/bin/sh\necho siple {v}\n");
        let ar = archive(
            tmp.path(),
            Tb::new().release("0.2.0", &script("0.2.0"), &script("0.2.0")),
        );
        let (p, t) = platform(&root, Arc::new(RealExec));
        let sd = p.stage_dir(&t).unwrap();
        let staged = p
            .stage(
                &ar,
                &sd,
                &t,
                &Expect {
                    version: version("0.2.0"),
                },
            )
            .unwrap();
        let running = std::fs::metadata(root.join("bin/siple")).unwrap();

        let c = p.commit(&staged, &t).unwrap();
        assert!(matches!(&c.relaunch, RelaunchSpec::Exe(e) if *e == t.exe));
        assert_eq!(
            std::fs::read_to_string(root.join("bin/siple")).unwrap(),
            script("0.2.0")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("LICENSE")).unwrap(),
            "license"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("share/doc/siple/README.md")).unwrap(),
            "readme"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("user-notes.txt")).unwrap(),
            "mine"
        );
        assert!(root.join("bin/siple").is_file());
        // The old inode lives on as the backup.
        let backups: Vec<_> = c.cleanup.iter().filter(|b| *b != &sd).collect();
        assert_eq!(backups.len(), 4);
        {
            use std::os::unix::fs::MetadataExt;
            let b = backups
                .iter()
                .find(|b| b.to_string_lossy().contains("bin/siple.old-"))
                .unwrap();
            assert_eq!(std::fs::metadata(b).unwrap().ino(), running.ino());
            let now = std::fs::metadata(root.join("bin/siple")).unwrap();
            assert_ne!(now.ino(), running.ino());
            assert_eq!(now.mode() & 0o777, 0o755);
        }
        assert!(c.cleanup.contains(&sd));
        assert!(!sd.join(JOURNAL_NAME).exists());

        // The new version removes what the update left.
        let rec = InstalledUpdate {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            at: chrono::Utc::now(),
            cleanup: c
                .cleanup
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        };
        p.cleanup(&rec);
        for b in &backups {
            assert!(!b.exists());
        }
        assert!(!sd.exists());
        assert!(root.join("bin/siple").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_only_removes_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("siple-0.1.0");
        installed(&root, "old");
        let (p, _) = platform(&root, says("x"));
        let outside = tmp.path().join("outside.old-123");
        std::fs::write(&outside, "x").unwrap();
        let inside_ok = root.join("bin/siple.old-123");
        std::fs::write(&inside_ok, "x").unwrap();
        let not_backup = root.join("LICENSE");
        let escape = format!("{}/bin/../../outside.old-123", root.display());
        let rec = InstalledUpdate {
            from: "a".into(),
            to: "b".into(),
            at: chrono::Utc::now(),
            cleanup: vec![
                outside.to_string_lossy().into_owned(),
                inside_ok.to_string_lossy().into_owned(),
                not_backup.to_string_lossy().into_owned(),
                escape,
                root.to_string_lossy().into_owned(),
                root.join(STAGING).to_string_lossy().into_owned(),
                "relative/.siple-update/x".into(),
            ],
        };
        p.cleanup(&rec);
        assert!(outside.exists());
        assert!(!inside_ok.exists());
        assert!(not_backup.exists());
        assert!(root.join("bin/siple").exists());
    }

    #[cfg(unix)]
    #[test]
    fn recover_rolls_back_an_interrupted_swap_and_sweeps() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("siple-0.1.0");
        installed(&root, "old");
        let (p, t) = platform(&root, says("x"));
        let sd = p.stage_dir(&t).unwrap();
        // An interrupted swap: journal says LICENSE -> new, but the staged file is gone and
        // the target was already moved aside.
        let staged = sd.join("LICENSE.new");
        std::fs::write(&staged, "new").unwrap();
        let ops = StdOps::unix();
        let target = root.join("LICENSE");
        let backup = journal::backup_name(&target, 0);
        let j = journal::Journal {
            v: 1,
            entries: vec![journal::Entry {
                staged: staged.clone(),
                target: target.clone(),
                backup: Some(backup.clone()),
                sha256: journal::hash_file(&staged).unwrap(),
            }],
        };
        std::fs::write(sd.join(JOURNAL_NAME), serde_json::to_vec(&j).unwrap()).unwrap();
        journal::Ops::aside(&ops, &target, &backup).unwrap();
        std::fs::remove_file(&staged).unwrap();
        std::fs::remove_file(&target).unwrap();

        // Leftover backups: shipped names are swept, foreign names are not.
        let shipped = root.join("bin/siple-cli.old-1700000000000");
        let readme = root.join("share/doc/README.md.old-5");
        let foreign = root.join("bin/mytool.old-5");
        let user = root.join("notes.old-5");
        std::fs::create_dir_all(readme.parent().unwrap()).unwrap();
        for f in [&shipped, &readme, &foreign, &user] {
            std::fs::write(f, "x").unwrap();
        }

        p.recover();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old LICENSE");
        assert!(!sd.join(JOURNAL_NAME).exists());
        assert!(!shipped.exists());
        assert!(!readme.exists());
        assert!(foreign.exists());
        assert!(user.exists());
        assert!(sd.exists(), "a fresh staging dir is kept");

        // A day-old staging dir without a journal goes.
        let old = root.join(STAGING).join("old-one");
        std::fs::create_dir_all(&old).unwrap();
        let ft = std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
        std::fs::File::open(&old).unwrap().set_modified(ft).unwrap();
        p.recover();
        assert!(!old.exists());
        assert!(sd.exists());
    }

    #[cfg(unix)]
    #[test]
    fn commit_rolls_back_when_a_swap_step_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("siple-0.1.0");
        installed(&root, "old");
        let ar = archive(tmp.path(), Tb::new().release("0.2.0", "a", "b"));
        let (p, t) = platform(&root, says("0.2.0"));
        let sd = p.stage_dir(&t).unwrap();
        let staged = p
            .stage(
                &ar,
                &sd,
                &t,
                &Expect {
                    version: version("0.2.0"),
                },
            )
            .unwrap();
        // The target of one file is a directory: the rename over it fails.
        std::fs::remove_file(root.join("share/applications/io.zyno.siple.desktop")).unwrap();
        std::fs::create_dir(root.join("share/applications/io.zyno.siple.desktop")).unwrap();
        assert!(p.commit(&staged, &t).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("bin/siple")).unwrap(),
            "old bin/siple"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("LICENSE")).unwrap(),
            "old LICENSE"
        );
    }
}
