//! The state machine with fakes: no network, no OS, paused time.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use super::*;
use crate::events::EventFilter;
use crate::update::checker::{FetchError, FetchOutcome, ReleaseFetcher, UpdaterOptions};
use crate::update::source::UpdateSource;

const KEY: &str = include_str!("../../../tests/fixtures/update/test-key.pub");
const SUMS: &[u8] = include_bytes!("../../../tests/fixtures/update/SHA256SUMS.0.2.0");
const SIG: &[u8] = include_bytes!("../../../tests/fixtures/update/SHA256SUMS.0.2.0.minisig");
const REPLAY: &[u8] =
    include_bytes!("../../../tests/fixtures/update/SHA256SUMS.0.2.0.replay.minisig");
const ARCHIVE: &str = "Siple-0.2.0-macos.zip";

/// The fixture list hashes `<version>/<name>` as the file content.
fn archive_bytes() -> Vec<u8> {
    format!("0.2.0/{ARCHIVE}").into_bytes()
}

fn releases_json(with_sig: bool) -> Vec<u8> {
    let mut assets = vec![
        format!(
            r#"{{"name":"SHA256SUMS","size":{},"state":"uploaded"}}"#,
            SUMS.len()
        ),
        format!(
            r#"{{"name":"{ARCHIVE}","size":{},"state":"uploaded"}}"#,
            archive_bytes().len()
        ),
    ];
    if with_sig {
        assets.push(format!(
            r#"{{"name":"SHA256SUMS.minisig","size":{},"state":"uploaded"}}"#,
            SIG.len()
        ));
    }
    format!(
        r#"[{{"tag_name":"v0.2.0","assets":[{}]}}]"#,
        assets.join(",")
    )
    .into_bytes()
}

struct ListFetcher(Vec<u8>);

#[async_trait]
impl ReleaseFetcher for ListFetcher {
    async fn fetch(&self, _: Option<&str>, _: &str) -> Result<FetchOutcome, FetchError> {
        Ok(FetchOutcome::Body {
            etag: None,
            body: self.0.clone(),
        })
    }
}

#[derive(Default)]
struct Idle(AtomicBool);
impl IdleProbe for Idle {
    fn idle(&self) -> bool {
        !self.0.load(Ordering::SeqCst)
    }
}

struct Relaunch {
    available: AtomicBool,
    fail: AtomicBool,
    calls: Mutex<Vec<RelaunchSpec>>,
}
impl Relauncher for Relaunch {
    fn available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
    fn relaunch(&self, spec: RelaunchSpec) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("no window".into());
        }
        self.calls.lock().unwrap().push(spec);
        Ok(())
    }
}

