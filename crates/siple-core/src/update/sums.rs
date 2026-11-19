//! The release's signed checksum list: minisign verification of `SHA256SUMS`, its strict parser
//! and the choice of the asset for this platform.
//!
//! The chain is: the release (from the TLS-fetched API list) names a validated version; the
//! `SHA256SUMS` and `SHA256SUMS.minisig` of that version are downloaded from a URL built from
//! it; the signature must verify against one of the embedded [`RELEASE_KEYS`] **and** its trusted
//! comment must be exactly `siple v<version> SHA256SUMS`, which stops an old release's signed list
//! from being replayed under a newer tag. Only then are its lines trusted.

use minisign_verify::{PublicKey, Signature};

use super::version::Version;

/// The release signing keys (minisign public keys, base64 of the key file's second line). A list
/// so that a key can be rotated: ship the next key one release before signing with it. The same
/// key is in `packaging/minisign.pub` (key id 2117F5F7CDB0EE58).
pub const RELEASE_KEYS: &[&str] = &["RWRY7rDN9/UXId/KmnBcB1BTI/HNfTk0mM1OD+bxK4JUKDG5RLZGYIIH"];

/// File name of the checksum list.
pub const SUMS_NAME: &str = "SHA256SUMS";
/// File name of its minisign signature.
pub const SIG_NAME: &str = "SHA256SUMS.minisig";
/// Largest signature file or checksum list accepted.
pub const MAX_SIG_FILE: u64 = 64 * 1024;

/// Why a checksum list is not trusted or no asset can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SumsError {
    /// The signature file is not a minisign signature, or a key is malformed.
    BadSignatureFile(String),
    /// No embedded key verifies the list.
    NotSigned(String),
    /// The signed trusted comment does not name this release.
    WrongRelease {
        /// What it should have said.
        expected: String,
        /// What it said.
        found: String,
    },
    /// A line of the list is not `<64 lowercase hex>  <plain file name>`.
    Malformed(usize),
    /// No entry matches this platform.
    NoAsset,
    /// More than one entry matches this platform.
    Ambiguous,
}

impl std::fmt::Display for SumsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSignatureFile(m) => write!(f, "unreadable signature: {m}"),
            Self::NotSigned(m) => {
                write!(f, "the checksum list is not signed by a release key ({m})")
            }
            Self::WrongRelease { expected, found } => write!(
                f,
                "the signature belongs to another release (expected {expected:?}, found {found:?})"
            ),
            Self::Malformed(n) => write!(f, "malformed checksum list (line {n})"),
            Self::NoAsset => f.write_str("the release has no file for this platform"),
            Self::Ambiguous => f.write_str("the release lists several files for this platform"),
        }
    }
}

impl std::error::Error for SumsError {}

/// The trusted comment `release.yml` writes for `version`.
pub fn trusted_comment(version: &Version) -> String {
    format!("siple v{version} SHA256SUMS")
}

