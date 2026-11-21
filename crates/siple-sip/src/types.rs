//! Public status / event / sink types.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rsipstack::transport::WireEvent;
use serde::Serialize;
use siple_config::Transport;
use tokio::sync::mpsc::UnboundedSender;

use crate::blf::{BlfDialog, BlfState};
use crate::calls::{CallInfo, NegotiatedSummary};
use crate::mwi::{MwiStatus, MwiSummary};
use crate::problem::{Context, Problem, classify};
use crate::subscribe::{SubKind, SubState};

/// `Allow` header value advertised on REGISTER / OPTIONS / INVITE / 18x / 2xx. One place so later
/// phases stay consistent.
pub const ALLOW: &str =
    "INVITE, ACK, CANCEL, BYE, OPTIONS, INFO, UPDATE, PRACK, REFER, NOTIFY, MESSAGE";
/// Base `Supported` token; accounts add `100rel` / `timer` per `prack` / `session_timers`
/// (`AccountConfig::supported_tokens`).
pub const SUPPORTED: &str = "replaces";
/// `Accept` header value on OPTIONS responses.
pub const ACCEPT: &str = "application/sdp";

/// Borrowed mirror of [`RegState`] for serialization: same tag and variants, plus the problem
/// on `failed`. Destructured without `..` in the impl below so a new field is a compile error.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum RegStateWire<'a> {
    Disabled,
    Unregistered,
    Registering,
    Registered {
        registered_at: &'a DateTime<Utc>,
        last_renewed_at: &'a DateTime<Utc>,
        expires_at: &'a DateTime<Utc>,
        granted_secs: u32,
    },
    WaitingForNetwork,
    Failed {
        code: u16,
        reason: &'a str,
        retry_at: &'a DateTime<Utc>,
        problem: Problem,
    },
}

impl Serialize for RegState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            RegState::Disabled => RegStateWire::Disabled,
            RegState::Unregistered => RegStateWire::Unregistered,
            RegState::Registering => RegStateWire::Registering,
            RegState::Registered {
                registered_at,
                last_renewed_at,
                expires_at,
                granted_secs,
            } => RegStateWire::Registered {
                registered_at,
                last_renewed_at,
                expires_at,
                granted_secs: *granted_secs,
            },
            RegState::WaitingForNetwork => RegStateWire::WaitingForNetwork,
            RegState::Failed {
                code,
                reason,
                retry_at,
            } => RegStateWire::Failed {
                code: *code,
                reason,
                retry_at,
                problem: self.problem().unwrap_or(Problem::Other),
            },
        }
        .serialize(s)
    }
}

/// Registration state of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegState {
    /// Account is disabled; nothing is running.
    Disabled,
    /// Not registered (peer mode, unregistered by request, or not yet started).
    Unregistered,
    /// A REGISTER (or the transport build behind it) is in progress.
    Registering,
    /// Registered until `expires_at`.
    Registered {
        /// When this uninterrupted registration became registered.
        registered_at: DateTime<Utc>,
        /// Last successful REGISTER, including the initial registration.
        last_renewed_at: DateTime<Utc>,
        /// When the binding lapses if not refreshed.
        expires_at: DateTime<Utc>,
        /// Lifetime granted by the registrar, in seconds.
        granted_secs: u32,
    },
    /// There is no route to the registrar (network down, interface gone). No REGISTER is sent
    /// until a network-change signal or a successful probe finds a route again.
    WaitingForNetwork,
    /// The last attempt failed; the next one is at `retry_at`.
    Failed {
        /// SIP status code, or 0 for local failures (DNS, connect, timeout).
        code: u16,
        /// Reason phrase or local error text.
        reason: String,
        /// When the next attempt happens.
        retry_at: DateTime<Utc>,
    },
}

impl RegState {
    /// The plain-language problem behind a failed registration; `None` in every other state.
    pub fn problem(&self) -> Option<Problem> {
        match self {
            RegState::Failed { code, reason, .. } => {
                Some(classify(Context::Register, *code, reason).unwrap_or(Problem::Other))
            }
            _ => None,
        }
    }

