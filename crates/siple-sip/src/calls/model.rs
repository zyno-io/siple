//! Public call model: ids, state, snapshots and API option structs.

use chrono::{DateTime, Utc};
use serde::Serialize;
use siple_config::{AudioSink, AudioSource, Codec, SrtpPolicy};
use siple_media::sdp_neg::Negotiated;

/// Call id, `c1`, `c2`, ... unique per process (across accounts).
pub type CallId = String;

/// Who started the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    /// The remote party called us.
    Inbound,
    /// We called the remote party.
    Outbound,
}

/// Lifecycle state of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    /// Outbound INVITE sent, nothing heard yet.
    Dialing,
    /// Inbound call alerting (not answered yet).
    Ringing,
    /// Outbound call received a provisional response (18x).
    Early,
    /// Answered, media flowing.
    Active,
    /// We put the call on hold.
    Held,
    /// The remote party put us on hold.
    RemoteHeld,
    /// Over; see [`CallInfo::end`].
    Ended,
}

/// Which side ended the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedBy {
    /// This softphone.
    Local,
    /// The remote party (or the network).
    Remote,
}

/// Why a call ended, as a stable category for UIs and automation (`CallEnd::code` / `reason`
/// keep the SIP detail).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCause {
    /// We hung up an established call.
    #[default]
    Hangup,
    /// The remote party hung up an established call.
    RemoteHangup,
    /// The call was handed over by a successful transfer (a final 2xx `refer` NOTIFY), whoever
    /// sent the BYE first.
    Transferred,
    /// An INVITE with `Replaces` took the call over.
    Replaced,
    /// Declined: we rejected an incoming call, or the remote refused ours with a final
    /// 4xx (not 408 / 487) or 6xx response (486 Busy, 603 Decline, ...).
    Rejected,
    /// An unanswered call was cancelled (our CANCEL, a 487, or a remote CANCEL that is not a
    /// miss, e.g. "call completed elsewhere").
    Cancelled,
    /// An incoming call that the caller abandoned while it was ringing.
    Missed,
    /// Setup or signalling failed: 408 / 5xx, no answer to a request, dialog lost, unusable
    /// answer SDP, transport error.
    Failed,
    /// The RFC 4028 session timer expired (the remote stopped refreshing).
    SessionExpired,
    /// The call was lost with the network: it could not be moved onto the new network path
    /// (the peer answered the re-INVITE with 481 / 408 / an error, or gave no final answer in
    /// 10 s), or `[network] move_calls = false` ended it.
    NetworkLost,
}

/// Operation in flight on a call (`CallInfo::pending`); the UI shows a spinner and disables
/// conflicting buttons while it is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingOp {
    /// A hold re-INVITE is outstanding.
    Hold,
    /// A resume re-INVITE is outstanding.
    Resume,
    /// A REFER is outstanding (transfer in progress).
    Refer,
    /// The line coordinator is switching to this call (`activate`).
    Activate,
}

/// What a successful [`crate::SipStack::answer`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// The call was answered (or, with a reliable 180 outstanding, the answer is queued).
    Answered,
    /// `AnswerMode::EndOthers`: the answer was sent; the named call is hung up in the
    /// background once the caller's ACK arrives (and left untouched if it never does).
    EndOthersPendingAck,
}

/// What `answer` does with the other live lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AnswerMode {
    /// Today's behaviour: the account's `auto_hold` decides whether the other calls in
    /// `hold_scope` are put on hold first.
    #[default]
    Default,
    /// Hold the other device-bound calls in `hold_scope` first, whatever `auto_hold` says.
    HoldOthers,
    /// Answer first, skip holding, and hang up the named call only once the new call is active.
    /// A failed answer leaves the named call untouched.
    EndOthers(CallId),
}

/// Why and how a call ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallEnd {
    /// SIP status code that ended the call: the final response of an unanswered call, the
    /// `Reason` cause of a BYE (200 when absent), 487 for CANCEL. 0 = local failure (transport
    /// error, no answer to a request).
    pub code: u16,
    /// Reason phrase / `Reason: text=` / local error text.
    pub reason: String,
    /// Who ended it.
    pub by: EndedBy,
    /// Inbound call that the caller abandoned before it was answered.
    pub missed: bool,
    /// Q.850 cause of a `Reason: Q.850;cause=N` header on the BYE / CANCEL (never mixed into
    /// `code`).
    pub q850_cause: Option<u16>,
    /// Category of the end; see [`EndCause`].
    pub cause: EndCause,
}

/// Hold flags of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct HoldFlags {
    /// We hold the remote party.
    pub local: bool,
    /// The remote party holds us.
    pub remote: bool,
}

/// What was negotiated for the media of a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NegotiatedSummary {
    /// Lower-case codec name (`opus`, `g722`, `pcmu`, `pcma`).
    pub codec: String,
    /// Payload type we send with.
    pub pt: u8,
    /// RTP clock rate of `pt`.
    pub clock_rate: u32,
    /// Packet time we send with.
    pub ptime_ms: u32,
    /// SDP name of the SRTP suite; `None` for plain RTP.
    pub srtp_suite: Option<String>,
    /// Where we send RTP.
    pub remote_rtp: String,
    /// telephone-event (RFC 4733) negotiated.
    pub rfc4733: bool,
    /// RTCP multiplexed on the RTP port.
    pub rtcp_mux: bool,
}

