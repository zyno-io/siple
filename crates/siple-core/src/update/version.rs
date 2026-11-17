//! Semantic versions: parsing, precedence (including `-rc.N` pre-releases) and the full version
//! string of this build.

use std::cmp::Ordering;
use std::fmt;
use std::sync::OnceLock;

/// One dot-separated pre-release identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Alpha(String),
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
            // Numeric identifiers always rank below alphanumeric ones.
            (Ident::Num(_), Ident::Alpha(_)) => Ordering::Less,
            (Ident::Alpha(_), Ident::Num(_)) => Ordering::Greater,
            (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A semantic version (`1.2.3`, `1.2.3-rc.4`). Build metadata (`+...`) is ignored, as the
/// precedence rules say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Ident>,
}

impl Version {
    /// Parses `1.2.3`, `v1.2.3`, `1.2.3-rc.4` or `1.2.3+meta`. `None` for anything else.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = match s.split_once('+') {
            // Build metadata: dot-separated, non-empty `[0-9A-Za-z-]` identifiers.
            Some((v, meta)) => {
                if !meta.split('.').all(|id| {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                }) {
                    return None;
                }
                v
            }
            None => s,
        };
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut nums = core.split('.');
        let major = number(nums.next()?)?;
        let minor = number(nums.next()?)?;
        let patch = number(nums.next()?)?;
        if nums.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => p
                .split('.')
                .map(|id| {
                    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        return None;
                    }
                    if id.bytes().all(|b| b.is_ascii_digit()) {
                        number(id).map(Ident::Num)
                    } else {
                        Some(Ident::Alpha(id.to_string()))
                    }
                })
                .collect::<Option<Vec<_>>>()?,
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// Whether this is a pre-release (`-rc.N`, ...).
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

/// A numeric component: digits only, no leading zeros.
fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A release outranks its own pre-releases.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, id) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match id {
                Ident::Num(n) => write!(f, "{n}")?,
                Ident::Alpha(a) => f.write_str(a)?,
            }
        }
        Ok(())
    }
}

/// The channel in effect: the configured one, else `prerelease` while the running build is a
/// pre-release and `stable` otherwise.
pub fn effective_channel(
    configured: Option<siple_config::UpdateChannel>,
    current: &Version,
) -> siple_config::UpdateChannel {
    use siple_config::UpdateChannel;
    configured.unwrap_or(if current.is_prerelease() {
        UpdateChannel::Prerelease
    } else {
        UpdateChannel::Stable
    })
}

/// The full version of a build from the Cargo version and the release tag it was built from
/// (`SIPLE_BUILD_TAG`, set by the release workflow on tag builds). The tag counts only when it is
/// a valid version that starts with the Cargo version (`v0.2.0-rc.4` for `0.2.0`); anything else
/// (no tag, a tag of another version) leaves the Cargo version.
pub fn resolve_version(cargo: &str, tag: Option<&str>) -> String {
    let Some(tag) = tag.map(str::trim).filter(|t| !t.is_empty()) else {
        return cargo.to_string();
    };
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    // The normalized form is what is shown: no raw suffix, no build metadata.
    match Version::parse(tag).map(|v| v.to_string()) {
        Some(v)
            if v.strip_prefix(cargo)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-')) =>
        {
            v
        }
        _ => cargo.to_string(),
    }
}

/// The full version of this build: the Cargo version, plus the `-rc.N` suffix on release
/// candidates (see [`resolve_version`]). Used for display, `status.version`, the User-Agent of
/// the update check and the comparison with newer releases.
pub fn full_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| resolve_version(env!("CARGO_PKG_VERSION"), option_env!("SIPLE_BUILD_TAG")))
}

