//! The site feed: direct builds ask siple.app, one anonymous HTTPS GET of a static JSON list (no
//! rate limit; an ETag makes an unchanged answer a 304). Nothing else is asked: the update check
//! sends no request to GitHub. Cargo feature `store` compiles this module out.
//!
//! The feed has the shape of the GitHub releases API, reduced to what [`super::releases`] reads
//! (no release notes), so the same parser and selection apply. It is unsigned and only a hint: the
//! installer trusts nothing in it beyond the version and the asset names and sizes. Downloads are
//! built from the validated version, and the minisign-signed `SHA256SUMS` of exactly that version
//! decides what is installed (see [`super::sums::verify_sums`]).

use std::time::Duration;

use async_trait::async_trait;

use super::FeedSource;
use super::checker::{FetchError, FetchOutcome, ReleaseFetcher};
use super::http::{get, interpret};
use super::releases::parse_releases;

/// How long the request may take before the check gives up on it.
pub(crate) const SITE_TIMEOUT: Duration = Duration::from_secs(8);

const HOST: &str = "siple.app";
/// `v1` leaves room for a schema change without breaking installed apps.
const PATH: &str = "/updates/v1/releases.json";

/// The request headers besides the four every request carries.
pub(crate) fn headers(etag: Option<&str>) -> Vec<(&'static str, &str)> {
    let mut h = vec![("Accept", "application/json")];
    if let Some(t) = etag {
        h.push(("If-None-Match", t));
    }
    h
}

/// The feed is usable: a list of releases with at least one entry the parser accepts.
fn usable(body: &[u8]) -> Result<(), String> {
    match parse_releases(body) {
        Ok(l) if l.is_empty() => Err("no releases in the feed".into()),
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

/// An answer the checker can use: a 304 only to a conditional request, a body only when usable.
fn vet(out: FetchOutcome, conditional: bool) -> Result<FetchOutcome, FetchError> {
    match out {
        FetchOutcome::NotModified if conditional => Ok(out),
        FetchOutcome::NotModified => Err(FetchError::Unreadable(
            "answered 304 to an unconditional request".into(),
        )),
        FetchOutcome::Body { ref body, .. } => match usable(body) {
            Ok(()) => Ok(out),
            Err(e) => Err(FetchError::Unreadable(format!("unreadable feed: {e}"))),
        },
    }
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
        let out = interpret(resp, chrono::Utc::now().timestamp().max(0) as u64)?;
        vet(out, etag.is_some())
    }

    fn answered_by(&self) -> Option<FeedSource> {
        Some(FeedSource::Site)
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
    use super::*;
    use crate::update::releases::FIXTURE;

    fn body(b: &str) -> FetchOutcome {
        FetchOutcome::Body {
            etag: Some("\"e\"".into()),
            body: b.as_bytes().to_vec(),
        }
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
        // The GitHub API list of the same releases also parses (same parser, same shape).
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

    #[test]
    fn vet_accepts_a_usable_body_and_a_conditional_304_only() {
        assert!(vet(body(FEED_FIXTURE), false).is_ok());
        assert!(vet(FetchOutcome::NotModified, true).is_ok());
        for bad in [body("[]"), body("garbage"), body("{\"not\":\"a list\"}")] {
            assert!(matches!(vet(bad, false), Err(FetchError::Unreadable(_))));
        }
        // 304 although nothing conditional was sent: not trusted.
        assert!(matches!(
            vet(FetchOutcome::NotModified, false),
            Err(FetchError::Unreadable(_))
        ));
    }

    #[test]
    fn the_site_is_always_the_feed_source() {
        assert_eq!(SiteFetcher.answered_by(), Some(FeedSource::Site));
    }
}
