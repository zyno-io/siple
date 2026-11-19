//! The GitHub releases list: parsing and choosing the newest eligible release.

use serde_json::Value;
use siple_config::UpdateChannel;

use super::links::release_url;
use super::version::Version;

/// One published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Parsed tag.
    pub version: Version,
    /// Tag without the leading `v`, as written in the release (`0.2.0-rc.1`).
    pub name: String,
    /// The release page.
    pub url: String,
    /// `published_at` (RFC 3339), when GitHub gives it.
    pub published_at: Option<String>,
    /// Marked as a pre-release on GitHub, or a pre-release version.
    pub prerelease: bool,
    /// The uploaded assets, by name and size. Only the names and sizes are used: a download URL
    /// is always built from the validated version and the asset name, never taken from here.
    pub assets: Vec<Asset>,
}

/// One uploaded release asset as the API lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// File name (no path; see [`super::sums::valid_asset_name`]).
    pub name: String,
    /// Size in bytes, as the API reports it.
    pub size: u64,
}

impl Release {
    /// The asset called `name`, if the release has it.
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Parses the body of `GET /repos/{owner}/{repo}/releases`. Drafts, entries whose tag is not a
/// version and entries that are not objects are skipped; a body that is not a JSON array is an
/// error.
pub fn parse_releases(body: &[u8]) -> Result<Vec<Release>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let Value::Array(items) = v else {
        return Err("unexpected response (not a list of releases)".into());
    };
    Ok(items.iter().filter_map(parse_one).collect())
}

fn parse_one(v: &Value) -> Option<Release> {
    if v.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let tag = v.get("tag_name")?.as_str()?;
    let version = Version::parse(tag)?;
    // Everything shown or opened is rebuilt from the validated version: the response's own
    // `html_url` and spelling of the tag are never used.
    let name = version.to_string();
    let url = release_url(&name);
    let prerelease = version.is_prerelease()
        || v.get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let published_at = v
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
        .map(str::to_string);
    Some(Release {
        version,
        name,
        url,
        published_at,
        prerelease,
        assets: parse_assets(v),
    })
}

/// The `assets[]` that are fully uploaded and have a plain file name.
fn parse_assets(v: &Value) -> Vec<Asset> {
    let Some(items) = v.get("assets").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|a| {
            if a.get("state").and_then(Value::as_str) != Some("uploaded") {
                return None;
            }
            let name = a.get("name")?.as_str()?;
            if !super::sums::valid_asset_name(name) {
                return None;
            }
            Some(Asset {
                name: name.to_string(),
                size: a.get("size")?.as_u64()?,
            })
        })
        .collect()
}

/// The highest release the channel allows (stable: no pre-releases), whether or not it is newer
/// than the running version.
pub fn pick(releases: &[Release], channel: UpdateChannel) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| channel == UpdateChannel::Prerelease || !r.prerelease)
        .max_by(|a, b| a.version.cmp(&b.version))
}

#[cfg(test)]
pub(crate) const FIXTURE: &str = r#"[
  {"tag_name":"v0.3.0-rc.1","draft":false,"prerelease":true,"html_url":"https://github.com/zyno-io/siple/releases/tag/v0.3.0-rc.1","published_at":"2026-11-10T10:00:00Z"},
  {"tag_name":"v0.9.0","draft":true,"prerelease":false,"html_url":"https://github.com/zyno-io/siple/releases/tag/v0.9.0","published_at":null},
  {"tag_name":"v0.2.0","draft":false,"prerelease":false,"html_url":"https://github.com/zyno-io/siple/releases/tag/v0.2.0","published_at":"2026-11-01T10:00:00Z","body":"notes"},
  {"tag_name":"nightly","draft":false,"prerelease":true,"html_url":"https://github.com/zyno-io/siple/releases/tag/nightly"},
  {"tag_name":"v0.2.0-rc.2","draft":false,"prerelease":true,"html_url":"https://example.com/phish"},
  {"tag_name":"v0.1.0","draft":false,"prerelease":false,"html_url":"https://github.com/zyno-io/siple/releases/tag/v0.1.0"},
  {"tag_name":"v0.4.0","prerelease":true,"html_url":"https://github.com/zyno-io/siple/releases/tag/v0.4.0"},
  {"draft":false},
  42,
  "text"
]"#;

#[cfg(test)]
mod tests {
    use super::super::version::effective_channel;
    use super::*;

