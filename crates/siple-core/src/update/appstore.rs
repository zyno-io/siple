//! The Mac App Store source: one anonymous iTunes lookup of Siple's own listing.
//!
//! `GET https://itunes.apple.com/lookup?id=<trackId>` with `User-Agent: Siple/<version>` as the
//! only identifying header. The answer is trusted only when `trackId` and `bundleId` are exactly
//! ours and `version` parses; `trackViewUrl` (and any other link) in it is never used: the page
//! that opens comes from [`super::links::MAS_STORE_URL`]. A listing whose `minimumOsVersion` is
//! above the running OS is not offered (the App Store would not install it either). TestFlight
//! builds make no request at all.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::checker::{FetchError, FetchOutcome};
use super::http::{REQUEST_TIMEOUT, get, interpret};
use super::version::Version;

/// The App Store id of Siple.
pub const TRACK_ID: u64 = 6836164262;
/// The bundle id the listing must carry.
pub const BUNDLE_ID: &str = "io.zyno.siple";

const HOST: &str = "itunes.apple.com";
const PATH: &str = "/lookup?id=6836164262";

/// An OS version (`major.minor.patch`), for comparing with `minimumOsVersion`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OsVersion(pub u32, pub u32, pub u32);

impl OsVersion {
    /// Matches every minimum (platforms where the App Store check does not apply).
    pub const ANY: OsVersion = OsVersion(u32::MAX, u32::MAX, u32::MAX);

    /// One to three numeric parts (`26`, `26.6`, `15.4.1`); missing parts are 0.
    pub fn parse(s: &str) -> Option<OsVersion> {
        let mut parts = [0u32; 3];
        for (n, p) in s.trim().split('.').enumerate() {
            if n == 3 || p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            parts[n] = p.parse().ok()?;
        }
        Some(OsVersion(parts[0], parts[1], parts[2]))
    }
}

/// The version of the running macOS (`NSProcessInfo`); [`OsVersion::ANY`] elsewhere.
pub fn running_os() -> OsVersion {
    #[cfg(target_os = "macos")]
    {
        let v = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
        OsVersion(
            v.majorVersion.max(0) as u32,
            v.minorVersion.max(0) as u32,
            v.patchVersion.max(0) as u32,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        OsVersion::ANY
    }
}

/// Whether the App Store receipt file is named like a TestFlight one.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_sandbox_receipt(file_name: &str) -> bool {
    file_name == "sandboxReceipt"
}

