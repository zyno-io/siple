//! Directed call pickup of a ringing BLF extension: resolving
//! what to dial for `pickup = "prefix"` and building the `Replaces` header of `"replaces"`.

use siple_config::{BlfConfig, PickupMode, PickupReplacesTarget, PickupSetting};

use crate::blf::BlfDialog;
use crate::error::{Result, SipError};

/// What a pickup of one BLF entry sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickupPlan {
    /// Dial this target on the BLF's account (feature code or explicit target).
    Dial(String),
    /// INVITE `target` carrying `Replaces: <replaces>` and `Require: replaces`.
    Replaces {
        /// Request-URI target (a SIP URI or an extension).
        target: String,
        /// Value of the `Replaces` header.
        replaces: String,
    },
}

/// `pickup_target` if set, else `<pickup_prefix><extension>`.
pub fn prefix_target(blf: &BlfConfig, setting: &PickupSetting) -> String {
    if setting.target.is_empty() {
        format!("{}{}", setting.prefix, blf.extension())
    } else {
        setting.target.clone()
    }
}

/// RFC 3261 `word` character (the Call-ID alphabet).
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-.!%*_+`'~()<>:\\\"/[]?{}".contains(c)
}

/// RFC 3261 `token` character (tags).
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-.!%*_+`'~".contains(c)
}

/// `callid = word [ "@" word ]`. NOTIFY content is untrusted: anything else (CR LF, `;`, space)
/// would let a notifier forge header parameters.
pub fn valid_call_id(s: &str) -> bool {
    let word = |w: &str| !w.is_empty() && w.chars().all(is_word_char);
    match s.split_once('@') {
        Some((a, b)) => word(a) && word(b),
        None => word(s),
    }
}

/// A tag must be a non-empty RFC 3261 `token`.
pub fn valid_tag(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_token_char)
}

/// A target URI from a NOTIFY: no whitespace or control characters, no `<>` / quotes that could
/// break out of a request line.
pub fn safe_uri(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| !c.is_control() && !c.is_whitespace() && !"<>\"".contains(c))
}

/// Reduces a NOTIFY-supplied target to `sip[s]:[user@]host[:port]`: URI parameters (`maddr`,
/// `transport`, `ttl`, ...) and `?headers` are dropped, as for redirect targets, so the notifier
/// cannot steer routing or add headers. `None` for anything that is not a plain `sip:` / `sips:`
/// URI.
pub fn clean_target(s: &str) -> Option<String> {
    if !safe_uri(s) {
        return None;
    }
    let (scheme, rest) = s.split_once(':')?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "sip" && scheme != "sips" {
        return None;
    }
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u.split([';', '?']).next().unwrap_or(u)), h),
        None => (None, rest),
    };
    let hostport = hostport.split([';', '?']).next().unwrap_or(hostport);
    if hostport.is_empty() || user.is_some_and(str::is_empty) {
        return None;
    }
    Some(match user {
        Some(u) => format!("{scheme}:{u}@{hostport}"),
        None => format!("{scheme}:{hostport}"),
    })
}

/// Value of the `Replaces` header (RFC 3891) for the early dialog `d` of the monitored phone,
/// with `early-only`. `None` when the call-id or a tag is missing or not a valid `word` /
/// `token`.
///
/// Tag orientation. RFC 3891 §3 reads `to-tag` as the *local* tag and `from-tag` as the *remote*
/// tag of the dialog **as seen by the UA that receives the INVITE** (our own `match_replaces`
/// does the same). RFC 4235 dialog-info describes the dialog from the *monitored* phone's view:
/// `local-tag` is the monitored phone's, `remote-tag` is the other end's. The dialog being
/// replaced is the one the *originating* UA (the caller, or the PBX that originated the callee
/// leg) has: its local tag is what dialog-info calls `remote-tag`. So for either request target
/// `to-tag = remote-tag` and `from-tag = local-tag`; `pickup_replaces_target` only changes the
/// Request-URI. (RFC 5359 §2.16; RFC 3891 §4 forbids replacing an early dialog on a UA that did
/// not originate it.)
pub fn replaces_value(d: &BlfDialog) -> Option<String> {
    let (cid, local, remote) = (
        d.call_id.as_deref().filter(|c| valid_call_id(c))?,
        d.local_tag.as_deref().filter(|t| valid_tag(t))?,
        d.remote_tag.as_deref().filter(|t| valid_tag(t))?,
    );
    Some(format!("{cid};to-tag={remote};from-tag={local};early-only"))
}

/// Request target of a `Replaces` pickup: the caller's dialog-info target (else identity),
/// reduced by [`clean_target`], or the configured monitored URI.
fn replaces_target(blf: &BlfConfig, d: &BlfDialog, who: PickupReplacesTarget) -> Option<String> {
    match who {
        PickupReplacesTarget::Monitored => Some(blf.uri.clone()),
        PickupReplacesTarget::Caller => d
            .remote_target
            .iter()
            .chain(d.remote_identity.iter())
            .find_map(|u| clean_target(u)),
    }
}

