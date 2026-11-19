//! macOS install: replace the running `Siple.app` by the verified one with one atomic swap.
//!
//! - The staging directory is an `NSItemReplacementDirectory` for the bundle (same volume, private
//!   to the user), so the final step is `renamex_np(RENAME_SWAP)`; the old bundle ends up in the
//!   staging area and is removed by the new version after it started.
//! - The archive is unpacked with `/usr/bin/ditto -x -k` after a pre-check of its entries, and the
//!   result must be exactly one top-level `Siple.app`.
//! - Trust: `codesign --verify --deep --strict -R=<requirement>` of the app and of
//!   `Contents/MacOS/siple-cli` (the requirement names the identifier, Apple's Developer ID chain
//!   and our team), `spctl --assess` must say `source=Notarized Developer ID`, and `Info.plist` must
//!   carry our bundle id, the expected version and a minimum macOS the running system meets. The
//!   running bundle must pass the same requirement before anything is downloaded into it.
//! - Running code stays valid: the process keeps its old vnodes. The bundle's path, id and
//!   requirement do not change, so Keychain items, microphone consent, the firewall and the login
//!   item keep working.
//! - Nothing here elevates; a bundle the user cannot replace falls back to the release page.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::env::Env;
use super::*;
use crate::state::InstalledUpdate;
use crate::update::appstore::{OsVersion, running_os};

/// Bundle identifier of the app.
pub const APP_ID: &str = "io.zyno.siple";
/// Code-signing identifier of the CLI inside the app.
pub const CLI_ID: &str = "io.zyno.siple-cli";
/// The Developer ID team that signs releases.
pub const TEAM_ID: &str = "VUDU4KEQ99";
/// Name of the bundle in the archive and on disk.
const BUNDLE_NAME: &str = "Siple.app";
/// Entries and total size an archive may declare.
const MAX_ENTRIES: usize = 20_000;
const MAX_UNPACKED: u64 = 2 * 1024 * 1024 * 1024;

/// The designated requirement for code with the signing identifier `id`: Apple's Developer ID
/// chain (`1.2.840.113635.100.6.2.6` on the intermediate: Developer ID CA; `.6.1.13` on the leaf:
/// Developer ID Application) and our team.
pub fn requirement(id: &str) -> String {
    format!(
        "identifier \"{id}\" and anchor apple generic and \
         certificate 1[field.1.2.840.113635.100.6.2.6] exists and \
         certificate leaf[field.1.2.840.113635.100.6.1.13] exists and \
         certificate leaf[subject.OU] = {TEAM_ID}"
    )
}

/// What a command printed (stdout and stderr together) and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Exit status 0.
    pub ok: bool,
    /// Everything it printed.
    pub text: String,
}

/// Runs the system tools (`codesign`, `spctl`, `ditto`, `plutil`, `xattr`); tests fake it.
pub trait CommandRunner: Send + Sync {
    /// Runs `program` with `args`; an `Err` is "could not start or timed out".
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> std::io::Result<Output>;
}

/// The real tools.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str], timeout: Duration) -> std::io::Result<Output> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut pipes = Vec::new();
        for p in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            pipes.push(std::thread::spawn(move || {
                let mut s = String::new();
                // At most 1 MiB of output is of any interest.
                let _ = p.take(1 << 20).read_to_string(&mut s);
                s
            }));
        }
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{program} took too long"),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let text = pipes
            .into_iter()
            .filter_map(|h| h.join().ok())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Output {
            ok: status.success(),
            text,
        })
    }
}

/// The macOS half of an install.
pub struct MacPlatform {
    env: Env,
    run: Arc<dyn CommandRunner>,
    os: OsVersion,
    /// A build made by the release workflow (a tag build); dev builds do not install.
    release_build: bool,
}

impl MacPlatform {
    /// For the running system.
    pub fn new(env: Env) -> Self {
        Self::with(
            env,
            Arc::new(SystemRunner),
            running_os(),
            crate::update::is_release_build(),
        )
    }

    /// With a given tool runner, OS version and build kind (tests).
    pub fn with(env: Env, run: Arc<dyn CommandRunner>, os: OsVersion, release_build: bool) -> Self {
        Self {
            env,
            run,
            os,
            release_build,
        }
    }

