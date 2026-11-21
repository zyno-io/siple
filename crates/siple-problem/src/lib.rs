//! The stable error catalog ("problems").
//!
//! Every failure Siple reports to a person or an API client maps to one [`Problem`]: a stable
//! snake_case kind, an English title and a one-sentence hint. The raw protocol text (`403
//! Forbidden`) stays in sibling fields; the problem is the plain-language layer on top.
//!
//! # Stability rules
//!
//! - Kinds are append-only: never renamed, never removed.
//! - A client that meets an unknown kind treats it as [`Problem::Other`].
//! - A SIP code may later move to a more specific kind.
//! - Title and hint text may be reworded; match on `kind`, never on text.
//!
//! # Translation
//!
//! The strings here are the English source. When Fluent arrives, each problem becomes the message
//! `problem-<kind>` with `.title` and `.hint` attributes, using these strings as the source text.
//! Never concatenate, case-change or interpolate these strings in the engine: parameters such as
//! host, account or code belong in sibling fields.

use serde::ser::{Serialize, SerializeStruct, Serializer};

macro_rules! catalog {
    ($( $(#[$m:meta])* $variant:ident, $id:literal, $title:literal, $hint:literal; )+) => {
        /// One stable error kind with its plain-language title and hint.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum Problem {
            $( $(#[$m])* $variant, )+
        }

        impl Problem {
            /// Every kind, in documentation order.
            pub const ALL: &'static [Problem] = &[ $( Problem::$variant, )+ ];

            /// The stable snake_case identifier.
            pub const fn id(self) -> &'static str {
                match self { $( Problem::$variant => $id, )+ }
            }

            /// Short English title (sentence case, no final period).
            pub const fn title(self) -> &'static str {
                match self { $( Problem::$variant => $title, )+ }
            }

            /// One-sentence English hint on what to try.
            pub const fn hint(self) -> &'static str {
                match self { $( Problem::$variant => $hint, )+ }
            }

            /// Looks a kind up by its identifier.
            pub fn from_id(id: &str) -> Option<Problem> {
                match id { $( $id => Some(Problem::$variant), )+ _ => None }
            }
        }
    };
}

catalog! {
    AuthFailed, "auth_failed", "Wrong username or password", "Check the account's username, auth username and password.";
    AuthForeignRefused, "auth_foreign_refused", "Password not sent to an unknown server", "A server outside your provider asked for your password; if your provider uses it, set auth_foreign = true for this account.";
    UserNotFound, "user_not_found", "Account not found on the server", "Check the username and the domain.";
    AccountConfig, "account_config", "Account settings are incomplete", "Check the account's domain, username and transport.";
    PortInUse, "port_in_use", "Local port already in use", "Another app is using the account's local port; close it or change local_port for this account.";
    DnsFailed, "dns_failed", "Can't find the server", "Check the domain or registrar for typos, and your internet connection.";
    NetworkUnreachable, "network_unreachable", "No network connection", "Check your Wi-Fi or cable; Siple reconnects when the network is back.";
    ConnectionFailed, "connection_failed", "Can't connect to the server", "Check the server address and port, and that the server accepts this transport.";
    NoResponse, "no_response", "The server did not answer", "Check the server address and your firewall; with UDP, try TCP or TLS.";
    TlsUntrusted, "tls_untrusted", "The server's certificate is not trusted", "If it is your server, trust its certificate from the account test; otherwise ask your provider.";
    TlsConfig, "tls_config", "Secure connection settings are invalid", "Check the account's TLS certificate, key, CA file and fingerprint.";
    ServerError, "server_error", "The server had a problem", "Try again in a moment; if it keeps happening, contact your provider.";
    Busy, "busy", "Busy", "The other side is on another call; try again later.";
    Declined, "declined", "Call declined", "The other side declined the call; try again later.";
    NumberNotFound, "number_not_found", "Number not found", "Check the number and try again.";
    Unavailable, "unavailable", "Not available right now", "The other side is offline or not taking calls; try again later.";
    Forbidden, "forbidden", "Not allowed", "The server does not allow this for your account; ask your provider or PBX administrator.";
    MediaMismatch, "media_mismatch", "No common audio format", "Check the account's codecs and SRTP setting, or ask your provider which ones it supports.";
    NotSupported, "not_supported", "Not supported by the other side", "The other phone or the PBX cannot do this; ask your administrator if you need it.";
    TryAgain, "try_again", "Busy with another request", "Wait a moment and try again.";
    CallGone, "call_gone", "The call no longer exists", "The other side or the server already ended it; call again if needed.";
    CallDropped, "call_dropped", "The call dropped", "The other side stopped responding; call again.";
    NetworkLost, "network_lost", "Call lost when the network changed", "Call again once the account shows registered.";
    MediaFailed, "media_failed", "Call audio could not start", "Check the selected microphone and speaker, then try again.";
    ExtensionNotFound, "extension_not_found", "Extension not found", "Check the extension number in the account's extension list.";
    BlfUnsupported, "blf_unsupported", "Line status not available", "The PBX does not report the status of this extension; ask your administrator or remove it.";
    Refused, "refused", "Refused by the server", "The details show the server's answer; your provider can tell you what it means.";
    Other, "other", "Something went wrong", "The details and the log have more information.";
}

