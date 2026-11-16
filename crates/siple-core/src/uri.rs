//! Safe parsing of `sip:` / `sips:` / `tel:` / `callto:` links.
//!
//! A link arrives from outside the app (browser, OS URL handler, another instance), so it is
//! untrusted. [`parse_link`] keeps only what a dial field needs: `user@host[:port]` for SIP URIs
//! plus a `transport` parameter, and a normalised number for `tel:`. URI headers (`?Replaces=`,
//! `?Call-Info=`, `?body=`) and every other parameter are dropped. Control characters, spaces
//! and percent-escapes that decode to them are refused. The result only ever prefills the dial
//! field; nothing here dials.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Longest link accepted, in bytes.
pub const MAX_LINK_LEN: usize = 256;
const MAX_USER_LEN: usize = 128;
const MAX_HOST_LEN: usize = 253;
const MAX_NUMBER_LEN: usize = 32;

/// Why a link was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// Nothing to dial.
    #[error("empty link")]
    Empty,
    /// Longer than [`MAX_LINK_LEN`].
    #[error("link is longer than {MAX_LINK_LEN} bytes")]
    TooLong,
    /// Not one of `sip:`, `sips:`, `tel:`, `callto:`.
    #[error("unsupported link scheme (expected sip:, sips:, tel: or callto:)")]
    Scheme,
    /// Control character, space or an escape that decodes to one.
    #[error("link contains control characters or whitespace")]
    Control,
    /// Anything else wrong with the shape of the link.
    #[error("invalid link: {0}")]
    Invalid(&'static str),
}

/// Scheme of an accepted link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkScheme {
    /// `sip:`
    Sip,
    /// `sips:`
    Sips,
    /// `tel:`
    Tel,
    /// `callto:`
    Callto,
}

/// A validated link: what to put in the dial field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkTarget {
    /// Scheme the link used.
    pub scheme: LinkScheme,
    /// Dial string: `+15551234567`, `1001`, `sip:user@host:5070;transport=tcp`, `user@host`.
    pub target: String,
}

/// Parameters of the `app.open_uri` request a second launch sends to the running instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenUriParams {
    /// The raw link (validated again by the receiver).
    pub uri: String,
}

impl OpenUriParams {
    /// The JSON-RPC params object.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("OpenUriParams always serializes")
    }

    /// Decodes request params.
    pub fn from_value(v: serde_json::Value) -> Result<Self, String> {
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}

fn split_scheme(s: &str) -> Option<(LinkScheme, &str)> {
    let (scheme, rest) = s.split_once(':')?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "sip" => LinkScheme::Sip,
        "sips" => LinkScheme::Sips,
        "tel" => LinkScheme::Tel,
        "callto" => LinkScheme::Callto,
        _ => return None,
    };
    Some((scheme, rest))
}

/// Refuses control characters, whitespace and `%XX` escapes that decode to them, and malformed
/// escapes.
fn check_chars(s: &str) -> Result<(), LinkError> {
    if s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(LinkError::Control);
    }
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b
                .get(i + 1..i + 3)
                .ok_or(LinkError::Invalid("bad escape"))?;
            let v = std::str::from_utf8(hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or(LinkError::Invalid("bad escape"))?;
            if v < 0x20 || v == 0x7f || v == b' ' {
                return Err(LinkError::Control);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn user_ok(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= MAX_USER_LEN
        && u.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.!~*'()&=+$,".contains(&c))
}

fn host_ok(h: &str) -> bool {
    if h.is_empty() || h.len() > MAX_HOST_LEN {
        return false;
    }
    if let Some(v6) = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')) {
        return !v6.is_empty() && v6.parse::<std::net::Ipv6Addr>().is_ok();
    }
    h.bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
        && !h.starts_with(['-', '.'])
}

/// `host[:port]`, validated; returns it as given (lowercase host).
fn hostport(hp: &str) -> Result<String, LinkError> {
    let bad = LinkError::Invalid("bad host or port");
    let (host, port) = if hp.starts_with('[') {
        let end = hp.find(']').ok_or_else(|| bad.clone())?;
        let (h, rest) = hp.split_at(end + 1);
        (h, rest.strip_prefix(':'))
    } else {
        match hp.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hp, None),
        }
    };
    if !host_ok(host) {
        return Err(bad);
    }
    let mut out = host.to_ascii_lowercase();
    if let Some(p) = port {
        let n: u16 = p.parse().map_err(|_| bad.clone())?;
        if n == 0 || !p.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad);
        }
        out.push(':');
        out.push_str(p);
    }
    Ok(out)
}

