//! The site feed: direct builds ask siple.app first (a static JSON list, no rate limit). GitHub's
//! releases API is the second opinion, on one budget of one request per process per 24 hours
//! ([`GITHUB_BUDGET`]), spent in one of two ways:
//!
//! - the cross-check: the site answered, but once a day GitHub is asked as well and the two lists
//!   are merged by tag, GitHub's entry winning. A stale or compromised site can then hide a
//!   release for about a day at most, and a release published since the last site rebuild (a
//!   release candidate) shows up within a day. A failed cross-check keeps the site's list;
//! - the fallback: the site gave no usable answer and the budget is available, so GitHub's list
//!   is used instead. Inside the window the site's error is returned, so the normal failure
//!   retries apply (5, 10, 20, 40 minutes, then hourly, against the site only).
//!
//! A siple.app outage therefore costs GitHub at most one request per install per day. Cargo
//! feature `store` compiles this module out, like [`super::github`].
//!
//! The feed has the shape of the GitHub releases API, reduced to what [`super::releases`] reads
//! (no release notes), so the same parser and selection apply. It is unsigned and only a hint: the
//! installer trusts nothing in it beyond the version and the asset names and sizes. Downloads are
//! built from the validated version, and the minisign-signed `SHA256SUMS` of exactly that version
//! decides what is installed (see [`super::sums::verify_sums`]).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::time::Instant;

use super::FeedSource;
use super::checker::{FetchError, FetchOutcome, ReleaseFetcher, STORE_CHECK_INTERVAL};
use super::http::{get, interpret};
use super::releases::parse_releases;

/// The site is asked with a shorter limit than GitHub, so a hanging site cannot use up the time
/// the GitHub request needs (the checker's overall limit covers both).
pub(crate) const SITE_TIMEOUT: Duration = Duration::from_secs(8);
/// How often GitHub may be asked in one process, for the cross-check or the fallback together.
const GITHUB_BUDGET: Duration = STORE_CHECK_INTERVAL;

const HOST: &str = "siple.app";
/// `v1` leaves room for a schema change without breaking installed apps.
const PATH: &str = "/updates/v1/releases.json";

/// Prefix of an ETag that came from the site (an ETag is only valid for the source that issued it).
const SITE_TAG: &str = "site:";
/// Same, GitHub.
const GITHUB_TAG: &str = "github:";

/// The request headers besides the four every request carries.
pub(crate) fn headers(etag: Option<&str>) -> Vec<(&'static str, &str)> {
    let mut h = vec![("Accept", "application/json")];
    if let Some(t) = etag {
        h.push(("If-None-Match", t));
    }
    h
}

/// Fetches the release feed of siple.app over HTTPS.
pub struct SiteFetcher;

#[async_trait]
impl ReleaseFetcher for SiteFetcher {
    async fn fetch(
        &self,
        etag: Option<&str>,
        user_agent: &str,
    ) -> Result<FetchOutcome, FetchError> {
        let resp = tokio::time::timeout(SITE_TIMEOUT, get(HOST, PATH, &headers(etag), user_agent))
            .await
            .map_err(|_| FetchError::Timeout)??;
        interpret(resp, chrono::Utc::now().timestamp().max(0) as u64)
    }
}

/// Site first, GitHub as the budgeted second opinion (see the module docs). Remembers which one
/// answered ([`ReleaseFetcher::answered_by`]) and tags the ETag it returns with that source, so a
/// later conditional request goes to the host that issued the tag.
pub struct FeedFetcher {
    site: Arc<dyn ReleaseFetcher>,
    github: Arc<dyn ReleaseFetcher>,
    answered: Mutex<Option<FeedSource>>,
    github_state: Mutex<GithubState>,
}

#[derive(Default)]
struct GithubState {
    /// When GitHub was last asked (cross-check or fallback, whatever the outcome); `None` before
    /// the first request of this process.
    last_github_at: Option<Instant>,
    /// The release objects of the last GitHub answer, merged into every later site list.
    items: Vec<Value>,
}

impl FeedFetcher {
    /// A fetcher over the two sources (the real ones, or fakes in tests).
    pub fn new(site: Arc<dyn ReleaseFetcher>, github: Arc<dyn ReleaseFetcher>) -> Self {
        Self {
            site,
            github,
            answered: Mutex::new(None),
            github_state: Mutex::default(),
        }
    }

    /// The real thing: siple.app, then GitHub.
    pub fn network() -> Self {
        Self::new(Arc::new(SiteFetcher), Arc::new(super::GithubFetcher))
    }

