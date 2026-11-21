//! Classification of failures into the stable problem catalog (`siple-problem`).
//!
//! The problem is always computed from what a failure already carries (context, SIP code, reason
//! text), never stored, so it cannot go stale. Local failures (code 0) arrive as text, so
//! [`classify`] matches the text; a test pins the typed [`crate::SipError`] path to the same
//! answer.

pub use siple_problem::{Context, Problem};

use crate::authz;
use crate::net::is_unreachable_text;
use crate::subscribe::TERMINATED;

/// Classifies a failure. `None` means "not a failure we can name" (a success code, or a local
/// text without a known category); callers usually fall back to [`Problem::Other`].
pub fn classify(ctx: Context, code: u16, reason: &str) -> Option<Problem> {
    // The dial path reports a refused challenge as 401 with this text.
    if reason.starts_with(authz::REFUSED) {
        return Some(Problem::AuthForeignRefused);
    }
    if code == 0 {
        return local(ctx, reason);
    }
    siple_problem::for_status(ctx, code)
}

fn local(ctx: Context, text: &str) -> Option<Problem> {
    let lower = text.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if text.starts_with("DNS: ")
        || text.contains("DNS resolution error")
        || text.contains("failed to lookup address")
    {
        return Some(Problem::DnsFailed);
    }
    if text.starts_with("no route: ") || is_unreachable_text(text) {
        return Some(Problem::NetworkUnreachable);
    }
    if has(&[
        "address already in use",
        "os error 10048",
        "only one usage of each socket address",
    ]) {
        return Some(Problem::PortInUse);
    }
    if text.contains("invalid peer certificate") || text == "TLS: certificate not trusted" {
        return Some(Problem::TlsUntrusted);
    }
    if text.starts_with("TLS: ") {
        return Some(Problem::TlsConfig);
    }
    if text.starts_with("invalid account config: ") {
        return Some(Problem::AccountConfig);
    }
    if text.starts_with("media: ") {
        return Some(Problem::MediaFailed);
    }
    if ctx == Context::Subscribe
        && let Some(why) = text.strip_prefix(TERMINATED)
    {
        return match why.trim().to_ascii_lowercase().as_str() {
            "noresource" => Some(Problem::ExtensionNotFound),
            "rejected" => Some(Problem::Forbidden),
            _ => None,
        };
    }
    if text.contains("too large") {
        return None;
    }
    if has(&["timed out", "timeout", "no response"]) {
        return Some(Problem::NoResponse);
    }
    // The account has no runtime (disabled, waiting for network, failed build): the account's own
    // state already says why, so this is not a connection problem.
    if text.contains("account transport is not up") || text.contains("account is not running") {
        return None;
    }
    if text.starts_with("transport: ")
        || text.contains("Transport layer error")
        || text.contains("I/O error")
        || has(&[
            "connection refused",
            "connection reset",
            "broken pipe",
            "connection closed",
        ])
    {
        return Some(Problem::ConnectionFailed);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SipError;
    use Context::*;

    #[test]
    fn local_texts() {
        let rows: &[(Context, &str, Option<Problem>)] = &[
            (Call, "transport: account transport is not up", None),
            (Call, "account is not running", None),
            (InDialog, "protocol: account is not running", None),
            (Register, "DNS: no such host", Some(Problem::DnsFailed)),
            (
                Register,
                "protocol: DNS resolution error: foo",
                Some(Problem::DnsFailed),
            ),
            (
                Register,
                "failed to lookup address information: Name or service not known",
                Some(Problem::DnsFailed),
            ),
            (
                Register,
                "no route: Network is unreachable (os error 51)",
                Some(Problem::NetworkUnreachable),
            ),
            (
                Register,
                "connect failed: No route to host (os error 65)",
                Some(Problem::NetworkUnreachable),
            ),
            (
                Register,
                "transport: Address already in use (os error 48)",
                Some(Problem::PortInUse),
            ),
            (
                Register,
                "only one usage of each socket address (os error 10048)",
                Some(Problem::PortInUse),
            ),
            (
                Register,
                "invalid peer certificate: UnknownIssuer",
                Some(Problem::TlsUntrusted),
            ),
            (
                Register,
                "TLS: certificate not trusted",
                Some(Problem::TlsUntrusted),
            ),
            (
                Register,
                "TLS: cannot read key file",
                Some(Problem::TlsConfig),
            ),
            (
                Register,
                "invalid account config: no domain",
                Some(Problem::AccountConfig),
            ),
            (Call, "media: no audio device", Some(Problem::MediaFailed)),
            (Register, "request timed out", Some(Problem::NoResponse)),
            (Call, "No response to INVITE", Some(Problem::NoResponse)),
            (
                Register,
                "Transport layer error: closed",
                Some(Problem::ConnectionFailed),
            ),
            (
                Register,
                "I/O error: whatever",
                Some(Problem::ConnectionFailed),
            ),
            (
                Register,
                "Connection refused (os error 61)",
                Some(Problem::ConnectionFailed),
            ),
            (
                Register,
                "connection reset by peer",
                Some(Problem::ConnectionFailed),
            ),
            (
                Call,
                "INVITE is about 1500 bytes, too large for one UDP datagram; use TCP",
                None,
            ),
            (Call, "the original call is over", None),
            (
                Subscribe,
                "subscription terminated: noresource",
                Some(Problem::ExtensionNotFound),
            ),
            (
                Subscribe,
                "subscription terminated: rejected",
                Some(Problem::Forbidden),
            ),
            (Subscribe, "subscription terminated: timeout", None),
            (Subscribe, "subscription terminated: no reason", None),
            (Call, "subscription terminated: noresource", None),
            (Register, "something else entirely", None),
        ];
        for (ctx, text, want) in rows {
            assert_eq!(classify(*ctx, 0, text), *want, "{ctx:?} {text:?}");
        }
    }

    #[test]
    fn statuses_and_refused() {
        assert_eq!(
            classify(Register, 403, "Forbidden"),
            Some(Problem::AuthFailed)
        );
        assert_eq!(classify(Call, 403, "Forbidden"), Some(Problem::Forbidden));
        assert_eq!(classify(Call, 486, "Busy Here"), Some(Problem::Busy));
        assert_eq!(classify(Call, 200, "OK"), None);
        assert_eq!(
            classify(
                Call,
                401,
                "Authentication refused: evil.example is not your provider"
            ),
            Some(Problem::AuthForeignRefused)
        );
    }

    #[test]
    fn typed_and_text_paths_agree() {
        let samples = [
            SipError::Config("no domain".into()),
            SipError::Dns("no such host".into()),
            SipError::NoRoute("Network is unreachable (os error 51)".into()),
            SipError::Timeout,
            SipError::Media("no device".into()),
            SipError::Tls("certificate not trusted".into()),
            SipError::Tls("bad key".into()),
            SipError::Transport("Connection refused (os error 61)".into()),
            SipError::Transport("Address already in use (os error 48)".into()),
            SipError::Protocol("Transport layer error: closed".into()),
            SipError::Protocol("Authentication refused: x is not your provider".into()),
            SipError::Protocol("something odd".into()),
        ];
        for e in samples {
            assert_eq!(e.problem(), classify(InDialog, 0, &e.to_string()), "{e:?}");
        }
    }

    #[test]
    fn sip_error_problems() {
        assert_eq!(
            SipError::Status {
                code: 486,
                reason: "Busy Here".into()
            }
            .problem(),
            Some(Problem::Busy)
        );
        assert_eq!(SipError::Closed.problem(), None);
        assert_eq!(SipError::UnknownCall("c1".into()).problem(), None);
        assert_eq!(
            SipError::CallState {
                call: "c1".into(),
                msg: "x".into()
            }
            .problem(),
            None
        );
        assert_eq!(SipError::InvalidArgument("x".into()).problem(), None);
        assert_eq!(SipError::DuplicateAccount("a".into()).problem(), None);
    }

    #[test]
    fn rsipstack_displays_classify() {
        use rsipstack::transport::SipAddr;
        let dns = rsipstack::Error::DnsResolutionError("no such host".into()).to_string();
        assert_eq!(classify(Register, 0, &dns), Some(Problem::DnsFailed));
        let tl = rsipstack::Error::TransportLayerError("connect failed".into(), SipAddr::default())
            .to_string();
        assert_eq!(classify(Register, 0, &tl), Some(Problem::ConnectionFailed));
    }

    mod wire {
        use super::*;
        use crate::blf::SubscriptionPhase;
        use crate::{
            BlfSubscription, CallEnd, Diagnosis, EndCause, EndedBy, RegState, TransferError,
        };
        use chrono::Utc;
        use serde_json::{Value, json};

        fn keys(v: &Value) -> Vec<String> {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        }

        #[test]
        fn reg_state_failed_carries_a_problem_and_registered_does_not() {
            let failed = RegState::Failed {
                code: 403,
                reason: "Forbidden".into(),
                retry_at: Utc::now(),
            };
            let v = serde_json::to_value(&failed).unwrap();
            assert_eq!(keys(&v), ["code", "problem", "reason", "retry_at", "state"]);
            assert_eq!(v["state"], "failed");
            assert_eq!(v["problem"]["kind"], "auth_failed");
            assert_eq!(failed.problem(), Some(Problem::AuthFailed));

            let now = Utc::now();
            let reg = RegState::Registered {
                registered_at: now,
                last_renewed_at: now,
                expires_at: now,
                granted_secs: 60,
            };
            let v = serde_json::to_value(&reg).unwrap();
            assert_eq!(
                keys(&v),
                [
                    "expires_at",
                    "granted_secs",
                    "last_renewed_at",
                    "registered_at",
                    "state"
                ]
            );
            assert_eq!(reg.problem(), None);
            for st in [
                RegState::Disabled,
                RegState::Unregistered,
                RegState::Registering,
                RegState::WaitingForNetwork,
            ] {
                let v = serde_json::to_value(&st).unwrap();
                assert_eq!(keys(&v), ["state"], "{st:?}");
            }
        }

        #[test]
        fn reg_state_unknown_failure_is_other() {
            let failed = RegState::Failed {
                code: 0,
                reason: "gremlins".into(),
                retry_at: Utc::now(),
            };
            assert_eq!(failed.problem(), Some(Problem::Other));
        }

        fn end(cause: EndCause, by: EndedBy, code: u16, reason: &str) -> CallEnd {
            CallEnd {
                code,
                reason: reason.into(),
                by,
                missed: false,
                q850_cause: None,
                cause,
            }
        }

        #[test]
        fn call_end_problem_table() {
            use EndCause::*;
            use EndedBy::*;
            let rows: &[(CallEnd, Option<Problem>)] = &[
                (end(Hangup, Local, 200, "OK"), None),
                (end(RemoteHangup, Remote, 200, "OK"), None),
                (end(Transferred, Remote, 200, "OK"), None),
                (end(Replaced, Remote, 200, "OK"), None),
                (end(Cancelled, Local, 487, "Request Terminated"), None),
                (end(Missed, Remote, 487, "Request Terminated"), None),
                (end(Rejected, Local, 486, "Busy Here"), None),
                (end(Rejected, Remote, 486, "Busy Here"), Some(Problem::Busy)),
                (
                    end(Rejected, Remote, 603, "Decline"),
                    Some(Problem::Declined),
                ),
                (
                    end(Rejected, Remote, 404, "Not Found"),
                    Some(Problem::NumberNotFound),
                ),
                (
                    end(Failed, Remote, 503, "Service Unavailable"),
                    Some(Problem::ServerError),
                ),
                (
                    end(Failed, Remote, 408, "Request Timeout"),
                    Some(Problem::NoResponse),
                ),
                (
                    end(Failed, Local, 0, "DNS: no such host"),
                    Some(Problem::DnsFailed),
                ),
                (end(Failed, Local, 0, "gremlins"), Some(Problem::Other)),
                (
                    end(SessionExpired, Remote, 408, "Session expired"),
                    Some(Problem::CallDropped),
                ),
                (
                    end(NetworkLost, Remote, 481, "gone"),
                    Some(Problem::NetworkLost),
                ),
            ];
            for (e, want) in rows {
                assert_eq!(e.problem(), *want, "{e:?}");
                let v = serde_json::to_value(e).unwrap();
                match want {
                    Some(p) => assert_eq!(v["problem"]["kind"], p.id()),
                    None => assert!(v.get("problem").is_none(), "{v}"),
                }
                assert_eq!(v["code"], e.code);
                assert_eq!(v["reason"], e.reason.as_str());
            }
            let v = serde_json::to_value(end(Hangup, Local, 200, "OK")).unwrap();
            assert_eq!(
                keys(&v),
                ["by", "cause", "code", "missed", "q850_cause", "reason"]
            );
        }

        #[test]
        fn blf_subscription_serialization() {
            let healthy = BlfSubscription {
                phase: SubscriptionPhase::Active,
                last_code: None,
                last_reason: None,
                expires_at: None,
                retry_at: None,
            };
            assert_eq!(healthy.problem(), None);
            let v = serde_json::to_value(&healthy).unwrap();
            assert_eq!(
                keys(&v),
                [
                    "expires_at",
                    "last_code",
                    "last_reason",
                    "phase",
                    "retry_at"
                ]
            );

            let failed = BlfSubscription {
                phase: SubscriptionPhase::Failed,
                last_code: Some(404),
                last_reason: Some("Not Found".into()),
                ..healthy.clone()
            };
            assert_eq!(failed.problem(), Some(Problem::ExtensionNotFound));
            let v = serde_json::to_value(&failed).unwrap();
            assert_eq!(v["last_problem"]["kind"], "extension_not_found");
            assert_eq!(v["last_code"], 404);

            let local = BlfSubscription {
                last_code: None,
                last_reason: Some(format!("{TERMINATED}noresource")),
                ..failed
            };
            assert_eq!(local.problem(), Some(Problem::ExtensionNotFound));
        }

        #[test]
        fn transfer_error_serialization() {
            let e = |stage: &str, code, reason: &str| TransferError {
                stage: stage.into(),
                code,
                reason: reason.into(),
            };
            assert_eq!(
                e("consult", 486, "Busy Here").problem(),
                Some(Problem::Busy)
            );
            assert_eq!(
                e("refer", 403, "Forbidden").problem(),
                Some(Problem::Forbidden)
            );
            assert_eq!(
                e("notify", 603, "Decline").problem(),
                Some(Problem::Declined)
            );
            assert_eq!(e("swap", 481, "gone").problem(), Some(Problem::CallGone));
            assert_eq!(
                e("resume", 0, "request timed out").problem(),
                Some(Problem::NoResponse)
            );
            assert_eq!(e("superseded", 0, "replaced").problem(), None);
            assert_eq!(e("refer", 0, "gremlins").problem(), Some(Problem::Other));
            let v = serde_json::to_value(e("refer", 403, "Forbidden")).unwrap();
            assert_eq!(
                v,
                json!({"stage":"refer","code":403,"reason":"Forbidden","problem":Problem::Forbidden})
            );
            let v = serde_json::to_value(e("superseded", 0, "x")).unwrap();
            assert!(v.get("problem").is_none());
        }

        #[test]
        fn diagnosis_mapping() {
            use siple_config::Transport;
            let other = |code, reason: &str| Diagnosis::Other {
                code,
                reason: reason.into(),
            };
            let ok = Diagnosis::Ok {
                transport: Transport::Udp,
                auto: false,
                server: "x".into(),
                granted_secs: 60,
                elapsed_ms: 1,
            };
            assert_eq!(ok.problem(), None);
            assert_eq!(
                Diagnosis::AuthFailed { code: 401 }.problem(),
                Some(Problem::AuthFailed)
            );
            assert_eq!(Diagnosis::NotFound.problem(), Some(Problem::UserNotFound));
            assert_eq!(
                Diagnosis::Timeout {
                    transport: Transport::Udp,
                    hint: "h".into()
                }
                .problem(),
                Some(Problem::NoResponse)
            );
            assert_eq!(
                Diagnosis::TlsUntrusted {
                    fingerprint: "ab".into(),
                    reason: "r".into()
                }
                .problem(),
                Some(Problem::TlsUntrusted)
            );
            assert_eq!(
                Diagnosis::DnsFailed {
                    message: "m".into()
                }
                .problem(),
                Some(Problem::DnsFailed)
            );
            assert_eq!(
                Diagnosis::NetworkUnreachable {
                    message: "m".into()
                }
                .problem(),
                Some(Problem::NetworkUnreachable)
            );
            assert_eq!(
                other(0, crate::diag::NO_REGISTRAR).problem(),
                Some(Problem::AccountConfig)
            );
            assert_eq!(
                other(0, crate::diag::BAD_ADDRESS).problem(),
                Some(Problem::AccountConfig)
            );
            assert_eq!(
                other(503, "Service Unavailable").problem(),
                Some(Problem::ServerError)
            );
            assert_eq!(
                other(0, "Connection refused (os error 61)").problem(),
                Some(Problem::ConnectionFailed)
            );
            assert_eq!(other(0, "gremlins").problem(), Some(Problem::Other));
        }
    }
}