/// Turns the settings and the ringing dialog into what to send.
pub(crate) fn plan(
    blf: &BlfConfig,
    setting: &PickupSetting,
    ringing: &BlfDialog,
) -> Result<PickupPlan> {
    let refuse = |msg: &str| SipError::CallState {
        call: blf.uri.clone(),
        msg: msg.into(),
    };
    match setting.mode {
        PickupMode::Off => Err(not_configured(blf)),
        PickupMode::Prefix => Ok(PickupPlan::Dial(prefix_target(blf, setting))),
        PickupMode::Replaces => {
            let who = setting.replaces_target;
            let replaces = replaces_value(ringing).ok_or_else(|| {
                refuse(
                    "pickup = \"replaces\" needs a dialog-info with a valid call-id, local-tag \
                     and remote-tag; this PBX did not send them (use pickup = \"prefix\")",
                )
            })?;
            let target = replaces_target(blf, ringing, who).ok_or_else(|| {
                refuse(
                    "pickup = \"replaces\" needs the caller's target in the dialog-info \
                     (<remote>); this PBX did not send it (use pickup = \"prefix\")",
                )
            })?;
            Ok(PickupPlan::Replaces { target, replaces })
        }
    }
}

/// The error of a pickup that is not configured.
pub(crate) fn not_configured(blf: &BlfConfig) -> SipError {
    SipError::CallState {
        call: blf.uri.clone(),
        msg: "pickup not configured".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blf::{DialogDirection, DialogPhase};
    use siple_config::{AccountConfig, OrInherit};

    fn dialog(tags: bool) -> BlfDialog {
        BlfDialog {
            id: "d".into(),
            call_id: Some("abc@10.0.0.1".into()),
            local_tag: tags.then(|| "LOCAL".into()),
            remote_tag: tags.then(|| "REMOTE".into()),
            direction: Some(DialogDirection::Recipient),
            state: DialogPhase::Early,
            remote_identity: None,
            remote_display: None,
            local_identity: None,
            local_target: None,
            remote_target: None,
            rendering: None,
        }
    }

    fn acct(mode: PickupMode) -> AccountConfig {
        AccountConfig {
            pickup: mode,
            ..Default::default()
        }
    }

    fn blf(uri: &str) -> BlfConfig {
        BlfConfig {
            uri: uri.into(),
            ..Default::default()
        }
    }

    #[test]
    fn method_resolution_account_vs_override() {
        let mut b = blf("1004");
        assert_eq!(b.pickup_for(&acct(PickupMode::Off)).mode, PickupMode::Off);
        assert_eq!(
            b.pickup_for(&acct(PickupMode::Prefix)).mode,
            PickupMode::Prefix
        );
        b.pickup = OrInherit::Value(PickupMode::Replaces);
        assert_eq!(
            b.pickup_for(&acct(PickupMode::Prefix)).mode,
            PickupMode::Replaces
        );
        // The override can also switch pickup off for one key.
        b.pickup = OrInherit::Value(PickupMode::Off);
        assert_eq!(
            b.pickup_for(&acct(PickupMode::Prefix)).mode,
            PickupMode::Off
        );
        let d = dialog(true);
        assert!(matches!(
            plan(&b, &b.pickup_for(&acct(PickupMode::Prefix)), &d),
            Err(SipError::CallState { msg, .. }) if msg == "pickup not configured"
        ));
    }

    #[test]
    fn prefix_target_building() {
        let a = acct(PickupMode::Prefix);
        let mut b = blf("1004");
        assert_eq!(prefix_target(&b, &b.pickup_for(&a)), "**1004");
        // Full URI: the user part is the extension.
        let u = blf("sip:1004@pbx.example");
        assert_eq!(prefix_target(&u, &u.pickup_for(&a)), "**1004");
        b.pickup_prefix = "*97".into();
        assert_eq!(
            prefix_target(&b, &b.pickup_for(&a)),
            "*97".to_string() + "1004"
        );
        b.pickup_target = "8004".into();
        assert_eq!(prefix_target(&b, &b.pickup_for(&a)), "8004");
        assert_eq!(
            plan(&b, &b.pickup_for(&a), &dialog(false)).unwrap(),
            PickupPlan::Dial("8004".into())
        );
    }

    #[test]
    fn replaces_header_orientation() {
        // to-tag = the dialog-info remote-tag (the originating UA's local tag),
        // from-tag = local-tag; the same for both request targets.
        assert_eq!(
            replaces_value(&dialog(true)).as_deref(),
            Some("abc@10.0.0.1;to-tag=REMOTE;from-tag=LOCAL;early-only")
        );
        assert!(replaces_value(&dialog(false)).is_none());
        let mut d = dialog(true);
        d.call_id = None;
        assert!(replaces_value(&d).is_none());
    }

    #[test]
    fn notify_targets_are_reduced_to_plain_uris() {
        assert_eq!(
            clean_target("sip:1019@pbx.example:5060").as_deref(),
            Some("sip:1019@pbx.example:5060")
        );
        assert_eq!(
            clean_target("SIP:**1018@pbx;maddr=6.6.6.6;transport=tcp?Route=%3Csip:evil%3E")
                .as_deref(),
            Some("sip:**1018@pbx")
        );
        assert_eq!(
            clean_target("sips:u;phone-context=x@h;ttl=1").as_deref(),
            Some("sips:u@h")
        );
        assert_eq!(
            clean_target("sip:pbx;maddr=1.2.3.4").as_deref(),
            Some("sip:pbx")
        );
        for bad in [
            "",
            "tel:+1555",
            "http://x",
            "sip:",
            "sip:@h",
            "sip:a@b\r\nX: 1",
            "sip:a b@h",
        ] {
            assert!(clean_target(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn replaces_plan_targets_caller_or_monitored() {
        let mut a = acct(PickupMode::Replaces);
        let b = blf("1004");
        let mut d = dialog(true);
        d.remote_target = Some("sip:**1004@pbx.example;maddr=1.2.3.4?X=y".into());
        d.remote_identity = Some("sip:1019@pbx.example".into());
        // Default: the caller's <remote><target>.
        assert_eq!(
            plan(&b, &b.pickup_for(&a), &d).unwrap(),
            PickupPlan::Replaces {
                target: "sip:**1004@pbx.example".into(),
                replaces: "abc@10.0.0.1;to-tag=REMOTE;from-tag=LOCAL;early-only".into()
            }
        );
        // No remote target: the remote identity.
        d.remote_target = None;
        assert!(matches!(
            plan(&b, &b.pickup_for(&a), &d).unwrap(),
            PickupPlan::Replaces { target, .. } if target == "sip:1019@pbx.example"
        ));
        // Neither: refused.
        d.remote_identity = None;
        assert!(matches!(
            plan(&b, &b.pickup_for(&a), &d),
            Err(SipError::CallState { .. })
        ));
        // Monitored: the monitored URI as target, same tags, no caller info needed.
        a.pickup_replaces_target = PickupReplacesTarget::Monitored;
        assert_eq!(
            plan(&b, &b.pickup_for(&a), &d).unwrap(),
            PickupPlan::Replaces {
                target: "1004".into(),
                replaces: "abc@10.0.0.1;to-tag=REMOTE;from-tag=LOCAL;early-only".into()
            }
        );
        assert!(matches!(
            plan(&b, &b.pickup_for(&a), &dialog(false)),
            Err(SipError::CallState { .. })
        ));
    }

    #[test]
    fn notify_content_cannot_inject_headers() {
        assert!(valid_call_id("abc@10.0.0.1") && valid_call_id("a1-b2.c3"));
        for bad in [
            "",
            "a\r\nRequire: x",
            "a b",
            "a;b=c",
            "a@b@c",
            "@b",
            "a@",
            "a\nb",
        ] {
            assert!(!valid_call_id(bad), "{bad:?}");
        }
        assert!(valid_tag("a1-B.2_x"));
        for bad in ["", "a;b", "a b", "a\r\nX: y", "a=b", "a\"b", "a@b"] {
            assert!(!valid_tag(bad), "{bad:?}");
        }
        assert!(safe_uri("sip:1019@pbx:5060;transport=udp"));
        for bad in ["", "sip:a@b\r\nX: y", "sip:a b", "sip:a@b>", "sip:\u{7}a@b"] {
            assert!(!safe_uri(bad), "{bad:?}");
        }
        // Forged values that bypassed the parser are refused too.
        let a = acct(PickupMode::Replaces);
        let b = blf("1004");
        for tweak in 0..3 {
            let mut d = dialog(true);
            d.remote_target = Some("sip:1019@pbx".into());
            match tweak {
                0 => d.call_id = Some("x@y\r\nRequire: foo".into()),
                1 => d.local_tag = Some("t\r\nX: 1".into()),
                _ => d.remote_tag = Some("t;early-only=no".into()),
            }
            assert!(
                matches!(
                    plan(&b, &b.pickup_for(&a), &d),
                    Err(SipError::CallState { .. })
                ),
                "{tweak}"
            );
        }
        // A forged target URI is skipped (falls back to the identity, or refuses).
        let mut d = dialog(true);
        d.remote_target = Some("sip:1019@pbx\r\nX: 1".into());
        d.remote_identity = None;
        assert!(plan(&b, &b.pickup_for(&a), &d).is_err());
    }
}
