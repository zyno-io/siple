//! Pure registration logic: timers, back-off, expiry parsing, Contact rewrite.
//! No I/O, so everything here is unit-tested.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// First retry delay after a failure.
pub const BACKOFF_MIN: Duration = Duration::from_secs(5);
/// Longest retry delay.
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Upper bound on a server-supplied `Retry-After`.
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(3600);

/// When to refresh a binding that was granted `granted` seconds: `min(85 %, granted - 30 s)`,
/// but never below half the lifetime (tiny lifetimes) or 1 s.
pub fn refresh_after(granted: u32) -> Duration {
    let pct = u64::from(granted) * 85 / 100;
    let lead = u64::from(granted).saturating_sub(30);
    let secs = pct.min(lead).max(u64::from(granted) / 2).max(1);
    Duration::from_secs(secs)
}

/// Spread applied to every back-off delay (+-20 %) so accounts do not retry in lock step.
pub const BACKOFF_JITTER: f64 = 0.2;

/// Un-jittered back-off after `fail_count` consecutive failures (1 = first): 5 s, 10 s, 20 s ...
/// 300 s.
pub fn backoff_base(fail_count: u32) -> Duration {
    let n = fail_count.saturating_sub(1).min(16);
    BACKOFF_MIN
        .checked_mul(1u32 << n)
        .unwrap_or(BACKOFF_MAX)
        .min(BACKOFF_MAX)
}

/// `base` scaled by `1 + BACKOFF_JITTER * (2 * unit - 1)`, where `unit` is in `[0, 1]`
/// (out-of-range values are clamped).
pub fn jittered(base: Duration, unit: f64) -> Duration {
    let unit = if unit.is_nan() {
        0.5
    } else {
        unit.clamp(0.0, 1.0)
    };
    base.mul_f64(1.0 + BACKOFF_JITTER * (2.0 * unit - 1.0))
}

/// Back-off after `fail_count` consecutive failures with +-20 % jitter: about 5 s, 10 s, 20 s ...
/// 300 s (so 4 s ..= 6 s for the first, up to 360 s at the cap).
pub fn backoff(fail_count: u32) -> Duration {
    jittered(backoff_base(fail_count), rand::random_range(0.0..=1.0))
}

/// Delay before the next attempt: `Retry-After` when the server sent one (clamped to
/// 1 s ..= 1 h, never jittered), else the jittered exponential back-off.
pub fn retry_delay(fail_count: u32, retry_after_secs: Option<u32>) -> Duration {
    match retry_after_secs {
        Some(s) => Duration::from_secs(u64::from(s)).clamp(Duration::from_secs(1), RETRY_AFTER_MAX),
        None => backoff(fail_count),
    }
}

/// `Retry-After` header value (`120`, `120 (comment)`, `120;duration=60`) to seconds.
pub fn parse_retry_after(v: &str) -> Option<u32> {
    let end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    v[..end].trim().parse().ok()
}

/// Expiry to use after a 423 (Interval Too Brief): the server's `Min-Expires` if it is larger
/// than what we asked for; `None` (give up) when it is missing or not larger.
pub fn expires_after_423(requested: u32, min_expires: Option<u32>) -> Option<u32> {
    min_expires.filter(|m| *m > requested)
}

/// Decide whether to re-REGISTER with a different Contact: the address the registrar saw us at
/// (`received` / `rport` of the Via in the 200) vs the one we advertised. `None` = keep.
pub fn contact_rewrite(
    advertised: SocketAddr,
    received: Option<IpAddr>,
    rport: Option<u16>,
) -> Option<SocketAddr> {
    let seen = SocketAddr::new(
        received.unwrap_or(advertised.ip()),
        rport.unwrap_or(advertised.port()),
    );
    (seen != advertised).then_some(seen)
}

/// Value of `;key=value` in a Via header value (`received`, `rport`).
pub fn via_param(via: &str, key: &str) -> Option<String> {
    via.split(';').skip(1).find_map(|p| {
        p.trim()
            .strip_prefix(key)
            .and_then(|r| r.strip_prefix('='))
            .map(|v| v.trim().to_string())
    })
}

/// `(received, rport)` from a raw Via header value.
pub fn via_received_rport(via: &str) -> (Option<IpAddr>, Option<u16>) {
    let received = via_param(via, "received").and_then(|s| s.trim_matches(['[', ']']).parse().ok());
    let rport = via_param(via, "rport").and_then(|s| s.parse().ok());
    (received, rport)
}