fn parse_sip(scheme: LinkScheme, rest: &str) -> Result<String, LinkError> {
    // Headers (`?...`) are dropped entirely.
    let rest = rest.split('?').next().unwrap_or("");
    // Split userinfo off first. `;` and `%` are refused in the user part (`user_ok`): the SIP
    // stack splits parameters at the first `;` even before the `@`, so `evil.test;x=@pbx.test`
    // would mean host `evil.test` to it and `pbx.test` to us.
    let (user, after) = match rest.rsplit_once('@') {
        // userinfo may carry a password (`user:pw@host`): never keep it.
        Some((u, h)) => (Some(u.split(':').next().unwrap_or("")), h),
        None => (None, rest),
    };
    let mut parts = after.split(';');
    let hp = parts.next().unwrap_or("");
    let transport = parts.find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.eq_ignore_ascii_case("transport")
            .then(|| v.to_ascii_lowercase())
    });
    let transport = match (scheme, transport.as_deref()) {
        (_, None) => None,
        // `sips:` means TLS end to end: never let a link downgrade it.
        (LinkScheme::Sips, Some("tls")) => Some("tls"),
        (LinkScheme::Sips, Some(_)) => {
            return Err(LinkError::Invalid("sips: links only use TLS"));
        }
        (_, Some(t @ ("udp" | "tcp" | "tls"))) => Some(t),
        _ => return Err(LinkError::Invalid("unsupported transport")),
    };
    if let Some(u) = user
        && !user_ok(u)
    {
        return Err(LinkError::Invalid("bad user part"));
    }
    let hp = hostport(hp)?;
    let mut out = format!(
        "{}:",
        if scheme == LinkScheme::Sips {
            "sips"
        } else {
            "sip"
        }
    );
    if let Some(u) = user {
        out.push_str(u);
        out.push('@');
    }
    out.push_str(&hp);
    if let Some(t) = transport {
        out.push_str(";transport=");
        out.push_str(t);
    }
    Ok(out)
}

/// `tel:` / `callto:` number: visual separators (`-`, `.`, `(`, `)`, spaces) removed; keeps
/// `+`, digits, `*` and `#`. Parameters such as `;phone-context=` are dropped.
fn parse_number(rest: &str) -> Result<String, LinkError> {
    let rest = rest.split(['?', ';']).next().unwrap_or("");
    let out: String = rest
        .chars()
        .filter(|c| !matches!(c, '-' | '.' | '(' | ')'))
        .collect();
    let digits = out.strip_prefix('+').unwrap_or(&out);
    if digits.is_empty()
        || out.len() > MAX_NUMBER_LEN
        || (out.contains('+') && !out.starts_with('+'))
        || !digits
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '*' | '#' | 'p' | 'w'))
    {
        return Err(LinkError::Invalid("not a phone number"));
    }
    // Pause/wait digits make no sense for a prefilled dial string.
    if digits.contains(['p', 'w']) {
        return Err(LinkError::Invalid("not a phone number"));
    }
    Ok(out)
}

/// Validates `link` and returns what to prefill. See the module docs.
pub fn parse_link(link: &str) -> Result<LinkTarget, LinkError> {
    let link = link.trim();
    if link.is_empty() {
        return Err(LinkError::Empty);
    }
    if link.len() > MAX_LINK_LEN {
        return Err(LinkError::TooLong);
    }
    check_chars(link)?;
    let (scheme, rest) = split_scheme(link).ok_or(LinkError::Scheme)?;
    // `sip://host` is not a SIP URI; browsers sometimes add the slashes.
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let target = match scheme {
        LinkScheme::Sip | LinkScheme::Sips => parse_sip(scheme, rest)?,
        LinkScheme::Tel => parse_number(rest)?,
        LinkScheme::Callto => {
            if rest.contains('@') {
                // `callto:user@host` is a SIP address.
                let t = parse_sip(LinkScheme::Sip, rest)?;
                t.strip_prefix("sip:").unwrap_or(&t).to_string()
            } else {
                parse_number(rest).or_else(|_| {
                    // `callto:1001` style extensions and plain names.
                    let r = rest.split(['?', ';']).next().unwrap_or("");
                    if user_ok(r) && !r.contains('%') {
                        Ok(r.to_string())
                    } else {
                        Err(LinkError::Invalid("bad callto target"))
                    }
                })?
            }
        }
    };
    Ok(LinkTarget { scheme, target })
}

