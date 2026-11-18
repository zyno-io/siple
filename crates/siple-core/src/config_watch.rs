//! Config file watching: a non-recursive watch on the config's parent directory
//! (so editors that save via write-temp-then-rename work), filtered to the config file name and
//! debounced; plus the content hash used to skip no-op saves. A symlinked config is watched in
//! both the link's and the target's directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify_debouncer_full::notify::{self, EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
};
use sha2::{Digest, Sha256};

/// Default debounce window.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// SHA-256 of a file's contents.
pub type ContentHash = [u8; 32];

/// Hashes `bytes`.
pub fn content_hash(bytes: &[u8]) -> ContentHash {
    Sha256::digest(bytes).into()
}

/// Lower-case hex of a content hash: the `hash` of `config.view` and the `base_hash` of
/// `config.edit`.
pub fn hash_hex(h: &ContentHash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// One file name in one directory that counts as "the config file".
#[derive(Clone)]
struct Target {
    name: std::ffi::OsString,
    /// Canonical directory, when it could be resolved.
    dir: Option<PathBuf>,
}

/// Directory and file name of `path` (directory canonicalised if possible).
fn target_of(path: &Path) -> Option<(PathBuf, Target)> {
    let dir: PathBuf = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let name = path.file_name()?.to_owned();
    let canon = dir.canonicalize().ok();
    Some((dir, Target { name, dir: canon }))
}

/// The directories to watch and the names to react to for `config_path`. If the config is a
/// symlink (a common setup: the real file lives in a dotfiles repo), both the link's directory
/// and the target's directory are watched and both names count: editors replacing the link
/// and tools rewriting the target are both seen. (Repointing the link at a file in a third
/// directory is noticed through the link's directory, but the new directory is not watched
/// until restart.)
fn watch_plan(config_path: &Path) -> Option<(Vec<PathBuf>, Vec<Target>)> {
    let (dir, link) = target_of(config_path)?;
    let mut dirs = vec![dir];
    let mut targets = vec![link.clone()];
    if let Ok(real) = config_path.canonicalize()
        && let Some((rdir, rt)) = target_of(&real)
    {
        let same = rt.name == link.name && rt.dir == link.dir;
        if !same {
            if !dirs.iter().any(|d| d.canonicalize().ok() == rt.dir.clone()) {
                dirs.push(rdir);
            }
            targets.push(rt);
        }
    }
    Some((dirs, targets))
}

/// Whether `ev` concerns the config file: a content-affecting kind, one of the watched file
/// names and (when both can be resolved) the right directory.
fn concerns(ev: &DebouncedEvent, targets: &[Target]) -> bool {
    if matches!(ev.event.kind, EventKind::Access(_) | EventKind::Other) {
        return false;
    }
    ev.event.paths.iter().any(|p| {
        let parent = p.parent().and_then(|d| d.canonicalize().ok());
        targets.iter().any(|t| {
            p.file_name() == Some(t.name.as_os_str())
                && match (&t.dir, &parent) {
                    (Some(want), Some(got)) => want == got,
                    _ => true,
                }
        })
    })
}

/// Keeps the watch alive; dropping it stops watching.
pub struct ConfigWatcher {
    _debouncer: Debouncer<RecommendedWatcher, RecommendedCache>,
}

impl ConfigWatcher {
    /// Watches `config_path`'s directory (and, for a symlinked config, the target's directory)
    /// and calls `on_change` (from the watcher thread; keep it cheap, e.g. a channel send) once
    /// per debounced burst of events touching the file.
    pub fn start(
        config_path: &Path,
        debounce: Duration,
        on_change: impl Fn() + Send + 'static,
    ) -> notify::Result<Self> {
        let (dirs, targets) = watch_plan(config_path)
            .ok_or_else(|| notify::Error::generic("config path has no file name"))?;
        let mut debouncer =
            new_debouncer(debounce, None, move |res: DebounceEventResult| match res {
                Ok(events) => {
                    if events.iter().any(|e| concerns(e, &targets)) {
                        on_change();
                    }
                }
                Err(errors) => {
                    for e in errors {
                        tracing::warn!(error = %e, "config watcher error");
                    }
                }
            })?;
        for d in &dirs {
            debouncer.watch(d, RecursiveMode::NonRecursive)?;
        }
        Ok(Self {
            _debouncer: debouncer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn wait_for(count: &AtomicUsize, at_least: usize, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if count.load(Ordering::SeqCst) >= at_least {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Waits until no event arrived for `quiet` (at most 15 s) and returns the count. Late,
    /// coalesced FSEvents deliveries for an earlier step must not be taken for a new event.
    fn settle(count: &AtomicUsize, quiet: Duration) -> usize {
        let end = Instant::now() + Duration::from_secs(15);
        let mut last = count.load(Ordering::SeqCst);
        let mut since = Instant::now();
        while Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
            let now = count.load(Ordering::SeqCst);
            if now != last {
                last = now;
                since = Instant::now();
            } else if since.elapsed() >= quiet {
                break;
            }
        }
        last
    }

    #[test]
    fn hash_differs_by_content() {
        assert_eq!(content_hash(b"a"), content_hash(b"a"));
        assert_ne!(content_hash(b"a"), content_hash(b"b"));
    }

    #[test]
    fn reacts_to_atomic_rename_write_and_ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "x = 1\n").unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _w = ConfigWatcher::start(&cfg, Duration::from_millis(200), move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        // FSEvents may still deliver the initial write late; count from a quiet baseline.
        let before = settle(&count, Duration::from_millis(500));

        // Unrelated file: no reaction.
        std::fs::write(dir.path().join("other.toml"), "y").unwrap();
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(count.load(Ordering::SeqCst), before);

        // Editor-style save: write a temp file, rename over the config.
        let tmp = dir.path().join(".config.toml.swp");
        std::fs::write(&tmp, "x = 2\n").unwrap();
        std::fs::rename(&tmp, &cfg).unwrap();
        assert!(wait_for(&count, before + 1, 10), "no event after rename");

        // Plain in-place write.
        let n = count.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(600));
        std::fs::write(&cfg, "x = 3\n").unwrap();
        assert!(wait_for(&count, n + 1, 10), "no event after in-place write");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_is_watched_through_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("dotfiles");
        let link_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let real = real_dir.join("siple.toml");
        let link = link_dir.join("config.toml");
        std::fs::write(&real, "x = 1\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _w = ConfigWatcher::start(&link, Duration::from_millis(200), move || {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let n = count.load(Ordering::SeqCst);

        // The editor writes the real file (atomic rename inside the target's directory): the
        // link's directory never sees anything.
        let tmp = real_dir.join(".siple.toml.swp");
        std::fs::write(&tmp, "x = 2\n").unwrap();
        std::fs::rename(&tmp, &real).unwrap();
        assert!(
            wait_for(&count, n + 1, 10),
            "no event for a rewrite of the link target"
        );

        // Replacing the link itself is seen through the link's directory.
        let n = count.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(600));
        let tmp_link = link_dir.join("tmp-link");
        std::os::unix::fs::symlink(&real, &tmp_link).unwrap();
        std::fs::rename(&tmp_link, &link).unwrap();
        assert!(wait_for(&count, n + 1, 10), "no event for a replaced link");

        // Unrelated files in either directory stay quiet.
        let n = settle(&count, Duration::from_secs(1));
        std::fs::write(real_dir.join("other.toml"), "y").unwrap();
        std::fs::write(link_dir.join("other.toml"), "y").unwrap();
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(count.load(Ordering::SeqCst), n);
    }
}