/// [`full_version`] as a [`Version`] (the Cargo version if the string somehow does not parse).
pub fn current_version() -> Version {
    Version::parse(full_version())
        .or_else(|| Version::parse(env!("CARGO_PKG_VERSION")))
        .unwrap_or(Version {
            major: 0,
            minor: 0,
            patch: 0,
            pre: Vec::new(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} must parse"))
    }

    #[test]
    fn parses_and_rejects() {
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc.4").to_string(), "1.2.3-rc.4");
        assert_eq!(v("1.2.3+build.5").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc.1+a-b.7").to_string(), "1.2.3-rc.1");
        assert!(v("0.2.0-rc.1").is_prerelease() && !v("0.2.0").is_prerelease());
        for bad in [
            "",
            "v",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.x",
            "latest",
            "1.2.3-",
            "1.2.3-rc..1",
            "1.2.3-rc.01",
            "nightly-2026",
            "1.2.3-r_c",
            "-1.2.3",
            "1.2.3+",
            "1.2.3+a..b",
            "1.2.3+a+b",
            "1.2.3+a_b",
            "1.2.3-rc.1+",
            "1.2.3+.a",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn precedence_follows_semver() {
        // The chain from the semver spec, plus rc numbering.
        let chain = [
            "0.1.0",
            "0.2.0-alpha",
            "0.2.0-alpha.1",
            "0.2.0-alpha.beta",
            "0.2.0-beta",
            "0.2.0-rc.1",
            "0.2.0-rc.2",
            "0.2.0-rc.10",
            "0.2.0",
            "0.2.1",
            "0.10.0",
            "1.0.0",
        ];
        for w in chain.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
            assert!(v(w[1]) > v(w[0]));
        }
        assert_eq!(v("1.0.0+a").cmp(&v("1.0.0+b")), Ordering::Equal);
        assert!(v("0.2.0-rc.9") < v("0.2.0-rc.10"), "numeric, not lexical");
    }

    #[test]
    fn build_tag_resolution() {
        let r = resolve_version;
        assert_eq!(r("0.2.0", None), "0.2.0");
        assert_eq!(r("0.2.0", Some("")), "0.2.0");
        assert_eq!(r("0.2.0", Some("v0.2.0-rc.4")), "0.2.0-rc.4");
        assert_eq!(r("0.2.0", Some("0.2.0-rc.4")), "0.2.0-rc.4");
        assert_eq!(r("0.2.0", Some("v0.2.0")), "0.2.0");
        assert_eq!(r("0.2.0", Some(" v0.2.0-rc.4\n")), "0.2.0-rc.4");
        // A tag of another version, or not a version, is ignored.
        assert_eq!(r("0.2.0", Some("v0.3.0-rc.1")), "0.2.0");
        assert_eq!(r("0.2.0", Some("v0.2.01")), "0.2.0");
        assert_eq!(r("0.2.0", Some("v0.2.0rc1")), "0.2.0");
        assert_eq!(r("0.2.0", Some("v0.2.0-")), "0.2.0");
        assert_eq!(r("0.2.0", Some("nightly")), "0.2.0");
        assert_eq!(r("0.2.0", Some("refs/tags/v0.2.0")), "0.2.0");
        assert_eq!(r("0.1.0", Some("v0.1.0-rc.2")), "0.1.0-rc.2");
        // Displayed normalized: metadata dropped, invalid metadata ignored.
        assert_eq!(r("0.2.0", Some("v0.2.0-rc.4+abc.1")), "0.2.0-rc.4");
        assert_eq!(r("0.2.0", Some("v0.2.0+abc")), "0.2.0");
        assert_eq!(r("0.2.0", Some("v0.2.0-rc.4+")), "0.2.0");
        assert_eq!(r("0.2.0", Some("v0.2.0+a b")), "0.2.0");
        assert_eq!(r("0.1.0", Some("v0.1.00")), "0.1.0");
    }

    #[test]
    fn this_build_has_a_valid_full_version() {
        assert!(full_version().starts_with(env!("CARGO_PKG_VERSION")));
        assert!(Version::parse(full_version()).is_some());
    }
}