/// `(user, host, port)` identity of a Contact URI (params ignored; default port by scheme).
fn contact_key(uri: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = uri.split_once(':')?;
    let default_port = if scheme.eq_ignore_ascii_case("sips") {
        5061
    } else {
        5060
    };
    let rest = rest.split([';', '?']).next()?;
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (u, h),
        None => ("", rest),
    };
    let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
        let (h, after) = stripped.split_once(']')?;
        (h, after.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()),
            None => (hostport, None),
        }
    };
    Some((
        user.to_ascii_lowercase(),
        host.to_ascii_lowercase(),
        port.unwrap_or(default_port),
    ))
}

/// Split a raw `Contact` header value into `(uri, expires param)`.
pub fn split_contact(value: &str) -> Option<(String, Option<u32>)> {
    let value = value.trim();
    let (uri, params) = if let (Some(lt), Some(gt)) = (value.find('<'), value.find('>')) {
        (
            value.get(lt + 1..gt)?.to_string(),
            value.get(gt + 1..).unwrap_or(""),
        )
    } else {
        // Without angle brackets, `;params` belong to the header, not the URI.
        match value.split_once(';') {
            Some((u, p)) => (u.trim().to_string(), p),
            None => (value.to_string(), ""),
        }
    };
    let expires = params.split(';').find_map(|p| {
        let p = p.trim();
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("expires")
            .then(|| v.trim().trim_matches('"').parse().ok())?
    });
    Some((uri, expires))
}

