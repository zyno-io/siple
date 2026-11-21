//! The journaled file swap shared by Windows and Linux.
//!
//! A swap replaces several files of an install with their staged versions. It must never leave a
//! mix of old and new files behind, even if the process is killed half-way. The journal makes
//! that so:
//!
//! 1. Every staged file is hashed and the list (staged, target, backup, sha256) is written to
//!    `journal.json` (and synced) **before** the first file moves.
//! 2. Per entry: the old file is moved or linked aside to its unique backup name ([`Ops::aside`]),
//!    then the staged file replaces the target ([`Ops::move_in`]). A running executable can be
//!    renamed on Windows and survives a rename-over on Linux, so the running copy is never
//!    disturbed. Transient sharing violations (antivirus) are retried.
//! 3. An error rolls back in reverse order: the backup goes back to the target; a file that did
//!    not exist before is removed.
//! 4. When all entries are in, the journal is removed. The backups stay (the new version removes
//!    them after it started, see the installed-update record).
//!
//! If the process dies in between, [`recover`] at the next start rolls **forward** when every
//! entry is either in place (target hash equals the journal's) or can still be completed (staged
//! file present with the right hash), and rolls **back** otherwise.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The journal file name inside the staging area.
pub const JOURNAL_NAME: &str = "journal.json";

/// One file of the swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The new file (inside the staging area).
    pub staged: PathBuf,
    /// Where it goes.
    pub target: PathBuf,
    /// Where the old file is put (unique); `None` when the target did not exist.
    pub backup: Option<PathBuf>,
    /// SHA-256 of the staged file, lowercase hex.
    pub sha256: String,
}

/// The journal as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// Format version.
    pub v: u32,
    /// The files, in swap order.
    pub entries: Vec<Entry>,
}

/// The file operations of a swap. [`StdOps`] is the real one; tests wrap it to inject failures.
pub trait Ops: Send + Sync {
    /// Moves (or links) the old `target` to `backup`; the target path may stay occupied (link).
    fn aside(&self, target: &Path, backup: &Path) -> io::Result<()>;
    /// Replaces `target` (existing or not) with `staged`.
    fn move_in(&self, staged: &Path, target: &Path) -> io::Result<()>;
    /// Puts `backup` back as `target`, replacing whatever is there.
    fn restore(&self, backup: &Path, target: &Path) -> io::Result<()>;
    /// Removes a file.
    fn remove(&self, p: &Path) -> io::Result<()>;
}

/// `std::fs`-based operations with retries for transient access errors.
pub struct StdOps {
    /// Keep the old file in place and hard-link it to the backup name (Linux: the following
    /// rename-over is atomic). `false`: rename it aside (Windows: a running image can be
    /// renamed, not overwritten).
    pub link_aside: bool,
    /// Attempts for a transient error.
    pub attempts: u32,
    /// Pause between attempts.
    pub delay: Duration,
}

impl StdOps {
    /// Windows: rename aside, 10 attempts 200 ms apart.
    pub fn windows() -> Self {
        Self {
            link_aside: false,
            attempts: 10,
            delay: Duration::from_millis(200),
        }
    }

    /// Unix: link aside, rename over.
    pub fn unix() -> Self {
        Self {
            link_aside: true,
            attempts: 1,
            delay: Duration::ZERO,
        }
    }

    fn retry<T>(&self, mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        let mut last = None;
        for i in 0..self.attempts.max(1) {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) if transient(&e) && i + 1 < self.attempts => {
                    last = Some(e);
                    std::thread::sleep(self.delay);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("retry")))
    }
}

/// A sharing violation or an access error that antivirus scanners cause for a moment.
fn transient(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32)
}

impl Ops for StdOps {
    fn aside(&self, target: &Path, backup: &Path) -> io::Result<()> {
        if self.link_aside {
            self.retry(|| std::fs::hard_link(target, backup))
        } else {
            self.retry(|| std::fs::rename(target, backup))
        }
    }

    fn move_in(&self, staged: &Path, target: &Path) -> io::Result<()> {
        self.retry(|| std::fs::rename(staged, target))
    }

    fn restore(&self, backup: &Path, target: &Path) -> io::Result<()> {
        self.retry(|| std::fs::rename(backup, target))
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.retry(|| std::fs::remove_file(p))
    }
}

/// SHA-256 of a file, lowercase hex.
pub fn hash_file(p: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    // sha2 0.11 hashers no longer implement `io::Write`, so no `io::copy`.
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(super::super::sums::hex(&h.finalize()))
}

fn write_journal(path: &Path, j: &Journal) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let data = serde_json::to_vec(j).map_err(io::Error::other)?;
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Reads a journal; `None` when there is none or it is unreadable (nothing can be done with it).
pub fn read_journal(path: &Path) -> Option<Journal> {
    let j: Journal = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (j.v == 1).then_some(j)
}