    fn tool(
        &self,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Output, InstallError> {
        self.run
            .run(program, args, timeout)
            .map_err(|e| InstallError::untrusted(format!("cannot run {program}: {e}")))
    }

    fn plist(&self, bundle: &Path, key: &str) -> Option<String> {
        let plist = bundle.join("Contents/Info.plist");
        let out = self
            .run
            .run(
                "/usr/bin/plutil",
                &["-extract", key, "raw", "-o", "-", &plist.to_string_lossy()],
                Duration::from_secs(10),
            )
            .ok()?;
        out.ok.then(|| out.text.trim().to_string())
    }

    /// `codesign` against the requirement of `id`; `path` is the app or the CLI inside it.
    fn check_signature(&self, path: &Path, id: &str) -> Result<(), InstallError> {
        let req = format!("-R={}", requirement(id));
        let out = self.tool(
            "/usr/bin/codesign",
            &[
                "--verify",
                "--deep",
                "--strict",
                &req,
                &path.to_string_lossy(),
            ],
            Duration::from_secs(120),
        )?;
        if out.ok {
            Ok(())
        } else {
            Err(InstallError::untrusted(format!(
                "{} is not signed by Siple's release key ({})",
                path.file_name().unwrap_or_default().to_string_lossy(),
                first_line(&out.text)
            )))
        }
    }

    fn check_notarized(&self, app: &Path) -> Result<(), InstallError> {
        let out = self.tool(
            "/usr/sbin/spctl",
            &[
                "--assess",
                "--type",
                "execute",
                "-vv",
                &app.to_string_lossy(),
            ],
            Duration::from_secs(120),
        )?;
        if out.ok && out.text.contains("source=Notarized Developer ID") {
            Ok(())
        } else {
            Err(InstallError::untrusted(format!(
                "the new app is not notarized by Apple ({})",
                first_line(&out.text)
            )))
        }
    }

    /// Everything that must hold for an unpacked app before it may replace the running one.
    fn verify_app(&self, app: &Path, expect: &Expect) -> Result<(), InstallError> {
        let cli = app.join("Contents/MacOS/siple-cli");
        let exe = app.join("Contents/MacOS/siple");
        for f in [&exe, &cli] {
            let meta = std::fs::symlink_metadata(f)
                .map_err(|_| InstallError::untrusted("the new app is missing a program"))?;
            if !meta.is_file() {
                return Err(InstallError::untrusted(
                    "the new app has an unexpected layout",
                ));
            }
        }
        self.check_signature(app, APP_ID)?;
        // `--deep` does not apply the requirement to nested code.
        self.check_signature(&cli, CLI_ID)?;
        self.check_notarized(app)?;
        let id = self.plist(app, "CFBundleIdentifier");
        if id.as_deref() != Some(APP_ID) {
            return Err(InstallError::untrusted("the new app has another bundle id"));
        }
        let version = self.plist(app, "CFBundleShortVersionString");
        if version.as_deref() != Some(expect.version.core().as_str()) {
            return Err(InstallError::untrusted(format!(
                "the new app is version {}, not {}",
                version.as_deref().unwrap_or("unknown"),
                expect.version.core()
            )));
        }
        if let Some(min) = self.plist(app, "LSMinimumSystemVersion") {
            match OsVersion::parse(&min) {
                Some(m) if m <= self.os => {}
                _ => {
                    return Err(InstallError::new(
                        ErrorKind::Unsupported,
                        format!("the new version needs macOS {min} or later"),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no details")
        .trim()
        .to_string()
}

/// Looks at the entries of the archive before anything is unpacked: only `Siple.app/...`, no
/// absolute or `..` paths, no entry that would leave the bundle, symlinks only to places inside
/// it, bounded count and size.
pub fn check_archive(archive: &Path) -> Result<(), InstallError> {
    let bad = |m: &str| InstallError::untrusted(format!("the archive is not acceptable: {m}"));
    let f = std::fs::File::open(archive)
        .map_err(|e| InstallError::io("cannot read the archive", &e))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| bad(&e.to_string()))?;
    if z.is_empty() || z.len() > MAX_ENTRIES {
        return Err(bad("unexpected number of entries"));
    }
    let mut total = 0u64;
    for i in 0..z.len() {
        let mut e = z.by_index(i).map_err(|e| bad(&e.to_string()))?;
        let name = std::str::from_utf8(e.name_raw())
            .map_err(|_| bad("an entry has a name that is not text"))?
            .to_string();
        let trimmed = name.trim_end_matches('/');
        if name.contains('\\') || name.contains('\0') || name.starts_with('/') {
            return Err(bad("an entry has an unusual name"));
        }
        let mut parts = trimmed.split('/');
        if parts.next() != Some(BUNDLE_NAME) {
            return Err(bad("an entry is outside Siple.app"));
        }
        if trimmed
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(bad("an entry has a path that is not plain"));
        }
        total = total.saturating_add(e.size());
        if total > MAX_UNPACKED {
            return Err(bad("it unpacks to too much"));
        }
        let mode = e.unix_mode().unwrap_or(0o100644);
        match mode & 0o170000 {
            0o120000 => {
                // A symlink: its target is the entry's content.
                let mut target = String::new();
                (&mut e)
                    .take(4096)
                    .read_to_string(&mut target)
                    .map_err(|_| bad("an unreadable link"))?;
                if !symlink_stays_inside(trimmed, &target) {
                    return Err(bad("a link leaves the app"));
                }
            }
            0o100000 | 0o040000 | 0 => {}
            _ => return Err(bad("an entry is not a file, folder or link")),
        }
    }
    Ok(())
}

/// A symlink at `path` (inside `Siple.app/`) to `target` that resolves inside `Siple.app/`.
fn symlink_stays_inside(path: &str, target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') || target.contains('\0') {
        return false;
    }
    let mut depth: i64 = path.split('/').count() as i64 - 1; // components below the app root, minus the link itself
    // depth counts directories between Siple.app and the link's parent, inclusive of the root = 0.
    depth -= 1;
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

impl Platform for MacPlatform {
    fn asset(&self) -> Option<AssetPlatform> {
        Some(AssetPlatform::MacosZip)
    }

    fn probe(&self) -> Result<Target, NotPossible> {
        fn np(reason: &'static str, detail: impl Into<String>) -> NotPossible {
            NotPossible::new(reason, detail)
        }
        let exe = &self.env.exe;
        // <bundle>/Contents/MacOS/siple
        let bundle = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .filter(|b| {
                b.extension().is_some_and(|e| e == "app")
                    && exe.file_name().is_some_and(|n| n == "siple")
                    && exe
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|n| n == "MacOS")
            })
            .ok_or_else(|| {
                np(
                    "not_bundle",
                    "this copy of Siple does not run from an app bundle",
                )
            })?
            .to_path_buf();
        if !self.release_build {
            return Err(np("unsigned_build", "this is not a release build of Siple"));
        }
        let path = bundle.to_string_lossy();
        if path.contains("/AppTranslocation/") {
            return Err(np(
                "translocated",
                "macOS is running Siple from a quarantined location; move it to Applications first",
            ));
        }
        if self.env.fs.read_only_volume(&bundle) {
            return Err(np(
                "read_only_volume",
                "Siple runs from a disk image or a read-only volume; copy it to Applications first",
            ));
        }
        for caskroom in ["/opt/homebrew/Caskroom/siple", "/usr/local/Caskroom/siple"] {
            if self.env.fs.exists(Path::new(caskroom)) {
                return Err(np(
                    "package_manager",
                    "Siple is managed by Homebrew; use `brew upgrade --cask siple`",
                ));
            }
        }
        if self.plist(&bundle, "CFBundleIdentifier").as_deref() != Some(APP_ID) {
            return Err(np("not_bundle", "this app is not Siple's own bundle"));
        }
        let parent = bundle.parent().unwrap_or(Path::new("/"));
        if !self.env.fs.can_create_in(parent) {
            return Err(np(
                "not_writable",
                format!("Siple cannot replace itself in {}", parent.display()),
            ));
        }
        if self.env.uid.is_some() && self.env.fs.owner(&bundle) != self.env.uid {
            return Err(np("not_owner", "the app belongs to another user"));
        }
        Ok(Target {
            root: bundle,
            exe: exe.clone(),
        })
    }

    fn stage_dir(&self, target: &Target) -> Result<PathBuf, InstallError> {
        replacement_directory(&target.root)
    }

    fn stage(
        &self,
        archive: &Path,
        stage_dir: &Path,
        target: &Target,
        expect: &Expect,
    ) -> Result<Staged, InstallError> {
        // The running app is the reference for what "ours" means: a copy that does not pass the
        // requirement itself must not install anything.
        self.check_signature(&target.root, APP_ID).map_err(|e| {
            InstallError::new(
                ErrorKind::Unsupported,
                format!("this copy of Siple is not a release build ({e})"),
            )
        })?;
        check_archive(archive)?;
        let new = stage_dir.join("new");
        std::fs::create_dir(&new)
            .map_err(|e| InstallError::io("cannot create the staging folder", &e))?;
        let out = self.tool(
            "/usr/bin/ditto",
            &[
                "-x",
                "-k",
                &archive.to_string_lossy(),
                &new.to_string_lossy(),
            ],
            Duration::from_secs(300),
        )?;
        if !out.ok {
            return Err(InstallError::untrusted(format!(
                "the archive cannot be unpacked ({})",
                first_line(&out.text)
            )));
        }
        let mut entries = std::fs::read_dir(&new)
            .map_err(|e| InstallError::io("cannot read the staging folder", &e))?
            .filter_map(Result::ok);
        let (first, second) = (entries.next(), entries.next());
        let app = match (first, second) {
            (Some(e), None)
                if e.file_name() == BUNDLE_NAME
                    && e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink()) =>
            {
                e.path()
            }
            _ => {
                return Err(InstallError::untrusted(
                    "the archive does not hold exactly one Siple.app",
                ));
            }
        };
        self.verify_app(&app, expect)?;
        // It should not be quarantined (our own process wrote it); remove the mark if it is.
        let _ = self.run.run(
            "/usr/bin/xattr",
            &["-dr", "com.apple.quarantine", &app.to_string_lossy()],
            Duration::from_secs(60),
        );
        Ok(Staged {
            dir: stage_dir.to_path_buf(),
            payload: app,
        })
    }

    fn commit(&self, staged: &Staged, target: &Target) -> Result<Committed, InstallError> {
        swap_bundles(&staged.payload, &target.root)?;
        // After the swap the old bundle is at the staged path, the new one at the target.
        Ok(Committed {
            relaunch: RelaunchSpec::MacBundle(target.root.clone()),
            cleanup: vec![staged.dir.clone()],
        })
    }

    fn recover(&self) {}

    fn cleanup(&self, record: &InstalledUpdate) {
        for p in &record.cleanup {
            let p = Path::new(p);
            if !is_staging_path(p) || !p.is_dir() {
                continue;
            }
            // Only a folder whose apps are ours: the old bundle sits in `new/`.
            let apps = apps_in(p);
            let ours = !apps.is_empty()
                && apps
                    .iter()
                    .all(|a| self.plist(a, "CFBundleIdentifier").as_deref() == Some(APP_ID));
            if ours {
                let _ = std::fs::remove_dir_all(p);
            } else {
                tracing::warn!(path = %p.display(), "not removing a folder with foreign content");
            }
        }
    }
}

/// `.app` directories directly in `dir` or in its subfolders one level down.
fn apps_in(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "app") {
            out.push(p);
        } else if e.file_type().is_ok_and(|t| t.is_dir()) {
            out.extend(
                std::fs::read_dir(&p)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|x| x.path())
                    .filter(|x| x.extension().is_some_and(|e| e == "app")),
            );
        }
    }
    out
}

