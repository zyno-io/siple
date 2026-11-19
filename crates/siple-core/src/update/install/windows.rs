//! Windows install: in-place update of the direct build (`siple.exe`, `siple-cli.exe` and the
//! other files of the release zip, next to each other in one directory).
//!
//! Trust model: the archive was already verified against the signed checksum list. On top of
//! that every `.exe` in it must carry an Authenticode signature of our publisher: a valid chain
//! (checked by `WinVerifyTrust`, revocation included) whose leaf names the publisher, carries the
//! identity-validation and code-signing EKUs, chains up to the pinned root, and has an RFC 3161
//! timestamp (the leaf certificates live only a few days). The running `siple.exe` must pass the
//! same pins, otherwise (development builds, unsigned copies) the install is not offered.
//!
//! The swap renames each running image aside and moves the new file in (a running image can be
//! renamed on Windows, not overwritten), journaled by [`super::journal`]. Nothing outside the
//! files of the archive and the `.siple-update` directory is touched; nothing is elevated.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::env::Env;
use super::journal::{self, JOURNAL_NAME, StdOps};
use super::*;

/// The staging area, next to the executables.
const STATE_DIR: &str = ".siple-update";
/// The unpacked archive inside a staging directory.
const PAYLOAD_DIR: &str = "new";
/// The program file.
const EXE_NAME: &str = "siple.exe";
/// The command-line tool.
const CLI_NAME: &str = "siple-cli.exe";

/// Leaf subject: common name and organization.
pub const PIN_SUBJECT: &str = "Signal24 LLC";
/// Azure Artifact Signing identity-validation EKU of our account.
pub const PIN_EKU_IDENTITY: &str = "1.3.6.1.4.1.311.97.924040550.696811518.865259399.405993542";
/// Code signing EKU.
pub const EKU_CODE_SIGNING: &str = "1.3.6.1.5.5.7.3.3";
/// SHA-1 thumbprint of the chain's root: Microsoft Identity Verification Root Certificate
/// Authority 2020. Neither the leaf (reissued every few days) nor the issuing CA (rotates) is
/// pinned.
pub const PIN_ROOT_SHA1: &str = "F40042E2E5F7E8EF8189FED15519AECE42C3BFA2";

/// `Machine` of an x64 image.
pub const MACHINE_X64: u16 = 0x8664;
/// `Machine` of an arm64 image.
pub const MACHINE_ARM64: u16 = 0xAA64;

/// Staging directories older than this, without a journal, are removed at start.
const STALE_STAGE: Duration = Duration::from_secs(24 * 3600);
/// Backups younger than this are left to the installed-update cleanup (the previous version
/// stays available for a while after an update).
const BACKUP_GRACE: Duration = Duration::from_secs(600);

/// Files the archive may contain at the top level.
const TOP_FILES: &[&str] = &[
    EXE_NAME,
    CLI_NAME,
    "config.example.toml",
    "LICENSE",
    "README.md",
    "THIRD-PARTY-NOTICES.html",
];

// ---------------------------------------------------------------------------------------------
// Signature pins
// ---------------------------------------------------------------------------------------------

/// One certificate of a signer's chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainCert {
    /// Common name of the subject.
    pub subject_cn: String,
    /// SHA-1 thumbprint, hex.
    pub sha1: String,
}

/// What `WinVerifyTrust` found about the signer of a file that passed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerInfo {
    /// Leaf subject common name.
    pub subject_cn: String,
    /// Leaf subject organization.
    pub subject_o: String,
    /// Leaf enhanced key usages (dotted OIDs).
    pub ekus: Vec<String>,
    /// SHA-1 thumbprint (hex) of the last certificate of the chain, the root.
    pub root_thumbprint_sha1: String,
    /// An RFC 3161 timestamp countersignature is present.
    pub has_timestamp: bool,
    /// The chain, leaf first.
    pub chain: Vec<ChainCert>,
}

/// Applies the publisher pins to a signature that is otherwise valid.
pub fn check_pins(s: &SignerInfo) -> Result<(), String> {
    if s.subject_cn != PIN_SUBJECT {
        return Err(format!(
            "the signer is \"{}\", not our publisher",
            s.subject_cn
        ));
    }
    if s.subject_o != PIN_SUBJECT {
        return Err(format!(
            "the signer's organization is \"{}\", not our publisher",
            s.subject_o
        ));
    }
    for eku in [PIN_EKU_IDENTITY, EKU_CODE_SIGNING] {
        if !s.ekus.iter().any(|e| e == eku) {
            return Err(format!("the signing certificate lacks the usage {eku}"));
        }
    }
    if !s.root_thumbprint_sha1.eq_ignore_ascii_case(PIN_ROOT_SHA1) {
        return Err("the certificate chain does not end at the expected root".into());
    }
    if s.chain.len() < 2 {
        return Err("the certificate chain is too short".into());
    }
    if !s.has_timestamp {
        return Err("the signature has no timestamp".into());
    }
    Ok(())
}

/// The OS-backed half of the checks (Authenticode and the version resource); fakes in tests.
pub trait Trust: Send + Sync {
    /// The file is validly signed (chain, and revocation when `revocation`) and passes
    /// [`check_pins`].
    fn verify(&self, exe: &Path, revocation: bool) -> Result<(), String>;
    /// `FileVersion` of the file's version resource; `None` when it has none.
    fn file_version(&self, exe: &Path) -> Result<Option<[u16; 4]>, String>;
}

/// The real checker.
#[cfg(windows)]
pub use win::AuthenticodeTrust as DefaultTrust;

/// Authenticode exists only on Windows: nothing passes elsewhere.
#[cfg(not(windows))]
pub struct DefaultTrust;

#[cfg(not(windows))]
impl Trust for DefaultTrust {
    fn verify(&self, _: &Path, _: bool) -> Result<(), String> {
        Err("Authenticode is only available on Windows".into())
    }
    fn file_version(&self, _: &Path) -> Result<Option<[u16; 4]>, String> {
        Err("version resources are only available on Windows".into())
    }
}

// ---------------------------------------------------------------------------------------------
// The archive
// ---------------------------------------------------------------------------------------------

/// What an archive may be.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Entries (files and directories).
    pub max_entries: usize,
    /// Uncompressed bytes in total.
    pub max_total: u64,
}

impl Limits {
    /// The limits of a release archive.
    pub const RELEASE: Limits = Limits {
        max_entries: 64,
        max_total: 500 * 1024 * 1024,
    };
}

fn untrusted(m: impl Into<String>) -> InstallError {
    InstallError::untrusted(m)
}

fn is_reserved_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or("");
    let up = stem.trim_end().to_ascii_uppercase();
    matches!(
        up.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((up.starts_with("COM") || up.starts_with("LPT"))
        && up.len() == 4
        && matches!(up.as_bytes()[3], b'1'..=b'9'))
}