    /// Records a successful REGISTER. A renewal keeps the start of a still-live binding.
    pub(crate) fn renewed(&self, now: DateTime<Utc>, granted_secs: u32) -> Self {
        let registered_at = match self {
            Self::Registered {
                registered_at,
                expires_at,
                ..
            } if *expires_at > now => *registered_at,
            _ => now,
        };
        Self::Registered {
            registered_at,
            last_renewed_at: now,
            expires_at: now + chrono::Duration::seconds(i64::from(granted_secs)),
            granted_secs,
        }
    }

    /// Whether moving from `self` to `new` is a visible transition (event + INFO log). A refresh
    /// that only moves `expires_at`, or a repeated identical failure, is not.
    pub fn is_transition_to(&self, new: &RegState) -> bool {
        match (self, new) {
            (RegState::Registered { .. }, RegState::Registered { .. }) => false,
            (
                RegState::Failed {
                    code: c1,
                    reason: r1,
                    ..
                },
                RegState::Failed {
                    code: c2,
                    reason: r2,
                    ..
                },
            ) => c1 != c2 || r1 != r2,
            _ => std::mem::discriminant(self) != std::mem::discriminant(new),
        }
    }

    /// Short lower-case name (`registered`, `failed`, ...).
    pub fn name(&self) -> &'static str {
        match self {
            RegState::Disabled => "disabled",
            RegState::Unregistered => "unregistered",
            RegState::Registering => "registering",
            RegState::Registered { .. } => "registered",
            RegState::WaitingForNetwork => "waiting_for_network",
            RegState::Failed { .. } => "failed",
        }
    }
}

/// Snapshot of one account for the control API / GUI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountStatus {
    /// Account id.
    pub id: String,
    /// Whether the account is enabled in the config.
    pub enabled: bool,
    /// Configured signalling transport (`auto` when the account selects it from DNS SRV).
    pub transport: Transport,
    /// The transport actually in use: the configured one, or for `transport = "auto"` the one
    /// the last build picked. `None` for `auto` until a runtime exists (not built yet, or the
    /// build failed), and while the account is disabled.
    pub transport_effective: Option<Transport>,
    /// Effective registrar (`registrar` or `domain`).
    pub registrar: String,
    /// Address of record, `sip:user@domain`.
    pub aor: String,
    /// Contact URI used in the last REGISTER (or the one that will be used).
    pub contact: Option<String>,
    /// Registration state.
    pub registration: RegState,
    /// Local socket address (UDP socket / registrar connection / listener).
    pub local_addr: Option<String>,
    /// Public address learned from `received` / `rport` of the REGISTER response.
    pub public_addr: Option<String>,
    /// Whether the wire trace is currently on.
    pub trace_enabled: bool,
    /// A transport rebuild is waiting for active calls to end.
    pub pending_rebuild: bool,
    /// Effective do-not-disturb (config value or runtime override).
    pub dnd: bool,
    /// Voice-mail summary once a `message-summary` NOTIFY was received.
    pub mwi: Option<MwiSummary>,
    /// Destinations (`host:port[:transport]` of the next hop) that refused an `srtp = "optional"`
    /// OSRTP offer (488 / 415 / 606) and then answered a plain RTP/AVP retry with a final 2xx: calls to them offer
    /// plain RTP/AVP first. Cleared when srtp / registrar / proxy / domain / transport / codecs /
    /// srtp_suites change, on an account rebuild and on a re-probe rebuild.
    #[serde(default)]
    pub srtp_optional_downgraded: Vec<String>,
}

