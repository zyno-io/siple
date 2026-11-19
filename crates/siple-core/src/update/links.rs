//! Fixed links of the update flow. Everything the app opens for an update comes from here (or is
//! rebuilt from a validated version): never from a network response.

/// Where releases live; used to build the page of a release.
pub const RELEASES_URL_PREFIX: &str = "https://github.com/zyno-io/siple/";

/// Mac App Store page of Siple (`macappstore://` opens the App Store app directly).
pub const MAS_STORE_URL: &str = "macappstore://apps.apple.com/app/id6836164262";

/// Microsoft Store page of Siple.
pub const MS_STORE_URL: &str = "ms-windows-store://pdp/?productid=9NWM0517P0JR";

/// The release page of version `name` (`0.2.0`, `0.2.0-rc.1`).
pub fn release_url(name: &str) -> String {
    format!("{RELEASES_URL_PREFIX}releases/tag/v{name}")
}

/// Whether `url` is exactly `https://github.com/zyno-io/siple/releases/tag/v<version>` with a
/// version made of `[0-9A-Za-z.-]`, no empty or dot-leading segment (so no `..`, `/.` or other path
/// trickery) and nothing after it.
pub fn is_release_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://github.com/zyno-io/siple/releases/tag/v") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && rest.split('.').all(|seg| !seg.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_links_are_the_expected_schemes() {
        assert!(MAS_STORE_URL.starts_with("macappstore://apps.apple.com/app/id"));
        assert!(MS_STORE_URL.starts_with("ms-windows-store://pdp/?productid="));
        assert!(!is_release_url(MAS_STORE_URL) && !is_release_url(MS_STORE_URL));
    }

    #[test]
    fn only_release_tag_pages_are_openable() {
        for ok in [
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0-rc.1",
        ] {
            assert!(is_release_url(ok), "{ok}");
        }
        for bad in [
            "https://github.com/zyno-io/siple/../../evil/repo/releases",
            "https://github.com/zyno-io/siple/releases/tag/v../../../evil",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0/../x",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0/",
            "https://github.com/zyno-io/siple/releases/tag/v..",
            "https://github.com/zyno-io/siple/releases/tag/v0..2",
            "https://github.com/zyno-io/siple/releases/tag/v",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0?x=1",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0#a",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0%2e%2e",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0 &calc",
            "https://github.com/zyno-io/siple/releases",
            "https://github.com/zyno-io/siple-evil/releases/tag/v1.0.0",
            "http://github.com/zyno-io/siple/releases/tag/v1.0.0",
            "https://example.com/zyno-io/siple/releases/tag/v1.0.0",
            "file:///etc/passwd",
            "",
        ] {
            assert!(!is_release_url(bad), "{bad}");
        }
    }
}