/// Outcome of [`hand_off`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandOff {
    /// The running instance took it.
    Done,
    /// An instance answered but refused (rate limit, bad link): it is alive, nothing more to do.
    Refused(String),
    /// The instance is headless (no window to show).
    NoGui,
    /// Nobody answered (stale `runtime.json`, wrong user, timeout).
    Unreachable(String),
}

/// Second-instance hand-off: connects to the instance running for `config_dir` and sends it
/// `ui.show` (no link) or `app.open_uri` (a link).
pub async fn hand_off(config_dir: &std::path::Path, link: Option<&str>) -> HandOff {
    use crate::control::client::{ClientError, ControlClient};
    use crate::instance::RuntimeInfo;
    let unreachable = |e: String| HandOff::Unreachable(e);
    let work = async {
        let info = match RuntimeInfo::read(config_dir) {
            Ok(i) => i,
            Err(e) => return unreachable(e.to_string()),
        };
        let mut client = match ControlClient::connect_endpoints(&info.control).await {
            Ok(c) => c,
            Err(e) => return unreachable(e.to_string()),
        };
        if let Err(e) = client
            .hello(Some(info.token.expose()), "siple-handoff")
            .await
        {
            return unreachable(e.to_string());
        }
        let (method, params) = match link {
            Some(uri) => (
                "app.open_uri",
                OpenUriParams {
                    uri: uri.to_string(),
                }
                .to_value(),
            ),
            None => ("ui.show", serde_json::json!({})),
        };
        match client.call(method, params).await {
            Ok(_) => HandOff::Done,
            Err(ClientError::Api(a))
                if a.extra.as_ref().and_then(|e| e["reason"].as_str()) == Some("no_gui") =>
            {
                HandOff::NoGui
            }
            Err(ClientError::Api(a)) => HandOff::Refused(a.message),
            Err(e) => unreachable(e.to_string()),
        }
    };
    tokio::time::timeout(Duration::from_secs(5), work)
        .await
        .unwrap_or_else(|_| unreachable("the running instance did not answer".into()))
}

/// Allows at most `max` links per `window`; extras are refused (a web page can fire links in a
/// loop).
#[derive(Debug)]
pub struct LinkRateLimit {
    max: usize,
    window: Duration,
    seen: Vec<Instant>,
}

