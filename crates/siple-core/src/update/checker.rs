//! The checker: schedule, back-off, ETag handling and the status it reports.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use siple_config::UpdatesConfig;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::UpdateStatus;
use super::releases::{Release, parse_releases, pick};
use super::version::{Version, effective_channel};
use crate::events::EventLog;
use crate::state::StateStore;

/// First check after start (unless one succeeded less than an interval ago).
pub const INITIAL_DELAY: Duration = Duration::from_secs(30);
/// Time between checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// A forced check within this time of the last attempt returns the last result.
pub const MIN_MANUAL_INTERVAL: Duration = Duration::from_secs(60);
/// The scheduler wakes at least this often to compare the wall clock with the due time: a
/// monotonic sleep does not run while the machine sleeps.
const MAX_SLEEP: Duration = Duration::from_secs(3600);
/// Upper bound for any fetcher, whatever it does.
const FETCH_LIMIT: Duration = Duration::from_secs(20);
/// First retry after a failed check; doubles per failure up to [`CHECK_INTERVAL`].
const RETRY_BASE: Duration = Duration::from_secs(3600);
/// Back-off when GitHub limits us without saying for how long.
const DEFAULT_BACKOFF: Duration = Duration::from_secs(3600);
const MIN_BACKOFF: Duration = Duration::from_secs(60);

/// Wall-clock source (replaced in tests).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

fn delta(d: Duration) -> chrono::TimeDelta {
    chrono::TimeDelta::from_std(d).unwrap_or(chrono::TimeDelta::MAX)
}

/// What a fetch returned.
#[derive(Debug)]
pub enum FetchOutcome {
    /// The list changed (or no `If-None-Match` was sent).
    Body {
        /// `ETag` to send next time.
        etag: Option<String>,
        /// The JSON list of releases.
        body: Vec<u8>,
    },
    /// 304: what we have is current.
    NotModified,
}

/// Why a fetch failed.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// HTTP 429, or 403 with an exhausted limit: back off.
    RateLimited {
        /// How long the server asked to wait, when it said.
        retry_after: Option<Duration>,
    },
    /// Any other HTTP status.
    Status(u16),
    /// DNS, connect, TLS, protocol.
    Network(String),
    /// The request exceeded its time limit.
    Timeout,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited { .. } => write!(f, "rate limited by GitHub"),
            FetchError::Status(s) => write!(f, "GitHub answered HTTP {s}"),
            FetchError::Network(m) => f.write_str(m),
            FetchError::Timeout => write!(f, "timed out"),
        }
    }
}

/// The network side of a check; replaced in tests so they never touch the network.
#[async_trait]
pub trait ReleaseFetcher: Send + Sync {
    /// Fetches the releases list, with `If-None-Match: etag` when given.
    async fn fetch(&self, etag: Option<&str>, user_agent: &str)
    -> Result<FetchOutcome, FetchError>;
}

/// The fetcher of an app whose host did not opt in to the network check (tests, embedders): it
/// never touches the network.
pub struct OfflineFetcher;

#[async_trait]
impl ReleaseFetcher for OfflineFetcher {
    async fn fetch(&self, _etag: Option<&str>, _ua: &str) -> Result<FetchOutcome, FetchError> {
        Err(FetchError::Network(
            "the update check is not enabled".into(),
        ))
    }
}

/// Everything an [`Updater`] needs.
pub struct UpdaterOptions {
    /// The running version.
    pub current: Version,
    /// Same, as shown and sent (`Siple/<this>`).
    pub current_text: String,
    /// The HTTP side.
    pub fetcher: Arc<dyn ReleaseFetcher>,
    /// Receives `app.update_available`.
    pub events: Arc<EventLog>,
    /// The `[updates]` section in effect right now.
    pub settings: Arc<dyn Fn() -> UpdatesConfig + Send + Sync>,
    /// Wall clock.
    pub clock: Clock,
    /// Where `state.toml` lives: the time of the last check and the announced version survive a
    /// restart there. `None` keeps them in memory only.
    pub state_dir: Option<PathBuf>,
    /// First check after [`Updater::run`] starts.
    pub initial_delay: Duration,
    /// Time between checks.
    pub interval: Duration,
    /// Minimum time between forced checks.
    pub min_manual_interval: Duration,
}