/// Whether this process is a TestFlight build (its receipt is `sandboxReceipt`).
pub fn running_from_testflight() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_foundation::NSBundle::mainBundle()
            .appStoreReceiptURL()
            .and_then(|u| u.lastPathComponent())
            .is_some_and(|n| is_sandbox_receipt(&n.to_string()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// What the App Store lists for Siple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreListing {
    /// The listed version.
    pub version: Version,
    /// `currentVersionReleaseDate` (RFC 3339), when Apple gives a valid one.
    pub released_at: Option<String>,
}

/// Parses the body of the lookup. `Ok(None)`: nothing to offer (no result, or a listing that
/// needs a newer OS than `os`). `Err`: the answer is not what Siple's listing looks like.
pub fn parse_lookup(body: &[u8], os: OsVersion) -> Result<Option<StoreListing>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .ok_or("unexpected response (no results list)")?;
    let Some(entry) = results.first() else {
        return Ok(None);
    };
    if entry.get("trackId").and_then(Value::as_u64) != Some(TRACK_ID) {
        return Err("the result is not Siple's listing (trackId)".into());
    }
    if entry.get("bundleId").and_then(Value::as_str) != Some(BUNDLE_ID) {
        return Err("the result is not Siple's listing (bundleId)".into());
    }
    let version = entry
        .get("version")
        .and_then(Value::as_str)
        .and_then(Version::parse)
        .ok_or("the listing has no valid version")?;
    if let Some(min) = entry.get("minimumOsVersion") {
        let min = min
            .as_str()
            .and_then(OsVersion::parse)
            .ok_or("the listing has an invalid minimumOsVersion")?;
        if min > os {
            return Ok(None);
        }
    }
    let released_at = entry
        .get("currentVersionReleaseDate")
        .and_then(Value::as_str)
        .filter(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
        .map(str::to_string);
    Ok(Some(StoreListing {
        version,
        released_at,
    }))
}

/// The network side of the App Store check; replaced in tests.
#[async_trait]
pub trait LookupFetcher: Send + Sync {
    /// Looks Siple up. `Ok(None)`: nothing to offer.
    async fn lookup(&self, user_agent: &str) -> Result<Option<StoreListing>, FetchError>;
}

/// The lookup of an app whose host did not opt in to the network check: never touches it.
pub struct OfflineLookup;

#[async_trait]
impl LookupFetcher for OfflineLookup {
    async fn lookup(&self, _ua: &str) -> Result<Option<StoreListing>, FetchError> {
        Err(FetchError::Network(
            "the update check is not enabled".into(),
        ))
    }
}

/// The real lookup against `itunes.apple.com`.
pub struct ItunesFetcher {
    os: Arc<dyn Fn() -> OsVersion + Send + Sync>,
    testflight: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl ItunesFetcher {
    /// Uses the running OS and receipt.
    pub fn new() -> Self {
        Self::with(running_os, running_from_testflight)
    }

    /// Injects the OS version and the TestFlight probe.
    pub fn with(
        os: impl Fn() -> OsVersion + Send + Sync + 'static,
        testflight: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            os: Arc::new(os),
            testflight: Arc::new(testflight),
        }
    }
}

impl Default for ItunesFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LookupFetcher for ItunesFetcher {
    async fn lookup(&self, user_agent: &str) -> Result<Option<StoreListing>, FetchError> {
        if (self.testflight)() {
            tracing::debug!("TestFlight build: no App Store lookup");
            return Ok(None);
        }
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, get(HOST, PATH, &[], user_agent))
            .await
            .map_err(|_| FetchError::Timeout)??;
        match interpret(resp, chrono::Utc::now().timestamp().max(0) as u64)? {
            FetchOutcome::Body { body, .. } => {
                parse_lookup(&body, (self.os)()).map_err(FetchError::Unreadable)
            }
            FetchOutcome::NotModified => Ok(None),
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    pub const MAC_26_6: super::OsVersion = super::OsVersion(26, 6, 0);

    pub fn listing(version: &str, min_os: &str) -> String {
        format!(
            r#"{{"resultCount":1,"results":[{{"kind":"mac-software","trackId":6836164262,"bundleId":"io.zyno.siple","version":"{version}","minimumOsVersion":"{min_os}","currentVersionReleaseDate":"2026-11-10T08:00:00Z","trackViewUrl":"https://evil.example/phish"}}]}}"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn parse(body: &str) -> Result<Option<StoreListing>, String> {
        parse_lookup(body.as_bytes(), MAC_26_6)
    }

    #[test]
    fn a_valid_listing_is_parsed() {
        let l = parse(&listing("0.2.0", "14.0")).unwrap().unwrap();
        assert_eq!(l.version, Version::parse("0.2.0").unwrap());
        assert_eq!(l.released_at.as_deref(), Some("2026-11-10T08:00:00Z"));
    }

    #[test]
    fn zero_results_is_no_offer_not_an_error() {
        assert_eq!(parse(r#"{"resultCount":0,"results":[]}"#), Ok(None));
    }

    #[test]
    fn foreign_listings_and_garbage_are_errors() {
        let ok = listing("0.2.0", "14.0");
        for bad in [
            ok.replace("6836164262", "1234"),
            ok.replace("io.zyno.siple", "com.example.other"),
            ok.replace("\"0.2.0\"", "\"banana\""),
            ok.replace("\"version\":\"0.2.0\",", ""),
            ok.replace("\"14.0\"", "\"x.y\""),
            ok.replace("\"14.0\"", "14"),
            r#"{"resultCount":1}"#.into(),
            r#"[1,2]"#.into(),
            "<html>".into(),
            String::new(),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_listing_url_is_never_taken_from_the_answer() {
        // `trackViewUrl` is in the fixture; the parsed value has no field that could carry it.
        let l = parse(&listing("0.2.0", "14.0")).unwrap().unwrap();
        assert!(!format!("{l:?}").contains("evil"));
    }

    #[test]
    fn minimum_os_is_compared_with_the_running_os() {
        assert!(parse(&listing("0.2.0", "26.6")).unwrap().is_some());
        assert!(parse(&listing("0.2.0", "26")).unwrap().is_some());
        assert!(parse(&listing("0.2.0", "26.6.1")).unwrap().is_none());
        assert!(parse(&listing("0.2.0", "27.0")).unwrap().is_none());
        assert_eq!(OsVersion::parse("15"), Some(OsVersion(15, 0, 0)));
        assert_eq!(OsVersion::parse("15.4.1"), Some(OsVersion(15, 4, 1)));
        for bad in ["", "15.", ".4", "1.2.3.4", "a", "1.-2", "1 .2"] {
            assert_eq!(OsVersion::parse(bad), None, "{bad:?}");
        }
        assert!(OsVersion::ANY >= OsVersion(99, 99, 99));
    }

    #[test]
    fn only_the_sandbox_receipt_marks_testflight() {
        assert!(is_sandbox_receipt("sandboxReceipt"));
        assert!(!is_sandbox_receipt("receipt"));
    }

    #[tokio::test]
    async fn testflight_makes_no_request() {
        // The host does not resolve to anything here; a request would fail, not return Ok.
        let f = ItunesFetcher::with(|| MAC_26_6, || true);
        assert_eq!(f.lookup("Siple/0.1.0").await.unwrap(), None);
    }
}