impl Serialize for Problem {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Problem", 3)?;
        st.serialize_field("kind", self.id())?;
        st.serialize_field("title", self.title())?;
        st.serialize_field("hint", self.hint())?;
        st.end()
    }
}

/// What kind of request a SIP status answered; the same code means different things in each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Context {
    Register,
    Call,
    Subscribe,
    InDialog,
}

impl Context {
    pub const ALL: &'static [Context] = &[
        Context::Register,
        Context::Call,
        Context::Subscribe,
        Context::InDialog,
    ];

    /// Name used in generated documentation.
    pub const fn label(self) -> &'static str {
        match self {
            Context::Register => "REGISTER",
            Context::Call => "INVITE",
            Context::Subscribe => "SUBSCRIBE",
            Context::InDialog => "in-dialog",
        }
    }
}

/// Maps a SIP failure status to a problem. 0-299 give `None`; 700 and up give `Other`.
pub fn for_status(ctx: Context, code: u16) -> Option<Problem> {
    use Context::*;
    use Problem::*;
    // (register, call, subscribe, in_dialog)
    let row = |r, c, s, d| match ctx {
        Register => r,
        Call => c,
        Subscribe => s,
        InDialog => d,
    };
    Some(match code {
        0..=299 => return None,
        401 | 407 => AuthFailed,
        403 => row(AuthFailed, Forbidden, Forbidden, Forbidden),
        404 | 410 | 484 | 604 => row(
            UserNotFound,
            NumberNotFound,
            ExtensionNotFound,
            NumberNotFound,
        ),
        405 | 501 => row(Refused, NotSupported, BlfUnsupported, NotSupported),
        408 => NoResponse,
        415 => row(Refused, MediaMismatch, BlfUnsupported, MediaMismatch),
        420 | 421 => row(Refused, NotSupported, Refused, NotSupported),
        480 => row(Refused, Unavailable, Refused, Unavailable),
        481 => row(Refused, CallGone, Refused, CallGone),
        486 | 600 => row(Refused, Busy, Refused, Busy),
        488 | 606 => row(Refused, MediaMismatch, Refused, MediaMismatch),
        489 => row(Refused, Refused, BlfUnsupported, NotSupported),
        491 => TryAgain,
        603 | 607 | 608 => row(Refused, Declined, Forbidden, Declined),
        500..=599 => ServerError,
        300..=699 => Refused,
        _ => Other,
    })
}

