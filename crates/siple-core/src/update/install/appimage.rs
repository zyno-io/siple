//! Linux AppImage edition: the running copy is one file, `$APPIMAGE`; an update stages the new
//! file next to it, test-runs it without FUSE and renames it over the old one.
//!
//! The running executable of an AppImage lies in a temporary mount (or extraction directory), so
//! [`Target::exe`] is the `.AppImage` file and [`Target::root`] its directory. The swap is the
//! journaled single-file swap of the tarball (hard-link aside, rename over), so a crash at any
//! point is recovered at the next start and the running copy keeps its inode.

use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::env::Env;
use super::journal::{self, JOURNAL_NAME};
use super::linux;
use super::*;
use crate::ui_host::RelaunchSpec;

/// The staging area inside the AppImage's directory.
const STAGING: &str = ".siple-update";
/// The first run extracts about 79 MB (slow disks and USB sticks take a while); the run stays
/// bounded so a hung payload still fails the update as untrusted.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// Leftover backups younger than this are never swept at start (the directory is a user folder).
const OLD_BACKUP: Duration = Duration::from_secs(24 * 60 * 60);
/// The name of the staged download.
const PAYLOAD: &str = "payload.AppImage";

/// Runs the staged AppImage with `--version`.
pub trait AppImageExec: Send + Sync {
    /// stdout of the `--version` of `program` (`siple` or `siple-cli`) of `<file>`, env
    /// `APPIMAGE_EXTRACT_AND_RUN=1`, `TMPDIR=<tmp>` and `APPIMAGELAUNCHER_DISABLE=1`. The CLI is
    /// selected with the explicit argument `--cli`, never by argv[0] (AppImageLauncher and the
    /// runtime can replace it).
    fn version_output(&self, file: &Path, program: &str, tmp: &Path) -> Result<String, String>;
}

impl<F> AppImageExec for F
where
    F: Fn(&Path, &str, &Path) -> Result<String, String> + Send + Sync,
{
    fn version_output(&self, file: &Path, program: &str, tmp: &Path) -> Result<String, String> {
        self(file, program, tmp)
    }
}

/// Runs the real process.
pub struct RealAppImageExec;

/// Variables of an AppImage run that this process may have inherited from its own runtime; the
/// staged file must set its own, so the payload is tested and not the installed copy.
const INHERITED_RUNTIME_VARS: [&str; 5] = ["TARGET_APPIMAGE", "APPIMAGE", "APPDIR", "ARGV0", "OWD"];

impl RealAppImageExec {
    /// The command that runs `program` of `file`: `<file> [--cli] --version`.
    fn command(file: &Path, program: &str, tmp: &Path) -> Command {
        let mut cmd = Command::new(file);
        // The staged file's own AppRun saves the library path it inherits as the host's: hand it
        // the real host path, not our bundled one.
        siple_config::channel::apply_host_env(&mut cmd);
        // AppRun dispatches on this argument whatever argv[0] the runtime hands it.
        if program == "siple-cli" {
            cmd.arg("--cli");
        }
        cmd.arg("--version")
            .env("APPIMAGE_EXTRACT_AND_RUN", "1")
            .env("TMPDIR", tmp)
            // AppImageLauncher's binfmt handler would ask to integrate the hidden staged file.
            .env("APPIMAGELAUNCHER_DISABLE", "1");
        for k in INHERITED_RUNTIME_VARS {
            cmd.env_remove(k);
        }
        cmd
    }
}

impl AppImageExec for RealAppImageExec {
    fn version_output(&self, file: &Path, program: &str, tmp: &Path) -> Result<String, String> {
        linux::run_capped(Self::command(file, program, tmp), RUN_TIMEOUT)
    }
}

/// The Linux AppImage install.
pub struct AppImagePlatform {
    env: Env,
    exec: Arc<dyn AppImageExec>,
    /// This copy runs from an extraction directory (no FUSE).
    extracted: bool,
}

/// Where the running AppImage is.
struct Located {
    /// The canonical `.AppImage` file.
    file: PathBuf,
    /// Its directory.
    dir: PathBuf,
}

impl AppImagePlatform {
    /// The platform of this machine.
    pub fn new(env: Env) -> Self {
        Self::with_exec(env, Arc::new(RealAppImageExec))
    }

    /// With another way to run the staged file (tests).
    pub fn with_exec(env: Env, exec: Arc<dyn AppImageExec>) -> Self {
        let extracted = env
            .var("APPDIR")
            .is_some_and(|d| siple_config::channel::appdir_is_extracted(Path::new(d)));
        Self {
            env,
            exec,
            extracted,
        }
    }