    fn names(r: &[Release]) -> Vec<&str> {
        r.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn fixture_skips_drafts_and_garbage() {
        let r = parse_releases(FIXTURE.as_bytes()).unwrap();
        assert_eq!(
            names(&r),
            ["0.3.0-rc.1", "0.2.0", "0.2.0-rc.2", "0.1.0", "0.4.0"]
        );
        let rc2 = &r[2];
        assert!(rc2.prerelease);
        // The response's html_url is never used: the page is built from the validated tag.
        assert_eq!(
            rc2.url,
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0-rc.2"
        );
        assert_eq!(r[1].published_at.as_deref(), Some("2026-11-01T10:00:00Z"));
        assert!(r[4].prerelease, "flagged prerelease without an rc tag");
    }

    #[test]
    fn html_url_is_ignored_and_names_are_normalized() {
        let body = br#"[
          {"tag_name":"v1.0.0","html_url":"https://github.com/zyno-io/siple/../../evil/repo/releases","published_at":"not a date"},
          {"tag_name":"v1.1.0-rc.1+meta.2","html_url":"https://github.com/zyno-io/siple/releases/tag/v1.1.0-rc.1+meta.2"},
          {"tag_name":"v1.2.0+bad meta"}
        ]"#;
        let r = parse_releases(body).unwrap();
        assert_eq!(names(&r), ["1.0.0", "1.1.0-rc.1"]);
        assert_eq!(
            r[0].url,
            "https://github.com/zyno-io/siple/releases/tag/v1.0.0"
        );
        assert_eq!(
            r[1].url,
            "https://github.com/zyno-io/siple/releases/tag/v1.1.0-rc.1"
        );
        assert_eq!(r[0].published_at, None);
    }

    #[test]
    fn assets_are_taken_by_name_and_size_only() {
        let body = br#"[{"tag_name":"v1.0.0","assets":[
          {"name":"SHA256SUMS","size":555,"state":"uploaded","browser_download_url":"https://evil.example/x"},
          {"name":"Siple-1.0.0-macos.zip","size":9,"state":"starter"},
          {"name":"../evil","size":1,"state":"uploaded"},
          {"name":"a/b","size":1,"state":"uploaded"},
          {"name":"nosize","state":"uploaded"},
          {"name":"Siple-1.0.0-windows-x64.zip","size":42,"state":"uploaded"},
          7
        ]},{"tag_name":"v1.1.0"},{"tag_name":"v1.2.0","assets":"x"}]"#;
        let r = parse_releases(body).unwrap();
        let a: Vec<_> = r[0]
            .assets
            .iter()
            .map(|a| (a.name.as_str(), a.size))
            .collect();
        assert_eq!(
            a,
            [("SHA256SUMS", 555), ("Siple-1.0.0-windows-x64.zip", 42)]
        );
        assert_eq!(r[0].asset("SHA256SUMS").unwrap().size, 555);
        assert!(r[1].assets.is_empty() && r[2].assets.is_empty());
    }

    #[test]
    fn rejects_non_lists() {
        assert!(parse_releases(b"{\"message\":\"Not Found\"}").is_err());
        assert!(parse_releases(b"<html>").is_err());
        assert_eq!(parse_releases(b"[]").unwrap(), vec![]);
    }

    #[test]
    fn channel_selection() {
        let r = parse_releases(FIXTURE.as_bytes()).unwrap();
        assert_eq!(pick(&r, UpdateChannel::Stable).unwrap().name, "0.2.0");
        assert_eq!(pick(&r, UpdateChannel::Prerelease).unwrap().name, "0.4.0");
        assert!(pick(&[], UpdateChannel::Stable).is_none());
        let only_pre: Vec<_> = r.iter().filter(|r| r.prerelease).cloned().collect();
        assert!(pick(&only_pre, UpdateChannel::Stable).is_none());
    }

    #[test]
    fn automatic_channel_follows_the_running_build() {
        let rc = Version::parse("0.2.0-rc.1").unwrap();
        let rel = Version::parse("0.2.0").unwrap();
        assert_eq!(effective_channel(None, &rc), UpdateChannel::Prerelease);
        assert_eq!(effective_channel(None, &rel), UpdateChannel::Stable);
        assert_eq!(
            effective_channel(Some(UpdateChannel::Stable), &rc),
            UpdateChannel::Stable
        );
        assert_eq!(
            effective_channel(Some(UpdateChannel::Prerelease), &rel),
            UpdateChannel::Prerelease
        );
    }
}