/// Splits a raw header value into its comma-separated elements, ignoring commas inside quoted
/// strings and `<...>` (a registrar may join several Contacts into one header line).
pub fn split_list(value: &str) -> Vec<&str> {
    let (mut out, mut start) = (Vec::new(), 0);
    let (mut in_quote, mut in_angle, mut escaped) = (false, false, false);
    for (i, c) in value.char_indices() {
        if in_quote {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_quote = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_quote = true,
            '<' => in_angle = true,
            '>' => in_angle = false,
            ',' if !in_angle => {
                out.push(value[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(value[start..].trim());
    out.retain(|p| !p.is_empty());
    out
}

/// Lifetime the registrar granted: the `expires` parameter of **our** Contact in the
/// 200, else the `Expires` header, else what we asked for. A grant of 0 is treated as "unknown".
pub fn granted_expiry(
    contact_headers: &[String],
    our_contact: &str,
    expires_header: Option<u32>,
    requested: u32,
) -> u32 {
    let ours = contact_key(our_contact);
    let from_contact = contact_headers
        .iter()
        .flat_map(|h| split_list(h))
        .filter_map(split_contact)
        .find(|(uri, _)| ours.is_some() && contact_key(uri) == ours)
        .and_then(|(_, e)| e);
    from_contact
        .or(expires_header)
        .filter(|e| *e > 0)
        .unwrap_or(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(d: Duration) -> u64 {
        d.as_secs()
    }

    #[test]
    fn refresh_math() {
        assert_eq!(secs(refresh_after(300)), 255); // 85 %
        assert_eq!(secs(refresh_after(120)), 90); // expiry - 30 s wins over 102
        assert_eq!(secs(refresh_after(60)), 30);
        assert_eq!(secs(refresh_after(3600)), 3060);
        assert_eq!(secs(refresh_after(30)), 15); // tiny lifetimes: half
        assert_eq!(secs(refresh_after(1)), 1);
        assert_eq!(secs(refresh_after(0)), 1);
    }

    #[test]
    fn backoff_base_doubles_to_cap() {
        let v: Vec<_> = (1..=9).map(|n| secs(backoff_base(n))).collect();
        assert_eq!(v, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
        assert_eq!(secs(backoff_base(0)), 5);
        assert_eq!(secs(backoff_base(u32::MAX)), 300);
    }

    #[test]
    fn jitter_maths_at_the_extremes() {
        let base = Duration::from_secs(10);
        assert_eq!(jittered(base, 0.0), Duration::from_secs(8));
        assert_eq!(jittered(base, 0.5), base);
        assert_eq!(jittered(base, 1.0), Duration::from_secs(12));
        // Out-of-range input is clamped, never produces a delay outside the +-20 % band.
        assert_eq!(jittered(base, -3.0), Duration::from_secs(8));
        assert_eq!(jittered(base, 9.0), Duration::from_secs(12));
        assert_eq!(jittered(base, f64::NAN), base);
    }

    #[test]
    fn backoff_stays_within_jitter_bounds() {
        for n in [0u32, 1, 2, 3, 4, 5, 6, 7, 8, 9, 100, u32::MAX] {
            let base = backoff_base(n).as_secs_f64();
            let (lo, hi) = (base * 0.8 - 1e-6, base * 1.2 + 1e-6);
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..300 {
                let d = backoff(n);
                let s = d.as_secs_f64();
                assert!((lo..=hi).contains(&s), "n={n}: {s} outside {lo}..={hi}");
                seen.insert(d.as_millis());
            }
            assert!(
                seen.len() > 50,
                "n={n}: delays are not jittered ({} distinct)",
                seen.len()
            );
        }
        assert!(backoff(1) >= Duration::from_secs(4) && backoff(1) <= Duration::from_secs(6));
    }

    #[test]
    fn retry_after_honoured() {
        assert_eq!(secs(retry_delay(3, Some(42))), 42);
        assert_eq!(secs(retry_delay(3, Some(0))), 1);
        assert_eq!(secs(retry_delay(3, Some(999_999))), 3600);
        // No Retry-After: the jittered back-off (20 s +- 20 %).
        let d = retry_delay(3, None).as_secs_f64();
        assert!((16.0..=24.0).contains(&d), "{d}");
        // Retry-After is exact, never jittered.
        for _ in 0..50 {
            assert_eq!(retry_delay(7, Some(42)), Duration::from_secs(42));
        }
        assert_eq!(parse_retry_after("120"), Some(120));
        assert_eq!(parse_retry_after("120 (I'm in a meeting)"), Some(120));
        assert_eq!(parse_retry_after("18000;duration=3600"), Some(18000));
        assert_eq!(parse_retry_after("soon"), None);
    }

    #[test]
    fn comma_joined_contacts() {
        assert_eq!(
            split_list(r#""Doe, J" <sip:a@x;p=1,2>;expires=5, <sip:b@y>"#),
            [r#""Doe, J" <sip:a@x;p=1,2>;expires=5"#, "<sip:b@y>"]
        );
        assert_eq!(split_list(r#""a\", b" <sip:c@d>, sip:e@f"#).len(), 2);
        assert_eq!(split_list(" , "), Vec::<&str>::new());
        // One header line carrying several bindings: ours is the second.
        let hdr = vec![
            "<sip:a@10.9.9.9:5060>;expires=3600, <sip:a@10.0.0.5:5060>;expires=60".to_string(),
        ];
        assert_eq!(granted_expiry(&hdr, "sip:a@10.0.0.5:5060", None, 300), 60);
        assert_eq!(granted_expiry(&hdr, "sip:a@10.9.9.9:5060", None, 300), 3600);
    }

    #[test]
    fn handling_423() {
        assert_eq!(expires_after_423(60, Some(600)), Some(600));
        assert_eq!(expires_after_423(600, Some(600)), None);
        assert_eq!(expires_after_423(600, Some(60)), None);
        assert_eq!(expires_after_423(60, None), None);
    }

    #[test]
    fn contact_rewrite_decision() {
        let adv: SocketAddr = "10.0.0.5:5060".parse().unwrap();
        let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        // Nothing learned / identical -> keep.
        assert_eq!(contact_rewrite(adv, None, None), None);
        assert_eq!(contact_rewrite(adv, None, Some(5060)), None);
        assert_eq!(contact_rewrite(adv, ip("10.0.0.5"), Some(5060)), None);
        // NAT: public IP and mapped port.
        assert_eq!(
            contact_rewrite(adv, ip("203.0.113.9"), Some(40000)),
            Some("203.0.113.9:40000".parse().unwrap())
        );
        // Only the port differs / only the IP differs.
        assert_eq!(
            contact_rewrite(adv, None, Some(6000)),
            Some("10.0.0.5:6000".parse().unwrap())
        );
        assert_eq!(
            contact_rewrite(adv, ip("203.0.113.9"), None),
            Some("203.0.113.9:5060".parse().unwrap())
        );
    }

    #[test]
    fn via_params() {
        let v = "SIP/2.0/UDP 192.0.2.20:54892;branch=z9hG4bK1;rport=54892;received=192.0.2.20";
        assert_eq!(via_param(v, "rport").as_deref(), Some("54892"));
        assert_eq!(
            via_received_rport(v),
            (Some("192.0.2.20".parse().unwrap()), Some(54892))
        );
        // Valueless rport (request form) is not a learned port.
        assert_eq!(
            via_received_rport("SIP/2.0/UDP 1.2.3.4;rport;branch=x"),
            (None, None)
        );
        assert_eq!(
            via_received_rport("SIP/2.0/UDP [2001:db8::1]:5060;received=2001:db8::2;rport=7").0,
            Some("2001:db8::2".parse().unwrap())
        );
    }

    #[test]
    fn expiry_from_our_contact_param() {
        let ours = "sip:1003@192.0.2.20:54892;transport=TCP";
        let hdrs = vec![
            "<sip:other@10.9.9.9:5060>;expires=999".to_string(),
            "<sip:1003@192.0.2.20:54892;transport=tcp>;expires=120".to_string(),
        ];
        // Our contact's param wins over the Expires header and over other contacts.
        assert_eq!(granted_expiry(&hdrs, ours, Some(3600), 300), 120);
    }

    #[test]
    fn expiry_falls_back_to_header_then_request() {
        let ours = "sip:1003@1.2.3.4:5060";
        let no_param = vec!["<sip:1003@1.2.3.4:5060>".to_string()];
        assert_eq!(granted_expiry(&no_param, ours, Some(90), 300), 90);
        assert_eq!(granted_expiry(&no_param, ours, None, 300), 300);
        assert_eq!(granted_expiry(&[], ours, None, 300), 300);
        // Not our contact -> ignored.
        let other = vec!["<sip:zzz@9.9.9.9:5060>;expires=5".to_string()];
        assert_eq!(granted_expiry(&other, ours, Some(77), 300), 77);
        // Zero is "unknown".
        let zero = vec!["<sip:1003@1.2.3.4:5060>;expires=0".to_string()];
        assert_eq!(granted_expiry(&zero, ours, None, 300), 300);
    }

    #[test]
    fn contact_header_forms() {
        assert_eq!(
            split_contact("\"\" <sip:a@b:1;transport=TCP>;expires=60"),
            Some(("sip:a@b:1;transport=TCP".into(), Some(60)))
        );
        assert_eq!(
            split_contact("sip:a@b;expires=30"),
            Some(("sip:a@b".into(), Some(30)))
        );
        assert_eq!(
            split_contact("<sip:a@b>;q=0.5"),
            Some(("sip:a@b".into(), None))
        );
        // Default ports and case are normalised.
        assert_eq!(contact_key("sip:A@Host"), contact_key("sip:a@host:5060"));
        assert_ne!(contact_key("sips:a@host"), contact_key("sip:a@host"));
        assert_eq!(contact_key("sip:a@[::1]:5070").unwrap().2, 5070);
    }

    #[test]
    fn registration_timestamps_track_renewal_and_reset_after_loss_or_expiry() {
        use chrono::{TimeZone, Utc};
        let start = Utc.timestamp_opt(1_000, 0).unwrap();
        let first = RegState::Registering.renewed(start, 120);
        let renewed_at = start + chrono::Duration::seconds(60);
        let renewed = first.renewed(renewed_at, 300);
        assert_eq!(
            renewed,
            RegState::Registered {
                registered_at: start,
                last_renewed_at: renewed_at,
                expires_at: renewed_at + chrono::Duration::seconds(300),
                granted_secs: 300,
            }
        );
        assert!(!first.is_transition_to(&renewed));
        let later = start + chrono::Duration::seconds(500);
        for previous in [
            first,
            RegState::WaitingForNetwork,
            RegState::Unregistered,
            RegState::Disabled,
            RegState::Registering,
            RegState::Failed {
                code: 408,
                reason: "timeout".into(),
                retry_at: later,
            },
        ] {
            assert_eq!(
                previous.renewed(later, 60),
                RegState::Registered {
                    registered_at: later,
                    last_renewed_at: later,
                    expires_at: later + chrono::Duration::seconds(60),
                    granted_secs: 60,
                }
            );
        }
    }

    #[test]
    fn transition_detection() {
        use chrono::Utc;
        let reg = |g| RegState::Registered {
            registered_at: Utc::now(),
            last_renewed_at: Utc::now(),
            expires_at: Utc::now(),
            granted_secs: g,
        };
        let fail = |c: u16, r: &str| RegState::Failed {
            code: c,
            reason: r.into(),
            retry_at: Utc::now(),
        };
        assert!(RegState::Unregistered.is_transition_to(&RegState::Registering));
        assert!(RegState::Registering.is_transition_to(&reg(60)));
        assert!(!reg(60).is_transition_to(&reg(120)));
        assert!(!fail(403, "Forbidden").is_transition_to(&fail(403, "Forbidden")));
        assert!(fail(403, "Forbidden").is_transition_to(&fail(408, "timeout")));
        assert!(reg(60).is_transition_to(&fail(0, "x")));
        assert!(reg(60).is_transition_to(&RegState::WaitingForNetwork));
        assert!(!RegState::WaitingForNetwork.is_transition_to(&RegState::WaitingForNetwork));
        assert!(RegState::WaitingForNetwork.is_transition_to(&RegState::Registering));
    }

    use crate::types::RegState;
}