    /// Checks only (no writability): the running copy is this AppImage's `usr/bin/siple`.
    fn locate(&self) -> Result<Located, NotPossible> {
        let env = &self.env;
        let fs = &env.fs;
        let not_bundle = || NotPossible::new("not_bundle", "this is not the Siple AppImage");
        let abs = |k: &str| {
            env.var(k)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .ok_or_else(not_bundle)
        };
        let appdir = fs.canonical(&abs("APPDIR")?);
        let file = fs.canonical(&abs("APPIMAGE")?);
        if env.exe != appdir.join("usr/bin/siple")
            || !fs.exists(&appdir.join("usr/bin/siple-cli"))
            || !fs.exists(&appdir.join("io.zyno.siple.desktop"))
            || !fs.is_file(&file)
        {
            return Err(not_bundle());
        }
        let dir = file.parent().ok_or_else(not_bundle)?.to_path_buf();
        Ok(Located { file, dir })
    }
}

impl Platform for AppImagePlatform {
    fn asset(&self) -> Option<AssetPlatform> {
        AssetPlatform::linux_appimage(std::env::consts::ARCH)
    }

    fn probe(&self) -> Result<Target, NotPossible> {
        let env = &self.env;
        let fs = &env.fs;
        let no = NotPossible::new;
        if env.var("SIPLE_NO_SELF_UPDATE").is_some() {
            return Err(no("disabled", "SIPLE_NO_SELF_UPDATE is set"));
        }
        if let Some(n) = linux::sandbox_refusal(env) {
            return Err(n);
        }
        if linux::in_container(env) {
            return Err(no("container", "running inside a container"));
        }
        let loc = self.locate()?;
        let dir = &loc.dir;
        if dir.starts_with("/nix/store") || dir.starts_with("/gnu/store") {
            return Err(no("package_manager", "installed by Nix or Guix"));
        }
        if dir.starts_with("/usr") {
            return Err(no("system_install", "installed under /usr"));
        }
        if env.uid == Some(0) {
            return Err(no(
                "root",
                "running as root; updates are not installed as root",
            ));
        }
        if fs.read_only_volume(dir) {
            return Err(no(
                "read_only_volume",
                "the AppImage is on a read-only volume",
            ));
        }
        if !fs.can_create_in(dir) {
            return Err(no(
                "not_writable",
                "the folder of the AppImage is not writable",
            ));
        }
        if env.uid.is_none() || fs.owner(&loc.file) != env.uid {
            return Err(no("not_owner", "the AppImage belongs to another user"));
        }
        Ok(Target {
            root: loc.dir,
            exe: loc.file,
        })
    }

    fn stage_dir(&self, target: &Target) -> Result<PathBuf, InstallError> {
        linux::new_stage_dir(&target.root)
    }

    fn stage(
        &self,
        archive: &Path,
        stage_dir: &Path,
        target: &Target,
        expect: &Expect,
    ) -> Result<Staged, InstallError> {
        check_header(archive, std::env::consts::ARCH)?;
        // `run_inner` deletes `archive` as soon as this returns: here it is the payload.
        let payload = stage_dir.join(PAYLOAD);
        std::fs::rename(archive, &payload)
            .map_err(|e| InstallError::io("cannot stage the download", &e))?;
        let result = (|| {
            let mode = std::fs::metadata(&target.exe).map_or(0o755, |m| m.mode() & 0o755 | 0o700);
            linux::set_mode(&payload, mode)
                .map_err(|e| InstallError::io("cannot set permissions", &e))?;
            let run = stage_dir.join("run");
            linux::make_dir(&run, false, 0o700)
                .map_err(|e| InstallError::io("cannot create staging", &e))?;
            let version = expect.version.to_string();
            let checked = (|| {
                for program in ["siple-cli", "siple"] {
                    let out = self
                        .exec
                        .version_output(&payload, program, &run)
                        .map_err(|e| {
                            InstallError::untrusted(format!(
                                "the new {program} does not run here: {e}"
                            ))
                        })?;
                    if !linux::reports_program_version(&out, program, &version) {
                        return Err(InstallError::untrusted(format!(
                            "the new {program} does not report version {version}"
                        )));
                    }
                }
                Ok(())
            })();
            // The runtime never removes its extraction directory.
            let _ = std::fs::remove_dir_all(&run);
            checked
        })();
        match result {
            Ok(()) => Ok(Staged {
                dir: stage_dir.to_path_buf(),
                payload,
            }),
            Err(e) => {
                let _ = std::fs::remove_file(&payload);
                Err(e)
            }
        }
    }

    fn commit(&self, staged: &Staged, target: &Target) -> Result<Committed, InstallError> {
        if !is_staging_path(&staged.dir)
            || !staged.payload.starts_with(&staged.dir)
            || !staged.payload.is_file()
        {
            return Err(InstallError::new(
                ErrorKind::Unsupported,
                "the staged files are not in a staging area",
            ));
        }
        let backups = journal::swap(
            &[(staged.payload.clone(), target.exe.clone())],
            &staged.dir.join(JOURNAL_NAME),
            &AppImageOps::default(),
        )
        .map_err(|e| InstallError::io("cannot replace the AppImage", &e))?;
        // Best effort: make the rename durable.
        if let Ok(d) = std::fs::File::open(&target.root) {
            let _ = d.sync_all();
        }
        let mut cleanup = backups;
        cleanup.push(staged.dir.clone());
        Ok(Committed {
            relaunch: RelaunchSpec::AppImage {
                file: target.exe.clone(),
                extract_and_run: self.extracted,
            },
            cleanup,
        })
    }