/// Splits an entry name into safe path components: relative, `/`-separated, nothing a Windows
/// path could interpret (drive, stream, device, parent, trailing dot or space).
fn safe_components(name: &str) -> Result<Vec<&str>, String> {
    if name.is_empty() {
        return Err("empty name".into());
    }
    if name.starts_with('/') || name.contains('\\') {
        return Err(format!("{name:?}: absolute path or backslash"));
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    let mut out = Vec::new();
    for c in trimmed.split('/') {
        if c.is_empty() || c == "." || c == ".." {
            return Err(format!("{name:?}: empty, '.' or '..' component"));
        }
        if c.chars()
            .any(|ch| ch.is_control() || matches!(ch, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
        {
            return Err(format!("{name:?}: character not allowed in a file name"));
        }
        if c.ends_with('.') || c.ends_with(' ') {
            return Err(format!("{name:?}: trailing dot or space"));
        }
        if is_reserved_device(c) {
            return Err(format!("{name:?}: reserved device name"));
        }
        out.push(c);
    }
    Ok(out)
}

fn allowed_file(comps: &[&str]) -> bool {
    match comps {
        [one] => TOP_FILES.contains(one),
        // Documentation only: no program under docs/ (only the two exes are signature-checked).
        [first, rest @ ..] => {
            *first == "docs" && !rest.is_empty() && rest.last().is_some_and(|f| f.ends_with(".md"))
        }
        [] => false,
    }
}

fn allowed_dir(comps: &[&str]) -> bool {
    comps.first() == Some(&"docs")
}

struct PlannedEntry {
    index: usize,
    comps: Vec<String>,
    dir: bool,
    size: u64,
}

/// Validates every entry of the archive, then unpacks it into `dest` (which must not exist).
/// Returns the relative names of the files written.
pub fn extract_zip<R: Read + Seek>(
    reader: R,
    dest: &Path,
    limits: &Limits,
) -> Result<Vec<String>, InstallError> {
    let mut zip = zip::ZipArchive::new(reader)
        .map_err(|e| untrusted(format!("the release archive is not a valid zip file: {e}")))?;
    if zip.len() > limits.max_entries {
        return Err(untrusted(format!(
            "the release archive has too many entries ({})",
            zip.len()
        )));
    }
    let mut plan = Vec::new();
    let mut seen = BTreeSet::new();
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let f = zip
            .by_index(i)
            .map_err(|e| untrusted(format!("cannot read an archive entry: {e}")))?;
        let name = f
            .name()
            .map_err(|e| untrusted(format!("bad entry name: {e}")))?
            .into_owned();
        let comps = safe_components(&name).map_err(untrusted)?;
        let mode = f.unix_mode().map(|m| m & 0o170_000);
        if f.is_symlink() || !matches!(mode, None | Some(0 | 0o100_000 | 0o040_000)) {
            return Err(untrusted(format!(
                "{name:?}: not a regular file or directory"
            )));
        }
        let dir = f.is_dir();
        let ok = if dir {
            allowed_dir(&comps)
        } else {
            allowed_file(&comps)
        };
        if !ok {
            return Err(untrusted(format!(
                "{name:?}: unexpected entry in the release"
            )));
        }
        if !seen.insert(comps.join("/").to_ascii_lowercase()) {
            return Err(untrusted(format!("{name:?}: duplicate entry")));
        }
        let size = if dir { 0 } else { f.size() };
        total = total
            .checked_add(size)
            .filter(|t| *t <= limits.max_total)
            .ok_or_else(|| untrusted("the release archive unpacks to too much data"))?;
        plan.push(PlannedEntry {
            index: i,
            comps: comps.iter().map(|c| (*c).to_string()).collect(),
            dir,
            size,
        });
    }
    if !plan
        .iter()
        .any(|p| !p.dir && p.comps.len() == 1 && p.comps[0] == EXE_NAME)
    {
        return Err(untrusted(format!("the release has no {EXE_NAME}")));
    }

    std::fs::create_dir(dest)
        .map_err(|e| InstallError::io("cannot create the staging area", &e))?;
    let mut remaining = limits.max_total;
    let mut written = Vec::new();
    for p in &plan {
        let mut path = dest.to_path_buf();
        path.extend(&p.comps);
        if p.dir {
            std::fs::create_dir_all(&path)
                .map_err(|e| InstallError::io("cannot unpack the release", &e))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| InstallError::io("cannot unpack the release", &e))?;
        }
        let mut f = zip
            .by_index(p.index)
            .map_err(|e| untrusted(format!("cannot read an archive entry: {e}")))?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| InstallError::io("cannot unpack the release", &e))?;
        let n = std::io::copy(&mut f.by_ref().take(remaining.saturating_add(1)), &mut out)
            .map_err(|e| InstallError::io("cannot unpack the release", &e))?;
        if n > remaining {
            return Err(untrusted("the release archive unpacks to too much data"));
        }
        if n != p.size {
            return Err(untrusted(format!(
                "{}: unpacked size differs from the declared size",
                p.comps.join("/")
            )));
        }
        remaining -= n;
        written.push(p.comps.join("/"));
    }
    Ok(written)
}

/// The `Machine` field of a PE image.
pub fn pe_machine<R: Read + Seek>(r: &mut R) -> Result<u16, String> {
    let mut dos = [0u8; 64];
    r.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    r.read_exact(&mut dos)
        .map_err(|_| "too short for a program".to_string())?;
    if &dos[..2] != b"MZ" {
        return Err("not a Windows program (no MZ header)".into());
    }
    let off = u32::from_le_bytes([dos[0x3C], dos[0x3D], dos[0x3E], dos[0x3F]]);
    if !(64..=0x10_0000).contains(&off) {
        return Err("not a Windows program (bad PE offset)".into());
    }
    r.seek(SeekFrom::Start(u64::from(off)))
        .map_err(|e| e.to_string())?;
    let mut h = [0u8; 6];
    r.read_exact(&mut h)
        .map_err(|_| "not a Windows program (truncated PE header)".to_string())?;
    if &h[..4] != b"PE\0\0" {
        return Err("not a Windows program (no PE signature)".into());
    }
    Ok(u16::from_le_bytes([h[4], h[5]]))
}

/// The `Machine` this build runs on.
pub const fn native_machine() -> u16 {
    if cfg!(target_arch = "aarch64") {
        MACHINE_ARM64
    } else {
        MACHINE_X64
    }
}

fn parse_core(core: &str) -> Option<[u16; 3]> {
    let mut it = core.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    it.next().is_none().then_some([a, b, c])
}

// ---------------------------------------------------------------------------------------------
// The platform
// ---------------------------------------------------------------------------------------------

/// Direct Windows build.
pub struct WindowsPlatform {
    env: Env,
    trust: Arc<dyn Trust>,
    machine: u16,
    ops: StdOps,
    limits: Limits,
    self_check: OnceLock<Result<(), String>>,
}

impl WindowsPlatform {
    /// The real machine's checks.
    pub fn new(env: Env) -> Self {
        Self::with_trust(env, Arc::new(DefaultTrust))
    }

    /// With explicit signature and version checks (tests).
    pub fn with_trust(env: Env, trust: Arc<dyn Trust>) -> Self {
        Self {
            env,
            trust,
            machine: native_machine(),
            ops: StdOps::windows(),
            limits: Limits::RELEASE,
            self_check: OnceLock::new(),
        }
    }

    /// Another expected `Machine` (tests).
    #[must_use]
    pub fn with_machine(mut self, machine: u16) -> Self {
        self.machine = machine;
        self
    }

    /// Other swap retry settings (tests).
    #[must_use]
    pub fn with_ops(mut self, ops: StdOps) -> Self {
        self.ops = ops;
        self
    }

    fn exe_dir(&self) -> Option<&Path> {
        self.env.exe.parent()
    }
}

/// Lowercase, backslash-separated text of a path, for comparisons.
fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_ascii_lowercase()
}

fn same_path(a: &Path, b: &Path) -> bool {
    norm(a).trim_end_matches('\\') == norm(b).trim_end_matches('\\')
}

