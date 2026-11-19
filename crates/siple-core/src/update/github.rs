//! The GitHub releases source: direct builds only (cargo feature `store` compiles it out, so a
//! store binary contains neither this request nor the releases API path).

use async_trait::async_trait;

use super::checker::{FetchError, FetchOutcome, ReleaseFetcher};
use super::http::{REQUEST_TIMEOUT, get, interpret};

const HOST: &str = "api.github.com";
const PATH: &str = "/repos/zyno-io/siple/releases?per_page=20";

/// The request headers besides the four every request carries.
pub(crate) fn headers(etag: Option<&str>) -> Vec<(&'static str, &str)> {
    let mut h = vec![
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
    ];
    if let Some(t) = etag {
        h.push(("If-None-Match", t));
    }
    h
}

/// Fetches the releases of this project from GitHub over HTTPS.
pub struct GithubFetcher;

#[async_trait]
impl ReleaseFetcher for GithubFetcher {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_headers() {
        assert_eq!(
            headers(None),
            [
                ("Accept", "application/vnd.github+json"),
                ("X-GitHub-Api-Version", "2022-11-28")
            ]
        );
        assert_eq!(
            headers(Some("\"e\"")).last(),
            Some(&("If-None-Match", "\"e\""))
        );
    }
}