/// Exchanges two directories atomically (`renamex_np` with `RENAME_SWAP`). Fails, changing
/// nothing, where the file system cannot (never a non-atomic double rename).
pub fn swap_bundles(new: &Path, current: &Path) -> Result<(), InstallError> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| InstallError::new(ErrorKind::Io, "a path contains a NUL byte"))
    };
    let (a, b) = (c(new)?, c(current)?);
    // SAFETY: both are valid NUL-terminated paths for the duration of the call.
    let r = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    if r == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    let why = match e.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => "this volume cannot swap the app in place".to_string(),
        _ => e.to_string(),
    };
    Err(InstallError::new(
        ErrorKind::Io,
        format!("cannot replace the app: {why}"),
    ))
}

/// A new private folder on the same volume as `bundle`, from `NSFileManager`
/// (`NSItemReplacementDirectory`).
fn replacement_directory(bundle: &Path) -> Result<PathBuf, InstallError> {
    use objc2_foundation::{
        NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask, NSString, NSURL,
    };
    let fail = |m: String| {
        InstallError::new(
            ErrorKind::Io,
            format!("cannot create a staging folder: {m}"),
        )
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    let dir = NSFileManager::defaultManager()
        .URLForDirectory_inDomain_appropriateForURL_create_error(
            NSSearchPathDirectory::ItemReplacementDirectory,
            NSSearchPathDomainMask::UserDomainMask,
            Some(&url),
            true,
        )
        .map_err(|e| fail(e.localizedDescription().to_string()))?;
    let path = dir
        .path()
        .map(|p| PathBuf::from(p.to_string()))
        .ok_or_else(|| fail("no path".into()))?;
    if !is_staging_path(&path) {
        // Only folders that cleanup will accept are used.
        let _ = std::fs::remove_dir_all(&path);
        return Err(fail(format!("unexpected folder {}", path.display())));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::super::env::fake::FakeFs;
    use super::*;
    use std::sync::Mutex;

    type Handler = Box<dyn Fn(&str, &[String]) -> std::io::Result<Output> + Send + Sync>;

    /// Scripted tools; records every call.
    struct Fake {
        handler: Handler,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }
    impl CommandRunner for Fake {
        fn run(&self, p: &str, a: &[&str], _: Duration) -> std::io::Result<Output> {
            let args: Vec<String> = a.iter().map(|s| (*s).to_string()).collect();
            self.calls
                .lock()
                .unwrap()
                .push((p.to_string(), args.clone()));
            (self.handler)(p, &args)
        }
    }
    fn ok(t: &str) -> std::io::Result<Output> {
        Ok(Output {
            ok: true,
            text: t.into(),
        })
    }
    fn fail(t: &str) -> std::io::Result<Output> {
        Ok(Output {
            ok: false,
            text: t.into(),
        })
    }

    const BUNDLE: &str = "/Applications/Siple.app";
    const EXE: &str = "/Applications/Siple.app/Contents/MacOS/siple";

    fn fs() -> FakeFs {
        FakeFs::default()
            .dir(BUNDLE)
            .file(EXE, "")
            .writable("/Applications")
            .owner(BUNDLE, 501)
    }

    /// A tool script that accepts everything and reports `id` / `version` from plutil.
    fn tools(id: &'static str, version: &'static str, min: &'static str) -> Handler {
        Box::new(move |p, a| match p {
            "/usr/bin/plutil" => match a[1].as_str() {
                "CFBundleIdentifier" => ok(id),
                "CFBundleShortVersionString" => ok(version),
                "LSMinimumSystemVersion" => ok(min),
                _ => fail("no such key"),
            },
            "/usr/bin/codesign" => ok(""),
            "/usr/sbin/spctl" => ok("accepted\nsource=Notarized Developer ID\n"),
            _ => ok(""),
        })
    }

    fn platform(fs: FakeFs, h: Handler, release: bool) -> (MacPlatform, Arc<Fake>) {
        let fake = Arc::new(Fake {
            handler: h,
            calls: Mutex::default(),
        });
        let env = fs.env(EXE, Some(501), &[]);
        (
            MacPlatform::with(env, fake.clone(), OsVersion(14, 0, 0), release),
            fake,
        )
    }

    fn expect(v: &str) -> Expect {
        Expect {
            version: Version::parse(v).unwrap(),
        }
    }

    #[test]
    fn the_requirement_names_identifier_chain_and_team() {
        assert_eq!(
            requirement("io.zyno.siple"),
            "identifier \"io.zyno.siple\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = VUDU4KEQ99"
        );
    }

    #[test]
    fn a_normal_install_in_applications_is_possible() {
        let (p, _) = platform(fs(), tools(APP_ID, "0.1.0", "12.0"), true);
        let t = p.probe().unwrap();
        assert_eq!(t.root, Path::new(BUNDLE));
    }

    #[test]
    fn the_probe_refuses_what_cannot_be_replaced() {
        let cases: Vec<(&str, FakeFs, &str, bool)> = vec![
            ("dev build", fs(), "unsigned_build", false),
            ("translocated", FakeFs::default().dir("/x").file("/private/var/folders/AppTranslocation/ABC/d/Siple.app/Contents/MacOS/siple", "").writable("/private/var/folders/AppTranslocation/ABC/d").owner("/private/var/folders/AppTranslocation/ABC/d/Siple.app", 501), "translocated", true),
            ("disk image", fs().read_only("/Applications"), "read_only_volume", true),
            ("brew arm", fs().dir("/opt/homebrew/Caskroom/siple"), "package_manager", true),
            ("brew intel", fs().dir("/usr/local/Caskroom/siple"), "package_manager", true),
            ("not writable", FakeFs::default().dir(BUNDLE).file(EXE, "").owner(BUNDLE, 501), "not_writable", true),
            ("other owner", fs().owner(BUNDLE, 0), "not_owner", true),
        ];
        for (name, f, reason, release) in cases {
            let exe = if name == "translocated" {
                "/private/var/folders/AppTranslocation/ABC/d/Siple.app/Contents/MacOS/siple"
            } else {
                EXE
            };
            let fake = Arc::new(Fake {
                handler: tools(APP_ID, "0.1.0", "12.0"),
                calls: Mutex::default(),
            });
            let p = MacPlatform::with(
                f.env(exe, Some(501), &[]),
                fake,
                OsVersion(14, 0, 0),
                release,
            );
            assert_eq!(p.probe().unwrap_err().reason, reason, "{name}");
        }
    }

    #[test]
    fn the_probe_wants_our_bundle_layout_and_id() {
        for exe in [
            "/usr/local/bin/siple",
            "/tmp/target/debug/siple",
            "/Applications/Siple.app/Contents/MacOS/other",
            "/Applications/Siple/Contents/MacOS/siple",
        ] {
            let fake = Arc::new(Fake {
                handler: tools(APP_ID, "0.1.0", "12.0"),
                calls: Mutex::default(),
            });
            let p = MacPlatform::with(
                fs().env(exe, Some(501), &[]),
                fake,
                OsVersion(14, 0, 0),
                true,
            );
            assert_eq!(p.probe().unwrap_err().reason, "not_bundle", "{exe}");
        }
        let (p, _) = platform(fs(), tools("com.evil.app", "0.1.0", "12.0"), true);
        assert_eq!(p.probe().unwrap_err().reason, "not_bundle");
    }

    /// `ditto` that lays out an app with the given top-level names.
    fn ditto_layout(names: &'static [&'static str], h: Handler) -> Handler {
        Box::new(move |p, a| {
            if p == "/usr/bin/ditto" {
                let dest = Path::new(&a[3]);
                for n in names {
                    let app = dest.join(n);
                    std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
                    std::fs::write(app.join("Contents/MacOS/siple"), b"x").unwrap();
                    std::fs::write(app.join("Contents/MacOS/siple-cli"), b"x").unwrap();
                }
                return ok("");
            }
            h(p, a)
        })
    }

    fn zip_with(entries: &[(&str, &[u8], Option<u32>)]) -> (tempfile::TempDir, PathBuf) {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("a.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (name, data, mode) in entries {
            let mut o = zip::write::SimpleFileOptions::default();
            if let Some(m) = mode {
                o = o.unix_permissions(*m);
            }
            if *mode == Some(0o120777) || *mode == Some(0o120755) {
                z.add_symlink(*name, std::str::from_utf8(data).unwrap(), o)
                    .unwrap();
            } else {
                z.start_file(*name, o).unwrap();
                z.write_all(data).unwrap();
            }
        }
        z.finish().unwrap();
        (d, path)
    }

    fn good_zip() -> (tempfile::TempDir, PathBuf) {
        zip_with(&[
            ("Siple.app/Contents/Info.plist", b"x", None),
            ("Siple.app/Contents/MacOS/siple", b"x", Some(0o100755)),
        ])
    }

    fn stage_in(
        p: &MacPlatform,
        archive: &Path,
        v: &str,
    ) -> (tempfile::TempDir, Result<Staged, InstallError>) {
        let d = tempfile::tempdir().unwrap();
        let t = Target {
            root: BUNDLE.into(),
            exe: EXE.into(),
        };
        let r = p.stage(archive, d.path(), &t, &expect(v));
        (d, r)
    }

    #[test]
    fn staging_runs_every_check_in_order_and_names_the_requirements() {
        let (p, fake) = platform(
            fs(),
            ditto_layout(&["Siple.app"], tools(APP_ID, "0.2.0", "12.0")),
            true,
        );
        let (_z, zip) = good_zip();
        let (_d, r) = stage_in(&p, &zip, "0.2.0-rc.1");
        let staged = r.unwrap();
        assert!(staged.payload.ends_with("new/Siple.app"));
        let calls = fake.calls.lock().unwrap();
        let cs: Vec<_> = calls
            .iter()
            .filter(|(p, _)| p.ends_with("codesign"))
            .collect();
        // The running app, the new app, the new CLI.
        assert_eq!(cs.len(), 3);
        assert!(cs[0].1.contains(&format!("-R={}", requirement(APP_ID))));
        assert_eq!(cs[0].1.last().unwrap(), BUNDLE);
        assert!(cs[1].1.contains(&format!("-R={}", requirement(APP_ID))));
        assert!(cs[2].1.contains(&format!("-R={}", requirement(CLI_ID))));
        assert!(
            cs[2]
                .1
                .last()
                .unwrap()
                .ends_with("Contents/MacOS/siple-cli")
        );
        for c in &cs {
            assert!(
                c.1.iter()
                    .take(3)
                    .eq(["--verify", "--deep", "--strict"].iter())
            );
        }
        let names: Vec<_> = calls
            .iter()
            .map(|(p, _)| p.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(names.first().unwrap(), "codesign", "self-check first");
        assert!(names.contains(&"spctl".to_string()) && names.last().unwrap() == "xattr");
        // The version check is of the numeric core.
        assert!(
            calls
                .iter()
                .any(|(_, a)| a.get(1).is_some_and(|k| k == "CFBundleShortVersionString"))
        );
    }

    #[test]
    fn each_failed_check_stops_the_install() {
        let new_app_fail: Vec<(&str, Handler)> = vec![
            (
                "signature of the app",
                Box::new(|p, a| {
                    if p.ends_with("codesign") && a.last().unwrap().ends_with("new/Siple.app") {
                        fail("a sealed resource is missing")
                    } else {
                        tools(APP_ID, "0.2.0", "12.0")(p, a)
                    }
                }),
            ),
            (
                "signature of the cli",
                Box::new(|p, a| {
                    if p.ends_with("codesign") && a.last().unwrap().ends_with("siple-cli") {
                        fail("failed to satisfy")
                    } else {
                        tools(APP_ID, "0.2.0", "12.0")(p, a)
                    }
                }),
            ),
            (
                "not notarized",
                Box::new(|p, a| {
                    if p.ends_with("spctl") {
                        ok("accepted\nsource=Developer ID\n")
                    } else {
                        tools(APP_ID, "0.2.0", "12.0")(p, a)
                    }
                }),
            ),
            (
                "rejected by gatekeeper",
                Box::new(|p, a| {
                    if p.ends_with("spctl") {
                        fail("rejected")
                    } else {
                        tools(APP_ID, "0.2.0", "12.0")(p, a)
                    }
                }),
            ),
            ("wrong bundle id", tools("com.evil.app", "0.2.0", "12.0")),
            ("wrong version", tools(APP_ID, "0.1.9", "12.0")),
            ("needs a newer macOS", tools(APP_ID, "0.2.0", "99.0")),
            ("garbage min os", tools(APP_ID, "0.2.0", "abc")),
            (
                "ditto fails",
                Box::new(|p, a| {
                    if p.ends_with("ditto") {
                        fail("bad zip")
                    } else {
                        tools(APP_ID, "0.2.0", "12.0")(p, a)
                    }
                }),
            ),
        ];
        for (name, h) in new_app_fail {
            let (p, _) = platform(fs(), ditto_layout(&["Siple.app"], h), true);
            let (_z, zip) = good_zip();
            // `ditto` of the wrong-case above fails itself: ditto_layout runs first, so wrap.
            let (_d, r) = if name == "ditto fails" {
                let (p2, _) = platform(
                    fs(),
                    Box::new(|p, a| {
                        if p.ends_with("ditto") {
                            fail("bad zip")
                        } else {
                            tools(APP_ID, "0.2.0", "12.0")(p, a)
                        }
                    }),
                    true,
                );
                stage_in(&p2, &zip, "0.2.0")
            } else {
                stage_in(&p, &zip, "0.2.0")
            };
            assert!(r.is_err(), "{name} must fail");
        }
    }

    #[test]
    fn the_running_app_must_pass_the_requirement_too() {
        let h: Handler = Box::new(|p, a| {
            if p.ends_with("codesign") && a.last().unwrap() == BUNDLE {
                fail("not signed")
            } else {
                tools(APP_ID, "0.2.0", "12.0")(p, a)
            }
        });
        let (p, fake) = platform(fs(), ditto_layout(&["Siple.app"], h), true);
        let (_z, zip) = good_zip();
        let (_d, r) = stage_in(&p, &zip, "0.2.0");
        assert_eq!(r.unwrap_err().kind, ErrorKind::Unsupported);
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _)| p.ends_with("ditto")),
            "nothing unpacked"
        );
    }

    #[test]
    fn the_unpacked_layout_must_be_exactly_one_siple_app() {
        for names in [&["Other.app"][..], &["Siple.app", "Extra.app"], &[]] {
            let names: &'static [&'static str] = Box::leak(names.to_vec().into_boxed_slice());
            let (p, _) = platform(
                fs(),
                ditto_layout(names, tools(APP_ID, "0.2.0", "12.0")),
                true,
            );
            let (_z, zip) = good_zip();
            let (_d, r) = stage_in(&p, &zip, "0.2.0");
            assert!(r.is_err(), "{names:?}");
        }
    }

    #[test]
    fn archives_with_odd_entries_are_refused_before_unpacking() {
        type Entry<'a> = (&'a str, &'a [u8], Option<u32>);
        let cases: Vec<(&str, Vec<Entry>)> = vec![
            ("outside the app", vec![("evil", b"x", None)]),
            ("another app", vec![("Other.app/x", b"x", None)]),
            ("traversal", vec![("Siple.app/../../evil", b"x", None)]),
            ("absolute", vec![("/Siple.app/x", b"x", None)]),
            ("backslash", vec![("Siple.app\\x", b"x", None)]),
            ("dot component", vec![("Siple.app/./x", b"x", None)]),
            ("empty component", vec![("Siple.app//x", b"x", None)]),
            (
                "symlink out",
                vec![("Siple.app/l", b"../../etc", Some(0o120777))],
            ),
            (
                "symlink absolute",
                vec![("Siple.app/l", b"/etc/passwd", Some(0o120777))],
            ),
            (
                "symlink up from the root",
                vec![("Siple.app/l", b"..", Some(0o120777))],
            ),
        ];
        for (name, entries) in cases {
            let (_z, zip) = zip_with(&entries);
            assert!(check_archive(&zip).is_err(), "{name}");
        }
        // Fine: files, nested folders and links that stay inside.
        let (_z, zip) = zip_with(&[
            ("Siple.app/Contents/MacOS/siple", b"x", Some(0o100755)),
            (
                "Siple.app/Contents/Frameworks/a.framework/Versions/Current",
                b"A",
                Some(0o120755),
            ),
            (
                "Siple.app/Contents/Frameworks/a.framework/Up",
                b"../../MacOS/siple",
                Some(0o120755),
            ),
        ]);
        check_archive(&zip).unwrap();
        // Not a zip at all.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"not a zip").unwrap();
        assert!(check_archive(&d.path().join("x")).is_err());
    }

    #[test]
    fn a_refused_archive_is_never_handed_to_ditto() {
        let (p, fake) = platform(fs(), tools(APP_ID, "0.2.0", "12.0"), true);
        let (_z, zip) = zip_with(&[("evil", b"x", None)]);
        let (_d, r) = stage_in(&p, &zip, "0.2.0");
        assert!(r.is_err());
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _)| p.ends_with("ditto"))
        );
    }

    #[test]
    fn symlink_resolution_counts_depth() {
        assert!(symlink_stays_inside("Siple.app/a/l", "b"));
        assert!(symlink_stays_inside("Siple.app/a/l", "../b"));
        assert!(!symlink_stays_inside("Siple.app/a/l", "../../b"));
        assert!(symlink_stays_inside("Siple.app/a/b/l", "../../c"));
        assert!(!symlink_stays_inside("Siple.app/l", "../x"));
        assert!(!symlink_stays_inside("Siple.app/l", ""));
    }

    #[test]
    fn the_swap_exchanges_two_folders_atomically_and_the_old_one_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let (cur, new) = (
            d.path().join("Siple.app"),
            d.path().join("stage/new/Siple.app"),
        );
        std::fs::create_dir_all(&cur).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(cur.join("v"), "old").unwrap();
        std::fs::write(new.join("v"), "new").unwrap();
        swap_bundles(&new, &cur).unwrap();
        assert_eq!(std::fs::read_to_string(cur.join("v")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(new.join("v")).unwrap(), "old");
        // A missing source changes nothing.
        let e = swap_bundles(&d.path().join("missing"), &cur).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Io);
        assert_eq!(std::fs::read_to_string(cur.join("v")).unwrap(), "new");
    }

    #[test]
    fn the_replacement_directory_is_on_the_bundles_volume_and_acceptable_to_cleanup() {
        let d = tempfile::tempdir().unwrap();
        let bundle = d.path().join("Siple.app");
        std::fs::create_dir(&bundle).unwrap();
        let dir = replacement_directory(&bundle).unwrap();
        assert!(dir.is_dir() && is_staging_path(&dir), "{dir:?}");
        // Same device as the bundle's parent: a swap is possible.
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().dev(),
            std::fs::metadata(d.path()).unwrap().dev()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_removes_only_our_old_bundle_in_a_staging_folder() {
        let d = tempfile::tempdir().unwrap();
        let mk = |name: &str, with_app: bool| {
            let dir = d.path().join(name);
            if with_app {
                std::fs::create_dir_all(dir.join("new/Siple.app/Contents")).unwrap();
            } else {
                std::fs::create_dir_all(&dir).unwrap();
            }
            std::fs::write(dir.join("f"), "x").unwrap();
            dir
        };
        let ours = mk("NSIRD_x", true);
        let foreign = mk("NSIRD_y", true);
        let not_staging = mk("plain", true);
        let h: Handler = Box::new(move |p, a| {
            if p.ends_with("plutil") {
                if a[5].contains("NSIRD_y") {
                    ok("com.evil.app")
                } else {
                    ok(APP_ID)
                }
            } else {
                ok("")
            }
        });
        let (p, _) = platform(fs(), h, true);
        let rec = InstalledUpdate {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            at: chrono::Utc::now(),
            cleanup: [
                &ours,
                &foreign,
                &not_staging,
                &PathBuf::from("/Applications/Siple.app"),
                &PathBuf::from("relative/NSIRD_z"),
            ]
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        };
        p.cleanup(&rec);
        assert!(!ours.exists(), "our own old bundle is removed");
        assert!(foreign.exists(), "a folder with a foreign app stays");
        assert!(
            not_staging.exists(),
            "outside a staging area nothing is removed"
        );
    }

    /// Real tools: an ad-hoc signed bundle never satisfies the requirement.
    #[test]
    fn an_ad_hoc_signature_does_not_pass_the_real_codesign_check() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path().join("Siple.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::copy("/usr/bin/true", app.join("Contents/MacOS/siple")).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>io.zyno.siple</string><key>CFBundleExecutable</key><string>siple</string></dict></plist>"#,
        )
        .unwrap();
        let signed = std::process::Command::new("/usr/bin/codesign")
            .args(["-s", "-", "--force"])
            .arg(&app)
            .output()
            .unwrap();
        assert!(
            signed.status.success(),
            "{}",
            String::from_utf8_lossy(&signed.stderr)
        );
        let p = MacPlatform::with(
            fs().env(EXE, Some(501), &[]),
            Arc::new(SystemRunner),
            OsVersion(14, 0, 0),
            true,
        );
        let e = p.check_signature(&app, APP_ID).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Untrusted, "{e}");
        // The real plist reader works.
        assert_eq!(p.plist(&app, "CFBundleIdentifier").as_deref(), Some(APP_ID));
        assert_eq!(p.plist(&app, "CFBundleShortVersionString"), None);
        // And an unsigned bundle fails as well.
        let e = p.check_notarized(&app).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Untrusted);
    }

    /// The real v0.1.1 release app (path in `SIPLE_TEST_SIGNED_APP`) passes the pins; skipped
    /// when unset.
    #[test]
    fn the_real_release_app_passes_the_pins() {
        let Ok(app) = std::env::var("SIPLE_TEST_SIGNED_APP") else {
            eprintln!("SIPLE_TEST_SIGNED_APP not set; skipped");
            return;
        };
        let app = PathBuf::from(app);
        let p = MacPlatform::with(
            fs().env(EXE, Some(501), &[]),
            Arc::new(SystemRunner),
            OsVersion(14, 0, 0),
            true,
        );
        p.check_signature(&app, APP_ID).unwrap();
        p.check_signature(&app.join("Contents/MacOS/siple-cli"), CLI_ID)
            .unwrap();
        // The CLI does not satisfy the app's identifier, nor the app the CLI's.
        assert!(
            p.check_signature(&app.join("Contents/MacOS/siple-cli"), APP_ID)
                .is_err()
        );
        assert!(p.check_signature(&app, CLI_ID).is_err());
        p.check_notarized(&app).unwrap();
        p.verify_app(&app, &expect("0.1.1")).unwrap();
        assert!(p.verify_app(&app, &expect("0.1.2")).is_err());
    }
}