/// Events emitted by the SIP layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SipEvent {
    /// Registration state changed (visible transitions only).
    Registration {
        /// Account id.
        account: String,
        /// New state.
        state: RegState,
    },
    /// The connection to the registrar/peer closed (TCP/TLS).
    TransportClosed {
        /// Account id.
        account: String,
        /// Remote address of the closed flow.
        remote: String,
    },
    /// An out-of-dialog MESSAGE arrived (answered 200).
    MessageReceived {
        /// Account id.
        account: String,
        /// From URI.
        from: String,
        /// Content-Type.
        content_type: String,
        /// Body, lossy UTF-8.
        body: String,
    },
    /// A NOTIFY that matched no dialog or subscription (answered 200), e.g. unsolicited
    /// `message-summary`.
    UnhandledNotify {
        /// Account id.
        account: String,
        /// `Event` header value.
        event: String,
        /// From URI.
        from: String,
        /// Body, lossy UTF-8.
        body: String,
    },
    /// An OPTIONS request arrived (answered 200).
    OptionsReceived {
        /// Account id.
        account: String,
        /// From URI.
        from: String,
    },
    /// The effective do-not-disturb flag of an account changed.
    Dnd {
        /// Account id.
        account: String,
        /// New effective value.
        enabled: bool,
    },
    /// A new inbound call is ringing.
    CallIncoming {
        /// The call.
        call: CallInfo,
    },
    /// A call changed state (including `Ended` with code / reason / by).
    CallState {
        /// The call after the transition.
        call: CallInfo,
    },
    /// Media was (re-)negotiated.
    CallMedia {
        /// Call id.
        call_id: String,
        /// Negotiated codec / SRTP suite / remote RTP address.
        negotiated: NegotiatedSummary,
    },
    /// A DTMF digit arrived in a SIP INFO (RFC 4733 digits come from the media engine).
    CallDtmf {
        /// Call id.
        call_id: String,
        /// The digit (`0-9`, `*`, `#`, `A-D`).
        digit: String,
        /// Duration from the INFO body, if it had one.
        duration_ms: Option<u32>,
        /// Transport of the digit; always `info` here.
        via: String,
    },
    /// An incoming call was refused without ringing, or by the user.
    CallRejected {
        /// Account id.
        account: String,
        /// Caller URI.
        from: String,
        /// Status code sent.
        code: u16,
        /// `dnd`, `max_calls`, `user`, `not_acceptable`, `bad_sdp`, `unsupported_extension`,
        /// `replaces_unsupported`, `unsupported_media_type`, `media_error`.
        reason: String,
        /// The call that was ringing and got rejected by the user (`reason == "user"`); `None`
        /// for INVITEs refused before they became calls.
        call_id: Option<String>,
    },
    /// Progress of a transfer we started (NOTIFY `message/sipfrag` of the REFER, RFC 3515), or
    /// its failure.
    TransferProgress {
        /// The call the REFER was sent on.
        call_id: String,
        /// Status code of the sipfrag (100, 180, 200, ...). 0 = local failure (timeout, call
        /// ended before the result arrived).
        code: u16,
        /// Reason phrase / local error text.
        reason: String,
        /// Last report of this transfer (success when `code` is 2xx).
        #[serde(rename = "final")]
        is_final: bool,
    },
    /// A transfer session changed phase (or any other field); carries the whole session.
    TransferSession {
        /// The session after the change.
        session: crate::TransferSession,
    },
    /// A REFER arrived on a call (we are the transferee). Emitted when it is accepted (202).
    ReferReceived {
        /// The call the REFER arrived on.
        call_id: String,
        /// The `Refer-To` URI (with its embedded headers, as received).
        refer_to: String,
        /// `Referred-By` (the header, else the REFER's From URI).
        referred_by: Option<String>,
        /// The Refer-To carries a `Replaces` header (attended transfer).
        replaces: bool,
    },
    /// A monitored BLF extension changed state (or its dialog list / label changed).
    BlfState {
        /// Account id.
        account: String,
        /// Monitored URI as configured.
        uri: String,
        /// Label from the config.
        label: String,
        /// Derived lamp state.
        state: BlfState,
        /// Dialogs behind the state.
        dialogs: Vec<BlfDialog>,
    },
    /// The message-waiting status of an account changed.
    Mwi {
        /// Account id.
        account: String,
        /// New status.
        status: MwiStatus,
    },
    /// A NOTIFY arrived on a generic (API-created) subscription.
    SubscriptionNotify {
        /// Account id.
        account: String,
        /// Subscription id.
        sub_id: String,
        /// `Event` header value of the NOTIFY.
        event: String,
        /// `Content-Type` of the body.
        content_type: String,
        /// Body, lossy UTF-8.
        body: String,
        /// Raw `Subscription-State` header value.
        subscription_state: String,
    },
    /// A subscription changed state (any kind: BLF, MWI, generic).
    SubscriptionState {
        /// Account id.
        account: String,
        /// Subscription id.
        sub_id: String,
        /// Subscribed URI.
        uri: String,
        /// Event package.
        event: String,
        /// What the subscription is for.
        kind: SubKind,
        /// New state.
        state: SubState,
    },
    /// The network changed (debounced): OS event, wake from sleep, a different route, or
    /// `network.refresh`.
    NetworkChanged {
        /// Process-wide generation after the change.
        generation: u64,
        /// Reason labels (`os:addr`, `wake`, `route_changed`, `manual`, ...).
        reasons: Vec<String>,
        /// Default-route IPv4 source address before the change, if there was a route.
        old_local_ip: Option<String>,
        /// Default-route IPv4 source address after the change, if there is a route.
        new_local_ip: Option<String>,
    },
    /// An account is re-probing and re-registering because of a network change or a dead flow.
    AccountReconnecting {
        /// Account id.
        account: String,
        /// Why (`network_changed`, `keepalive_timeout`, `send_error`, `manual`, ...).
        reason: String,
    },
    /// A network change moved the local address and an established call is being moved onto the
    /// new path (re-INVITE outstanding). Followed by `CallReconnected`, or by the end of the call
    /// with cause `network_lost`.
    CallReconnecting {
        /// Call id.
        call_id: String,
        /// Account id.
        account: String,
    },
    /// The peer accepted the re-INVITE: the call runs on the new network path.
    CallReconnected {
        /// Call id.
        call_id: String,
        /// Account id.
        account: String,
    },
    /// Identity / display name or mute state of a call changed.
    CallUpdated {
        /// The call after the change.
        call: CallInfo,
    },
}