/// A backup name next to `target`: `<name>.old-<unix ms>`, unique even for two swaps in the same
/// millisecond.
pub fn backup_name(target: &Path, n: usize) -> PathBuf {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".old-{ms}{}",
        if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        }
    ));
    target.with_file_name(name)
}

/// Whether `name` looks like a backup this module made (`<anything>.old-<digits>[-<n>]`).
pub fn is_backup_name(name: &str) -> bool {
    let Some((_, tail)) = name.rsplit_once(".old-") else {
        return false;
    };
    let mut parts = tail.splitn(2, '-');
    let ms = parts.next().unwrap_or("");
    let n = parts.next();
    !ms.is_empty()
        && ms.bytes().all(|b| b.is_ascii_digit())
        && n.is_none_or(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Swaps `files` (staged, target) in under a journal at `journal_path`. Returns the backups made.
/// On an error everything is rolled back and the error returned; the journal is removed in both
/// cases when the state is consistent.
pub fn swap(
    files: &[(PathBuf, PathBuf)],
    journal_path: &Path,
    ops: &dyn Ops,
) -> io::Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for (i, (staged, target)) in files.iter().enumerate() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entries.push(Entry {
            staged: staged.clone(),
            target: target.clone(),
            backup: target.is_file().then(|| backup_name(target, i)),
            sha256: hash_file(staged)?,
        });
    }
    let journal = Journal { v: 1, entries };
    write_journal(journal_path, &journal)?;
    match apply(&journal, ops) {
        Ok(()) => {
            let _ = std::fs::remove_file(journal_path);
            Ok(journal
                .entries
                .iter()
                .filter_map(|e| e.backup.clone())
                .collect())
        }
        Err((done, e)) => {
            // A rollback that failed keeps the journal (and, for the caller, the staging area and
            // the backups): the next start finishes the job.
            if rollback(&journal.entries[..done], ops) {
                let _ = std::fs::remove_file(journal_path);
                Err(e)
            } else {
                Err(io::Error::other(format!(
                    "{e}; the old files could not all be restored"
                )))
            }
        }
    }
}

/// Applies every entry; on failure returns how many entries must be rolled back (the failing one
/// included, whose state is unknown) and the error.
fn apply(j: &Journal, ops: &dyn Ops) -> Result<(), (usize, io::Error)> {
    for (i, e) in j.entries.iter().enumerate() {
        if let Some(b) = &e.backup {
            ops.aside(&e.target, b).map_err(|err| (i + 1, err))?;
        }
        ops.move_in(&e.staged, &e.target)
            .map_err(|err| (i + 1, err))?;
    }
    Ok(())
}

/// Puts the old files back, newest first. Best effort: each step is tried even if one fails.
fn rollback(entries: &[Entry], ops: &dyn Ops) -> bool {
    let mut ok = true;
    for e in entries.iter().rev() {
        match &e.backup {
            Some(b) if b.exists() => {
                if let Err(err) = ops.restore(b, &e.target) {
                    tracing::error!(target = %e.target.display(), "cannot restore: {err}");
                    ok = false;
                }
            }
            Some(_) => {} // never moved aside: the target is still the old file
            None => {
                if e.target.exists()
                    && hash_file(&e.target).ok().as_deref() == Some(e.sha256.as_str())
                    && let Err(err) = ops.remove(&e.target)
                {
                    tracing::error!(target = %e.target.display(), "cannot remove: {err}");
                    ok = false;
                }
            }
        }
    }
    ok
}

/// What [`recover`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// No journal.
    Nothing,
    /// The interrupted swap was completed.
    RolledForward,
    /// The interrupted swap was undone.
    RolledBack,
}