/// `<stem>.old-<digits>` where the stem is a file of the archive; returns the stem and the
/// milliseconds in the name.
fn backup_of_ours(name: &str, in_docs: bool) -> Option<(&str, u128)> {
    if !journal::is_backup_name(name) {
        return None;
    }
    let (stem, tail) = name.rsplit_once(".old-")?;
    let ok = if in_docs {
        stem.ends_with(".md")
    } else {
        TOP_FILES.contains(&stem)
    };
    let ms = tail.split('-').next()?.parse().ok()?;
    ok.then_some((stem, ms))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Files below `root`, relative, regular files only.
fn list_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    fn walk(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for e in std::fs::read_dir(base.join(rel))? {
            let e = e?;
            let r = rel.join(e.file_name());
            let t = e.file_type()?;
            if t.is_dir() {
                walk(base, &r, out)?;
            } else if t.is_file() {
                out.push(r);
            } else {
                return Err(std::io::Error::other("unexpected file type in the payload"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out)?;
    // The programs go last: if anything fails before them nothing important changed yet, and
    // the interrupted-swap recovery completes the rest.
    out.sort_by_key(|p| {
        let n = p.to_string_lossy().into_owned();
        (n == CLI_NAME, n == EXE_NAME, n)
    });
    Ok(out)
}

impl Platform for WindowsPlatform {
    fn asset(&self) -> Option<AssetPlatform> {
        Some(if self.machine == MACHINE_ARM64 {
            AssetPlatform::WindowsArm64
        } else {
            AssetPlatform::WindowsX64
        })
    }

    fn probe(&self) -> Result<Target, NotPossible> {
        let exe = &self.env.exe;
        let fs = &self.env.fs;
        let is_siple = exe
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(EXE_NAME));
        let Some(dir) = exe.parent().filter(|_| is_siple) else {
            return Err(NotPossible::new(
                "not_bundle",
                "the running program is not the Siple release layout",
            ));
        };
        let p = norm(exe);
        if [
            "\\winget\\packages\\",
            "\\scoop\\apps\\",
            "\\chocolatey\\lib\\",
        ]
        .iter()
        .any(|m| p.contains(m))
        {
            return Err(NotPossible::new(
                "package_manager",
                "Siple was installed by a package manager; update it with that",
            ));
        }
        let state = dir.join(STATE_DIR);
        let writable = if fs.is_dir(&state) {
            fs.can_create_in(&state)
        } else {
            fs.can_create_in(dir)
        };
        if !writable {
            return Err(NotPossible::new(
                "not_writable",
                format!("{} is not writable by this user", dir.display()),
            ));
        }
        let cli = dir.join(CLI_NAME);
        if !fs.can_delete(exe) || (fs.exists(&cli) && !fs.can_delete(&cli)) {
            return Err(NotPossible::new(
                "in_use",
                "the program files cannot be replaced right now",
            ));
        }
        if let Err(e) = self
            .self_check
            .get_or_init(|| self.trust.verify(exe, false))
        {
            return Err(NotPossible::new(
                "unsigned_build",
                format!("this copy of Siple is not a signed release ({e})"),
            ));
        }
        Ok(Target {
            root: dir.to_path_buf(),
            exe: exe.clone(),
        })
    }

    fn stage_dir(&self, target: &Target) -> Result<PathBuf, InstallError> {
        let state = target.root.join(STATE_DIR);
        std::fs::create_dir_all(&state)
            .map_err(|e| InstallError::io("cannot create the staging area", &e))?;
        for _ in 0..8 {
            let mut r = [0u8; 6];
            getrandom::fill(&mut r)
                .map_err(|e| InstallError::new(ErrorKind::Io, format!("no randomness: {e}")))?;
            let id = format!(
                "{:x}-{}",
                now_ms(),
                r.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            let dir = state.join(id);
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(InstallError::io("cannot create the staging directory", &e)),
            }
        }
        Err(InstallError::new(
            ErrorKind::Io,
            "cannot create a unique staging directory",
        ))
    }

    fn stage(
        &self,
        archive: &Path,
        stage_dir: &Path,
        _target: &Target,
        expect: &Expect,
    ) -> Result<Staged, InstallError> {
        let payload = stage_dir.join(PAYLOAD_DIR);
        let file = File::open(archive)
            .map_err(|e| InstallError::io("cannot open the release archive", &e))?;
        extract_zip(file, &payload, &self.limits)?;

        let want = parse_core(&expect.version.core())
            .ok_or_else(|| untrusted("the release version is not numeric"))?;
        for name in [EXE_NAME, CLI_NAME] {
            let path = payload.join(name);
            if !path.is_file() {
                tracing::warn!("the release has no {name}");
                continue;
            }
            let mut f = File::open(&path).map_err(|e| InstallError::io("cannot read", &e))?;
            let machine = pe_machine(&mut f).map_err(|e| untrusted(format!("{name}: {e}")))?;
            drop(f);
            if machine != self.machine {
                return Err(untrusted(format!(
                    "{name} is built for another processor (0x{machine:04X})"
                )));
            }
            self.trust
                .verify(&path, true)
                .map_err(|e| untrusted(format!("{name}: signature check failed: {e}")))?;
            let v = self
                .trust
                .file_version(&path)
                .map_err(|e| untrusted(format!("{name}: {e}")))?;
            let Some(v) = v else {
                // The command-line tool of early releases carries no version resource; its
                // signature and the archive checksum still hold.
                if name == CLI_NAME {
                    tracing::warn!("{name} has no version resource");
                    continue;
                }
                return Err(untrusted(format!("{name} has no version resource")));
            };
            if v[..3] != want || v[3] != 0 {
                return Err(untrusted(format!(
                    "{name} is version {}.{}.{}.{}, not {}",
                    v[0],
                    v[1],
                    v[2],
                    v[3],
                    expect.version.core()
                )));
            }
        }
        Ok(Staged {
            dir: stage_dir.to_path_buf(),
            payload,
        })
    }

    fn commit(&self, staged: &Staged, target: &Target) -> Result<Committed, InstallError> {
        if !is_staging_path(&staged.dir) || !staged.payload.starts_with(&staged.dir) {
            return Err(InstallError::new(
                ErrorKind::Unsupported,
                "the staged files are not in a staging area",
            ));
        }
        let rels = list_files(&staged.payload)
            .map_err(|e| InstallError::io("cannot list the staged files", &e))?;
        let files: Vec<(PathBuf, PathBuf)> = rels
            .iter()
            .map(|r| (staged.payload.join(r), target.root.join(r)))
            .collect();
        let backups = journal::swap(&files, &staged.dir.join(JOURNAL_NAME), &self.ops)
            .map_err(|e| InstallError::io("cannot replace the program files", &e))?;
        let mut cleanup = backups;
        cleanup.push(staged.dir.clone());
        Ok(Committed {
            relaunch: RelaunchSpec::Exe(target.exe.clone()),
            cleanup,
        })
    }

    fn recover(&self) {
        let Some(dir) = self.exe_dir() else { return };
        let state = dir.join(STATE_DIR);
        if let Ok(rd) = std::fs::read_dir(&state) {
            for e in rd.flatten() {
                let p = e.path();
                if !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let jp = p.join(JOURNAL_NAME);
                if jp.exists() {
                    let r = journal::recover(&jp, &self.ops);
                    tracing::info!(?r, dir = %p.display(), "recovered an interrupted update swap");
                } else if std::fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > STALE_STAGE)
                {
                    let _ = std::fs::remove_dir_all(&p);
                }
            }
            // Only succeeds when empty.
            let _ = std::fs::remove_dir(&state);
        }
        let pending = std::fs::read_dir(&state)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join(JOURNAL_NAME).exists());
        if pending {
            return; // an unfinished swap still needs its backups
        }
        for (d, in_docs) in [(dir.to_path_buf(), false), (dir.join("docs"), true)] {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name();
                let Some((_, ms)) = name.to_str().and_then(|n| backup_of_ours(n, in_docs)) else {
                    continue;
                };
                if now_ms().saturating_sub(ms) < BACKUP_GRACE.as_millis() {
                    continue;
                }
                if e.file_type().is_ok_and(|t| t.is_file()) {
                    // A locked file (still running) fails here and is retried at the next start.
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    fn cleanup(&self, record: &InstalledUpdate) {
        let Some(dir) = self.exe_dir() else { return };
        let docs = dir.join("docs");
        for s in &record.cleanup {
            let p = PathBuf::from(s);
            if is_staging_path(&p) {
                // Only our own staging area next to this exe.
                if !p.starts_with(dir.join(STATE_DIR)) {
                    continue;
                }
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(&p);
                } else {
                    let _ = std::fs::remove_file(&p);
                }
                continue;
            }
            let (Some(parent), Some(name)) = (p.parent(), p.file_name().and_then(|n| n.to_str()))
            else {
                continue;
            };
            if p.is_absolute()
                && !p.components().any(|c| matches!(c, Component::ParentDir))
                && ((same_path(parent, dir) && backup_of_ours(name, false).is_some())
                    || (same_path(parent, &docs) && backup_of_ours(name, true).is_some()))
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Authenticode and version resources (Windows only)
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::{ChainCert, SignerInfo, Trust, check_pins};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Security::Cryptography::{
        CERT_CONTEXT, CERT_NAME_ATTR_TYPE, CERT_SHA1_HASH_PROP_ID, CTL_USAGE,
        CertGetCertificateContextProperty, CertGetEnhancedKeyUsage, CertGetNameStringW,
    };
    use windows_sys::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO, WTD_CHOICE_FILE,
        WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE,
        WTD_REVOKE_WHOLECHAIN, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
        WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };

    /// `WinVerifyTrust` plus the signer pins; `GetFileVersionInfoW` for the version.
    pub struct AuthenticodeTrust;

    /// The path as the Win32 file APIs of these checks want it: no `\\?\` prefix (except UNC).
    fn wide(p: &Path) -> Vec<u16> {
        let s = p.to_string_lossy();
        let plain = match s.strip_prefix(r"\\?\") {
            Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
            _ => s.into_owned(),
        };
        std::ffi::OsStr::new(&plain)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    unsafe fn name_attr(cert: *const CERT_CONTEXT, oid: &[u8]) -> String {
        // SAFETY: `cert` is a live certificate context; `oid` is NUL-terminated ASCII.
        unsafe {
            let n = CertGetNameStringW(
                cert,
                CERT_NAME_ATTR_TYPE,
                0,
                oid.as_ptr().cast(),
                null_mut(),
                0,
            );
            if n <= 1 {
                return String::new();
            }
            let mut buf = vec![0u16; n as usize];
            let n = CertGetNameStringW(
                cert,
                CERT_NAME_ATTR_TYPE,
                0,
                oid.as_ptr().cast(),
                buf.as_mut_ptr(),
                n,
            );
            String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)])
        }
    }

    unsafe fn sha1(cert: *const CERT_CONTEXT) -> String {
        let mut buf = [0u8; 20];
        let mut len = 20u32;
        // SAFETY: `buf` holds `len` bytes.
        let ok = unsafe {
            CertGetCertificateContextProperty(
                cert,
                CERT_SHA1_HASH_PROP_ID,
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if ok == 0 {
            String::new()
        } else {
            hex(&buf[..len as usize])
        }
    }

    unsafe fn ekus(cert: *const CERT_CONTEXT) -> Vec<String> {
        // SAFETY: the buffer is sized by the first call and aligned for CTL_USAGE.
        unsafe {
            let mut size = 0u32;
            if CertGetEnhancedKeyUsage(cert, 0, null_mut(), &mut size) == 0 || size == 0 {
                return Vec::new();
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let usage = buf.as_mut_ptr().cast::<CTL_USAGE>();
            if CertGetEnhancedKeyUsage(cert, 0, usage, &mut size) == 0 {
                return Vec::new();
            }
            let n = (*usage).cUsageIdentifier as usize;
            (0..n)
                .filter_map(|i| {
                    let p = *(*usage).rgpszUsageIdentifier.add(i);
                    (!p.is_null()).then(|| {
                        std::ffi::CStr::from_ptr(p.cast())
                            .to_string_lossy()
                            .into_owned()
                    })
                })
                .collect()
        }
    }

    /// Runs `WinVerifyTrust` and reads the signer of the file when it passes.
    pub fn read_signer(path: &Path, revocation: bool) -> Result<SignerInfo, String> {
        let wpath = wide(path);
        let mut file = WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: wpath.as_ptr(),
            hFile: null_mut(),
            pgKnownSubject: null_mut(),
        };
        // SAFETY: WINTRUST_DATA is plain data; zero is its documented default.
        let mut data: WINTRUST_DATA = unsafe { std::mem::zeroed() };
        data.cbStruct = size_of::<WINTRUST_DATA>() as u32;
        data.dwUIChoice = WTD_UI_NONE;
        data.fdwRevocationChecks = if revocation {
            WTD_REVOKE_WHOLECHAIN
        } else {
            WTD_REVOKE_NONE
        };
        data.dwUnionChoice = WTD_CHOICE_FILE;
        data.Anonymous.pFile = &mut file;
        data.dwStateAction = WTD_STATEACTION_VERIFY;
        data.dwProvFlags = if revocation {
            WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT
        } else {
            WTD_REVOCATION_CHECK_NONE
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let hwnd = -1isize as HWND;
        // SAFETY: `data` and `file` outlive both calls; the state handle is closed below.
        let rc = unsafe {
            WinVerifyTrust(
                hwnd,
                &mut action,
                (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
            )
        };
        let result = if rc == 0 {
            // SAFETY: the verify call succeeded, so the state data is valid until CLOSE.
            unsafe { signer_of(data.hWVTStateData) }
        } else {
            Err(format!("the signature is not valid (0x{:08X})", rc as u32))
        };
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        // SAFETY: as above.
        unsafe {
            WinVerifyTrust(
                hwnd,
                &mut action,
                (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
            );
        }
        result
    }

    unsafe fn signer_of(
        state: windows_sys::Win32::Foundation::HANDLE,
    ) -> Result<SignerInfo, String> {
        // SAFETY: all pointers come from the trust provider and are valid until the state is
        // closed; the chain is read before that.
        unsafe {
            let pd = WTHelperProvDataFromStateData(state);
            if pd.is_null() {
                return Err("no provider data".into());
            }
            let sg = WTHelperGetProvSignerFromChain(pd, 0, 0, 0);
            if sg.is_null() {
                return Err("no signer".into());
            }
            let s = &*sg;
            if s.pasCertChain.is_null() || s.csCertChain == 0 {
                return Err("no certificate chain".into());
            }
            let certs = std::slice::from_raw_parts(s.pasCertChain, s.csCertChain as usize);
            let chain: Vec<ChainCert> = certs
                .iter()
                .filter(|c| !c.pCert.is_null())
                .map(|c| ChainCert {
                    subject_cn: name_attr(c.pCert, b"2.5.4.3\0"),
                    sha1: sha1(c.pCert),
                })
                .collect();
            let leaf = certs[0].pCert;
            if leaf.is_null() || chain.is_empty() {
                return Err("no signing certificate".into());
            }
            Ok(SignerInfo {
                subject_cn: name_attr(leaf, b"2.5.4.3\0"),
                subject_o: name_attr(leaf, b"2.5.4.10\0"),
                ekus: ekus(leaf),
                root_thumbprint_sha1: chain.last().map(|c| c.sha1.clone()).unwrap_or_default(),
                has_timestamp: s.csCounterSigners > 0,
                chain,
            })
        }
    }

    /// `FileVersion` of the fixed version info.
    pub fn file_version(path: &Path) -> Result<Option<[u16; 4]>, String> {
        let w = wide(path);
        let root: Vec<u16> = "\\".encode_utf16().chain(Some(0)).collect();
        // SAFETY: buffers are sized by the API's own answers.
        unsafe {
            let mut handle = 0u32;
            let size = GetFileVersionInfoSizeW(w.as_ptr(), &mut handle);
            if size == 0 {
                return Ok(None);
            }
            let mut buf = vec![0u8; size as usize];
            if GetFileVersionInfoW(w.as_ptr(), 0, size, buf.as_mut_ptr().cast()) == 0 {
                return Err("cannot read the version resource".into());
            }
            let mut p: *mut c_void = null_mut();
            let mut len = 0u32;
            if VerQueryValueW(buf.as_ptr().cast(), root.as_ptr(), &mut p, &mut len) == 0
                || p.is_null()
                || (len as usize) < size_of::<VS_FIXEDFILEINFO>()
            {
                return Err("no fixed version info".into());
            }
            let info = std::ptr::read_unaligned(p.cast::<VS_FIXEDFILEINFO>());
            if info.dwSignature != 0xFEEF_04BD {
                return Err("bad version info signature".into());
            }
            Ok(Some([
                (info.dwFileVersionMS >> 16) as u16,
                info.dwFileVersionMS as u16,
                (info.dwFileVersionLS >> 16) as u16,
                info.dwFileVersionLS as u16,
            ]))
        }
    }

    impl Trust for AuthenticodeTrust {
        fn verify(&self, exe: &Path, revocation: bool) -> Result<(), String> {
            check_pins(&read_signer(exe, revocation)?)
        }
        fn file_version(&self, exe: &Path) -> Result<Option<[u16; 4]>, String> {
            file_version(exe)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::env::fake::FakeFs;
    use super::super::journal::{Entry, Journal, hash_file};
    use super::*;
    use std::io::{Cursor, Write};

    // ---- helpers -------------------------------------------------------------------------

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, o).unwrap();
            } else {
                w.start_file(*name, o).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap().into_inner()
    }

    fn extract(
        bytes: Vec<u8>,
        limits: &Limits,
    ) -> (tempfile::TempDir, Result<Vec<String>, InstallError>) {
        let d = tempfile::tempdir().unwrap();
        let r = extract_zip(Cursor::new(bytes), &d.path().join("new"), limits);
        (d, r)
    }

    fn rejected(entries: &[(&str, &[u8])]) -> String {
        let (d, r) = extract(zip_of(entries), &Limits::RELEASE);
        let e = r.expect_err("must be rejected");
        assert_eq!(e.kind, ErrorKind::Untrusted, "{}", e.message);
        // Nothing may appear outside the destination.
        assert!(
            std::fs::read_dir(d.path()).unwrap().count() <= 1,
            "{entries:?}"
        );
        e.message
    }

    fn pe(machine: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x100];
        b[..2].copy_from_slice(b"MZ");
        b[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        b
    }

    fn release(machine: u16) -> Vec<u8> {
        let exe = pe(machine);
        zip_of(&[
            ("config.example.toml", b"x"),
            ("docs/", b""),
            ("docs/API.md", b"api"),
            ("docs/CONFIG.md", b"cfg"),
            ("LICENSE", b"l"),
            ("README.md", b"r"),
            ("siple.exe", &exe),
            ("siple-cli.exe", &exe),
            ("THIRD-PARTY-NOTICES.html", b"t"),
        ])
    }

    struct FakeTrust {
        bad: Option<&'static str>,
        version: [u16; 4],
        no_version_for: Option<&'static str>,
    }

    impl Trust for FakeTrust {
        fn verify(&self, exe: &Path, _: bool) -> Result<(), String> {
            match self.bad {
                Some(n) if exe.file_name().is_some_and(|f| f == n) => Err("bad signer".into()),
                _ => Ok(()),
            }
        }
        fn file_version(&self, exe: &Path) -> Result<Option<[u16; 4]>, String> {
            if self
                .no_version_for
                .is_some_and(|n| exe.file_name().is_some_and(|f| f == n))
            {
                return Ok(None);
            }
            Ok(Some(self.version))
        }
    }

    fn good_trust() -> Arc<dyn Trust> {
        Arc::new(FakeTrust {
            bad: None,
            version: [0, 2, 0, 0],
            no_version_for: None,
        })
    }

    fn real_env(exe: &Path) -> Env {
        Env {
            exe: exe.to_path_buf(),
            vars: Default::default(),
            uid: None,
            fs: Arc::new(super::super::env::RealFs),
        }
    }

    fn quick_ops() -> StdOps {
        StdOps {
            link_aside: false,
            attempts: 2,
            delay: Duration::from_millis(20),
        }
    }

    fn expect(v: &str) -> Expect {
        Expect {
            version: Version::parse(v).unwrap(),
        }
    }

    // ---- pins ----------------------------------------------------------------------------

    fn signer() -> SignerInfo {
        SignerInfo {
            subject_cn: PIN_SUBJECT.into(),
            subject_o: PIN_SUBJECT.into(),
            ekus: vec![EKU_CODE_SIGNING.into(), PIN_EKU_IDENTITY.into()],
            root_thumbprint_sha1: PIN_ROOT_SHA1.to_lowercase(),
            has_timestamp: true,
            chain: vec![
                ChainCert {
                    subject_cn: PIN_SUBJECT.into(),
                    sha1: "AA".into(),
                },
                ChainCert {
                    subject_cn: "root".into(),
                    sha1: PIN_ROOT_SHA1.into(),
                },
            ],
        }
    }

    #[test]
    fn pins_accept_the_publisher() {
        assert_eq!(check_pins(&signer()), Ok(()));
    }

    #[test]
    fn pins_reject_each_deviation() {
        type Mutate = fn(&mut SignerInfo);
        let cases: [(&str, Mutate); 9] = [
            ("cn", |s| s.subject_cn = "Signal24 LLC Evil".into()),
            ("cn substring", |s| s.subject_cn = "xSignal24 LLC".into()),
            ("o", |s| s.subject_o = "Other".into()),
            ("o empty", |s| s.subject_o.clear()),
            ("identity eku", |s| s.ekus.retain(|e| e != PIN_EKU_IDENTITY)),
            ("code signing eku", |s| {
                s.ekus.retain(|e| e != EKU_CODE_SIGNING)
            }),
            ("eku prefix", |s| {
                s.ekus.retain(|e| e != PIN_EKU_IDENTITY);
                s.ekus.push(format!("{PIN_EKU_IDENTITY}.1"));
            }),
            ("root", |s| s.root_thumbprint_sha1 = "00".repeat(20)),
            ("timestamp", |s| s.has_timestamp = false),
        ];
        for (what, f) in cases {
            let mut s = signer();
            f(&mut s);
            assert!(check_pins(&s).is_err(), "{what} must be rejected");
        }
    }

    // ---- archive -------------------------------------------------------------------------

    #[test]
    fn extracts_the_release_layout() {
        let (d, r) = extract(release(MACHINE_X64), &Limits::RELEASE);
        let names = r.unwrap();
        assert_eq!(names.len(), 8, "{names:?}");
        let new = d.path().join("new");
        assert!(new.join("siple.exe").is_file());
        assert_eq!(
            std::fs::read(new.join("docs").join("API.md")).unwrap(),
            b"api"
        );
    }

    #[test]
    fn rejects_hostile_names() {
        let exe = pe(MACHINE_X64);
        for name in [
            "../siple.exe",
            "docs/../../x",
            "/siple.exe",
            "\\siple.exe",
            "docs\\API.md",
            "C:/Windows/x.dll",
            "C:siple.exe",
            "siple.exe:evil",
            "siple.exe::$DATA",
            "docs//API.md",
            "docs/./API.md",
            "docs/NUL",
            "docs/con.txt",
            "docs/COM1.md",
            "docs/a.md.",
            "docs/a.md ",
            "docs/a*.md",
        ] {
            rejected(&[("siple.exe", &exe), (name, b"x")]);
        }
    }

    #[test]
    fn rejects_unexpected_entries() {
        let exe = pe(MACHINE_X64);
        rejected(&[("siple.exe", &exe), ("evil.dll", b"x")]);
        rejected(&[("siple.exe", &exe), ("SIPLE.EXE.bak", b"x")]);
        rejected(&[("siple.exe", &exe), ("other/x.md", b"x")]);
        rejected(&[("siple.exe", &exe), ("docs", b"x")]);
        rejected(&[("top/siple.exe", &exe)]);
        rejected(&[("Siple.exe", &exe)]);
        // Mandatory file.
        let m = rejected(&[("LICENSE", b"x")]);
        assert!(m.contains("siple.exe"), "{m}");
    }

    #[test]
    fn rejects_duplicates_ignoring_case() {
        let exe = pe(MACHINE_X64);
        rejected(&[
            ("siple.exe", &exe),
            ("docs/API.md", b"a"),
            ("docs/api.md", b"b"),
        ]);
    }

    #[test]
    fn rejects_symlinks() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default();
        w.start_file("siple.exe", o).unwrap();
        w.write_all(&pe(MACHINE_X64)).unwrap();
        w.add_symlink("README.md", "C:/secret", o).unwrap();
        let bytes = w.finish().unwrap().into_inner();
        let (_d, r) = extract(bytes, &Limits::RELEASE);
        assert_eq!(r.unwrap_err().kind, ErrorKind::Untrusted);
    }

    #[test]
    fn limits_on_entries_and_size() {
        assert_eq!(Limits::RELEASE.max_entries, 64);
        assert_eq!(Limits::RELEASE.max_total, 500 * 1024 * 1024);
        let exe = pe(MACHINE_X64);
        let names: Vec<String> = (0..65).map(|i| format!("docs/f{i}.md")).collect();
        let mut entries: Vec<(&str, &[u8])> = vec![("siple.exe", &exe)];
        entries.extend(names.iter().map(|n| (n.as_str(), &b"x"[..])));
        let (_d, r) = extract(zip_of(&entries), &Limits::RELEASE);
        assert!(r.unwrap_err().message.contains("too many"));

        // Declared sizes add up past the total.
        let big = vec![7u8; 1000];
        let limits = Limits {
            max_entries: 64,
            max_total: 1500,
        };
        let (_d, r) = extract(
            zip_of(&[
                ("siple.exe", &exe),
                ("docs/a.md", &big),
                ("docs/b.md", &big),
            ]),
            &limits,
        );
        assert!(r.unwrap_err().message.contains("too much"));
        let (_d, r) = extract(zip_of(&[("siple.exe", &exe), ("docs/a.md", &big)]), &limits);
        assert!(r.is_ok());
    }

    #[test]
    fn refuses_a_destination_that_exists() {
        let d = tempfile::tempdir().unwrap();
        let r = extract_zip(
            Cursor::new(release(MACHINE_X64)),
            d.path(),
            &Limits::RELEASE,
        );
        assert_eq!(r.unwrap_err().kind, ErrorKind::Io);
    }

    #[test]
    fn not_a_zip() {
        let (_d, r) = extract(b"not a zip".to_vec(), &Limits::RELEASE);
        assert_eq!(r.unwrap_err().kind, ErrorKind::Untrusted);
    }

    // ---- PE ------------------------------------------------------------------------------

    #[test]
    fn pe_machine_reads_the_header() {
        assert_eq!(
            pe_machine(&mut Cursor::new(pe(MACHINE_X64))),
            Ok(MACHINE_X64)
        );
        assert_eq!(
            pe_machine(&mut Cursor::new(pe(MACHINE_ARM64))),
            Ok(MACHINE_ARM64)
        );
        let mut bad = pe(MACHINE_X64);
        bad[0] = b'Z';
        assert!(pe_machine(&mut Cursor::new(bad)).is_err());
        let mut bad = pe(MACHINE_X64);
        bad[0x80] = b'X';
        assert!(pe_machine(&mut Cursor::new(bad)).is_err());
        let mut bad = pe(MACHINE_X64);
        bad[0x3C..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(pe_machine(&mut Cursor::new(bad)).is_err());
        let mut bad = pe(MACHINE_X64);
        bad[0x3C..0x40].copy_from_slice(&0xFFu32.to_le_bytes());
        assert!(pe_machine(&mut Cursor::new(bad)).is_err());
        assert!(pe_machine(&mut Cursor::new(b"MZ".to_vec())).is_err());
    }

    // ---- probe ---------------------------------------------------------------------------

    fn p(dir: &str, name: &str) -> String {
        Path::new(dir).join(name).to_str().unwrap().to_string()
    }

    fn fake_env(dir: &str, writable: bool, cli: bool, undeletable: Option<&str>) -> Env {
        let exe = p(dir, "siple.exe");
        let mut fs = FakeFs::default().file(&exe, "");
        if writable {
            fs = fs.writable(dir);
        }
        if cli {
            fs = fs.file(&p(dir, "siple-cli.exe"), "");
        }
        if let Some(u) = undeletable {
            fs = fs.undeletable(&p(dir, u));
        }
        fs.env(&exe, None, &[])
    }

    fn probe(env: Env, trust: Arc<dyn Trust>) -> Result<Target, NotPossible> {
        WindowsPlatform::with_trust(env, trust).probe()
    }

    #[test]
    fn probe_accepts_a_writable_signed_install() {
        let t = probe(fake_env("/apps/siple", true, true, None), good_trust()).unwrap();
        assert_eq!(t.root, Path::new("/apps/siple"));
        assert_eq!(t.exe, Path::new("/apps/siple").join("siple.exe"));
    }

    #[test]
    fn probe_checks_the_staging_dir_when_it_exists() {
        let dir = "/apps/siple";
        let state = p(dir, ".siple-update");
        let exe = p(dir, "siple.exe");
        let fs = FakeFs::default().file(&exe, "").dir(&state).writable(dir);
        let n = probe(fs.env(&exe, None, &[]), good_trust()).unwrap_err();
        assert_eq!(n.reason, "not_writable");
        let fs = FakeFs::default()
            .file(&exe, "")
            .dir(&state)
            .writable(&state);
        assert!(probe(fs.env(&exe, None, &[]), good_trust()).is_ok());
    }

    #[test]
    fn probe_reasons() {
        let reason = |e: Env, t: Arc<dyn Trust>| probe(e, t).unwrap_err().reason;
        assert_eq!(
            reason(
                fake_env("/Program Files/Siple", false, true, None),
                good_trust()
            ),
            "not_writable"
        );
        assert_eq!(
            reason(
                fake_env("/apps/siple", true, true, Some("siple.exe")),
                good_trust()
            ),
            "in_use"
        );
        assert_eq!(
            reason(
                fake_env("/apps/siple", true, true, Some("siple-cli.exe")),
                good_trust()
            ),
            "in_use"
        );
        // A missing CLI is fine.
        assert!(probe(fake_env("/apps/siple", true, false, None), good_trust()).is_ok());
        for dir in [
            "/Users/u/AppData/Local/Microsoft/WinGet/Packages/Signal24.Siple_x",
            "/Users/u/scoop/apps/siple/current",
            "/ProgramData/Chocolatey/lib/siple/tools",
            "/Users/u/AppData/Local/MICROSOFT/WINGET/PACKAGES/Siple",
        ] {
            assert_eq!(
                reason(fake_env(dir, true, true, None), good_trust()),
                "package_manager",
                "{dir}"
            );
        }
        let unsigned: Arc<dyn Trust> = Arc::new(FakeTrust {
            bad: Some("siple.exe"),
            version: [0, 0, 0, 0],
            no_version_for: None,
        });
        assert_eq!(
            reason(fake_env("/apps/siple", true, true, None), unsigned),
            "unsigned_build"
        );
        let e = FakeFs::default()
            .file("/apps/siple/other.exe", "")
            .writable("/apps/siple")
            .env("/apps/siple/other.exe", None, &[]);
        assert_eq!(reason(e, good_trust()), "not_bundle");
    }

    #[test]
    fn asset_follows_the_machine() {
        let env = || fake_env("/apps/siple", true, true, None);
        let x = WindowsPlatform::with_trust(env(), good_trust()).with_machine(MACHINE_X64);
        assert_eq!(x.asset(), Some(AssetPlatform::WindowsX64));
        let a = WindowsPlatform::with_trust(env(), good_trust()).with_machine(MACHINE_ARM64);
        assert_eq!(a.asset(), Some(AssetPlatform::WindowsArm64));
    }

    // ---- stage, commit, recover ----------------------------------------------------------

    struct Install {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        target: Target,
    }

    fn install() -> Install {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Siple");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        let exe = root.join("siple.exe");
        std::fs::write(&exe, b"old-gui").unwrap();
        std::fs::write(root.join("siple-cli.exe"), b"old-cli").unwrap();
        std::fs::write(root.join("docs").join("API.md"), b"old-api").unwrap();
        std::fs::write(root.join("user-notes.txt"), b"mine").unwrap();
        Install {
            target: Target {
                root: root.clone(),
                exe,
            },
            root,
            _tmp: tmp,
        }
    }

    fn platform(i: &Install, trust: Arc<dyn Trust>) -> WindowsPlatform {
        WindowsPlatform::with_trust(real_env(&i.target.exe), trust)
            .with_machine(MACHINE_X64)
            .with_ops(quick_ops())
    }

    fn stage_release(
        pl: &WindowsPlatform,
        i: &Install,
        zip: &[u8],
        v: &str,
    ) -> (PathBuf, Result<Staged, InstallError>) {
        let dir = pl.stage_dir(&i.target).unwrap();
        let archive = dir.join("Siple-0.2.0-windows-x64.zip");
        std::fs::write(&archive, zip).unwrap();
        let r = pl.stage(&archive, &dir, &i.target, &expect(v));
        (dir, r)
    }

    #[test]
    fn stage_dirs_are_new_and_inside_the_staging_area() {
        let i = install();
        let pl = platform(&i, good_trust());
        let a = pl.stage_dir(&i.target).unwrap();
        let b = pl.stage_dir(&i.target).unwrap();
        assert_ne!(a, b);
        assert!(is_staging_path(&a));
        assert_eq!(a.parent().unwrap(), i.root.join(".siple-update"));
        assert_eq!(std::fs::read_dir(&a).unwrap().count(), 0);
    }

    #[test]
    fn stage_runs_every_check() {
        let i = install();
        let pl = platform(&i, good_trust());
        let (dir, r) = stage_release(&pl, &i, &release(MACHINE_X64), "0.2.0");
        let s = r.unwrap();
        assert_eq!(s.dir, dir);
        assert_eq!(s.payload, dir.join("new"));
        assert!(s.payload.join("siple-cli.exe").is_file());

        let fail = |pl: &WindowsPlatform, zip: &[u8], v: &str, what: &str| {
            let (_d, r) = stage_release(pl, &i, zip, v);
            let e = r.expect_err(what);
            assert_eq!(e.kind, ErrorKind::Untrusted, "{what}: {}", e.message);
            e.message
        };
        let m = fail(&pl, &release(MACHINE_ARM64), "0.2.0", "arch");
        assert!(m.contains("another processor"), "{m}");
        let m = fail(&pl, &release(MACHINE_X64), "0.3.0", "version");
        assert!(m.contains("0.2.0.0"), "{m}");
        // Pre-releases are numeric on Windows.
        let (_d, r) = stage_release(&pl, &i, &release(MACHINE_X64), "0.2.0-rc.1");
        assert!(r.is_ok());
        for bad in ["siple.exe", "siple-cli.exe"] {
            let t: Arc<dyn Trust> = Arc::new(FakeTrust {
                bad: Some(bad),
                version: [0, 2, 0, 0],
                no_version_for: None,
            });
            let m = fail(&platform(&i, t), &release(MACHINE_X64), "0.2.0", bad);
            assert!(m.contains("signature"), "{m}");
        }
        let t: Arc<dyn Trust> = Arc::new(FakeTrust {
            bad: None,
            version: [0, 2, 0, 1],
            no_version_for: None,
        });
        fail(&platform(&i, t), &release(MACHINE_X64), "0.2.0", "4th");
        // The CLI may lack a version resource; the GUI may not.
        let t = |n| -> Arc<dyn Trust> {
            Arc::new(FakeTrust {
                bad: None,
                version: [0, 2, 0, 0],
                no_version_for: Some(n),
            })
        };
        let (_d, r) = stage_release(
            &platform(&i, t("siple-cli.exe")),
            &i,
            &release(MACHINE_X64),
            "0.2.0",
        );
        assert!(r.is_ok());
        let m = fail(
            &platform(&i, t("siple.exe")),
            &release(MACHINE_X64),
            "0.2.0",
            "gui",
        );
        assert!(m.contains("no version resource"), "{m}");
        let notpe = zip_of(&[("siple.exe", b"MZ-not-really")]);
        fail(&pl, &notpe, "0.2.0", "not a pe");
        // Without the CLI the release is still installable.
        let exe = pe(MACHINE_X64);
        let (_d, r) = stage_release(&pl, &i, &zip_of(&[("siple.exe", &exe)]), "0.2.0");
        assert!(r.is_ok());
    }

    #[test]
    fn commit_swaps_and_cleanup_removes_only_ours() {
        let i = install();
        let pl = platform(&i, good_trust());
        let (dir, r) = stage_release(&pl, &i, &release(MACHINE_X64), "0.2.0");
        let staged = r.unwrap();
        let c = pl.commit(&staged, &i.target).unwrap();
        assert_eq!(c.relaunch, RelaunchSpec::Exe(i.target.exe.clone()));
        let new = pe(MACHINE_X64);
        assert_eq!(std::fs::read(&i.target.exe).unwrap(), new);
        assert_eq!(std::fs::read(i.root.join("siple-cli.exe")).unwrap(), new);
        assert_eq!(
            std::fs::read(i.root.join("docs").join("API.md")).unwrap(),
            b"api"
        );
        assert_eq!(
            std::fs::read(i.root.join("docs").join("CONFIG.md")).unwrap(),
            b"cfg"
        );
        assert_eq!(std::fs::read(i.root.join("LICENSE")).unwrap(), b"l");
        assert_eq!(
            std::fs::read(i.root.join("user-notes.txt")).unwrap(),
            b"mine"
        );
        assert!(!dir.join(JOURNAL_NAME).exists());
        // Three old files went aside (siple.exe, siple-cli.exe, docs/API.md) and the stage dir.
        assert_eq!(c.cleanup.len(), 4, "{:?}", c.cleanup);
        assert!(c.cleanup.contains(&dir));
        let backups: Vec<&PathBuf> = c.cleanup.iter().filter(|p| **p != dir).collect();
        assert!(backups.iter().all(|b| b.exists()));
        let old: Vec<Vec<u8>> = backups.iter().map(|b| std::fs::read(b).unwrap()).collect();
        assert!(old.contains(&b"old-gui".to_vec()) && old.contains(&b"old-api".to_vec()));

        // A decoy that merely looks like a backup is never removed by cleanup.
        let decoy = i.root.join("user-notes.txt.old-123");
        std::fs::write(&decoy, b"x").unwrap();
        let outside = i._tmp.path().join("outside.old-1");
        std::fs::write(&outside, b"x").unwrap();
        let mut cleanup: Vec<String> = c.cleanup.iter().map(|p| p.display().to_string()).collect();
        cleanup.push(decoy.display().to_string());
        cleanup.push(outside.display().to_string());
        cleanup.push(i.root.join("siple.exe").display().to_string());
        let rec = InstalledUpdate {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            at: chrono::Utc::now(),
            cleanup,
        };
        pl.cleanup(&rec);
        assert!(backups.iter().all(|b| !b.exists()));
        assert!(!dir.exists());
        assert!(decoy.exists() && outside.exists() && i.target.exe.exists());
    }

    #[test]
    fn commit_refuses_payloads_outside_a_staging_area() {
        let i = install();
        let pl = platform(&i, good_trust());
        let staged = Staged {
            dir: i.root.join("elsewhere"),
            payload: i.root.join("elsewhere").join("new"),
        };
        assert_eq!(
            pl.commit(&staged, &i.target).unwrap_err().kind,
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn recover_rolls_an_interrupted_swap_forward() {
        let i = install();
        let pl = platform(&i, good_trust());
        let (dir, r) = stage_release(&pl, &i, &release(MACHINE_X64), "0.2.0");
        let staged = r.unwrap();
        // The state after a crash: the old exe is aside, the new one not yet in place.
        let staged_exe = staged.payload.join("siple.exe");
        let backup = journal::backup_name(&i.target.exe, 0);
        std::fs::rename(&i.target.exe, &backup).unwrap();
        let j = Journal {
            v: 1,
            entries: vec![Entry {
                staged: staged_exe.clone(),
                target: i.target.exe.clone(),
                backup: Some(backup.clone()),
                sha256: hash_file(&staged_exe).unwrap(),
            }],
        };
        std::fs::write(dir.join(JOURNAL_NAME), serde_json::to_vec(&j).unwrap()).unwrap();
        pl.recover();
        assert_eq!(std::fs::read(&i.target.exe).unwrap(), pe(MACHINE_X64));
        assert!(!dir.join(JOURNAL_NAME).exists());
        // The fresh backup is within its grace period and stays.
        assert!(backup.exists());
    }

    #[test]
    fn recover_rolls_back_when_the_staged_file_is_gone() {
        let i = install();
        let pl = platform(&i, good_trust());
        let dir = pl.stage_dir(&i.target).unwrap();
        let backup = journal::backup_name(&i.target.exe, 0);
        std::fs::rename(&i.target.exe, &backup).unwrap();
        let j = Journal {
            v: 1,
            entries: vec![Entry {
                staged: dir.join("new").join("siple.exe"),
                target: i.target.exe.clone(),
                backup: Some(backup.clone()),
                sha256: "00".repeat(32),
            }],
        };
        std::fs::write(dir.join(JOURNAL_NAME), serde_json::to_vec(&j).unwrap()).unwrap();
        pl.recover();
        assert_eq!(std::fs::read(&i.target.exe).unwrap(), b"old-gui");
        assert!(!backup.exists());
    }

    #[test]
    fn recover_removes_old_backups_and_stale_staging_only() {
        let i = install();
        let pl = platform(&i, good_trust());
        let state = i.root.join(STATE_DIR);
        let fresh = pl.stage_dir(&i.target).unwrap();
        let stale = state.join("stale");
        std::fs::create_dir_all(stale.join("new")).unwrap();
        #[cfg(unix)]
        {
            let t = SystemTime::now() - Duration::from_secs(3 * 24 * 3600);
            File::open(&stale).unwrap().set_modified(t).unwrap();
        }
        let old = |name: &str| i.root.join(format!("{name}.old-1000"));
        let young = i.root.join(format!("siple.exe.old-{}", now_ms()));
        for f in [
            old("siple.exe"),
            old("siple-cli.exe"),
            old("README.md"),
            old("user-notes.txt"),
            old("other.exe"),
            young.clone(),
            i.root.join("docs").join("API.md.old-1000"),
            i.root.join("docs").join("evil.exe.old-1000"),
        ] {
            std::fs::write(f, b"x").unwrap();
        }
        pl.recover();
        assert!(!old("siple.exe").exists());
        assert!(!old("siple-cli.exe").exists());
        assert!(!old("README.md").exists());
        assert!(!i.root.join("docs").join("API.md.old-1000").exists());
        assert!(old("user-notes.txt").exists(), "not an archive file");
        assert!(old("other.exe").exists());
        assert!(i.root.join("docs").join("evil.exe.old-1000").exists());
        assert!(young.exists(), "inside the grace period");
        assert!(fresh.exists());
        #[cfg(unix)]
        assert!(!stale.exists());
        assert!(i.target.exe.exists() && i.root.join("user-notes.txt").exists());
    }

    #[test]
    fn backup_names_of_ours() {
        assert!(backup_of_ours("siple.exe.old-123", false).is_some());
        assert!(backup_of_ours("siple.exe.old-123-2", false).is_some());
        assert!(backup_of_ours("siple.exe.old-", false).is_none());
        assert!(backup_of_ours("siple.exe.old-12a", false).is_none());
        assert!(backup_of_ours("x.exe.old-1", false).is_none());
        assert!(backup_of_ours("API.md.old-1", true).is_some());
        assert!(backup_of_ours("API.md.old-1", false).is_none());
    }

    // ---- real Windows: file system semantics and signatures ------------------------------

    #[cfg(windows)]
    mod real {
        use super::*;
        use std::os::windows::fs::OpenOptionsExt;

        fn spawn_copy(dir: &Path, name: &str) -> std::process::Child {
            let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            let dst = dir.join(name);
            std::fs::copy(Path::new(&sys).join(r"System32\ping.exe"), &dst).unwrap();
            std::process::Command::new(&dst)
                .args(["-n", "60", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        }

        #[test]
        fn a_running_image_can_be_renamed_not_overwritten() {
            let tmp = tempfile::tempdir().unwrap();
            let mut child = spawn_copy(tmp.path(), "siple.exe");
            std::thread::sleep(Duration::from_millis(500));
            let exe = tmp.path().join("siple.exe");
            let new = tmp.path().join("new.exe");
            std::fs::write(&new, b"new").unwrap();
            assert!(std::fs::write(&exe, b"x").is_err(), "overwrite must fail");
            let aside = journal::backup_name(&exe, 0);
            std::fs::rename(&exe, &aside).unwrap();
            std::fs::rename(&new, &exe).unwrap();
            assert_eq!(std::fs::read(&exe).unwrap(), b"new");
            assert!(std::fs::metadata(&aside).unwrap().len() > 3);
            // The old image is locked while it runs; after it ended it can be removed.
            assert!(std::fs::remove_file(&aside).is_err());
            child.kill().unwrap();
            child.wait().unwrap();
            std::fs::remove_file(&aside).unwrap();
        }

        #[test]
        fn probe_sees_a_running_exe_as_replaceable() {
            let tmp = tempfile::tempdir().unwrap();
            let mut child = spawn_copy(tmp.path(), "siple.exe");
            std::thread::sleep(Duration::from_millis(500));
            let env = real_env(&tmp.path().join("siple.exe"));
            assert!(env.fs.can_delete(&env.exe));
            assert!(env.fs.can_create_in(tmp.path()));
            child.kill().unwrap();
            child.wait().unwrap();
        }

        fn files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
            let staged = root.join("stage");
            std::fs::create_dir_all(&staged).unwrap();
            std::fs::write(staged.join("siple-cli.exe"), b"new-cli").unwrap();
            std::fs::write(staged.join("siple.exe"), b"new-gui").unwrap();
            vec![
                (staged.join("siple-cli.exe"), root.join("siple-cli.exe")),
                (staged.join("siple.exe"), root.join("siple.exe")),
            ]
        }

        #[test]
        fn journal_rolls_forward_on_ntfs() {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("siple.exe"), b"old-gui").unwrap();
            let f = files(tmp.path());
            let backups =
                journal::swap(&f, &tmp.path().join(JOURNAL_NAME), &StdOps::windows()).unwrap();
            assert_eq!(backups.len(), 1);
            assert_eq!(
                std::fs::read(tmp.path().join("siple.exe")).unwrap(),
                b"new-gui"
            );
            assert_eq!(
                std::fs::read(tmp.path().join("siple-cli.exe")).unwrap(),
                b"new-cli"
            );
            assert_eq!(std::fs::read(&backups[0]).unwrap(), b"old-gui");
        }

        #[test]
        fn journal_rolls_back_when_a_file_is_held_open() {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("siple.exe"), b"old-gui").unwrap();
            std::fs::write(tmp.path().join("siple-cli.exe"), b"old-cli").unwrap();
            let f = files(tmp.path());
            // Read and write sharing, but not delete: the rename of siple.exe is refused
            // after the CLI was already swapped.
            let _held = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(tmp.path().join("siple.exe"))
                .unwrap();
            let r = journal::swap(&f, &tmp.path().join(JOURNAL_NAME), &quick_ops());
            assert!(r.is_err());
            assert_eq!(
                std::fs::read(tmp.path().join("siple.exe")).unwrap(),
                b"old-gui"
            );
            assert_eq!(
                std::fs::read(tmp.path().join("siple-cli.exe")).unwrap(),
                b"old-cli"
            );
            assert!(!tmp.path().join(JOURNAL_NAME).exists());
            let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
                .unwrap()
                .flatten()
                .filter(|e| journal::is_backup_name(&e.file_name().to_string_lossy()))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }

        fn windir() -> PathBuf {
            PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        }

        #[test]
        fn microsoft_signed_files_pass_the_os_check_but_not_the_pins() {
            let mut seen = 0;
            for rel in [
                r"explorer.exe",
                r"System32\notepad.exe",
                r"System32\cmd.exe",
                r"System32\kernel32.dll",
            ] {
                let path = windir().join(rel);
                match win::read_signer(&path, false) {
                    Ok(info) => {
                        seen += 1;
                        println!("{rel}: {info:?}");
                        assert!(check_pins(&info).is_err(), "{rel} must fail the pins");
                    }
                    Err(e) => println!("{rel}: not embedded-signed here: {e}"),
                }
                assert!(DefaultTrust.verify(&path, false).is_err(), "{rel}");
            }
            assert!(seen > 0, "no embedded-signed Microsoft file found");
        }

        #[test]
        fn an_unsigned_file_fails() {
            let me = std::env::current_exe().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let copy = tmp.path().join("siple.exe");
            std::fs::copy(me, &copy).unwrap();
            assert!(win::read_signer(&copy, false).is_err());
            assert!(DefaultTrust.verify(&copy, true).is_err());
            // And a dev build is not offered an install.
            let pl = WindowsPlatform::new(real_env(&copy));
            assert_eq!(pl.probe().unwrap_err().reason, "unsigned_build");
        }

        #[test]
        fn the_released_siple_passes() {
            let Ok(p) = std::env::var("SIPLE_TEST_SIGNED_EXE") else {
                println!("SIPLE_TEST_SIGNED_EXE not set; skipped");
                return;
            };
            let p = PathBuf::from(p);
            let info = win::read_signer(&p, true).expect("WinVerifyTrust");
            println!("{info:#?}");
            assert_eq!(check_pins(&info), Ok(()));
            assert_eq!(DefaultTrust.verify(&p, true), Ok(()));
            assert_eq!(DefaultTrust.verify(&p, false), Ok(()));
            let v = DefaultTrust.file_version(&p).unwrap().unwrap();
            println!("file version {v:?}");
            assert_eq!(v[..2], [0, 1]);
            // Copy it: a different location changes nothing.
            let tmp = tempfile::tempdir().unwrap();
            let c = tmp.path().join("siple.exe");
            std::fs::copy(&p, &c).unwrap();
            assert_eq!(DefaultTrust.verify(&c, false), Ok(()));
            // A flipped byte breaks the signature.
            let mut b = std::fs::read(&c).unwrap();
            let mid = b.len() / 3;
            b[mid] ^= 0xFF;
            std::fs::write(&c, b).unwrap();
            assert!(DefaultTrust.verify(&c, false).is_err());
        }

        #[test]
        fn the_released_archive_stages_and_commits() {
            let Ok(z) = std::env::var("SIPLE_TEST_RELEASE_ZIP") else {
                println!("SIPLE_TEST_RELEASE_ZIP not set; skipped");
                return;
            };
            let i = install();
            let pl = WindowsPlatform::new(real_env(&i.target.exe));
            let dir = pl.stage_dir(&i.target).unwrap();
            let r = pl.stage(Path::new(&z), &dir, &i.target, &expect("0.1.1"));
            let staged = r.expect("stage");
            // Another version is refused.
            let dir2 = pl.stage_dir(&i.target).unwrap();
            let e = pl
                .stage(Path::new(&z), &dir2, &i.target, &expect("0.1.2"))
                .unwrap_err();
            assert_eq!(e.kind, ErrorKind::Untrusted, "{}", e.message);
            let c = pl.commit(&staged, &i.target).unwrap();
            println!("{:?}", c.cleanup);
            assert!(std::fs::metadata(&i.target.exe).unwrap().len() > 1_000_000);
            assert!(i.root.join("siple-cli.exe").is_file());
            assert_eq!(
                std::fs::read(i.root.join("user-notes.txt")).unwrap(),
                b"mine"
            );
            // The installed copy is a signed release.
            assert_eq!(DefaultTrust.verify(&i.target.exe, true), Ok(()));
        }

        #[test]
        fn file_version_of_a_system_file() {
            let v = DefaultTrust
                .file_version(&windir().join(r"System32\kernel32.dll"))
                .unwrap()
                .unwrap();
            assert!(v[0] >= 6, "{v:?}");
        }
    }
}