    fn recover(&self) {
        let Ok(loc) = self.locate() else {
            return;
        };
        let pending = linux::recover_staging(&loc.dir.join(STAGING), &AppImageOps::default());
        if !pending && let Some(name) = loc.file.file_name().and_then(|n| n.to_str()) {
            sweep_old_backups(&loc.dir, name);
        }
    }

    fn cleanup(&self, record: &InstalledUpdate) {
        let Ok(loc) = self.locate() else {
            return;
        };
        let Some(name) = loc.file.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        let staging = loc.dir.join(STAGING);
        for p in record.cleanup.iter().map(PathBuf::from) {
            let clean =
                p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir));
            if !clean {
                continue;
            }
            if is_staging_path(&p) && p.starts_with(&staging) && p != staging {
                let _ = std::fs::remove_dir_all(&p);
            } else if p.parent() == Some(loc.dir.as_path())
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| backup_of(n, name).is_some())
                && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file())
            {
                let _ = std::fs::remove_file(&p);
            }
        }
        let _ = std::fs::remove_dir(&staging); // only when empty
    }
}

/// The ELF `e_machine` of a CPU architecture (`std::env::consts::ARCH`).
fn machine_for(arch: &str) -> Option<u16> {
    match arch {
        "x86_64" => Some(62),
        "aarch64" => Some(183),
        _ => None,
    }
}

/// The 64-byte ELF header must say: 64-bit little-endian, the machine of `arch`, with the type 2
/// AppImage magic `AI\x02` at offset 8.
fn check_header(archive: &Path, arch: &str) -> Result<(), InstallError> {
    use std::io::Read;
    let not = || InstallError::untrusted(format!("the download is not a {arch} AppImage"));
    let machine = machine_for(arch).ok_or_else(not)?;
    let mut head = [0u8; 64];
    std::fs::File::open(archive)
        .and_then(|mut f| f.read_exact(&mut head))
        .map_err(|_| not())?;
    let ok = head.starts_with(b"\x7fELF")
        && head[4] == 2
        && head[5] == 1
        && &head[8..11] == b"AI\x02"
        && u16::from_le_bytes([head[18], head[19]]) == machine;
    if ok { Ok(()) } else { Err(not()) }
}

/// The `<ms>` of a backup of `file_name` (`<file_name>.old-<ms>[-n]`, see
/// [`journal::backup_name`]); `None` for any other name.
fn backup_of(name: &str, file_name: &str) -> Option<u128> {
    let tail = name.strip_prefix(file_name)?.strip_prefix(".old-")?;
    if !journal::is_backup_name(name) {
        return None;
    }
    tail.split('-').next()?.parse().ok()
}