impl LinkRateLimit {
    /// New limiter.
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            seen: Vec::new(),
        }
    }

    /// Records a link at `now`; `false` when over the limit.
    pub fn allow(&mut self, now: Instant) -> bool {
        self.seen
            .retain(|t| now.saturating_duration_since(*t) < self.window);
        if self.seen.len() >= self.max {
            return false;
        }
        self.seen.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> String {
        parse_link(s).unwrap_or_else(|e| panic!("{s}: {e}")).target
    }

    #[test]
    fn good_sip_links() {
        assert_eq!(t("sip:1001@pbx.example.test"), "sip:1001@pbx.example.test");
        assert_eq!(
            t("SIP:Alice@PBX.Example.Test:5070"),
            "sip:Alice@pbx.example.test:5070"
        );
        assert_eq!(t("sips:bob@example.test"), "sips:bob@example.test");
        assert_eq!(t("sip:example.test"), "sip:example.test");
        assert_eq!(
            t("sip:bob@[2001:db8::1]:5060"),
            "sip:bob@[2001:db8::1]:5060"
        );
        assert_eq!(t("sip://bob@example.test"), "sip:bob@example.test");
        assert_eq!(
            t("sip:bob@example.test;transport=TCP"),
            "sip:bob@example.test;transport=tcp"
        );
    }

    #[test]
    fn headers_params_and_passwords_are_stripped() {
        assert_eq!(
            t("sip:bob@example.test?Replaces=abc%3Bto-tag%3Dx&Call-Info=%3Chttp://evil%3E"),
            "sip:bob@example.test"
        );
        assert_eq!(
            t("sip:bob@example.test;user=phone;maddr=10.0.0.1;transport=tls?subject=hi"),
            "sip:bob@example.test;transport=tls"
        );
        assert_eq!(t("sip:bob:secret@example.test"), "sip:bob@example.test");
        assert_eq!(
            t("sips:bob@example.test;transport=tls"),
            "sips:bob@example.test;transport=tls"
        );
    }

    #[test]
    fn tel_is_normalised() {
        assert_eq!(t("tel:+1-555-123-4567"), "+15551234567");
        assert_eq!(
            t("tel:+1(555)123.4567;phone-context=example.test"),
            "+15551234567"
        );
        assert_eq!(t("tel:*72"), "*72");
        assert_eq!(t("tel:1001"), "1001");
        assert_eq!(parse_link("tel:+1555").unwrap().scheme, LinkScheme::Tel);
    }

    #[test]
    fn callto_forms() {
        assert_eq!(t("callto:+15551234567"), "+15551234567");
        assert_eq!(t("callto:1001"), "1001");
        assert_eq!(t("callto:bob@example.test"), "bob@example.test");
        assert_eq!(t("callto:alice"), "alice");
    }

    #[test]
    fn rejects_injection_and_junk() {
        for bad in [
            "sip:bob@example.test\r\nBye: x",
            "sip:bob@example.test%0d%0aRoute:evil",
            "sip:bob@example.test%00",
            "sip:bob@exam ple.test",
            "sip:bob@example.test>",
            "sip:<bob>@example.test",
            "sip:bob@example.test;transport=sctp",
            "sips:bob@example.test;transport=tcp",
            "sips:bob@example.test;transport=udp",
            "sip:bob@example.test:0",
            "sip:bob@example.test:99999",
            "sip:bob@example.test:80x",
            "sip:@example.test",
            "sip:bob@",
            "sip:bob@-bad.test",
            "sip:bob@[zzz]",
            "sip:bob@example.test%zz",
            "tel:abc",
            "tel:",
            "tel:+",
            "tel:12+3",
            "tel:1234p5",
            "tel:1\n2",
            "javascript:alert(1)",
            "http://example.test",
            "file:///etc/passwd",
            "",
            "   ",
            "sip",
        ] {
            assert!(parse_link(bad).is_err(), "should reject {bad:?}");
        }
    }

    /// Links whose `;` / `%` in the user part make this parser and rsipstack disagree about the
    /// host (rsipstack splits parameters at the first `;`, before the `@`).
    #[test]
    fn spoofing_user_parts_are_refused() {
        for bad in [
            "sip:evil.attacker.test;x=@pbx.example.test",
            "sips:evil.test;transport=udp;x=@pbx.example.test",
            "sip:alice;ext=1@example.test;user=phone",
            "sip:a%3Bb@example.test",
            "sip:a%41@example.test",
            "callto:evil.test;x=@pbx.example.test",
            "callto:a%3Bb@example.test",
        ] {
            assert!(parse_link(bad).is_err(), "should reject {bad:?}");
        }
    }

    /// Every accepted link, re-parsed by the SIP stack's own URI parser, means what we said:
    /// same scheme, user, host, port and transport, no headers, and `sips` stays TLS. Also the
    /// host the stack would read out of the *original* link is the one we kept.
    #[test]
    fn accepted_links_mean_the_same_to_rsipstack() {
        use rsipstack::sip::{Scheme, Uri, uri::Param};
        let schemes = ["sip", "SIPS", "sips", "callto"];
        let users = [
            "",
            "a@",
            "bob@",
            "a;b@",
            "a;x=@",
            "a%3Bb@",
            "a%41@",
            "a:pw@",
            "+1555@",
            "evil.test;x=@",
            "a=b@",
            "a,b@",
            "<a>@",
        ];
        let hosts = [
            "pbx.example.test",
            "PBX.example.test",
            "[2001:db8::1]",
            "-x.test",
            "h_h",
        ];
        let ports = ["", ":5070", ":0", ":x"];
        let tails = [
            "",
            ";transport=tcp",
            ";transport=TLS",
            ";transport=udp",
            ";x=1;transport=udp",
            ";user=phone",
            "?Replaces=x",
            ";transport=tcp?h=v",
        ];
        let mut accepted = 0;
        for sc in schemes {
            for u in users {
                for h in hosts {
                    for p in ports {
                        for t in tails {
                            let link = format!("{sc}:{u}{h}{p}{t}");
                            let Ok(l) = parse_link(&link) else { continue };
                            if l.scheme == LinkScheme::Tel || !l.target.contains(':') {
                                continue; // bare callto names are not SIP URIs
                            }
                            accepted += 1;
                            // `callto:user@host` yields a bare `user@host`; the dialer adds `sip:`.
                            let full = if l.target.starts_with("sip") {
                                l.target.clone()
                            } else {
                                format!("sip:{}", l.target)
                            };
                            let uri = Uri::try_from(full.as_str())
                                .unwrap_or_else(|e| panic!("{link:?} -> {full:?}: {e}"));
                            assert!(uri.headers.is_empty(), "{link:?}");
                            assert!(
                                uri.auth.as_ref().is_none_or(|a| a.password.is_none()),
                                "{link:?}"
                            );
                            let want_sips = matches!(l.scheme, LinkScheme::Sips);
                            let scheme_ok = if want_sips {
                                uri.scheme == Some(Scheme::Sips)
                            } else {
                                uri.scheme == Some(Scheme::Sip)
                            };
                            assert!(scheme_ok, "{link:?} -> {:?}", l.target);
                            let transports: Vec<_> = uri
                                .params
                                .iter()
                                .filter_map(|p| match p {
                                    Param::Transport(t) => Some(t.to_string().to_lowercase()),
                                    _ => None,
                                })
                                .collect();
                            assert_eq!(
                                uri.params.len(),
                                transports.len(),
                                "{link:?}: {:?}",
                                uri.params
                            );
                            if want_sips {
                                assert!(
                                    transports.iter().all(|t| t == "tls"),
                                    "{link:?}: sips downgraded"
                                );
                            }
                            // Our output, rendered back, is exactly what we emitted.
                            assert_eq!(
                                uri.to_string().to_lowercase(),
                                full.to_lowercase(),
                                "{link:?}"
                            );
                            // The original link must not read as a different host to the stack.
                            let orig = link.split_once(':').map(|x| x.1).unwrap_or("");
                            if let Ok(ou) = Uri::try_from(format!("sip:{orig}").as_str()) {
                                assert!(
                                    ou.host_with_port
                                        .host
                                        .to_string()
                                        .eq_ignore_ascii_case(&uri.host_with_port.host.to_string()),
                                    "{link:?}: stack reads host {} from the original, we kept {}",
                                    ou.host_with_port.host,
                                    uri.host_with_port.host
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(accepted > 20, "corpus too thin: {accepted}");
    }

    #[test]
    fn rejects_overlong() {
        let long = format!("sip:{}@example.test", "a".repeat(MAX_LINK_LEN));
        assert_eq!(parse_link(&long), Err(LinkError::TooLong));
        let long_user = format!("sip:{}@example.test", "a".repeat(MAX_USER_LEN + 1));
        assert!(parse_link(&long_user).is_err());
        let long_tel = format!("tel:{}", "1".repeat(MAX_NUMBER_LEN + 1));
        assert!(parse_link(&long_tel).is_err());
    }

    #[test]
    fn open_uri_params_roundtrip() {
        let p = OpenUriParams {
            uri: "sip:bob@example.test".into(),
        };
        let v = p.to_value();
        assert_eq!(v, serde_json::json!({"uri": "sip:bob@example.test"}));
        assert_eq!(OpenUriParams::from_value(v).unwrap(), p);
        assert!(OpenUriParams::from_value(serde_json::json!({})).is_err());
        assert!(OpenUriParams::from_value(serde_json::json!({"uri":"x","y":1})).is_err());
    }

    #[test]
    fn rate_limit() {
        let mut l = LinkRateLimit::new(3, Duration::from_secs(10));
        let t0 = Instant::now();
        assert!(l.allow(t0) && l.allow(t0) && l.allow(t0));
        assert!(!l.allow(t0 + Duration::from_secs(1)));
        assert!(l.allow(t0 + Duration::from_secs(11)));
    }
}