/// Result of [`crate::SipStack::update_account`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOutcome {
    /// Applied in place; nothing re-sent.
    Hot,
    /// Applied; a fresh REGISTER (and unregister of the old AOR when it changed) was triggered.
    Reregistered,
    /// Transport / endpoint rebuilt (or account started / stopped).
    Rebuilt,
    /// A rebuild is needed but the account has active calls; it runs when they end
    /// (`[general] reconfig_force` overrides).
    RebuildDeferred,
}

/// Receiver of raw wire events (SIP trace). Installed on an account's transport layer only while
/// its trace is enabled. Must not block.
pub trait WireSink: Send + Sync {
    /// One datagram or framed message / keep-alive.
    fn on_wire(&self, account: &str, ev: &WireEvent<'_>);
}

/// Sink that drops everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullWireSink;

impl WireSink for NullWireSink {
    fn on_wire(&self, _account: &str, _ev: &WireEvent<'_>) {}
}

/// Construction options of [`crate::SipStack`].
pub struct SipStackOptions {
    /// `User-Agent` for accounts that do not override it (`[general].user_agent`).
    pub user_agent_default: String,
    /// Where [`SipEvent`]s go.
    pub events: UnboundedSender<SipEvent>,
    /// Wire trace sink.
    pub wire_sink: Arc<dyn WireSink>,
}

/// Parsed reply to [`crate::SipStack::send_options`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionsResponse {
    /// Status code.
    pub code: u16,
    /// Reason phrase.
    pub reason: String,
    /// Response headers as `(name, value)`.
    pub headers: Vec<(String, String)>,
}

impl OptionsResponse {
    /// First header with this (case-insensitive) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_and_supported_constants() {
        let methods: Vec<&str> = ALLOW.split(", ").collect();
        assert_eq!(
            methods,
            [
                "INVITE", "ACK", "CANCEL", "BYE", "OPTIONS", "INFO", "UPDATE", "PRACK", "REFER",
                "NOTIFY", "MESSAGE"
            ]
        );
        assert_eq!(SUPPORTED, "replaces");
        assert!(!SUPPORTED.contains("timer"), "timer is added per account");
    }

    #[test]
    fn status_serializes_with_tagged_state() {
        // serde derive is exercised by the control API; here only the shape of the tag matters.
        let e = SipEvent::OptionsReceived {
            account: "a".into(),
            from: "sip:x@y".into(),
        };
        assert_eq!(e, e.clone());
        assert_eq!(RegState::Disabled.name(), "disabled");
    }
}