#[derive(Default)]
struct State {
    releases: Vec<Release>,
    etag: Option<String>,
    /// Last successful check (wall clock).
    checked_at: Option<DateTime<Utc>>,
    error: Option<String>,
    last_attempt: Option<DateTime<Utc>>,
    backoff_until: Option<DateTime<Utc>>,
    failures: u32,
    /// The version `app.update_available` was last emitted for.
    announced: Option<Version>,
    next_due: Option<DateTime<Utc>>,
    /// Version the user chose to skip (GUI); remembered here so status readers see it.
    skipped: Option<String>,
}

/// The update checker. See the module docs of [`crate::update`].
pub struct Updater {
    o: UpdaterOptions,
    state: Mutex<State>,
    /// One check at a time.
    busy: tokio::sync::Mutex<()>,
    /// A check finished or the configuration changed: recompute the schedule.
    wake: Notify,
}

impl Updater {
    /// A checker that has not checked yet (the persisted last check and announced version, if
    /// any, are loaded).
    pub fn new(o: UpdaterOptions) -> Arc<Self> {
        let mut st = State::default();
        if let Some(dir) = &o.state_dir {
            let store = StateStore::open(dir);
            st.announced = store.announced_update().and_then(|v| Version::parse(&v));
            // A time in the future is clock skew: ignore it.
            st.checked_at = store.last_update_check().filter(|t| *t <= (o.clock)());
        }
        Arc::new(Self {
            o,
            state: Mutex::new(st),
            busy: tokio::sync::Mutex::new(()),
            wake: Notify::new(),
        })
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> DateTime<Utc> {
        (self.o.clock)()
    }

    /// The current result, computed from the last fetched list and the config in effect (a
    /// channel change applies at once, without a new fetch).
    pub fn status(&self) -> UpdateStatus {
        let cfg = (self.o.settings)();
        let channel = effective_channel(cfg.channel, &self.o.current);
        let mut s = UpdateStatus::disabled(channel.as_str());
        s.current = self.o.current_text.clone();
        if !cfg.check {
            return s;
        }
        s.enabled = true;
        let st = self.st();
        s.checked_at = st
            .checked_at
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true));
        s.error = st.error.clone();
        if let Some(r) = pick(&st.releases, channel) {
            s.latest = Some(r.name.clone());
            s.available = r.version > self.o.current;
            s.release_url = Some(r.url.clone());
            s.published_at = r.published_at.clone();
        }
        s
    }

    /// The version the user skipped, if any.
    pub fn skipped(&self) -> Option<String> {
        self.st().skipped.clone()
    }

    /// Remembers the skipped version (persisting it is the caller's business).
    pub fn set_skipped(&self, v: Option<String>) {
        self.st().skipped = v;
    }

    /// The configuration changed: look at the new settings.
    pub fn config_changed(&self) {
        self.wake.notify_one();
    }

    /// Checks now (unless disabled, backing off, or a check ran less than the minimum interval
    /// ago) and returns the result. Never fails and never panics: errors are part of the status.
    pub async fn check_now(&self) -> UpdateStatus {
        self.check(true).await;
        // The scheduled check is due an interval after this one, not after the old one.
        self.wake.notify_one();
        self.status()
    }

    async fn check(&self, manual: bool) {
        if !(self.o.settings)().check {
            return;
        }
        let _one_at_a_time = self.busy.lock().await;
        let (etag, have_cache) = {
            let mut st = self.st();
            let now = self.now();
            if let Some(until) = st.backoff_until
                && now < until
            {
                let mins = ((until - now).num_seconds().max(0) as u64).div_ceil(60);
                st.error = Some(format!(
                    "rate limited by GitHub; the next check is in {mins} min"
                ));
                return;
            }
            if manual
                && let Some(last) = st.last_attempt
                && now >= last
                && now - last < delta(self.o.min_manual_interval)
            {
                return;
            }
            st.last_attempt = Some(now);
            (st.etag.clone(), !st.releases.is_empty())
        };
        // A conditional request only makes sense while the list it names is still held.
        let etag = etag.filter(|_| have_cache);
        let user_agent = format!("Siple/{}", self.o.current_text);
        // Own task: a panic or a hang in the fetcher becomes an error status, never a stuck
        // caller.
        let fetcher = self.o.fetcher.clone();
        // Aborted if this future is dropped (shutdown, a cancelled `check_now`).
        let mut task = AbortOnDrop(tokio::spawn(async move {
            fetcher.fetch(etag.as_deref(), &user_agent).await
        }));
        let result = match tokio::time::timeout(FETCH_LIMIT, &mut task.0).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => Err(FetchError::Network(if e.is_panic() {
                "internal error while checking for updates".into()
            } else {
                "the check was cancelled".into()
            })),
            Err(_) => {
                task.0.abort();
                Err(FetchError::Timeout)
            }
        };
        let now = self.now();
        let mut persist = None;
        {
            let mut st = self.st();
            match result {
                Ok(FetchOutcome::NotModified) => {
                    self.succeeded(&mut st, now);
                    persist = Some(());
                }
                Ok(FetchOutcome::Body { etag, body }) => match parse_releases(&body) {
                    Ok(list) => {
                        st.releases = list;
                        st.etag = etag;
                        self.succeeded(&mut st, now);
                        persist = Some(());
                    }
                    Err(e) => self.failed(&mut st, now, &format!("unreadable response: {e}")),
                },
                Err(FetchError::RateLimited { retry_after }) => {
                    let wait = retry_after
                        .unwrap_or(DEFAULT_BACKOFF)
                        .clamp(MIN_BACKOFF, self.o.interval);
                    st.backoff_until = Some(now + delta(wait));
                    st.next_due = Some(now + delta(wait));
                    st.error = Some(format!(
                        "rate limited by GitHub; the next check is in {} min",
                        wait.as_secs().div_ceil(60)
                    ));
                    tracing::info!(wait_secs = wait.as_secs(), "update check rate limited");
                }
                Err(e) => self.failed(&mut st, now, &e.to_string()),
            }
        }
        if persist.is_some() {
            // Only "nothing newer" is remembered across restarts: while an update is available
            // there is no cached list after a restart, so the next start checks again.
            let available = self.status().available;
            self.persist(|s| s.set_last_update_check((!available).then_some(now)));
        }
        self.announce();
    }

    fn persist(&self, f: impl FnOnce(&StateStore) -> std::io::Result<()>) {
        if let Some(dir) = &self.o.state_dir
            && let Err(e) = f(&StateStore::open(dir))
        {
            tracing::debug!("cannot update the update-check state: {e}");
        }
    }

    fn succeeded(&self, st: &mut State, now: DateTime<Utc>) {
        st.checked_at = Some(now);
        st.error = None;
        st.failures = 0;
        st.backoff_until = None;
        st.next_due = Some(now + delta(self.o.interval));
        tracing::debug!("update check succeeded");
    }

    fn failed(&self, st: &mut State, now: DateTime<Utc>, why: &str) {
        st.failures = st.failures.saturating_add(1);
        let retry = RETRY_BASE
            .saturating_mul(1u32 << (st.failures - 1).min(5))
            .min(self.o.interval);
        st.next_due = Some(now + delta(retry));
        st.error = Some(why.to_string());
        tracing::info!(
            error = why,
            retry_in_secs = retry.as_secs(),
            "update check failed"
        );
    }

    /// Emits `app.update_available` once per newly found version (remembered across restarts).
    fn announce(&self) {
        let s = self.status();
        let (true, true, Some(v), Some(url)) = (s.enabled, s.available, &s.latest, &s.release_url)
        else {
            return;
        };
        let Some(version) = Version::parse(v) else {
            return;
        };
        {
            let mut st = self.st();
            // Newer only: switching channels back and forth must not re-announce an older one.
            if st.announced.as_ref().is_some_and(|a| *a >= version) {
                return;
            }
            st.announced = Some(version);
        }
        self.persist(|s| s.set_announced_update(v));
        tracing::info!(version = %v, "a newer Siple is available");
        self.o.events.emit(
            "app.update_available",
            None,
            None,
            json!({"version": v, "url": url}),
        );
    }

    /// The scheduler: first check after the initial delay (right away is not wanted; and not at
    /// all within an interval of the last successful one), then every interval, and again when the
    /// settings change. Compares the wall clock, waking at least hourly, so time spent asleep
    /// counts. Runs until `cancel` fires.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        {
            let now = self.now();
            let mut st = self.st();
            let first = match st.checked_at {
                Some(t) if now - t < delta(self.o.interval) => t + delta(self.o.interval),
                _ => now + delta(self.o.initial_delay),
            };
            st.next_due = Some(first);
        }
        let mut was_enabled = (self.o.settings)().check;
        loop {
            let enabled = (self.o.settings)().check;
            let now = self.now();
            let due = {
                // After the clock jumps backwards, nothing may stay due further away than one
                // interval (or a "last check" lie in the future).
                let mut st = self.st();
                let cap = now + delta(self.o.interval);
                if st.next_due.is_some_and(|d| d > cap) {
                    st.next_due = Some(cap);
                }
                if st.backoff_until.is_some_and(|d| d > cap) {
                    st.backoff_until = Some(cap);
                }
                if st.checked_at.is_some_and(|t| t > now) {
                    st.checked_at = None;
                }
                st.next_due.unwrap_or(now)
            };
            if enabled && now >= due {
                self.check(false).await;
                // A check that did not run (backing off) must not spin.
                let mut st = self.st();
                let now = self.now();
                if st.next_due.is_none_or(|d| d <= now) {
                    st.next_due = Some(now + delta(self.o.interval));
                }
                continue;
            }
            let wait = if enabled {
                (due - now).to_std().unwrap_or_default().min(MAX_SLEEP)
            } else {
                MAX_SLEEP
            };
            tokio::select! {
                () = cancel.cancelled() => return,
                () = self.wake.notified() => {
                    let enabled = (self.o.settings)().check;
                    if enabled {
                        let now = self.now();
                        let mut st = self.st();
                        // Switched on, or the settings (e.g. the channel) changed before this run
                        // fetched anything (a restart within the interval only remembers "nothing
                        // newer on the channel in effect then"): check soon, not inside a back-off.
                        let stale = st
                            .checked_at
                            .is_none_or(|t| now - t >= delta(self.o.interval));
                        if (!was_enabled && stale) || (st.releases.is_empty() && st.last_attempt.is_none()) {
                            let soon = (now + delta(Duration::from_secs(5)))
                                .max(st.backoff_until.unwrap_or(now));
                            st.next_due = Some(st.next_due.map_or(soon, |d| d.min(soon)));
                        }
                        drop(st);
                        self.announce();
                    }
                    was_enabled = enabled;
                }
                () = tokio::time::sleep(wait) => {}
            }
        }
    }
}