/// A plain file name: 1..=200 characters of `[0-9A-Za-z._+-]`, not starting with a dot. Nothing
/// that is a path, drive or device reference survives this.
pub fn valid_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// Verifies `sums` against `sig` (the text of `SHA256SUMS.minisig`) with the first of `keys` that
/// accepts it, requires the trusted comment of `version`, and parses the list. Legacy
/// (non-prehashed) signatures are refused.
pub fn verify_sums(
    sums: &[u8],
    sig: &[u8],
    version: &Version,
    keys: &[&str],
) -> Result<Sums, SumsError> {
    let text =
        std::str::from_utf8(sig).map_err(|_| SumsError::BadSignatureFile("not UTF-8".into()))?;
    let signature =
        Signature::decode(text).map_err(|e| SumsError::BadSignatureFile(e.to_string()))?;
    let mut last = String::from("no keys");
    let mut verified = false;
    for key in keys {
        let pk =
            PublicKey::from_base64(key).map_err(|e| SumsError::BadSignatureFile(e.to_string()))?;
        match pk.verify(sums, &signature, false) {
            Ok(()) => {
                verified = true;
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    if !verified {
        return Err(SumsError::NotSigned(last));
    }
    let expected = trusted_comment(version);
    if signature.trusted_comment() != expected {
        return Err(SumsError::WrongRelease {
            expected,
            found: signature.trusted_comment().to_string(),
        });
    }
    Sums::parse(sums)
}

/// A parsed `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sums {
    entries: Vec<(String, [u8; 32])>,
}

impl Sums {
    /// Parses strictly: every line is 64 lowercase hex digits, two spaces and a plain file name
    /// ([`valid_asset_name`]); lines end with `\n`; no blank lines.
    pub fn parse(bytes: &[u8]) -> Result<Sums, SumsError> {
        let text = std::str::from_utf8(bytes).map_err(|_| SumsError::Malformed(1))?;
        let mut entries = Vec::new();
        let body = text.strip_suffix('\n').unwrap_or(text);
        if body.is_empty() {
            return Err(SumsError::Malformed(1));
        }
        for (i, line) in body.split('\n').enumerate() {
            let bad = || SumsError::Malformed(i + 1);
            let (hex, name) = line.split_once("  ").ok_or_else(bad)?;
            if !valid_asset_name(name) {
                return Err(bad());
            }
            entries.push((name.to_string(), parse_hex32(hex).ok_or_else(bad)?));
        }
        Ok(Sums { entries })
    }

    /// The digest listed for exactly `name`, when it is listed once.
    pub fn digest_of(&self, name: &str) -> Option<[u8; 32]> {
        let mut it = self.entries.iter().filter(|(n, _)| n == name);
        match (it.next(), it.next()) {
            (Some((_, d)), None) => Some(*d),
            _ => None,
        }
    }

    /// The asset for `platform` and `version`: exactly one entry may match.
    pub fn select(
        &self,
        platform: AssetPlatform,
        version: &Version,
    ) -> Result<(String, [u8; 32]), SumsError> {
        let names = platform.names(version);
        let mut found: Option<&(String, [u8; 32])> = None;
        for e in self.entries.iter().filter(|(n, _)| names.contains(n)) {
            if found.is_some() {
                return Err(SumsError::Ambiguous);
            }
            found = Some(e);
        }
        found
            .map(|(n, d)| (n.clone(), *d))
            .ok_or(SumsError::NoAsset)
    }
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = nibble(pair[0])?;
        let lo = nibble(pair[1])?;
        out[i] = hi << 4 | lo;
    }
    Some(out)
}

fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Lowercase hex of a digest.
pub fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Which release archive a build installs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetPlatform {
    /// `Siple-<v>-macos.zip` (universal).
    MacosZip,
    /// `Siple-<v>-windows-x64.zip`.
    WindowsX64,
    /// `Siple-<v>-windows-arm64.zip`.
    WindowsArm64,
    /// `Siple-<v>-linux-x86_64.tar.gz`.
    LinuxX64,
}

impl AssetPlatform {
    /// The archive of the running binary, when there is one for this OS and architecture.
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::MacosZip)
        } else if cfg!(all(windows, target_arch = "x86_64")) {
            Some(Self::WindowsX64)
        } else if cfg!(all(windows, target_arch = "aarch64")) {
            Some(Self::WindowsArm64)
        } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Self::LinuxX64)
        } else {
            None
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::MacosZip => "macos.zip",
            Self::WindowsX64 => "windows-x64.zip",
            Self::WindowsArm64 => "windows-arm64.zip",
            Self::LinuxX64 => "linux-x86_64.tar.gz",
        }
    }

    /// The names the archive may have for `version`: with the full version (`0.2.0-rc.1`, the
    /// macOS release candidates) or with the numeric core (`0.2.0`, the Windows and Linux ones).
    pub fn names(self, version: &Version) -> Vec<String> {
        let mut v = vec![format!("Siple-{version}-{}", self.suffix())];
        let core = format!("Siple-{}-{}", version.core(), self.suffix());
        if !v.contains(&core) {
            v.push(core);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = include_str!("../../tests/fixtures/update/test-key.pub");
    const SUMS: &[u8] = include_bytes!("../../tests/fixtures/update/SHA256SUMS.0.2.0");
    const SIG: &[u8] = include_bytes!("../../tests/fixtures/update/SHA256SUMS.0.2.0.minisig");
    const REPLAY: &[u8] =
        include_bytes!("../../tests/fixtures/update/SHA256SUMS.0.2.0.replay.minisig");
    const RC_SUMS: &[u8] = include_bytes!("../../tests/fixtures/update/SHA256SUMS.0.3.0-rc.1");
    const RC_SIG: &[u8] =
        include_bytes!("../../tests/fixtures/update/SHA256SUMS.0.3.0-rc.1.minisig");

    fn key() -> &'static str {
        KEY.lines().nth(1).unwrap()
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn the_embedded_key_matches_the_published_one() {
        let file = include_str!("../../../../packaging/minisign.pub");
        assert_eq!(RELEASE_KEYS, [file.lines().nth(1).unwrap().trim()]);
        PublicKey::from_base64(RELEASE_KEYS[0]).unwrap();
    }

    #[test]
    fn a_correctly_signed_list_is_accepted() {
        let s = verify_sums(SUMS, SIG, &v("0.2.0"), &[key()]).unwrap();
        assert!(s.digest_of("Siple-0.2.0-macos.zip").is_some());
        assert!(verify_sums(RC_SUMS, RC_SIG, &v("0.3.0-rc.1"), &[key()]).is_ok());
    }

    #[test]
    fn a_key_list_accepts_any_listed_key() {
        let other = RELEASE_KEYS[0];
        assert!(verify_sums(SUMS, SIG, &v("0.2.0"), &[other, key()]).is_ok());
    }

    #[test]
    fn the_wrong_key_is_refused() {
        let e = verify_sums(SUMS, SIG, &v("0.2.0"), RELEASE_KEYS).unwrap_err();
        assert!(matches!(e, SumsError::NotSigned(_)), "{e:?}");
        assert!(matches!(
            verify_sums(SUMS, SIG, &v("0.2.0"), &[]),
            Err(SumsError::NotSigned(_))
        ));
    }

    #[test]
    fn a_replayed_signature_for_another_release_is_refused() {
        // Valid signature, valid key, valid list: but the signed comment names v0.1.0.
        let e = verify_sums(SUMS, REPLAY, &v("0.2.0"), &[key()]).unwrap_err();
        assert!(
            matches!(&e, SumsError::WrongRelease { expected, found }
                if expected == "siple v0.2.0 SHA256SUMS" && found == "siple v0.1.0 SHA256SUMS"),
            "{e:?}"
        );
        // And the right list under the wrong tag.
        assert!(matches!(
            verify_sums(SUMS, SIG, &v("0.2.1"), &[key()]),
            Err(SumsError::WrongRelease { .. })
        ));
        assert!(matches!(
            verify_sums(SUMS, SIG, &v("0.2.0-rc.1"), &[key()]),
            Err(SumsError::WrongRelease { .. })
        ));
    }

    #[test]
    fn a_tampered_list_is_refused() {
        let mut bad = SUMS.to_vec();
        bad[0] = if bad[0] == b'0' { b'1' } else { b'0' };
        assert!(matches!(
            verify_sums(&bad, SIG, &v("0.2.0"), &[key()]),
            Err(SumsError::NotSigned(_))
        ));
        let mut appended = SUMS.to_vec();
        appended.extend_from_slice(&[b'0'; 64]);
        appended.extend_from_slice(b"  Siple-0.2.0-macos.zip\n");
        assert!(verify_sums(&appended, SIG, &v("0.2.0"), &[key()]).is_err());
    }

    #[test]
    fn garbage_signatures_are_refused() {
        for sig in [&b""[..], b"hello", b"\xff\xfe", &SIG[..SIG.len() / 2]] {
            assert!(verify_sums(SUMS, sig, &v("0.2.0"), &[key()]).is_err());
        }
        assert!(matches!(
            verify_sums(SUMS, SIG, &v("0.2.0"), &["not a key"]),
            Err(SumsError::BadSignatureFile(_))
        ));
    }

    #[test]
    fn asset_selection_follows_platform_and_naming() {
        let s = verify_sums(SUMS, SIG, &v("0.2.0"), &[key()]).unwrap();
        let ver = v("0.2.0");
        for (p, name) in [
            (AssetPlatform::MacosZip, "Siple-0.2.0-macos.zip"),
            (AssetPlatform::WindowsX64, "Siple-0.2.0-windows-x64.zip"),
            (AssetPlatform::WindowsArm64, "Siple-0.2.0-windows-arm64.zip"),
            (AssetPlatform::LinuxX64, "Siple-0.2.0-linux-x86_64.tar.gz"),
        ] {
            let (n, d) = s.select(p, &ver).unwrap();
            assert_eq!(n, name);
            assert_eq!(d, s.digest_of(name).unwrap());
        }
        // Release candidates: macOS carries the full version, Windows and Linux the numeric one.
        let rc = verify_sums(RC_SUMS, RC_SIG, &v("0.3.0-rc.1"), &[key()]).unwrap();
        let ver = v("0.3.0-rc.1");
        assert_eq!(
            rc.select(AssetPlatform::MacosZip, &ver).unwrap().0,
            "Siple-0.3.0-rc.1-macos.zip"
        );
        assert_eq!(
            rc.select(AssetPlatform::WindowsX64, &ver).unwrap().0,
            "Siple-0.3.0-windows-x64.zip"
        );
        assert_eq!(
            rc.select(AssetPlatform::LinuxX64, &ver).unwrap().0,
            "Siple-0.3.0-linux-x86_64.tar.gz"
        );
    }

    #[test]
    fn selection_refuses_zero_and_two_matches_and_other_versions() {
        let h = "a".repeat(64);
        let list = |names: &[&str]| {
            let t: String = names.iter().map(|n| format!("{h}  {n}\n")).collect();
            Sums::parse(t.as_bytes()).unwrap()
        };
        let ver = v("0.3.0-rc.1");
        // Zero: other platform, other version.
        assert_eq!(
            list(&["Siple-0.3.0-linux-x86_64.tar.gz"]).select(AssetPlatform::MacosZip, &ver),
            Err(SumsError::NoAsset)
        );
        assert_eq!(
            list(&["Siple-0.2.0-macos.zip", "Siple-0.3.0-rc.2-macos.zip"])
                .select(AssetPlatform::MacosZip, &ver),
            Err(SumsError::NoAsset)
        );
        assert_eq!(
            list(&["Siple-0.3.0-rc.1-macos.dmg"]).select(AssetPlatform::MacosZip, &ver),
            Err(SumsError::NoAsset)
        );
        // Two: full and numeric spelling both present, or the same name twice.
        assert_eq!(
            list(&["Siple-0.3.0-rc.1-macos.zip", "Siple-0.3.0-macos.zip"])
                .select(AssetPlatform::MacosZip, &ver),
            Err(SumsError::Ambiguous)
        );
        assert_eq!(
            list(&["Siple-0.3.0-macos.zip", "Siple-0.3.0-macos.zip"])
                .select(AssetPlatform::MacosZip, &ver),
            Err(SumsError::Ambiguous)
        );
        assert_eq!(list(&["x", "x"]).digest_of("x"), None);
    }

    #[test]
    fn the_list_parser_is_strict() {
        let h = "0123456789abcdef".repeat(4);
        assert!(Sums::parse(format!("{h}  a.zip\n").as_bytes()).is_ok());
        assert!(Sums::parse(format!("{h}  a.zip").as_bytes()).is_ok());
        for bad in [
            String::new(),
            "\n".into(),
            format!("{h} a.zip\n"),
            format!("{h}   a.zip\n"),
            format!("{h} *a.zip\n"),
            format!("{}  a.zip\n", h.to_uppercase()),
            format!("{}  a.zip\n", &h[..63]),
            format!("{h}  \n"),
            format!("{h}  dir/a.zip\n"),
            format!("{h}  ..\\a.zip\n"),
            format!("{h}  ../a.zip\n"),
            format!("{h}  .hidden\n"),
            format!("{h}  a.zip\n\n"),
            format!("{h}  a.zip\r\n"),
            format!("{h}  a b.zip\n"),
            format!("{}z  a.zip\n", &h[..63]),
        ] {
            assert!(Sums::parse(bad.as_bytes()).is_err(), "{bad:?}");
        }
        assert!(Sums::parse(b"\xff").is_err());
    }

    #[test]
    fn names_are_plain() {
        assert!(valid_asset_name("Siple-0.1.0-rc.6-macos.zip"));
        for bad in [
            "",
            ".a",
            "a/b",
            "a\\b",
            "a b",
            "a:b",
            "a\0b",
            "é",
            &"a".repeat(201),
        ] {
            assert!(!valid_asset_name(bad), "{bad:?}");
        }
    }
}