struct Plat {
    base: PathBuf,
    probe: Mutex<Result<Target, NotPossible>>,
    log: Mutex<Vec<String>>,
    fail_commit: AtomicBool,
    n: AtomicUsize,
}
impl Plat {
    fn log(&self, s: &str) {
        self.log.lock().unwrap().push(s.to_string());
    }
    fn count(&self, s: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.as_str() == s)
            .count()
    }
}
impl Platform for Plat {
    fn asset(&self) -> Option<AssetPlatform> {
        Some(AssetPlatform::MacosZip)
    }
    fn probe(&self) -> Result<Target, NotPossible> {
        self.probe.lock().unwrap().clone()
    }
    fn stage_dir(&self, _: &Target) -> Result<PathBuf, InstallError> {
        let d = self
            .base
            .join(format!("stage{}", self.n.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir(&d).map_err(|e| InstallError::io("mkdir", &e))?;
        self.log("stage_dir");
        Ok(d)
    }
    fn stage(
        &self,
        archive: &Path,
        dir: &Path,
        _: &Target,
        e: &Expect,
    ) -> Result<Staged, InstallError> {
        assert_eq!(std::fs::read(archive).unwrap(), archive_bytes());
        assert_eq!(e.version.to_string(), "0.2.0");
        self.log("stage");
        Ok(Staged {
            dir: dir.to_path_buf(),
            payload: dir.join("new"),
        })
    }
    fn commit(&self, s: &Staged, t: &Target) -> Result<Committed, InstallError> {
        if self.fail_commit.load(Ordering::SeqCst) {
            self.log("commit_failed");
            return Err(InstallError::io("swap", &std::io::Error::other("locked")));
        }
        self.log("commit");
        Ok(Committed {
            relaunch: RelaunchSpec::Exe(t.exe.clone()),
            cleanup: vec![s.dir.clone()],
        })
    }
    fn recover(&self) {
        self.log("recover");
    }
    fn cleanup(&self, r: &InstalledUpdate) {
        self.log(&format!("cleanup {}", r.cleanup.len()));
    }
    fn discard(&self, d: &Path) {
        self.log("discard");
        let _ = std::fs::remove_dir_all(d);
    }
}

struct Assets {
    sig: Vec<u8>,
    paused: AtomicBool,
    fetched: Mutex<Vec<String>>,
}
#[async_trait]
impl AssetFetcher for Assets {
    async fn fetch(
        &self,
        name: &str,
        _: &Version,
        dest: &Path,
        expect: &Expected,
        progress: &(dyn Fn(u64) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<[u8; 32], InstallError> {
        self.fetched.lock().unwrap().push(name.to_string());
        let body = match name {
            "SHA256SUMS" => SUMS.to_vec(),
            "SHA256SUMS.minisig" => self.sig.clone(),
            n if n == ARCHIVE => {
                while self.paused.load(Ordering::SeqCst) {
                    if cancel.is_cancelled() {
                        return Err(InstallError::cancelled());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                archive_bytes()
            }
            _ => return Err(InstallError::network("404")),
        };
        assert_eq!(body.len() as u64, expect.size, "{name}");
        std::fs::write(dest, &body).unwrap();
        progress(body.len() as u64);
        let d: [u8; 32] = Sha256::digest(&body).into();
        if let Some(w) = expect.sha256 {
            assert_eq!(w, d);
        }
        Ok(d)
    }
}

struct Rig {
    i: Arc<Installer>,
    u: Arc<Updater>,
    idle: Arc<Idle>,
    rl: Arc<Relaunch>,
    plat: Arc<Plat>,
    assets: Arc<Assets>,
    events: Arc<EventLog>,
    cfg: Arc<Mutex<UpdatesConfig>>,
    dir: tempfile::TempDir,
}

async fn rig_with(sig: &[u8], with_sig: bool, state: bool) -> Rig {
    rig_keyed(sig, with_sig, state, KEY.lines().nth(1).unwrap()).await
}

async fn rig_keyed(sig: &[u8], with_sig: bool, state: bool, key: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(Mutex::new(UpdatesConfig::default()));
    let events = Arc::new(EventLog::default());
    let c = cfg.clone();
    let c2 = cfg.clone();
    let fetcher = Arc::new(ListFetcher(releases_json(with_sig)));
    let u = Updater::new(UpdaterOptions {
        current: Version::parse("0.1.0").unwrap(),
        current_text: "0.1.0".into(),
        source: UpdateSource::Github(fetcher),
        package_family: None,
        events: events.clone(),
        settings: Arc::new(move || c.lock().unwrap().clone()),
        clock: Arc::new(chrono::Utc::now),
        state_dir: None,
        initial_delay: Duration::from_secs(30),
        interval: Duration::from_secs(86400),
        min_manual_interval: Duration::from_secs(60),
    });
    u.check_now().await;
    let plat_dir = dir.path().join("plat");
    std::fs::create_dir(&plat_dir).unwrap();
    let plat = Arc::new(Plat {
        base: plat_dir,
        // A root of its own: the install lock is per target.
        probe: Mutex::new(Ok(Target {
            root: dir.path().join("Siple.app"),
            exe: dir.path().join("Siple.app/siple"),
        })),
        log: Mutex::default(),
        fail_commit: AtomicBool::new(false),
        n: AtomicUsize::new(0),
    });
    let idle = Arc::new(Idle::default());
    let rl = Arc::new(Relaunch {
        available: AtomicBool::new(true),
        fail: AtomicBool::new(false),
        calls: Mutex::default(),
    });
    let assets = Arc::new(Assets {
        sig: sig.to_vec(),
        paused: AtomicBool::new(false),
        fetched: Mutex::default(),
    });
    let mut o = InstallerOptions::new(
        u.clone(),
        plat.clone(),
        assets.clone(),
        idle.clone(),
        rl.clone(),
        events.clone(),
        Arc::new(move || c2.lock().unwrap().clone()),
        "0.1.0".into(),
        state.then(|| dir.path().join("state")),
        CancellationToken::new(),
    );
    if state {
        std::fs::create_dir(dir.path().join("state")).unwrap();
    }
    o.keys = vec![key.to_string()];
    o.cleanup_delay = Duration::from_secs(60);
    let i = Installer::new(o);
    Rig {
        i,
        u,
        idle,
        rl,
        plat,
        assets,
        events,
        cfg,
        dir,
    }
}

async fn rig() -> Rig {
    rig_with(SIG, true, false).await
}

impl Rig {
    fn phase(&self) -> InstallPhase {
        self.u.status().install.unwrap().state
    }

    async fn until(&self, want: InstallPhase) {
        for _ in 0..400 {
            if self.phase() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "never reached {want:?}; at {:?}: {:?}",
            self.phase(),
            self.u.status().install
        );
    }

    fn states(&self) -> Vec<String> {
        self.events
            .since(0, &EventFilter::kinds(["app.update_progress"]), 1000)
            .iter()
            .map(|e| e.data["state"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .into_iter()
            .fold(Vec::new(), |mut v, s| {
                if v.last() != Some(&s) {
                    v.push(s);
                }
                v
            })
    }

    fn reason(e: &ApiError) -> String {
        e.to_wire()["data"]["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

#[tokio::test(start_paused = true)]
async fn the_status_offers_an_install_when_everything_is_in_place() {
    let r = rig().await;
    let s = r.u.status();
    assert_eq!(s.action, UpdateAction::Install);
    let i = s.install.unwrap();
    assert!(i.possible, "{i:?}");
    assert_eq!(i.state, InstallPhase::Idle);
    assert_eq!(i.reason, None);
}

#[tokio::test(start_paused = true)]
async fn it_waits_during_a_call_then_commits_after_the_quiet_time_and_restarts() {
    let r = rig().await;
    r.idle.0.store(true, Ordering::SeqCst); // busy
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Waiting).await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(r.phase(), InstallPhase::Waiting);
    assert_eq!(r.plat.count("commit"), 0, "never swaps during a call");
    assert!(r.rl.calls.lock().unwrap().is_empty());

    r.idle.0.store(false, Ordering::SeqCst); // call over
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(r.plat.count("commit"), 0, "quiet time (5 s) not over yet");
    tokio::time::sleep(Duration::from_secs(4)).await;
    r.until(InstallPhase::Restarting).await;
    assert_eq!(r.plat.count("commit"), 1);
    assert_eq!(r.rl.calls.lock().unwrap().len(), 1);
    assert_eq!(
        r.states(),
        [
            "downloading",
            "verifying",
            "waiting",
            "committing",
            "installed",
            "restarting"
        ]
    );
    // The signature files, then the archive, in that order.
    assert_eq!(
        *r.assets.fetched.lock().unwrap(),
        ["SHA256SUMS", "SHA256SUMS.minisig", ARCHIVE]
    );
    // The progress events carry bytes while downloading.
    let evs = r
        .events
        .since(0, &EventFilter::kinds(["app.update_progress"]), 100);
    assert_eq!(evs[0].data["version"], "0.2.0");
    assert!(
        evs.iter()
            .any(|e| e.data["state"] == "downloading" && e.data["total"] == archive_bytes().len())
    );
}

#[tokio::test(start_paused = true)]
async fn manual_stops_at_ready_until_the_restart_is_requested() {
    let r = rig().await;
    r.i.install("0.2.0", Restart::Manual).unwrap();
    r.until(InstallPhase::Ready).await;
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(r.phase(), InstallPhase::Ready);
    assert_eq!(r.plat.count("commit"), 0);
    assert_eq!(r.plat.count("stage"), 1);
    // The same version again, now: idempotent, no second download, restart at once (no quiet wait).
    r.i.install("0.2.0", Restart::Now).unwrap();
    r.until(InstallPhase::Restarting).await;
    assert_eq!(r.plat.count("stage_dir"), 1);
    assert_eq!(r.plat.count("commit"), 1);
}

#[tokio::test(start_paused = true)]
async fn now_is_refused_during_a_call_and_goes_ahead_without_the_quiet_wait() {
    let r = rig().await;
    r.idle.0.store(true, Ordering::SeqCst);
    let e = r.i.install("0.2.0", Restart::Now).unwrap_err();
    assert_eq!(Rig::reason(&e), "call_active");
    assert_eq!(r.phase(), InstallPhase::Idle, "nothing started");
    r.idle.0.store(false, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::Now).unwrap();
    // Well under QUIET: the download and commit run on blocking threads, so give them a moment.
    for _ in 0..50 {
        if r.phase() == InstallPhase::Restarting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(r.phase(), InstallPhase::Restarting);
}

#[tokio::test(start_paused = true)]
async fn a_call_after_the_swap_keeps_the_restart_pending() {
    let r = rig().await;
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    // Let the swap happen, then a call starts before the relaunch is requested.
    for _ in 0..2000 {
        if r.plat.count("commit") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    r.idle.0.store(true, Ordering::SeqCst);
    r.rl.fail.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(60)).await;
    if r.phase() == InstallPhase::Restarting {
        // The relaunch won the race against the call: nothing to assert about waiting.
        return;
    }
    assert_eq!(r.phase(), InstallPhase::Installed);
    assert!(
        r.rl.calls.lock().unwrap().is_empty(),
        "no restart during a call"
    );
    r.idle.0.store(false, Ordering::SeqCst);
    r.until(InstallPhase::Restarting).await;
    assert_eq!(r.rl.calls.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn cancel_before_the_swap_leaves_nothing_and_after_it_is_too_late() {
    let r = rig().await;
    r.assets.paused.store(true, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Downloading).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(r.i.cancel().unwrap());
    r.until(InstallPhase::Idle).await;
    assert_eq!(r.plat.count("discard"), 1);
    assert_eq!(r.plat.count("commit"), 0);
    assert_eq!(
        std::fs::read_dir(&r.plat.base).unwrap().count(),
        0,
        "staging removed"
    );
    assert!(!r.i.cancel().unwrap(), "nothing to cancel");
    // A new install works after a cancel; then, once swapped, cancel is too late.
    r.assets.paused.store(false, Ordering::SeqCst);
    r.rl.available.store(true, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Restarting).await;
    let e = r.i.cancel().unwrap_err();
    assert_eq!(Rig::reason(&e), "too_late");
}

#[tokio::test(start_paused = true)]
async fn turning_install_off_stops_an_install_that_has_not_swapped() {
    let r = rig().await;
    r.idle.0.store(true, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Waiting).await;
    r.cfg.lock().unwrap().install = false;
    r.i.config_changed();
    r.until(InstallPhase::Idle).await;
    assert_eq!(r.plat.count("commit"), 0);
    assert_eq!(r.plat.count("discard"), 1);
    let s = r.u.status();
    assert_eq!(
        s.install.as_ref().unwrap().reason.as_deref(),
        Some("disabled")
    );
    assert_eq!(s.action, UpdateAction::OpenReleasePage);
    let e = r.i.install("0.2.0", Restart::WhenIdle).unwrap_err();
    assert_eq!(Rig::reason(&e), "disabled");
}

#[tokio::test(start_paused = true)]
async fn refusals_carry_stable_reasons() {
    let r = rig().await;
    let e = r.i.install("0.9.9", Restart::WhenIdle).unwrap_err();
    assert_eq!(Rig::reason(&e), "not_offered");
    assert_eq!(e.to_wire()["data"]["latest"], "0.2.0");

    *r.plat.probe.lock().unwrap() = Err(NotPossible::new(
        "package_manager",
        "installed with Homebrew",
    ));
    // (The probe result is cached for a few seconds.)
    tokio::time::sleep(Duration::from_secs(30)).await;
    let e = r.i.install("0.2.0", Restart::WhenIdle).unwrap_err();
    assert_eq!(Rig::reason(&e), "install_not_possible");
    let d = &e.to_wire()["data"];
    assert_eq!(d["why"], "package_manager");
    assert_eq!(
        d["release_url"],
        "https://github.com/zyno-io/siple/releases/tag/v0.2.0"
    );
    let s = r.u.status();
    assert_eq!(
        s.action,
        UpdateAction::OpenReleasePage,
        "falls back to the release page"
    );
    assert_eq!(
        s.install.unwrap().reason.as_deref(),
        Some("package_manager")
    );
    // The probe is cached for a while; a changed machine shows up afterwards.
    *r.plat.probe.lock().unwrap() = Ok(Target {
        root: "/a".into(),
        exe: "/a/b".into(),
    });
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(r.u.status().action, UpdateAction::Install);

    r.rl.available.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(30)).await;
    let e = r.i.install("0.2.0", Restart::WhenIdle).unwrap_err();
    assert_eq!(
        r.u.status().install.unwrap().reason.as_deref(),
        Some("headless")
    );
    assert_eq!(Rig::reason(&e), "install_not_possible");
    assert_eq!(r.plat.count("stage_dir"), 0, "nothing was downloaded");
}

#[tokio::test(start_paused = true)]
async fn a_release_without_a_signature_falls_back() {
    let r = rig_with(SIG, false, false).await;
    let s = r.u.status();
    assert_eq!(s.action, UpdateAction::OpenReleasePage);
    assert_eq!(
        s.install.unwrap().reason.as_deref(),
        Some("release_not_signed")
    );
    let e = r.i.install("0.2.0", Restart::WhenIdle).unwrap_err();
    assert_eq!(Rig::reason(&e), "install_not_possible");
    assert!(r.assets.fetched.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_bad_signature_fails_without_touching_anything() {
    let right = KEY.lines().nth(1).unwrap();
    let real = "RWRY7rDN9/UXId/KmnBcB1BTI/HNfTk0mM1OD+bxK4JUKDG5RLZGYIIH";
    // A replayed signature of another release, a key that did not sign it, a garbage signature.
    for (sig, key) in [(REPLAY, right), (SIG, real), (&[b'x'; 285][..], right)] {
        let r = rig_keyed(sig, true, false, key).await;
        r.i.install("0.2.0", Restart::WhenIdle).unwrap();
        r.until(InstallPhase::Failed).await;
        let i = r.u.status().install.unwrap();
        assert_eq!(i.error_kind.as_deref(), Some("untrusted"), "{i:?}");
        assert_eq!(r.plat.count("commit"), 0);
        assert_eq!(r.plat.count("stage"), 0, "the archive is never unpacked");
        assert_eq!(r.plat.count("discard"), 1);
        assert!(
            !r.assets
                .fetched
                .lock()
                .unwrap()
                .contains(&ARCHIVE.to_string()),
            "nor even fetched"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_swap_is_reported_and_a_retry_works() {
    let r = rig().await;
    r.plat.fail_commit.store(true, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Failed).await;
    let i = r.u.status().install.unwrap();
    assert_eq!(i.error_kind.as_deref(), Some("io"));
    assert!(i.error.unwrap().contains("locked"));
    assert_eq!(r.plat.count("discard"), 1);
    assert!(r.rl.calls.lock().unwrap().is_empty());
    assert_eq!(
        r.u.status().action,
        UpdateAction::Install,
        "still offered: Try again"
    );
    r.plat.fail_commit.store(false, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Restarting).await;
}

#[tokio::test(start_paused = true)]
async fn a_failing_relaunch_leaves_the_update_installed_with_a_note() {
    let r = rig().await;
    r.rl.fail.store(true, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    for _ in 0..400 {
        if r.u.status().install.unwrap().error.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let i = r.u.status().install.unwrap();
    assert_eq!(i.state, InstallPhase::Installed);
    assert!(i.error.unwrap().contains("restart Siple"));
}

#[tokio::test(start_paused = true)]
async fn a_repeat_for_the_running_version_does_not_start_a_second_download() {
    let r = rig().await;
    r.idle.0.store(true, Ordering::SeqCst);
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Waiting).await;
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.i.install("0.2.0", Restart::Manual).unwrap();
    assert_eq!(r.assets.fetched.lock().unwrap().len(), 3);
    r.until(InstallPhase::Ready).await;
    assert_eq!(
        r.u.status().install.unwrap().restart.as_deref(),
        Some("manual")
    );
}

#[tokio::test(start_paused = true)]
async fn startup_confirms_the_update_and_cleans_up_later() {
    let r = rig_with(SIG, true, true).await;
    let store = StateStore::open(&r.dir.path().join("state"));
    // A record of an update to the running version.
    let mut o = InstallerOptions::new(
        r.u.clone(),
        r.plat.clone(),
        r.assets.clone(),
        r.idle.clone(),
        r.rl.clone(),
        r.events.clone(),
        Arc::new(UpdatesConfig::default),
        "0.2.0".into(),
        Some(r.dir.path().join("state")),
        CancellationToken::new(),
    );
    o.cleanup_delay = Duration::from_secs(60);
    store
        .set_installed_update(Some(InstalledUpdate {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            at: chrono::Utc::now(),
            cleanup: vec!["/x/.siple-update/a".into()],
        }))
        .unwrap();
    let i2 = Installer::new(o);
    i2.startup();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(r.plat.count("recover"), 1);
    let evs = r.events.since(0, &EventFilter::kinds(["app.updated"]), 10);
    assert_eq!(evs.len(), 1);
    assert_eq!(
        (evs[0].data["from"].as_str(), evs[0].data["to"].as_str()),
        (Some("0.1.0"), Some("0.2.0"))
    );
    assert_eq!(r.plat.count("cleanup 1"), 0, "the old copy stays a while");
    assert!(
        StateStore::open(&r.dir.path().join("state"))
            .installed_update()
            .is_some()
    );
    tokio::time::sleep(Duration::from_secs(61)).await;
    for _ in 0..50 {
        if StateStore::open(&r.dir.path().join("state"))
            .installed_update()
            .is_none()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(r.plat.count("cleanup 1"), 1);
    assert_eq!(
        StateStore::open(&r.dir.path().join("state")).installed_update(),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn the_commit_writes_the_record_the_new_version_reads() {
    let r = rig_with(SIG, true, true).await;
    r.i.install("0.2.0", Restart::WhenIdle).unwrap();
    r.until(InstallPhase::Restarting).await;
    let rec = StateStore::open(&r.dir.path().join("state"))
        .installed_update()
        .unwrap();
    assert_eq!((rec.from.as_str(), rec.to.as_str()), ("0.1.0", "0.2.0"));
    assert_eq!(rec.cleanup.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_lock_is_free_once_dropped_even_while_a_copy_is_open() {
    let r = rig().await;
    let t = r.plat.probe().unwrap();
    let held = r.i.lock_target(&t).unwrap();
    // A child that another thread is starting holds a copy of every open file until its exec.
    let copy = held.0.try_clone().unwrap();
    drop(held);
    let _lock = r.i.lock_target(&t).expect("free while the copy is open");
    drop(copy);
    let e = r.i.lock_target(&t).err().expect("held");
    assert_eq!(e.message, "another Siple is installing an update");
}