/// Starts the scheduler on the current runtime.
pub fn spawn_updater(u: Arc<Updater>, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(u.run(cancel))
}

/// Aborts the task when dropped (a plain `JoinHandle` only detaches).
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use siple_config::UpdateChannel;

    use super::*;
    use crate::events::EventFilter;
    use crate::update::releases::FIXTURE;

    type Step = Result<FetchOutcome, FetchError>;

    #[derive(Default)]
    struct Fake {
        script: Mutex<VecDeque<Step>>,
        /// (etag sent, user agent) of every call.
        calls: Mutex<Vec<(Option<String>, String)>>,
    }

    #[async_trait]
    impl ReleaseFetcher for Fake {
        async fn fetch(&self, etag: Option<&str>, ua: &str) -> Step {
            self.calls
                .lock()
                .unwrap()
                .push((etag.map(str::to_string), ua.to_string()));
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(FetchError::Network("script exhausted".into())))
        }
    }

    impl Fake {
        fn push(&self, s: Step) {
            self.script.lock().unwrap().push_back(s);
        }
        fn body(etag: &str) -> Step {
            Ok(FetchOutcome::Body {
                etag: Some(etag.into()),
                body: FIXTURE.as_bytes().to_vec(),
            })
        }
        fn n(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    struct Rig {
        u: Arc<Updater>,
        fake: Arc<Fake>,
        cfg: Arc<Mutex<UpdatesConfig>>,
        events: Arc<EventLog>,
        /// Extra wall-clock time that passed without the monotonic clock noticing (sleep).
        skew: Arc<Mutex<chrono::TimeDelta>>,
    }

    fn rig(current: &str) -> Rig {
        rig_in(current, None, Arc::new(Fake::default()))
    }

    fn rig_in(current: &str, state_dir: Option<PathBuf>, fake: Arc<Fake>) -> Rig {
        let cfg = Arc::new(Mutex::new(UpdatesConfig::default()));
        let events = Arc::new(EventLog::default());
        let c = cfg.clone();
        let skew = Arc::new(Mutex::new(chrono::TimeDelta::zero()));
        // Wall clock = a fixed start plus the (paused) tokio time plus the skew.
        let (t0, base, sk) = (tokio::time::Instant::now(), Utc::now(), skew.clone());
        let u = Updater::new(UpdaterOptions {
            current: Version::parse(current).unwrap(),
            current_text: current.into(),
            fetcher: fake.clone(),
            events: events.clone(),
            settings: Arc::new(move || c.lock().unwrap().clone()),
            clock: Arc::new(move || {
                base + chrono::TimeDelta::from_std(t0.elapsed()).unwrap() + *sk.lock().unwrap()
            }),
            state_dir,
            initial_delay: INITIAL_DELAY,
            interval: CHECK_INTERVAL,
            min_manual_interval: MIN_MANUAL_INTERVAL,
        });
        Rig {
            u,
            fake,
            cfg,
            events,
            skew,
        }
    }

    fn due_in(r: &Rig) -> Duration {
        (r.u.st().next_due.unwrap() - r.u.now()).to_std().unwrap()
    }

    fn announced(r: &Rig) -> Vec<String> {
        r.events
            .since(0, &EventFilter::kinds(["app.update_available"]), 100)
            .iter()
            .map(|e| e.data["version"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn finds_the_newest_stable_release() {
        let r = rig("0.1.0");
        r.fake.push(Fake::body("\"e1\""));
        let s = r.u.check_now().await;
        assert!(s.enabled && s.available, "{s:?}");
        assert_eq!(s.channel, "stable");
        assert_eq!(s.current, "0.1.0");
        assert_eq!(s.latest.as_deref(), Some("0.2.0"));
        assert_eq!(
            s.release_url.as_deref(),
            Some("https://github.com/zyno-io/siple/releases/tag/v0.2.0")
        );
        assert_eq!(s.published_at.as_deref(), Some("2026-11-01T10:00:00Z"));
        assert!(s.checked_at.is_some() && s.error.is_none());
        assert_eq!(
            r.fake.calls.lock().unwrap()[0],
            (None, "Siple/0.1.0".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn up_to_date_is_not_available() {
        let r = rig("0.2.0");
        r.fake.push(Fake::body("\"e1\""));
        let s = r.u.check_now().await;
        assert!(!s.available);
        assert_eq!(s.latest.as_deref(), Some("0.2.0"));
        assert!(announced(&r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn release_candidates_follow_the_channel() {
        // Running rc.1 of 0.2.0: automatic channel is prerelease, so 0.4.0 (flagged) wins.
        let r = rig("0.2.0-rc.1");
        r.fake.push(Fake::body("\"e1\""));
        let s = r.u.check_now().await;
        assert_eq!(s.channel, "prerelease");
        assert_eq!(s.latest.as_deref(), Some("0.4.0"));
        // Explicit stable: the final 0.2.0 is newer than its own rc.
        r.cfg.lock().unwrap().channel = Some(UpdateChannel::Stable);
        let s = r.u.status();
        assert_eq!(
            (s.channel.as_str(), s.latest.as_deref()),
            ("stable", Some("0.2.0"))
        );
        assert!(s.available);
        // A stable build with the prerelease channel sees release candidates too.
        let r = rig("0.2.0");
        r.cfg.lock().unwrap().channel = Some(UpdateChannel::Prerelease);
        r.fake.push(Fake::body("\"e1\""));
        let s = r.u.check_now().await;
        assert_eq!(s.latest.as_deref(), Some("0.4.0"));
        assert!(s.available);
    }

    #[tokio::test(start_paused = true)]
    async fn announces_each_version_once() {
        let r = rig("0.1.0");
        r.fake.push(Fake::body("\"e1\""));
        r.fake.push(Ok(FetchOutcome::NotModified));
        r.u.check_now().await;
        tokio::time::advance(Duration::from_secs(120)).await;
        r.u.check_now().await;
        assert_eq!(announced(&r), ["0.2.0"]);
        // A channel switch that surfaces another version announces that one.
        r.cfg.lock().unwrap().channel = Some(UpdateChannel::Prerelease);
        r.u.announce();
        r.u.announce();
        assert_eq!(announced(&r), ["0.2.0", "0.4.0"]);
        // Switching back does not re-announce the older 0.2.0.
        r.cfg.lock().unwrap().channel = Some(UpdateChannel::Stable);
        r.u.announce();
        assert_eq!(announced(&r), ["0.2.0", "0.4.0"]);
    }

    #[tokio::test(start_paused = true)]
    async fn etag_is_sent_back_and_304_keeps_the_result() {
        let r = rig("0.1.0");
        r.fake.push(Fake::body("\"e1\""));
        r.fake.push(Ok(FetchOutcome::NotModified));
        r.u.check_now().await;
        tokio::time::advance(Duration::from_secs(120)).await;
        let s = r.u.check_now().await;
        let calls = r.fake.calls.lock().unwrap().clone();
        assert_eq!(calls[1].0.as_deref(), Some("\"e1\""));
        assert!(s.available && s.error.is_none());
        assert_eq!(s.latest.as_deref(), Some("0.2.0"));
    }

    #[tokio::test(start_paused = true)]
    async fn forced_checks_respect_the_minimum_interval() {
        let r = rig("0.1.0");
        r.fake.push(Fake::body("\"e1\""));
        r.fake.push(Fake::body("\"e2\""));
        r.u.check_now().await;
        r.u.check_now().await;
        assert_eq!(r.fake.n(), 1, "second call within 60 s must not fetch");
        tokio::time::advance(MIN_MANUAL_INTERVAL).await;
        r.u.check_now().await;
        assert_eq!(r.fake.n(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_backs_off_and_keeps_the_last_result() {
        let r = rig("0.1.0");
        r.fake.push(Fake::body("\"e1\""));
        r.fake.push(Err(FetchError::RateLimited {
            retry_after: Some(Duration::from_secs(1800)),
        }));
        r.fake.push(Fake::body("\"e2\""));
        r.u.check_now().await;
        tokio::time::advance(Duration::from_secs(120)).await;
        let s = r.u.check_now().await;
        assert!(
            s.error.as_deref().unwrap().contains("rate limited"),
            "{s:?}"
        );
        assert!(s.available, "the earlier result stays");
        // Within the back-off nothing is fetched, even when forced.
        tokio::time::advance(Duration::from_secs(600)).await;
        r.u.check_now().await;
        assert_eq!(r.fake.n(), 2);
        // After it, checking works again and the error clears.
        tokio::time::advance(Duration::from_secs(1300)).await;
        let s = r.u.check_now().await;
        assert_eq!(r.fake.n(), 3);
        assert!(s.error.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_without_a_wait_uses_the_default_and_is_clamped() {
        let r = rig("0.1.0");
        r.fake
            .push(Err(FetchError::RateLimited { retry_after: None }));
        r.u.check_now().await;
        let until = r.u.st().backoff_until.unwrap();
        assert_eq!((until - r.u.now()).to_std().unwrap(), DEFAULT_BACKOFF);
        let r = rig("0.1.0");
        r.fake.push(Err(FetchError::RateLimited {
            retry_after: Some(Duration::from_secs(10_000_000)),
        }));
        r.u.check_now().await;
        let until = r.u.st().backoff_until.unwrap();
        assert_eq!((until - r.u.now()).to_std().unwrap(), CHECK_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_reported_not_raised_and_retry_later() {
        let r = rig("0.1.0");
        r.fake.push(Err(FetchError::Timeout));
        r.fake.push(Err(FetchError::Status(500)));
        let s = r.u.check_now().await;
        assert_eq!(s.error.as_deref(), Some("timed out"));
        assert!(!s.available && s.latest.is_none() && s.checked_at.is_none());
        assert_eq!(due_in(&r), RETRY_BASE);
        tokio::time::advance(Duration::from_secs(120)).await;
        r.u.check_now().await;
        assert_eq!(due_in(&r), RETRY_BASE * 2);
        // Garbage that is not a release list is a failure too.
        tokio::time::advance(Duration::from_secs(120)).await;
        r.fake.push(Ok(FetchOutcome::Body {
            etag: None,
            body: b"{\"message\":\"nope\"}".to_vec(),
        }));
        let s = r.u.check_now().await;
        assert!(s.error.as_deref().unwrap().contains("unreadable"), "{s:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_never_fetches() {
        let r = rig("0.1.0");
        r.cfg.lock().unwrap().check = false;
        let s = r.u.check_now().await;
        assert!(!s.enabled && !s.available && s.latest.is_none());
        assert_eq!(r.fake.n(), 0);
        // Switching off hides an earlier result.
        r.cfg.lock().unwrap().check = true;
        r.fake.push(Fake::body("\"e1\""));
        assert!(r.u.check_now().await.available);
        r.cfg.lock().unwrap().check = false;
        let s = r.u.status();
        assert!(!s.enabled && !s.available && s.latest.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_checks_after_30s_then_daily_and_stops_on_cancel() {
        let r = rig("0.1.0");
        for i in 0..3 {
            r.fake.push(Fake::body(&format!("\"e{i}\"")));
        }
        let cancel = CancellationToken::new();
        let h = spawn_updater(r.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(29)).await;
        assert_eq!(r.fake.n(), 0, "nothing before the initial delay");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(r.fake.n(), 1);
        tokio::time::sleep(Duration::from_secs(23 * 3600)).await;
        assert_eq!(r.fake.n(), 1);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(r.fake.n(), 2);
        assert_eq!(announced(&r), ["0.2.0"]);
        cancel.cancel();
        h.await.unwrap();
        tokio::time::sleep(Duration::from_secs(48 * 3600)).await;
        assert_eq!(r.fake.n(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_stays_idle_while_disabled_and_checks_when_enabled() {
        let r = rig("0.1.0");
        r.cfg.lock().unwrap().check = false;
        r.fake.push(Fake::body("\"e1\""));
        let cancel = CancellationToken::new();
        let h = spawn_updater(r.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(3 * 24 * 3600)).await;
        assert_eq!(r.fake.n(), 0);
        r.cfg.lock().unwrap().check = true;
        r.u.config_changed();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(r.fake.n(), 1);
        cancel.cancel();
        h.await.unwrap();
    }

    #[test]
    fn skipped_version_is_remembered() {
        let r = rig("0.1.0");
        assert_eq!(r.u.skipped(), None);
        r.u.set_skipped(Some("0.2.0".into()));
        assert_eq!(r.u.skipped().as_deref(), Some("0.2.0"));
    }

    struct Panics;
    #[async_trait]
    impl ReleaseFetcher for Panics {
        async fn fetch(&self, _: Option<&str>, _: &str) -> Step {
            panic!("boom");
        }
    }

    struct Hangs;
    #[async_trait]
    impl ReleaseFetcher for Hangs {
        async fn fetch(&self, _: Option<&str>, _: &str) -> Step {
            std::future::pending().await
        }
    }

    fn with_fetcher(f: Arc<dyn ReleaseFetcher>) -> Arc<Updater> {
        let r = rig("0.1.0");
        Updater::new(UpdaterOptions {
            current: Version::parse("0.1.0").unwrap(),
            current_text: "0.1.0".into(),
            fetcher: f,
            events: r.events.clone(),
            settings: Arc::new(UpdatesConfig::default),
            clock: Arc::new(Utc::now),
            state_dir: None,
            initial_delay: INITIAL_DELAY,
            interval: CHECK_INTERVAL,
            min_manual_interval: MIN_MANUAL_INTERVAL,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_or_hanging_fetcher_still_gets_an_answer() {
        let s = with_fetcher(Arc::new(Panics)).check_now().await;
        assert!(s.enabled && !s.available);
        assert!(
            s.error.as_deref().unwrap().contains("internal error"),
            "{s:?}"
        );
        let s = with_fetcher(Arc::new(Hangs)).check_now().await;
        assert_eq!(s.error.as_deref(), Some("timed out"));
        let s = with_fetcher(Arc::new(OfflineFetcher)).check_now().await;
        assert!(s.error.is_some() && !s.available);
    }

    #[tokio::test(start_paused = true)]
    async fn a_manual_check_pushes_the_scheduled_one_back() {
        let r = rig("0.1.0");
        for i in 0..3 {
            r.fake.push(Fake::body(&format!("\"e{i}\"")));
        }
        let cancel = CancellationToken::new();
        let h = spawn_updater(r.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert_eq!(r.fake.n(), 1);
        // 23 h later a manual check; the scheduled one must not fire an hour after that.
        tokio::time::sleep(Duration::from_secs(23 * 3600)).await;
        r.u.check_now().await;
        assert_eq!(r.fake.n(), 2);
        tokio::time::sleep(Duration::from_secs(2 * 3600)).await;
        assert_eq!(r.fake.n(), 2, "due 24 h after the manual check");
        tokio::time::sleep(Duration::from_secs(22 * 3600 + 120)).await;
        assert_eq!(r.fake.n(), 3);
        cancel.cancel();
        h.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn time_asleep_counts_towards_the_interval() {
        let r = rig("0.1.0");
        r.fake.push(Fake::body("\"e1\""));
        r.fake.push(Fake::body("\"e2\""));
        let cancel = CancellationToken::new();
        let h = spawn_updater(r.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert_eq!(r.fake.n(), 1);
        // The machine sleeps for 25 h: the monotonic clock barely moves, the wall clock does.
        *r.skew.lock().unwrap() = chrono::TimeDelta::hours(25);
        tokio::time::sleep(MAX_SLEEP + Duration::from_secs(5)).await;
        assert_eq!(r.fake.n(), 2);
        cancel.cancel();
        h.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_backoff_counts_wall_clock_time_too() {
        let r = rig("0.1.0");
        r.fake.push(Err(FetchError::RateLimited {
            retry_after: Some(Duration::from_secs(3600)),
        }));
        r.fake.push(Fake::body("\"e1\""));
        r.u.check_now().await;
        *r.skew.lock().unwrap() = chrono::TimeDelta::hours(2);
        tokio::time::advance(MIN_MANUAL_INTERVAL).await;
        assert!(r.u.check_now().await.error.is_none());
        assert_eq!(r.fake.n(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_recent_persisted_check_delays_the_first_one() {
        let dir = tempfile::tempdir().unwrap();
        // First run: up to date (0.2.0 is the newest stable) -> remembered.
        let r = rig_in("0.2.0", Some(dir.path().into()), Arc::new(Fake::default()));
        r.fake.push(Fake::body("\"e1\""));
        r.u.check_now().await;
        // Restart a little later: no check at 30 s, one when 24 h have passed since.
        let fake = Arc::new(Fake::default());
        fake.push(Fake::body("\"e2\""));
        let r2 = rig_in("0.2.0", Some(dir.path().into()), fake);
        assert!(r2.u.status().checked_at.is_some());
        let cancel = CancellationToken::new();
        let h = spawn_updater(r2.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(r2.fake.n(), 0);
        tokio::time::sleep(Duration::from_secs(25 * 3600)).await;
        assert_eq!(r2.fake.n(), 1);
        cancel.cancel();
        h.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_available_update_is_rechecked_after_restart_but_announced_once() {
        let dir = tempfile::tempdir().unwrap();
        let r = rig_in("0.1.0", Some(dir.path().into()), Arc::new(Fake::default()));
        r.fake.push(Fake::body("\"e1\""));
        r.u.check_now().await;
        assert_eq!(announced(&r), ["0.2.0"]);
        let fake = Arc::new(Fake::default());
        fake.push(Fake::body("\"e2\""));
        let r2 = rig_in("0.1.0", Some(dir.path().into()), fake);
        let cancel = CancellationToken::new();
        let h = spawn_updater(r2.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert_eq!(
            r2.fake.n(),
            1,
            "no cached list after a restart: checked at 30 s"
        );
        assert!(r2.u.status().available);
        assert!(
            announced(&r2).is_empty(),
            "already announced in the previous run"
        );
        cancel.cancel();
        h.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_backward_clock_jump_does_not_stall_checks() {
        let r = rig("0.2.0");
        r.fake.push(Fake::body("\"e1\""));
        r.fake.push(Fake::body("\"e2\""));
        r.u.check_now().await;
        // The clock was a month fast when that check ran, then NTP corrected it.
        *r.skew.lock().unwrap() = chrono::TimeDelta::days(-30);
        let cancel = CancellationToken::new();
        let h = spawn_updater(r.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(26 * 3600)).await;
        assert_eq!(
            r.fake.n(),
            2,
            "checked within one interval of the corrected clock"
        );
        cancel.cancel();
        h.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_channel_change_after_a_restart_checks_soon() {
        let dir = tempfile::tempdir().unwrap();
        // First run: nothing newer on the stable channel -> remembered.
        let r = rig_in("0.2.0", Some(dir.path().into()), Arc::new(Fake::default()));
        r.fake.push(Fake::body("\"e1\""));
        r.u.check_now().await;
        // Restart within the interval, then switch to release candidates.
        let fake = Arc::new(Fake::default());
        fake.push(Fake::body("\"e2\""));
        let r2 = rig_in("0.2.0", Some(dir.path().into()), fake);
        let cancel = CancellationToken::new();
        let h = spawn_updater(r2.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(r2.fake.n(), 0);
        r2.cfg.lock().unwrap().channel = Some(UpdateChannel::Prerelease);
        r2.u.config_changed();
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(r2.fake.n(), 1);
        assert_eq!(r2.u.status().latest.as_deref(), Some("0.4.0"));
        cancel.cancel();
        h.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn switching_on_inside_a_backoff_waits_for_it_not_a_day() {
        let r = rig("0.1.0");
        r.fake.push(Err(FetchError::RateLimited {
            retry_after: Some(Duration::from_secs(3600)),
        }));
        r.fake.push(Fake::body("\"e1\""));
        let cancel = CancellationToken::new();
        let h = spawn_updater(r.u.clone(), cancel.clone());
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert_eq!(r.fake.n(), 1);
        r.cfg.lock().unwrap().check = false;
        r.u.config_changed();
        tokio::time::sleep(Duration::from_secs(600)).await;
        r.cfg.lock().unwrap().check = true;
        r.u.config_changed();
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(r.fake.n(), 2, "checked when the back-off ended");
        cancel.cancel();
        h.await.unwrap();
    }
}