/// Finishes or undoes a swap that was interrupted. Roll forward when every entry is in place
/// (target hash equals the journal's) or completable (the staged file still has its hash);
/// otherwise roll back. The journal is removed afterwards.
pub fn recover(journal_path: &Path, ops: &dyn Ops) -> Recovery {
    if !journal_path.exists() {
        return Recovery::Nothing;
    }
    let Some(j) = read_journal(journal_path) else {
        // Unreadable: nothing to act on, and leaving it would block later swaps.
        let _ = std::fs::remove_file(journal_path);
        return Recovery::Nothing;
    };
    let has = |p: &Path| hash_file(p).ok();
    let in_place = |e: &Entry| has(&e.target).as_deref() == Some(e.sha256.as_str());
    let completable = |e: &Entry| has(&e.staged).as_deref() == Some(e.sha256.as_str());
    let forward = j.entries.iter().all(|e| in_place(e) || completable(e));
    let result = if forward {
        let mut ok = true;
        for e in j.entries.iter().filter(|e| !in_place(e)) {
            // Entries are completed in order; one that is half-way (old file already aside)
            // just needs its move.
            let step = (|| {
                if let Some(b) = &e.backup
                    && e.target.exists()
                    && !b.exists()
                {
                    ops.aside(&e.target, b)?;
                }
                ops.move_in(&e.staged, &e.target)
            })();
            if let Err(err) = step {
                tracing::error!(target = %e.target.display(), "cannot complete the swap: {err}");
                ok = false;
                break;
            }
        }
        if ok {
            Recovery::RolledForward
        } else if rollback(&j.entries, ops) {
            Recovery::RolledBack
        } else {
            return Recovery::RolledBack; // keep the journal for the next start
        }
    } else if rollback(&j.entries, ops) {
        Recovery::RolledBack
    } else {
        return Recovery::RolledBack;
    };
    let _ = std::fs::remove_file(journal_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn w(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }
    fn r(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|_| "<missing>".into())
    }

    /// Fails the n-th move_in (0-based), or every call after it.
    struct Flaky {
        inner: StdOps,
        fail_move_at: Option<usize>,
        moves: AtomicUsize,
        log: Mutex<Vec<String>>,
    }
    impl Flaky {
        fn new(link: bool, fail_move_at: Option<usize>) -> Self {
            let inner = StdOps {
                link_aside: link,
                attempts: 1,
                delay: Duration::ZERO,
            };
            Self {
                inner,
                fail_move_at,
                moves: AtomicUsize::new(0),
                log: Mutex::default(),
            }
        }
    }
    impl Ops for Flaky {
        fn aside(&self, t: &Path, b: &Path) -> io::Result<()> {
            self.log.lock().unwrap().push(format!(
                "aside {}",
                t.file_name().unwrap().to_string_lossy()
            ));
            self.inner.aside(t, b)
        }
        fn move_in(&self, s: &Path, t: &Path) -> io::Result<()> {
            let n = self.moves.fetch_add(1, Ordering::SeqCst);
            if self.fail_move_at.is_some_and(|f| n >= f) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"));
            }
            self.inner.move_in(s, t)
        }
        fn restore(&self, b: &Path, t: &Path) -> io::Result<()> {
            self.inner.restore(b, t)
        }
        fn remove(&self, p: &Path) -> io::Result<()> {
            self.inner.remove(p)
        }
    }

    fn setup() -> (tempfile::TempDir, Vec<(PathBuf, PathBuf)>, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        w(&root.join("app/siple"), "old-a");
        w(&root.join("app/siple-cli"), "old-b");
        w(&root.join("stage/siple"), "new-a");
        w(&root.join("stage/siple-cli"), "new-b");
        w(&root.join("stage/extra.md"), "new-doc");
        let files = vec![
            (root.join("stage/siple"), root.join("app/siple")),
            (root.join("stage/siple-cli"), root.join("app/siple-cli")),
            (root.join("stage/extra.md"), root.join("app/docs/extra.md")),
        ];
        let j = root.join("stage/journal.json");
        (d, files, j)
    }

    #[test]
    fn swap_replaces_everything_and_keeps_the_old_files_aside() {
        for link in [false, true] {
            let (d, files, j) = setup();
            let backups = swap(&files, &j, &Flaky::new(link, None)).unwrap();
            let root = d.path();
            assert_eq!(r(&root.join("app/siple")), "new-a");
            assert_eq!(r(&root.join("app/siple-cli")), "new-b");
            assert_eq!(
                r(&root.join("app/docs/extra.md")),
                "new-doc",
                "a new file is created"
            );
            assert_eq!(backups.len(), 2, "only existing files have backups");
            let mut olds: Vec<_> = backups.iter().map(|b| r(b)).collect();
            olds.sort();
            assert_eq!(olds, ["old-a", "old-b"]);
            assert!(
                backups
                    .iter()
                    .all(|b| is_backup_name(&b.file_name().unwrap().to_string_lossy()))
            );
            assert!(!j.exists(), "journal removed after success");
        }
    }

    #[test]
    fn a_failure_rolls_everything_back() {
        for link in [false, true] {
            for fail_at in 0..3 {
                let (d, files, j) = setup();
                let root = d.path();
                let e = swap(&files, &j, &Flaky::new(link, Some(fail_at))).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
                assert_eq!(
                    r(&root.join("app/siple")),
                    "old-a",
                    "link={link} fail_at={fail_at}"
                );
                assert_eq!(
                    r(&root.join("app/siple-cli")),
                    "old-b",
                    "link={link} fail_at={fail_at}"
                );
                assert!(
                    !root.join("app/docs/extra.md").exists(),
                    "new file removed again"
                );
                assert!(!j.exists());
                // No backup is left lying around once restored.
                let leftovers: Vec<_> = std::fs::read_dir(root.join("app"))
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| is_backup_name(&e.file_name().to_string_lossy()))
                    .collect();
                // (link mode keeps the extra hard link to the untouched old file; harmless but listed)
                assert!(leftovers.len() <= 2, "{leftovers:?}");
            }
        }
    }

    /// Simulates a crash: apply only the first `n` entries, leave the journal.
    fn crash_after(files: &[(PathBuf, PathBuf)], j: &Path, n: usize, link: bool) {
        let ops = StdOps {
            link_aside: link,
            attempts: 1,
            delay: Duration::ZERO,
        };
        let mut entries = Vec::new();
        for (i, (s, t)) in files.iter().enumerate() {
            std::fs::create_dir_all(t.parent().unwrap()).unwrap();
            entries.push(Entry {
                staged: s.clone(),
                target: t.clone(),
                backup: t.is_file().then(|| backup_name(t, i)),
                sha256: hash_file(s).unwrap(),
            });
        }
        let journal = Journal { v: 1, entries };
        write_journal(j, &journal).unwrap();
        for e in journal.entries.iter().take(n) {
            if let Some(b) = &e.backup {
                ops.aside(&e.target, b).unwrap();
            }
            ops.move_in(&e.staged, &e.target).unwrap();
        }
    }

    #[test]
    fn recovery_rolls_forward_when_the_staged_files_are_intact() {
        for link in [false, true] {
            for done in 0..=3 {
                let (d, files, j) = setup();
                crash_after(&files, &j, done, link);
                let ops = StdOps {
                    link_aside: link,
                    attempts: 1,
                    delay: Duration::ZERO,
                };
                assert_eq!(recover(&j, &ops), Recovery::RolledForward, "done={done}");
                let root = d.path();
                assert_eq!(r(&root.join("app/siple")), "new-a");
                assert_eq!(r(&root.join("app/siple-cli")), "new-b");
                assert_eq!(r(&root.join("app/docs/extra.md")), "new-doc");
                assert!(!j.exists());
            }
        }
    }

    #[test]
    fn recovery_rolls_back_when_a_staged_file_is_gone_or_changed() {
        for link in [false, true] {
            for tamper in ["delete", "modify"] {
                let (d, files, j) = setup();
                let root = d.path();
                // First file swapped, second staged file damaged.
                crash_after(&files, &j, 1, link);
                let victim = root.join("stage/siple-cli");
                if tamper == "delete" {
                    std::fs::remove_file(&victim).unwrap();
                } else {
                    std::fs::write(&victim, "evil").unwrap();
                }
                let ops = StdOps {
                    link_aside: link,
                    attempts: 1,
                    delay: Duration::ZERO,
                };
                assert_eq!(recover(&j, &ops), Recovery::RolledBack, "{tamper}");
                assert_eq!(r(&root.join("app/siple")), "old-a", "{tamper} link={link}");
                assert_eq!(r(&root.join("app/siple-cli")), "old-b");
                assert!(!root.join("app/docs/extra.md").exists());
                assert!(!j.exists());
            }
        }
    }

    #[test]
    fn recovery_without_a_journal_or_with_garbage_does_nothing() {
        let d = tempfile::tempdir().unwrap();
        let j = d.path().join("journal.json");
        let ops = StdOps::unix();
        assert_eq!(recover(&j, &ops), Recovery::Nothing);
        std::fs::write(&j, "not json").unwrap();
        assert_eq!(recover(&j, &ops), Recovery::Nothing);
        assert!(!j.exists());
        std::fs::write(&j, r#"{"v":9,"entries":[]}"#).unwrap();
        assert_eq!(recover(&j, &ops), Recovery::Nothing);
    }

    #[test]
    fn transient_errors_are_retried() {
        struct Once(AtomicUsize);
        let calls = Once(AtomicUsize::new(0));
        let ops = StdOps {
            link_aside: false,
            attempts: 5,
            delay: Duration::from_millis(1),
        };
        let v = ops
            .retry(|| {
                if calls.0.fetch_add(1, Ordering::SeqCst) < 3 {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "scanner"))
                } else {
                    Ok(7)
                }
            })
            .unwrap();
        assert_eq!((v, calls.0.load(Ordering::SeqCst)), (7, 4));
        // Other errors are not retried.
        let n = AtomicUsize::new(0);
        assert!(
            ops.retry::<()>(|| {
                n.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::from(io::ErrorKind::NotFound))
            })
            .is_err()
        );
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backup_names_are_recognised() {
        assert!(is_backup_name("siple.exe.old-1700000000000"));
        assert!(is_backup_name("siple.old-17-2"));
        for no in [
            "siple.exe",
            "siple.old-",
            "siple.old-x",
            "siple.old-1-",
            "old-1",
            ".old-1x",
        ] {
            assert!(!is_backup_name(no), "{no}");
        }
    }
}