impl Problem {
    /// Every SIP status in 300..=699 that `for_status` maps to this problem in `ctx` (for docs).
    pub fn codes(self, ctx: Context) -> Vec<u16> {
        (300..=699)
            .filter(|c| for_status(ctx, *c) == Some(self))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Kinds produced only by local classification, never by a SIP status.
    const LOCAL_ONLY: &[Problem] = &[
        Problem::AuthForeignRefused,
        Problem::AccountConfig,
        Problem::PortInUse,
        Problem::DnsFailed,
        Problem::NetworkUnreachable,
        Problem::ConnectionFailed,
        Problem::TlsUntrusted,
        Problem::TlsConfig,
        Problem::CallDropped,
        Problem::NetworkLost,
        Problem::MediaFailed,
        Problem::Other,
    ];

    /// The kinds shipped in V1. Append-only: every one must keep resolving.
    const V1: &[&str] = &[
        "auth_failed",
        "auth_foreign_refused",
        "user_not_found",
        "account_config",
        "port_in_use",
        "dns_failed",
        "network_unreachable",
        "connection_failed",
        "no_response",
        "tls_untrusted",
        "tls_config",
        "server_error",
        "busy",
        "declined",
        "number_not_found",
        "unavailable",
        "forbidden",
        "media_mismatch",
        "not_supported",
        "try_again",
        "call_gone",
        "call_dropped",
        "network_lost",
        "media_failed",
        "extension_not_found",
        "blf_unsupported",
        "refused",
        "other",
    ];

    #[test]
    fn ids_are_well_formed_unique_and_round_trip() {
        let mut seen = HashSet::new();
        for p in Problem::ALL {
            let id = p.id();
            assert!(id.starts_with(|c: char| c.is_ascii_lowercase()), "{id}");
            assert!(
                id.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{id}"
            );
            assert!(seen.insert(id), "duplicate {id}");
            assert_eq!(Problem::from_id(id), Some(*p));
        }
        assert_eq!(Problem::from_id("nope"), None);
    }

    #[test]
    fn v1_kinds_still_resolve() {
        assert_eq!(V1.len(), 28);
        for id in V1 {
            assert!(
                Problem::from_id(id).is_some(),
                "{id} was removed or renamed"
            );
        }
        assert!(Problem::ALL.len() >= V1.len());
    }

    #[test]
    fn text_rules() {
        for p in Problem::ALL {
            let (t, h) = (p.title(), p.hint());
            assert!(!t.is_empty() && t.chars().count() <= 48, "{t}");
            assert!(t.starts_with(|c: char| c.is_uppercase()), "{t}");
            assert!(!t.ends_with('.'), "{t}");
            assert!(!t.chars().any(|c| c.is_ascii_digit()), "{t}");
            assert!(!h.is_empty() && h.chars().count() <= 140, "{h}");
            assert!(h.ends_with('.'), "{h}");
            assert!(!h.contains(". "), "{h}");
            assert!(!h.contains('\n'), "{h}");
        }
    }

    #[test]
    fn serializes_kind_title_hint() {
        let v = serde_json::to_value(Problem::Busy).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"kind":"busy","title":"Busy","hint":"The other side is on another call; try again later."})
        );
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn status_table_rows() {
        use Context::*;
        use Problem::*;
        let rows: &[(Context, u16, Problem)] = &[
            (Register, 401, AuthFailed),
            (Call, 401, AuthFailed),
            (Subscribe, 407, AuthFailed),
            (InDialog, 407, AuthFailed),
            (Register, 403, AuthFailed),
            (Call, 403, Forbidden),
            (Subscribe, 403, Forbidden),
            (InDialog, 403, Forbidden),
            (Register, 404, UserNotFound),
            (Call, 404, NumberNotFound),
            (Subscribe, 404, ExtensionNotFound),
            (InDialog, 404, NumberNotFound),
            (Register, 410, UserNotFound),
            (Call, 484, NumberNotFound),
            (Subscribe, 604, ExtensionNotFound),
            (Register, 405, Refused),
            (Call, 405, NotSupported),
            (Subscribe, 501, BlfUnsupported),
            (InDialog, 501, NotSupported),
            (Register, 408, NoResponse),
            (Call, 408, NoResponse),
            (Register, 415, Refused),
            (Call, 415, MediaMismatch),
            (Subscribe, 415, BlfUnsupported),
            (Call, 420, NotSupported),
            (Subscribe, 421, Refused),
            (Call, 480, Unavailable),
            (Register, 480, Refused),
            (InDialog, 481, CallGone),
            (Subscribe, 481, Refused),
            (Call, 486, Busy),
            (InDialog, 600, Busy),
            (Subscribe, 486, Refused),
            (Call, 488, MediaMismatch),
            (InDialog, 606, MediaMismatch),
            (Call, 489, Refused),
            (Subscribe, 489, BlfUnsupported),
            (InDialog, 489, NotSupported),
            (Register, 491, TryAgain),
            (Call, 491, TryAgain),
            (Call, 603, Declined),
            (Subscribe, 603, Forbidden),
            (InDialog, 608, Declined),
            (Register, 500, ServerError),
            (Call, 503, ServerError),
            (Subscribe, 599, ServerError),
            (Register, 300, Refused),
            (Call, 302, Refused),
            (InDialog, 400, Refused),
            (Call, 699, Refused),
        ];
        assert!(rows.len() >= 40);
        for (ctx, code, want) in rows {
            assert_eq!(for_status(*ctx, *code), Some(*want), "{ctx:?} {code}");
        }
    }

    #[test]
    fn status_ranges() {
        for ctx in Context::ALL {
            for code in 0..=299 {
                assert_eq!(for_status(*ctx, code), None);
            }
            for code in 300..=699 {
                assert!(for_status(*ctx, code).is_some(), "{ctx:?} {code}");
            }
            for code in [700, 999, 65535] {
                assert_eq!(for_status(*ctx, code), Some(Problem::Other));
            }
        }
    }

    #[test]
    fn no_orphans() {
        for p in Problem::ALL {
            let from_status = Context::ALL.iter().any(|c| !p.codes(*c).is_empty());
            let local = LOCAL_ONLY.contains(p);
            assert!(
                from_status || local,
                "{} is neither produced by for_status nor LOCAL_ONLY",
                p.id()
            );
            // Local-only kinds must not also be status-produced (keeps the sets honest).
            assert!(!(from_status && local), "{} is in both sets", p.id());
        }
    }
}
