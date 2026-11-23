//! The site feed: direct builds ask siple.app first (a static JSON list, no rate limit) and fall
//! back to the GitHub releases API in the same check. Cargo feature `store` compiles it out, like
//! [`super::github`].
//!
//! The feed has the shape of the GitHub releases API, reduced to what [`super::releases`] reads, so
//! the same parser and selection apply. It is unsigned and only a hint: the installer trusts
//! nothing in it beyond the version and the asset names and sizes. Downloads are built from the
//! validated version, and the minisign-signed `SHA256SUMS` of exactly that version decides what
//! is installed (see [`super::sums::verify_sums`]).

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;

use super::FeedSource;
use super::checker::{FetchError, FetchOutcome, ReleaseFetcher};
use super::http::{REQUEST_TIMEOUT, get, interpret};
use super::releases::parse_releases;

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
        let resp =
            tokio::time::timeout(REQUEST_TIMEOUT, get(HOST, PATH, &headers(etag), user_agent))
                .await
                .map_err(|_| FetchError::Timeout)??;
        interpret(resp, chrono::Utc::now().timestamp().max(0) as u64)
    }
}

/// Site first, GitHub when the site does not give a usable answer. Remembers which one answered
/// ([`ReleaseFetcher::answered_by`]) and tags the ETag it returns with that source, so a later
/// conditional request goes to the host that issued the tag.
pub struct FeedFetcher {
    site: Arc<dyn ReleaseFetcher>,
    github: Arc<dyn ReleaseFetcher>,
    answered: Mutex<Option<FeedSource>>,
}

impl FeedFetcher {
    /// A fetcher over the two sources (the real ones, or fakes in tests).
    pub fn new(site: Arc<dyn ReleaseFetcher>, github: Arc<dyn ReleaseFetcher>) -> Self {
        Self {
            site,
            github,
            answered: Mutex::new(None),
        }
    }

    /// The real thing: siple.app, then GitHub.
    pub fn network() -> Self {
        Self::new(Arc::new(SiteFetcher), Arc::new(super::GithubFetcher))
    }

    fn set(&self, s: FeedSource) {
        *self.answered.lock().unwrap_or_else(PoisonError::into_inner) = Some(s);
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

#[async_trait]
impl ReleaseFetcher for FeedFetcher {
    async fn fetch(
        &self,
        etag: Option<&str>,
        user_agent: &str,
    ) -> Result<FetchOutcome, FetchError> {
        let site_etag = etag.and_then(|t| t.strip_prefix(SITE_TAG));
        let github_etag = etag.and_then(|t| t.strip_prefix(GITHUB_TAG));
        let why = match self.site.fetch(site_etag, user_agent).await {
            Ok(FetchOutcome::NotModified) if site_etag.is_some() => {
                self.set(FeedSource::Site);
                tracing::debug!("update feed: siple.app says unchanged");
                return Ok(FetchOutcome::NotModified);
            }
            Ok(FetchOutcome::NotModified) => {
                String::from("answered 304 to an unconditional request")
            }
            Ok(FetchOutcome::Body { etag, body }) => match usable(&body) {
                Ok(()) => {
                    self.set(FeedSource::Site);
                    tracing::debug!("update feed: siple.app answered");
                    return Ok(FetchOutcome::Body {
                        etag: tag(SITE_TAG, etag),
                        body,
                    });
                }
                Err(e) => format!("unreadable feed: {e}"),
            },
            Err(e) => e.to_string(),
        };
        tracing::info!(reason = %why, "update feed: siple.app gave no usable answer; asking GitHub");
        let out = self.github.fetch(github_etag, user_agent).await?;
        self.set(FeedSource::Github);
        tracing::info!("update feed: GitHub answered");
        Ok(match out {
            FetchOutcome::Body { etag, body } => FetchOutcome::Body {
                etag: tag(GITHUB_TAG, etag),
                body,
            },
            FetchOutcome::NotModified => FetchOutcome::NotModified,
        })
    }

    fn answered_by(&self) -> Option<FeedSource> {
        *self.answered.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The shape siple.app publishes: what [`parse_releases`] reads of the GitHub API, nothing else.
#[cfg(test)]
pub(crate) const FEED_FIXTURE: &str = r#"[
  {"tag_name":"v0.3.0-rc.1","name":"v0.3.0-rc.1","draft":false,"prerelease":true,
   "published_at":"2026-11-10T10:00:00Z",
   "html_url":"https://github.com/zyno-io/siple/releases/tag/v0.3.0-rc.1","body":"rc notes",
   "assets":[{"name":"SHA256SUMS","size":300,"state":"uploaded",
              "browser_download_url":"https://github.com/zyno-io/siple/releases/download/v0.3.0-rc.1/SHA256SUMS"}]},
  {"tag_name":"v0.2.0","name":"v0.2.0","draft":false,"prerelease":false,
   "published_at":"2026-11-01T10:00:00Z",
   "html_url":"https://github.com/zyno-io/siple/releases/tag/v0.2.0","body":"notes",
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

    #[tokio::test]
    async fn the_site_answers_first_and_its_etag_is_tagged() {
        let site = Fake::with(vec![body("\"s1\"", FEED_FIXTURE)]);
        let gh = Fake::with(vec![]);
        let f = FeedFetcher::new(site, gh.clone());
        let Ok(FetchOutcome::Body { etag, .. }) = f.fetch(None, "ua").await else {
            panic!("expected a body");
        };
        assert_eq!(etag.as_deref(), Some("site:\"s1\""));
        assert_eq!(f.answered_by(), Some(FeedSource::Site));
        assert_eq!(gh.calls(), 0, "GitHub is not asked when the site answers");
    }

    #[tokio::test]
    async fn a_site_etag_goes_back_to_the_site_and_304_is_unchanged() {
        let site = Fake::with(vec![Ok(FetchOutcome::NotModified)]);
        let gh = Fake::with(vec![]);
        let f = FeedFetcher::new(site.clone(), gh.clone());
        let out = f.fetch(Some("site:\"s1\""), "ua").await;
        assert!(matches!(out, Ok(FetchOutcome::NotModified)));
        assert_eq!(f.answered_by(), Some(FeedSource::Site));
        assert_eq!(site.etags.lock().unwrap()[0].as_deref(), Some("\"s1\""));
        assert_eq!(gh.calls(), 0);
    }

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
}