impl NegotiatedSummary {
    /// Summary of a negotiation result.
    pub fn from_negotiated(n: &Negotiated) -> Self {
        Self {
            codec: n.codec.sdp_encoding_name().to_ascii_lowercase(),
            pt: n.pt,
            clock_rate: n.clock_rate,
            ptime_ms: n.ptime_ms,
            srtp_suite: n.srtp.as_ref().map(|s| s.suite.sdp_name().to_string()),
            remote_rtp: n.remote_rtp.to_string(),
            rfc4733: n.te.is_some(),
            rtcp_mux: n.rtcp_mux,
        }
    }
}

/// Snapshot of one call (what `call.get` / events carry).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallInfo {
    /// Call id.
    pub id: CallId,
    /// Account id.
    pub account: String,
    /// Direction.
    pub direction: CallDirection,
    /// State.
    pub state: CallState,
    /// Remote party URI (outbound: the target; inbound: PAI, else RPID, else From).
    pub remote_uri: String,
    /// Remote party display name.
    pub remote_display: Option<String>,
    /// Our URI for this call (AOR, or the To URI of an inbound call).
    pub local_uri: String,
    /// SIP Call-ID of the current dialog.
    pub sip_call_id: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// When the call was answered.
    pub answered_at: Option<DateTime<Utc>>,
    /// When the call ended.
    pub ended_at: Option<DateTime<Utc>>,
    /// End details once `state == Ended`.
    pub end: Option<CallEnd>,
    /// Hold flags.
    pub hold: HoldFlags,
    /// When the hold flags (local or remote) last changed; `None` while the call was never held.
    pub hold_changed_at: Option<DateTime<Utc>>,
    /// Operation in flight on this call, if any.
    pub pending: Option<PendingOp>,
    /// Local mute.
    pub muted: bool,
    /// Negotiated media, once known.
    pub negotiated: Option<NegotiatedSummary>,
    /// Outbound: a provisional response carried an SDP answer (the remote plays early media, so
    /// no local ringback is needed).
    pub early_media: bool,
    /// Raw `Alert-Info` of the INVITE.
    pub alert_info: Option<String>,
    /// Raw `P-Asserted-Identity`.
    pub pai: Option<String>,
    /// Raw `Remote-Party-ID`.
    pub rpid: Option<String>,
    /// Outbound: the PBX refused the SRTP (`srtp = optional`) offer with 488 / 415 / 606 and the
    /// call was retried once with a plain RTP/AVP offer.
    pub srtp_fallback: bool,
    /// Call that was transferred to produce this one (we were the transferee of an inbound
    /// REFER): the id of the original call.
    pub transferred_from: Option<CallId>,
    /// Inbound INVITE with `Replaces`: the id of the call this one replaced (and ended).
    pub replaces: Option<CallId>,
    /// The (latest) transfer session this call belongs to; the session itself is in
    /// [`crate::SipStack::transfers`].
    pub transfer: Option<TransferRef>,
    /// A network change moved the local address and the call is being moved onto the new path
    /// (re-INVITE outstanding): "Reconnecting call...". Cleared on success or when the call ends.
    pub reconnecting: bool,
}

/// Which side of a transfer session a call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferRole {
    /// The call being transferred (A).
    Original,
    /// The consult call (B).
    Consult,
}

/// Back-reference from a call to its transfer session ([`CallInfo::transfer`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferRef {
    /// Session id (`t1`, `t2`, ...).
    pub session_id: String,
    /// Role of the call in it.
    pub role: TransferRole,
}

/// Options of [`crate::SipStack::dial`].
#[derive(Debug, Clone, Default)]
pub struct DialOptions {
    /// Extra headers added to the INVITE (after the account's `extra_headers`).
    pub headers: Vec<(String, String)>,
    /// Codec preference for this call; `None` = account / global list.
    pub codecs: Option<Vec<Codec>>,
    /// SRTP policy for this call; `None` = account setting.
    pub srtp: Option<SrtpPolicy>,
    /// Audio source; `None` = account default.
    pub source: Option<AudioSource>,
    /// Audio sink; `None` = account default.
    pub sink: Option<AudioSink>,
    /// Always send the INVITE through the account's outbound proxy / registrar, whatever host
    /// the target URI names (directed pickups, whose target may come from a NOTIFY).
    pub via_registrar: bool,
}

/// Options of [`crate::SipStack::answer`].
#[derive(Debug, Clone, Default)]
pub struct AnswerOptions {
    /// Audio source; `None` = account default.
    pub source: Option<AudioSource>,
    /// Audio sink; `None` = account default.
    pub sink: Option<AudioSink>,
    /// What to do with the other lines; see [`AnswerMode`].
    pub mode: AnswerMode,
}