/// Removes regular files `<file_name>.old-<ms>[-n]` in `dir` whose `<ms>` is more than a day old.
fn sweep_old_backups(dir: &Path, file_name: &str) {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(Result::ok) {
        if e.file_type().is_ok_and(|t| t.is_file())
            && let Some(name) = e.file_name().to_str()
            && let Some(ms) = backup_of(name, file_name)
            && now.saturating_sub(ms) > OLD_BACKUP.as_millis()
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The single-file swap: the old AppImage is hard-linked aside (copied where the file system has
/// no hard links, e.g. a FAT USB stick), so `$APPIMAGE` exists at every instant; the new one is
/// renamed over it.
#[derive(Default)]
struct AppImageOps {
    /// Never hard-link (tests).
    force_copy: bool,
}

impl journal::Ops for AppImageOps {
    fn aside(&self, target: &Path, backup: &Path) -> std::io::Result<()> {
        if !self.force_copy && std::fs::hard_link(target, backup).is_ok() {
            return Ok(());
        }
        let copied =
            std::fs::copy(target, backup).and_then(|_| std::fs::File::open(backup)?.sync_all());
        if copied.is_err() {
            let _ = std::fs::remove_file(backup);
        }
        copied
    }

    fn move_in(&self, staged: &Path, target: &Path) -> std::io::Result<()> {
        std::fs::rename(staged, target)
    }

    fn restore(&self, backup: &Path, target: &Path) -> std::io::Result<()> {
        std::fs::rename(backup, target)
    }

    fn remove(&self, p: &Path) -> std::io::Result<()> {
        std::fs::remove_file(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::install::env::fake::FakeFs;
    use crate::update::version::Version;
    use std::sync::Mutex;

    const MOUNT: &str = "/tmp/.mount_SipleAB";
    const EXE: &str = "/tmp/.mount_SipleAB/usr/bin/siple";
    const DIR: &str = "/home/u/Applications";
    const FILE: &str = "/home/u/Applications/Siple.AppImage";

    fn good_fs() -> FakeFs {
        FakeFs::default()
            .file(EXE, "")
            .file("/tmp/.mount_SipleAB/usr/bin/siple-cli", "")
            .file("/tmp/.mount_SipleAB/io.zyno.siple.desktop", "")
            .file(FILE, "")
            .writable(DIR)
            .owner(FILE, 1000)
    }

    const VARS: &[(&str, &str)] = &[("APPIMAGE", FILE), ("APPDIR", MOUNT)];

    fn probe(
        fs: FakeFs,
        exe: &str,
        uid: Option<u32>,
        vars: &[(&str, &str)],
    ) -> Result<Target, NotPossible> {
        AppImagePlatform::new(fs.env(exe, uid, vars)).probe()
    }

    fn version(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn probe_accepts_a_user_owned_appimage() {
        let t = probe(good_fs(), EXE, Some(1000), VARS).unwrap();
        assert_eq!(t.root, Path::new(DIR));
        assert_eq!(t.exe, Path::new(FILE));
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
        let v = || VARS.to_vec();
        let with = |extra: (&'static str, &'static str)| {
            let mut v = VARS.to_vec();
            v.retain(|(k, _)| *k != extra.0);
            v.push(extra);
            v
        };
        let cases: Vec<Case> = vec![
            (
                "disabled",
                good_fs(),
                EXE,
                Some(1000),
                with(("SIPLE_NO_SELF_UPDATE", "1")),
                "disabled",
            ),
            (
                "flatpak",
                good_fs().file("/.flatpak-info", ""),
                EXE,
                Some(1000),
                v(),
                "sandboxed",
            ),
            (
                "snap",
                good_fs(),
                EXE,
                Some(1000),
                with(("SNAP", "/snap/x")),
                "sandboxed",
            ),
            (
                "dockerenv",
                good_fs().file("/.dockerenv", ""),
                EXE,
                Some(1000),
                v(),
                "container",
            ),
            (
                "container var",
                good_fs(),
                EXE,
                Some(1000),
                with(("container", "podman")),
                "container",
            ),
            (
                "cgroup docker",
                good_fs().file("/proc/1/cgroup", "0::/docker/abc\n"),
                EXE,
                Some(1000),
                v(),
                "container",
            ),
            (
                "exe outside appdir",
                good_fs(),
                "/home/u/siple-0.1.0/bin/siple",
                Some(1000),
                v(),
                "not_bundle",
            ),
            (
                "exe is siple-cli",
                good_fs(),
                "/tmp/.mount_SipleAB/usr/bin/siple-cli",
                Some(1000),
                v(),
                "not_bundle",
            ),
            (
                "no root desktop file",
                FakeFs::default()
                    .file(EXE, "")
                    .file("/tmp/.mount_SipleAB/usr/bin/siple-cli", "")
                    .file(FILE, "")
                    .writable(DIR)
                    .owner(FILE, 1000),
                EXE,
                Some(1000),
                v(),
                "not_bundle",
            ),
            (
                "no siple-cli",
                FakeFs::default()
                    .file(EXE, "")
                    .file("/tmp/.mount_SipleAB/io.zyno.siple.desktop", "")
                    .file(FILE, "")
                    .writable(DIR)
                    .owner(FILE, 1000),
                EXE,
                Some(1000),
                v(),
                "not_bundle",
            ),
            (
                "relative APPIMAGE",
                good_fs(),
                EXE,
                Some(1000),
                with(("APPIMAGE", "Siple.AppImage")),
                "not_bundle",
            ),
            (
                "no APPDIR",
                good_fs(),
                EXE,
                Some(1000),
                vec![("APPIMAGE", FILE)],
                "not_bundle",
            ),
            (
                "missing file",
                good_fs(),
                EXE,
                Some(1000),
                with(("APPIMAGE", "/home/u/Applications/Gone.AppImage")),
                "not_bundle",
            ),
            ("tarball", good_fs(), EXE, Some(1000), vec![], "not_bundle"),
            (
                "under /usr",
                good_fs()
                    .file("/usr/bin/Siple.AppImage", "")
                    .writable("/usr/bin"),
                EXE,
                Some(1000),
                with(("APPIMAGE", "/usr/bin/Siple.AppImage")),
                "system_install",
            ),
            (
                "nix",
                good_fs().file("/nix/store/x/Siple.AppImage", ""),
                EXE,
                Some(1000),
                with(("APPIMAGE", "/nix/store/x/Siple.AppImage")),
                "package_manager",
            ),
            ("root", good_fs(), EXE, Some(0), v(), "root"),
            (
                "read only",
                good_fs().read_only(DIR),
                EXE,
                Some(1000),
                v(),
                "read_only_volume",
            ),
            (
                "not writable",
                FakeFs::default()
                    .file(EXE, "")
                    .file("/tmp/.mount_SipleAB/usr/bin/siple-cli", "")
                    .file("/tmp/.mount_SipleAB/io.zyno.siple.desktop", "")
                    .file(FILE, "")
                    .owner(FILE, 1000),
                EXE,
                Some(1000),
                v(),
                "not_writable",
            ),
            (
                "other owner",
                good_fs().owner(FILE, 0),
                EXE,
                Some(1000),
                v(),
                "not_owner",
            ),
            ("unknown uid", good_fs(), EXE, None, v(), "not_owner"),
        ];
        for (name, fs, exe, uid, vars, reason) in cases {
            let got = probe(fs, exe, uid, &vars).unwrap_err();
            assert_eq!(got.reason, reason, "{name}: {}", got.detail);
        }
    }

    #[test]
    fn asset_is_the_appimage() {
        let p = AppImagePlatform::new(good_fs().env(EXE, Some(1000), VARS));
        assert_eq!(
            p.asset(),
            AssetPlatform::linux_appimage(std::env::consts::ARCH)
        );
    }

    #[test]
    fn extract_mode_follows_appdir_name() {
        let mk = |appdir: &str| {
            AppImagePlatform::new(good_fs().env(
                EXE,
                Some(1000),
                &[("APPIMAGE", FILE), ("APPDIR", appdir)],
            ))
            .extracted
        };
        assert!(mk("/tmp/appimage_extracted_0123abcd"));
        assert!(!mk("/tmp/.mount_SipleAB"));
    }

    // ---- staging and swapping, on the real file system ----

    fn host_machine() -> u16 {
        machine_for(std::env::consts::ARCH).unwrap()
    }

    /// An architecture's machine that is not this one's.
    fn other_machine() -> u16 {
        if host_machine() == 62 { 183 } else { 62 }
    }

    #[test]
    fn the_elf_machine_follows_the_architecture() {
        assert_eq!(machine_for("x86_64"), Some(62));
        assert_eq!(machine_for("aarch64"), Some(183));
        assert_eq!(machine_for("riscv64"), None);
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a");
        for (arch, good, bad) in [("x86_64", 62, 183), ("aarch64", 183, 62)] {
            std::fs::write(&f, fake_image(good, "x")).unwrap();
            assert!(check_header(&f, arch).is_ok(), "{arch}");
            std::fs::write(&f, fake_image(bad, "x")).unwrap();
            assert!(check_header(&f, arch).is_err(), "{arch}");
        }
        assert!(check_header(&f, "riscv64").is_err());
    }

    /// A fake AppImage: the ELF header, the type 2 magic and machine `machine`.
    fn fake_image(machine: u16, body: &str) -> Vec<u8> {
        let mut h = vec![0u8; 64];
        h[..4].copy_from_slice(b"\x7fELF");
        h[4] = 2;
        h[5] = 1;
        h[8..11].copy_from_slice(b"AI\x02");
        h[18..20].copy_from_slice(&machine.to_le_bytes());
        h.extend_from_slice(body.as_bytes());
        h
    }

    /// An "installed" AppImage in a tempdir, a platform around it and the target.
    fn setup(
        exec: Arc<dyn AppImageExec>,
    ) -> (tempfile::TempDir, PathBuf, AppImagePlatform, Target) {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("Siple.AppImage");
        std::fs::write(&file, fake_image(host_machine(), "old")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let appdir = root.join(".mount_SipleAB");
        std::fs::create_dir_all(appdir.join("usr/bin")).unwrap();
        let exe = appdir.join("usr/bin/siple");
        std::fs::write(&exe, "").unwrap();
        std::fs::write(appdir.join("usr/bin/siple-cli"), "").unwrap();
        std::fs::write(appdir.join("io.zyno.siple.desktop"), "").unwrap();
        let env = Env {
            exe,
            vars: [
                ("APPIMAGE".to_string(), file.display().to_string()),
                ("APPDIR".to_string(), appdir.display().to_string()),
            ]
            .into(),
            uid: Some(unsafe { libc::geteuid() }),
            fs: Arc::new(super::super::env::RealFs),
        };
        let p = AppImagePlatform::with_exec(env, exec);
        let t = Target {
            root: root.clone(),
            exe: file,
        };
        (tmp, root, p, t)
    }

    fn says(v: &'static str) -> Arc<dyn AppImageExec> {
        Arc::new(
            move |_: &Path, program: &str, _: &Path| -> Result<String, String> {
                Ok(format!("{program} {v}\n"))
            },
        )
    }

    fn download(root: &Path, bytes: &[u8]) -> PathBuf {
        let p = root.join("dl.bin");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn expect(v: &str) -> Expect {
        Expect {
            version: version(v),
        }
    }

    #[test]
    fn stage_moves_checks_and_runs_the_download() {
        type Call = (String, PathBuf, PathBuf);
        let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
        let c2 = calls.clone();
        let exec: Arc<dyn AppImageExec> = Arc::new(
            move |f: &Path, program: &str, tmp: &Path| -> Result<String, String> {
                assert!(tmp.is_dir());
                c2.lock()
                    .unwrap()
                    .push((program.to_string(), tmp.to_path_buf(), f.to_path_buf()));
                Ok(format!("{program} 0.2.0\n"))
            },
        );
        let (_tmp, root, p, t) = setup(exec);
        let ar = download(&root, &fake_image(host_machine(), "new"));
        let sd = p.stage_dir(&t).unwrap();
        let staged = p.stage(&ar, &sd, &t, &expect("0.2.0")).unwrap();
        assert_eq!(staged.dir, sd);
        assert_eq!(staged.payload, sd.join(PAYLOAD));
        assert!(!ar.exists(), "the download was moved");
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(&staged.payload)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o755);
        }
        let calls = calls.lock().unwrap();
        let names: Vec<_> = calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(names, ["siple-cli", "siple"]);
        for (_, tmp, file) in calls.iter() {
            assert!(tmp.starts_with(&sd));
            assert!(!tmp.exists(), "the run directory is removed");
            assert_eq!(file, &staged.payload);
        }
    }

    #[test]
    fn stage_refuses_non_appimages() {
        let mut elf32 = fake_image(host_machine(), "x");
        elf32[4] = 1;
        let mut be = fake_image(host_machine(), "x");
        be[5] = 2;
        let mut nomagic = fake_image(host_machine(), "x");
        nomagic[10] = 1;
        for (name, bytes) in [
            ("not elf", b"#!/bin/sh\necho hi\n".to_vec()),
            ("tiny", b"\x7fELF".to_vec()),
            ("no magic", nomagic),
            ("32-bit", elf32),
            ("big endian", be),
            ("aarch64", fake_image(other_machine(), "x")),
        ] {
            let (_tmp, root, p, t) = setup(says("0.2.0"));
            let ar = download(&root, &bytes);
            let sd = p.stage_dir(&t).unwrap();
            let e = p.stage(&ar, &sd, &t, &expect("0.2.0")).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Untrusted, "{name}: {e}");
            assert_eq!(std::fs::read_dir(&sd).unwrap().count(), 0, "{name}");
        }
    }

    #[test]
    fn stage_refuses_a_wrong_version_or_a_failed_run() {
        let wrong = Arc::new(
            |_: &Path, program: &str, _: &Path| -> Result<String, String> {
                Ok(if program == "siple" {
                    "siple 0.9.9".into()
                } else {
                    "siple-cli 0.2.0".into()
                })
            },
        ) as Arc<dyn AppImageExec>;
        let fails = Arc::new(|_: &Path, _: &str, _: &Path| -> Result<String, String> {
            Err("no answer within 120 s".into())
        }) as Arc<dyn AppImageExec>;
        for exec in [says("0.1.9"), wrong, fails] {
            let (_tmp, root, p, t) = setup(exec);
            let ar = download(&root, &fake_image(host_machine(), "new"));
            let sd = p.stage_dir(&t).unwrap();
            let e = p.stage(&ar, &sd, &t, &expect("0.2.0")).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Untrusted, "{e}");
            assert!(!sd.join(PAYLOAD).exists());
            assert!(!sd.join("run").exists());
        }
    }

    #[test]
    fn real_exec_sets_the_environment() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fake.AppImage");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"$APPIMAGE_EXTRACT_AND_RUN $TMPDIR $APPIMAGELAUNCHER_DISABLE $*\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = RealAppImageExec
            .version_output(&script, "siple-cli", Path::new("/x/run"))
            .unwrap();
        assert_eq!(out.trim(), "1 /x/run 1 --cli --version");
        let out = RealAppImageExec
            .version_output(&script, "siple", Path::new("/x/run"))
            .unwrap();
        assert_eq!(out.trim(), "1 /x/run 1 --version");
    }

    #[test]
    fn probe_command_selects_the_cli_by_argument_and_drops_inherited_runtime_vars() {
        let args = |program: &str| -> Vec<String> {
            RealAppImageExec::command(Path::new("/x/p.AppImage"), program, Path::new("/x/run"))
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(args("siple-cli"), ["--cli", "--version"]);
        assert_eq!(args("siple"), ["--version"]);
        let cmd =
            RealAppImageExec::command(Path::new("/x/p.AppImage"), "siple-cli", Path::new("/x/run"));
        let envs: Vec<_> = cmd.get_envs().collect();
        for k in INHERITED_RUNTIME_VARS {
            assert!(
                envs.iter()
                    .any(|(name, val)| *name == std::ffi::OsStr::new(k) && val.is_none()),
                "{k} is removed"
            );
        }
    }

    /// An update to `new` staged in a fresh setup.
    fn staged(p: &AppImagePlatform, t: &Target, root: &Path, new: &str) -> (PathBuf, Staged) {
        let ar = download(root, &fake_image(host_machine(), new));
        let sd = p.stage_dir(t).unwrap();
        let staged = p.stage(&ar, &sd, t, &expect("0.2.0")).unwrap();
        (sd, staged)
    }

    #[test]
    fn commit_replaces_the_file_atomically_and_keeps_the_old_inode() {
        let (_tmp, root, p, t) = setup(says("0.2.0"));
        let running = std::fs::metadata(&t.exe).unwrap();
        let (sd, st) = staged(&p, &t, &root, "new");
        let c = p.commit(&st, &t).unwrap();
        assert_eq!(
            c.relaunch,
            RelaunchSpec::AppImage {
                file: t.exe.clone(),
                extract_and_run: false
            }
        );
        assert_eq!(
            std::fs::read(&t.exe).unwrap(),
            fake_image(host_machine(), "new")
        );
        let now = std::fs::metadata(&t.exe).unwrap();
        assert_ne!(now.ino(), running.ino());
        assert_eq!(now.mode() & 0o777, 0o755);
        let backups: Vec<_> = c.cleanup.iter().filter(|b| **b != sd).collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::metadata(backups[0]).unwrap().ino(), running.ino());
        assert_eq!(
            std::fs::read(backups[0]).unwrap(),
            fake_image(host_machine(), "old")
        );
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
        assert!(!backups[0].exists());
        assert!(!sd.exists());
        assert!(t.exe.exists());
    }

    #[test]
    fn extract_mode_is_carried_into_the_relaunch() {
        let (_tmp, root, mut p, t) = setup(says("0.2.0"));
        p.extracted = true;
        let (_sd, st) = staged(&p, &t, &root, "new");
        let c = p.commit(&st, &t).unwrap();
        assert!(matches!(
            c.relaunch,
            RelaunchSpec::AppImage {
                extract_and_run: true,
                ..
            }
        ));
    }

    #[test]
    fn commit_copies_aside_without_hard_links() {
        let (_tmp, root, p, t) = setup(says("0.2.0"));
        let running = std::fs::metadata(&t.exe).unwrap();
        let (sd, st) = staged(&p, &t, &root, "new");
        let backups = journal::swap(
            &[(st.payload.clone(), t.exe.clone())],
            &sd.join(JOURNAL_NAME),
            &AppImageOps { force_copy: true },
        )
        .unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read(&backups[0]).unwrap(),
            fake_image(host_machine(), "old")
        );
        assert_ne!(std::fs::metadata(&backups[0]).unwrap().ino(), running.ino());
        assert_eq!(
            std::fs::read(&t.exe).unwrap(),
            fake_image(host_machine(), "new")
        );
    }

    #[test]
    fn commit_rolls_back_when_the_rename_fails() {
        let (_tmp, root, p, t) = setup(says("0.2.0"));
        let (sd, st) = staged(&p, &t, &root, "new");
        // The target is a directory from now on: the rename over it fails.
        std::fs::remove_file(&t.exe).unwrap();
        std::fs::create_dir(&t.exe).unwrap();
        assert!(p.commit(&st, &t).is_err());
        assert!(t.exe.is_dir());
        assert!(!sd.join(JOURNAL_NAME).exists());
    }

    #[test]
    fn commit_refuses_a_payload_outside_the_staging_area() {
        let (_tmp, root, p, t) = setup(says("0.2.0"));
        let f = download(&root, &fake_image(host_machine(), "new"));
        let bad = Staged {
            dir: root.clone(),
            payload: f,
        };
        assert!(p.commit(&bad, &t).is_err());
        assert_eq!(
            std::fs::read(&t.exe).unwrap(),
            fake_image(host_machine(), "old")
        );
    }

    /// Writes an interrupted-swap journal: the old file is already linked aside.
    fn interrupted(
        p: &AppImagePlatform,
        t: &Target,
        root: &Path,
        keep_staged: bool,
    ) -> (PathBuf, PathBuf) {
        let (sd, st) = staged(p, t, root, "new");
        let backup = journal::backup_name(&t.exe, 0);
        let j = journal::Journal {
            v: 1,
            entries: vec![journal::Entry {
                staged: st.payload.clone(),
                target: t.exe.clone(),
                backup: Some(backup.clone()),
                sha256: journal::hash_file(&st.payload).unwrap(),
            }],
        };
        std::fs::write(sd.join(JOURNAL_NAME), serde_json::to_vec(&j).unwrap()).unwrap();
        journal::Ops::aside(&AppImageOps::default(), &t.exe, &backup).unwrap();
        if !keep_staged {
            std::fs::remove_file(&st.payload).unwrap();
        }
        (sd, backup)
    }

    #[test]
    fn recover_rolls_forward_and_back() {
        // Crash before the rename, staged file intact: roll forward.
        let (_tmp, root, p, t) = setup(says("0.2.0"));
        let (sd, _) = interrupted(&p, &t, &root, true);
        p.recover();
        assert_eq!(
            std::fs::read(&t.exe).unwrap(),
            fake_image(host_machine(), "new")
        );
        assert!(!sd.join(JOURNAL_NAME).exists());

        // Payload deleted before recovery: roll back to the old file.
        let (_tmp, root, p, t) = setup(says("0.2.0"));
        let (sd, _) = interrupted(&p, &t, &root, false);
        p.recover();
        assert_eq!(
            std::fs::read(&t.exe).unwrap(),
            fake_image(host_machine(), "old")
        );
        assert!(!sd.join(JOURNAL_NAME).exists());
    }

    #[test]
    fn recover_sweeps_only_old_backups_of_this_file() {
        let (_tmp, root, p, _t) = setup(says("0.2.0"));
        let ms = |ago: Duration| {
            (SystemTime::now() - ago)
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        };
        let mk = |name: String| {
            let f = root.join(name);
            std::fs::write(&f, "x").unwrap();
            f
        };
        let young = mk(format!("Siple.AppImage.old-{}", ms(Duration::ZERO)));
        let old = mk(format!(
            "Siple.AppImage.old-{}",
            ms(Duration::from_secs(48 * 3600))
        ));
        let old_n = mk(format!(
            "Siple.AppImage.old-{}-2",
            ms(Duration::from_secs(48 * 3600))
        ));
        let other = mk("Other.AppImage.old-1".into());
        let notes = mk("notes.old-1".into());
        let prefix = mk("Siple.AppImage.old-1.bak".into());
        p.recover();
        assert!(young.exists());
        assert!(!old.exists());
        assert!(!old_n.exists());
        assert!(other.exists());
        assert!(notes.exists());
        assert!(prefix.exists());
        assert!(root.join("Siple.AppImage").exists());
    }

    #[test]
    fn cleanup_only_removes_ours() {
        let (tmp, root, p, t) = setup(says("0.2.0"));
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("Siple.AppImage.old-123");
        std::fs::write(&outside, "x").unwrap();
        let ours = root.join("Siple.AppImage.old-123");
        std::fs::write(&ours, "x").unwrap();
        let other = root.join("Other.AppImage.old-123");
        std::fs::write(&other, "x").unwrap();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let nested = sub.join("Siple.AppImage.old-123");
        std::fs::write(&nested, "x").unwrap();
        let sd = p.stage_dir(&t).unwrap();
        let escape = format!("{}/sub/../../x.old-1", root.display());
        let rec = InstalledUpdate {
            from: "a".into(),
            to: "b".into(),
            at: chrono::Utc::now(),
            cleanup: vec![
                outside.to_string_lossy().into_owned(),
                ours.to_string_lossy().into_owned(),
                other.to_string_lossy().into_owned(),
                nested.to_string_lossy().into_owned(),
                t.exe.to_string_lossy().into_owned(),
                root.to_string_lossy().into_owned(),
                root.join(STAGING).to_string_lossy().into_owned(),
                escape,
                "relative/.siple-update/x".into(),
                sd.to_string_lossy().into_owned(),
            ],
        };
        p.cleanup(&rec);
        assert!(outside.exists());
        assert!(!ours.exists());
        assert!(other.exists());
        assert!(nested.exists());
        assert!(t.exe.exists());
        assert!(!sd.exists());
        drop(tmp);
    }

    /// Needs a real AppImage: `SIPLE_TEST_APPIMAGE=<file> cargo test -p siple-core appimage_real
    /// -- --ignored`. Runs the real staging check (extract-and-run, `--version` of both programs)
    /// and the swap on copies of it.
    #[test]
    #[ignore = "needs SIPLE_TEST_APPIMAGE"]
    fn appimage_real_stage_and_commit() {
        let src = std::env::var_os("SIPLE_TEST_APPIMAGE").expect("SIPLE_TEST_APPIMAGE");
        let (_tmp, root, p, t) = setup(Arc::new(RealAppImageExec));
        std::fs::copy(&src, &t.exe).unwrap();
        let ar = root.join("download.AppImage");
        std::fs::copy(&src, &ar).unwrap();
        let sd = p.stage_dir(&t).unwrap();
        let ver = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let staged = p.stage(&ar, &sd, &t, &Expect { version: ver }).unwrap();
        assert!(!sd.join("run").exists());
        let c = p.commit(&staged, &t).unwrap();
        assert_eq!(c.cleanup.len(), 2);
        assert_eq!(std::fs::read(&t.exe).unwrap(), std::fs::read(&src).unwrap());
    }
}