    fn set(&self, s: FeedSource) {
        *self.answered.lock().unwrap_or_else(PoisonError::into_inner) = Some(s);
    }

    fn github(&self) -> std::sync::MutexGuard<'_, GithubState> {
        self.github_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// GitHub has not been asked within [`GITHUB_BUDGET`].
    fn github_due(&self) -> bool {
        self.github()
            .last_github_at
            .is_none_or(|t| t.elapsed() >= GITHUB_BUDGET)
    }

    /// Spends the budget: true (and the request counts from now on) when GitHub may be asked.
    fn take_github_budget(&self) -> bool {
        let mut g = self.github();
        if g.last_github_at
            .is_some_and(|t| t.elapsed() < GITHUB_BUDGET)
        {
            return false;
        }
        g.last_github_at = Some(Instant::now());
        true
    }

    /// The site's list with the releases GitHub last reported merged in.
    fn merged(&self, site: Vec<u8>) -> Vec<u8> {
        merge(site, &self.github().items)
    }
}

/// The feed is usable: a list of releases with at least one entry the parser accepts.
fn usable(body: &[u8]) -> Result<(), String> {
    match parse_releases(body) {
        Ok(l) if l.is_empty() => Err("no releases in the feed".into()),
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

fn tag(prefix: &str, etag: Option<String>) -> Option<String> {
    etag.map(|e| format!("{prefix}{e}"))
}

fn tag_name(v: &Value) -> Option<&str> {
    v.get("tag_name").and_then(Value::as_str)
}

/// The release objects of a usable GitHub answer.
fn github_items(body: &[u8]) -> Option<Vec<Value>> {
    usable(body).ok()?;
    match serde_json::from_slice(body).ok()? {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

/// The union of the site's list and GitHub's by tag; where both have a tag GitHub's entry wins.
/// A site list that does not parse as an array is returned as it is (it was already checked).
fn merge(site: Vec<u8>, github: &[Value]) -> Vec<u8> {
    if github.is_empty() {
        return site;
    }
    let Ok(Value::Array(mut items)) = serde_json::from_slice::<Value>(&site) else {
        return site;
    };
    let theirs: HashSet<&str> = github.iter().filter_map(tag_name).collect();
    items.retain(|v| tag_name(v).is_none_or(|t| !theirs.contains(t)));
    items.extend(github.iter().cloned());
    serde_json::to_vec(&items).unwrap_or(site)
}

#[async_trait]
impl ReleaseFetcher for FeedFetcher {
    async fn fetch(
        &self,
        etag: Option<&str>,
        user_agent: &str,
    ) -> Result<FetchOutcome, FetchError> {
        let github_etag = etag.and_then(|t| t.strip_prefix(GITHUB_TAG));
        // The cross-check merges two full lists, so the site is asked without a validator then.
        let cross_check = self.github_due();
        let site_etag = etag
            .and_then(|t| t.strip_prefix(SITE_TAG))
            .filter(|_| !cross_check);
        let err = match self.site.fetch(site_etag, user_agent).await {
            Ok(FetchOutcome::NotModified) if site_etag.is_some() => {
                self.set(FeedSource::Site);
                tracing::debug!("update feed: siple.app says unchanged");
                return Ok(FetchOutcome::NotModified);
            }
            Ok(FetchOutcome::NotModified) => {
                FetchError::Unreadable("answered 304 to an unconditional request".into())
            }
            Ok(FetchOutcome::Body { etag, body }) => match usable(&body) {
                Ok(()) => {
                    let body = if cross_check && self.take_github_budget() {
                        self.cross_check(body, user_agent).await
                    } else {
                        self.merged(body)
                    };
                    self.set(FeedSource::Site);
                    tracing::debug!("update feed: siple.app answered");
                    return Ok(FetchOutcome::Body {
                        etag: tag(SITE_TAG, etag),
                        body,
                    });
                }
                Err(e) => FetchError::Unreadable(format!("unreadable feed: {e}")),
            },
            Err(e) => e,
        };
        if !self.take_github_budget() {
            tracing::info!(reason = %err, "update feed: siple.app gave no usable answer; GitHub was asked less than a day ago, not asking again");
            return Err(err);
        }
        tracing::info!(reason = %err, "update feed: siple.app gave no usable answer; asking GitHub");
        let out = self.github.fetch(github_etag, user_agent).await?;
        self.set(FeedSource::Github);
        tracing::info!("update feed: GitHub answered");
        Ok(match out {
            FetchOutcome::Body { etag, body } => {
                if let Some(items) = github_items(&body) {
                    self.github().items = items;
                }
                FetchOutcome::Body {
                    etag: tag(GITHUB_TAG, etag),
                    body,
                }
            }
            FetchOutcome::NotModified => FetchOutcome::NotModified,
        })
    }

    fn answered_by(&self) -> Option<FeedSource> {
        *self.answered.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl FeedFetcher {
    /// Asks GitHub too (the budget is already spent) and merges its list into the site's. Any
    /// failure keeps the site's list, merged with what GitHub last said.
    async fn cross_check(&self, site: Vec<u8>, user_agent: &str) -> Vec<u8> {
        match self.github.fetch(None, user_agent).await {
            Ok(FetchOutcome::Body { body, .. }) => match github_items(&body) {
                Some(items) => {
                    self.github().items = items;
                    tracing::debug!("update feed: cross-checked with GitHub");
                }
                None => tracing::info!("update feed: the GitHub cross-check was unreadable"),
            },
            Ok(FetchOutcome::NotModified) => {
                tracing::info!("update feed: the GitHub cross-check answered 304 unasked");
            }
            Err(e) => tracing::info!(error = %e, "update feed: the GitHub cross-check failed"),
        }
        self.merged(site)
    }
}

/// The shape siple.app publishes: what [`parse_releases`] reads of the GitHub API, nothing else
/// (in particular no release notes).
#[cfg(test)]
pub(crate) const FEED_FIXTURE: &str = r#"[
  {"tag_name":"v0.3.0-rc.1","name":"v0.3.0-rc.1","draft":false,"prerelease":true,
   "published_at":"2026-11-10T10:00:00Z",
   "html_url":"https://github.com/zyno-io/siple/releases/tag/v0.3.0-rc.1",
   "assets":[{"name":"SHA256SUMS","size":300,"state":"uploaded",
              "browser_download_url":"https://github.com/zyno-io/siple/releases/download/v0.3.0-rc.1/SHA256SUMS"}]},
  {"tag_name":"v0.2.0","name":"v0.2.0","draft":false,"prerelease":false,
   "published_at":"2026-11-01T10:00:00Z",
   "html_url":"https://github.com/zyno-io/siple/releases/tag/v0.2.0",
   "assets":[{"name":"SHA256SUMS","size":200,"state":"uploaded",
              "browser_download_url":"https://github.com/zyno-io/siple/releases/download/v0.2.0/SHA256SUMS"},
             {"name":"SHA256SUMS.minisig","size":300,"state":"uploaded",
              "browser_download_url":"https://github.com/zyno-io/siple/releases/download/v0.2.0/SHA256SUMS.minisig"}]}
]"#;

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::update::releases::FIXTURE;

    type Step = Result<FetchOutcome, FetchError>;

    #[derive(Default)]
    struct Fake {
        script: Mutex<VecDeque<Step>>,
        etags: Mutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl ReleaseFetcher for Fake {
        async fn fetch(&self, etag: Option<&str>, _ua: &str) -> Step {
            self.etags.lock().unwrap().push(etag.map(str::to_string));
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(FetchError::Network("script exhausted".into())))
        }
    }

    impl Fake {
        fn with(steps: Vec<Step>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(steps.into()),
                etags: Mutex::default(),
            })
        }
        fn calls(&self) -> usize {
            self.etags.lock().unwrap().len()
        }
    }

    fn body(etag: &str, b: &str) -> Step {
        Ok(FetchOutcome::Body {
            etag: Some(etag.into()),
            body: b.as_bytes().to_vec(),
        })
    }

    #[test]
    fn site_headers() {
        assert_eq!(headers(None), [("Accept", "application/json")]);
        assert_eq!(
            headers(Some("\"e\"")).last(),
            Some(&("If-None-Match", "\"e\""))
        );
    }

    #[test]
    fn the_feed_and_the_github_fixture_parse_alike() {
        let feed = parse_releases(FEED_FIXTURE.as_bytes()).unwrap();
        let names: Vec<_> = feed.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["0.3.0-rc.1", "0.2.0"]);
        assert!(feed[0].prerelease && !feed[1].prerelease);
        assert_eq!(
            feed[1].published_at.as_deref(),
            Some("2026-11-01T10:00:00Z")
        );
        assert_eq!(feed[1].asset("SHA256SUMS").unwrap().size, 200);
        assert_eq!(feed[1].asset("SHA256SUMS.minisig").unwrap().size, 300);
        // The GitHub list of the same releases also parses (same parser, same shape).
        assert!(parse_releases(FIXTURE.as_bytes()).unwrap().len() >= 2);
    }

    #[test]
    fn usable_needs_a_non_empty_list() {
        assert!(usable(FEED_FIXTURE.as_bytes()).is_ok());
        assert!(usable(b"[]").is_err());
        assert!(usable(b"{\"message\":\"x\"}").is_err());
        assert!(usable(b"<html>").is_err());
        // Only drafts and junk: nothing the parser accepts.
        assert!(usable(br#"[{"tag_name":"v1.0.0","draft":true},{"tag_name":"nightly"}]"#).is_err());
    }

    /// Marks the GitHub budget as spent, as if GitHub was asked just now.
    fn spend(f: &FeedFetcher) {
        f.github().last_github_at = Some(Instant::now());
    }

    fn names(body: &[u8]) -> Vec<String> {
        let mut n: Vec<_> = parse_releases(body)
            .unwrap()
            .iter()
            .map(|r| r.name.clone())
            .collect();
        n.sort();
        n
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);

    #[tokio::test(start_paused = true)]
    async fn the_site_answers_first_and_its_etag_is_tagged() {
        let site = Fake::with(vec![body("\"s1\"", FEED_FIXTURE)]);
        let gh = Fake::with(vec![]);
        let f = FeedFetcher::new(site, gh.clone());
        spend(&f);
        let Ok(FetchOutcome::Body { etag, body }) = f.fetch(None, "ua").await else {
            panic!("expected a body");
        };
        assert_eq!(etag.as_deref(), Some("site:\"s1\""));
        assert_eq!(f.answered_by(), Some(FeedSource::Site));
        assert_eq!(gh.calls(), 0, "GitHub is not asked inside the window");
        assert_eq!(names(&body), ["0.2.0", "0.3.0-rc.1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_check_cross_checks_github_and_merges_by_tag() {
        let site = Fake::with(vec![body("\"s1\"", FEED_FIXTURE)]);
        let gh = Fake::with(vec![body("\"g1\"", FIXTURE)]);
        let f = FeedFetcher::new(site.clone(), gh.clone());
        let Ok(FetchOutcome::Body { etag, body }) = f.fetch(Some("site:\"old\""), "ua").await
        else {
            panic!("expected a body");
        };
        // The site's tag, and GitHub's list is merged in: the union by tag.
        assert_eq!(etag.as_deref(), Some("site:\"s1\""));
        assert_eq!(f.answered_by(), Some(FeedSource::Site));
        assert_eq!(
            names(&body),
            ["0.1.0", "0.2.0", "0.2.0-rc.2", "0.3.0-rc.1", "0.4.0"]
        );
        // GitHub's entry wins where both have the tag (the fixture's 0.2.0 has no assets).
        let merged = parse_releases(&body).unwrap();
        let v020 = merged.iter().find(|r| r.name == "0.2.0").unwrap();
        assert!(v020.asset("SHA256SUMS").is_none());
        // The cross-check needs the full site list: no validator was sent, GitHub got none either.
        assert_eq!(site.etags.lock().unwrap()[0], None);
        assert_eq!(gh.etags.lock().unwrap()[0], None);
        assert_eq!(gh.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cross_check_happens_once_a_day() {
        let site = Fake::with(vec![
            body("\"s1\"", FEED_FIXTURE),
            Ok(FetchOutcome::NotModified),
            body("\"s2\"", FEED_FIXTURE),
            Ok(FetchOutcome::NotModified),
            body("\"s3\"", FEED_FIXTURE),
        ]);
        let gh = Fake::with(vec![body("\"g1\"", FIXTURE), body("\"g2\"", FIXTURE)]);
        let f = FeedFetcher::new(site.clone(), gh.clone());
        f.fetch(None, "ua").await.unwrap();
        assert_eq!(gh.calls(), 1);
        // An hour later, and again: the site is asked conditionally, GitHub not at all.
        tokio::time::advance(Duration::from_secs(3600)).await;
        let out = f.fetch(Some("site:\"s1\""), "ua").await;
        assert!(matches!(out, Ok(FetchOutcome::NotModified)));
        assert_eq!(site.etags.lock().unwrap()[1].as_deref(), Some("\"s1\""));
        // A changed site list still carries what GitHub reported (the 0.4.0 prerelease).
        tokio::time::advance(Duration::from_secs(3600)).await;
        let Ok(FetchOutcome::Body { body, .. }) = f.fetch(Some("site:\"s1\""), "ua").await else {
            panic!("expected a body");
        };
        assert!(names(&body).contains(&"0.4.0".to_string()));
        assert_eq!(gh.calls(), 1);
        tokio::time::advance(DAY - Duration::from_secs(2 * 3600 + 60)).await;
        f.fetch(Some("site:\"s2\""), "ua").await.unwrap();
        assert_eq!(gh.calls(), 1, "still inside the 24 h window");
        // Past 24 h since the cross-check: the next check asks GitHub again, and the site
        // unconditionally (the merge needs its whole list).
        tokio::time::advance(Duration::from_secs(120)).await;
        f.fetch(Some("site:\"s2\""), "ua").await.unwrap();
        assert_eq!(gh.calls(), 2);
        assert_eq!(site.etags.lock().unwrap()[4], None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_cross_check_keeps_the_site_list() {
        for step in [
            Err(FetchError::Timeout),
            Err(FetchError::RateLimited { retry_after: None }),
            body("\"g\"", "garbage"),
            body("\"g\"", "[]"),
            Ok(FetchOutcome::NotModified),
        ] {
            let desc = format!("{step:?}");
            let site = Fake::with(vec![body("\"s1\"", FEED_FIXTURE)]);
            let gh = Fake::with(vec![step]);
            let f = FeedFetcher::new(site, gh.clone());
            let Ok(FetchOutcome::Body { etag, body }) = f.fetch(None, "ua").await else {
                panic!("{desc}: expected the site's body");
            };
            assert_eq!(etag.as_deref(), Some("site:\"s1\""), "{desc}");
            assert_eq!(names(&body), ["0.2.0", "0.3.0-rc.1"], "{desc}");
            assert_eq!(f.answered_by(), Some(FeedSource::Site), "{desc}");
            assert_eq!(gh.calls(), 1, "{desc}");
            // The failed request used the budget: no second try within the day.
            f.fetch(None, "ua").await.ok();
            assert_eq!(gh.calls(), 1, "{desc}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_site_etag_goes_back_to_the_site_and_304_is_unchanged() {
        let site = Fake::with(vec![Ok(FetchOutcome::NotModified)]);
        let gh = Fake::with(vec![]);
        let f = FeedFetcher::new(site.clone(), gh.clone());
        spend(&f);
        let out = f.fetch(Some("site:\"s1\""), "ua").await;
        assert!(matches!(out, Ok(FetchOutcome::NotModified)));
        assert_eq!(f.answered_by(), Some(FeedSource::Site));
        assert_eq!(site.etags.lock().unwrap()[0].as_deref(), Some("\"s1\""));
        assert_eq!(gh.calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn every_site_failure_falls_back_to_github() {
        let failures: Vec<Step> = vec![
            Err(FetchError::Network("dns".into())),
            Err(FetchError::Timeout),
            Err(FetchError::Status(503)),
            Err(FetchError::Status(404)),
            Err(FetchError::RateLimited { retry_after: None }),
            body("\"x\"", "{\"not\":\"a list\"}"),
            body("\"x\"", "[]"),
            body("\"x\"", "garbage"),
            // 304 although nothing conditional was sent: not trusted.
            Ok(FetchOutcome::NotModified),
        ];
        for step in failures {
            let desc = format!("{step:?}");
            let site = Fake::with(vec![step]);
            let gh = Fake::with(vec![body("\"g1\"", FIXTURE)]);
            let f = FeedFetcher::new(site, gh.clone());
            let Ok(FetchOutcome::Body { etag, .. }) = f.fetch(None, "ua").await else {
                panic!("{desc}: expected GitHub's body");
            };
            assert_eq!(etag.as_deref(), Some("github:\"g1\""), "{desc}");
            assert_eq!(f.answered_by(), Some(FeedSource::Github), "{desc}");
            assert_eq!(gh.calls(), 1, "{desc}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_github_etag_is_only_sent_to_github() {
        let site = Fake::with(vec![Err(FetchError::Timeout)]);
        let gh = Fake::with(vec![Ok(FetchOutcome::NotModified)]);
        let f = FeedFetcher::new(site.clone(), gh.clone());
        let out = f.fetch(Some("github:\"g1\""), "ua").await;
        assert!(matches!(out, Ok(FetchOutcome::NotModified)));
        assert_eq!(f.answered_by(), Some(FeedSource::Github));
        assert_eq!(site.etags.lock().unwrap()[0], None);
        assert_eq!(gh.etags.lock().unwrap()[0].as_deref(), Some("\"g1\""));
    }

    #[tokio::test(start_paused = true)]
    async fn when_both_fail_the_github_error_is_reported() {
        let site = Fake::with(vec![Err(FetchError::Timeout)]);
        let gh = Fake::with(vec![Err(FetchError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(60)),
        })]);
        let f = FeedFetcher::new(site, gh);
        assert!(matches!(
            f.fetch(None, "ua").await,
            Err(FetchError::RateLimited { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn the_github_fallback_happens_once_then_is_refused_within_the_window() {
        let site = Fake::with(vec![
            Err(FetchError::Timeout),
            Err(FetchError::Status(503)),
            Err(FetchError::Network("dns".into())),
            Err(FetchError::Timeout),
        ]);
        let gh = Fake::with(vec![body("\"g1\"", FIXTURE), body("\"g2\"", FIXTURE)]);
        let f = FeedFetcher::new(site, gh.clone());
        let Ok(FetchOutcome::Body { etag, .. }) = f.fetch(None, "ua").await else {
            panic!("expected GitHub's body");
        };
        assert_eq!(etag.as_deref(), Some("github:\"g1\""));
        assert_eq!(gh.calls(), 1);
        // Inside the window the site's own error comes back, GitHub is not asked.
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(
            f.fetch(None, "ua").await.unwrap_err(),
            FetchError::Status(503)
        );
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(
            f.fetch(None, "ua").await.unwrap_err(),
            FetchError::Network("dns".into())
        );
        assert_eq!(gh.calls(), 1);
        // A day after the request, the fallback is open again.
        tokio::time::advance(DAY).await;
        assert!(f.fetch(None, "ua").await.is_ok());
        assert_eq!(gh.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_fallback_reports_the_sites_failure_kinds() {
        for e in [
            FetchError::Timeout,
            FetchError::RateLimited { retry_after: None },
            FetchError::Unreadable("x".into()),
        ] {
            let site = Fake::with(vec![Err(match &e {
                FetchError::Timeout => FetchError::Timeout,
                FetchError::RateLimited { .. } => FetchError::RateLimited { retry_after: None },
                _ => FetchError::Unreadable("x".into()),
            })]);
            let f = FeedFetcher::new(site, Fake::with(vec![]));
            spend(&f);
            assert_eq!(f.fetch(None, "ua").await.unwrap_err(), e);
        }
        // An unusable site body inside the window is an error too, not a success.
        let site = Fake::with(vec![body("\"x\"", "[]")]);
        let f = FeedFetcher::new(site, Fake::with(vec![]));
        spend(&f);
        assert!(matches!(
            f.fetch(None, "ua").await,
            Err(FetchError::Unreadable(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn the_cross_check_and_the_fallback_share_one_budget() {
        // Cross-check first: a later site failure inside the day gets no fallback.
        let site = Fake::with(vec![body("\"s1\"", FEED_FIXTURE), Err(FetchError::Timeout)]);
        let gh = Fake::with(vec![body("\"g1\"", FIXTURE), body("\"g2\"", FIXTURE)]);
        let f = FeedFetcher::new(site, gh.clone());
        f.fetch(None, "ua").await.unwrap();
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(f.fetch(None, "ua").await.unwrap_err(), FetchError::Timeout);
        assert_eq!(gh.calls(), 1);

        // Fallback first: a later site answer inside the day gets no cross-check.
        let site = Fake::with(vec![Err(FetchError::Timeout), body("\"s1\"", FEED_FIXTURE)]);
        let gh = Fake::with(vec![body("\"g1\"", FIXTURE), body("\"g2\"", FIXTURE)]);
        let f = FeedFetcher::new(site, gh.clone());
        f.fetch(None, "ua").await.unwrap();
        assert_eq!(f.answered_by(), Some(FeedSource::Github));
        tokio::time::advance(Duration::from_secs(3600)).await;
        let Ok(FetchOutcome::Body { body, .. }) = f.fetch(None, "ua").await else {
            panic!("expected the site's body");
        };
        assert_eq!(f.answered_by(), Some(FeedSource::Site));
        assert_eq!(gh.calls(), 1);
        // The fallback's list is merged into the site's answer.
        assert!(names(&body).contains(&"0.4.0".to_string()));
    }
}
