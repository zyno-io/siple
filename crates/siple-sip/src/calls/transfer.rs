//! Call transfers: outbound blind / attended / semi-attended REFER with our own
//! sipfrag NOTIFY parsing (rsipstack's `ReferStatus::parse` is unusable), inbound REFER (we are
//! the transferee) and the INVITE-with-`Replaces` matching (RFC 3891) used by attended transfers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rsipstack::sip::{Header, Method, Request, StatusCode};
use siple_config::{AccountConfig, AttendedReferTarget};
use tokio::task::AbortHandle;

use super::indialog::Responder;
use super::util;
use super::{
    Call, CallDirection, CallInner, CallManager, CallState, DialOptions, EndCause, PendingOp,
};
use crate::error::{Result, SipError};
use crate::types::SipEvent;

/// No NOTIFY within this long after the REFER was accepted (or after the last progress NOTIFY
/// without an `expires`): the transfer is reported as failed (the call stays as it is, held).
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(32);
/// Test hook: overrides [`TRANSFER_TIMEOUT`] (ms, 0 = default). Process-wide.
pub(crate) static TRANSFER_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

fn transfer_timeout() -> Duration {
    match TRANSFER_TIMEOUT_MS.load(Ordering::Relaxed) {
        0 => TRANSFER_TIMEOUT,
        ms => Duration::from_millis(ms),
    }
}
/// Longest wait a `Subscription-State: ...;expires=` may ask for between two NOTIFYs.
const TRANSFER_MAX_WAIT: Duration = Duration::from_secs(300);
/// How long a call that ended with its REFER pending still waits for the final NOTIFY.
const ENDED_NOTIFY_GRACE: Duration = Duration::from_secs(2);
/// The call that stays up after a successful attended transfer is hung up this long afterwards
/// if the transfer target did not end it.
const OTHER_CALL_GRACE: Duration = Duration::from_secs(5);
/// Bound for the hold re-INVITE sent before a REFER.
const HOLD_TIMEOUT: Duration = Duration::from_secs(6);
/// How long the call placed for an inbound REFER may take before it is given up.
const REFER_CALL_TIMEOUT: Duration = Duration::from_secs(120);

static NEXT_TRANSFER: AtomicU64 = AtomicU64::new(1);

/// A REFER of ours that waits for its NOTIFYs.
pub(crate) struct TransferOut {
    id: u64,
    /// Attended: the call that stays up for the transfer target to end it.
    other: Option<Arc<Call>>,
    timeout: Option<AbortHandle>,
    /// CSeq of the REFER (the `id` of its `Event: refer` subscription) once it was accepted.
    refer_cseq: Option<u32>,
    phase: ReferPhase,
    /// The transfer session this REFER belongs to.
    session: String,
}

/// Where a [`TransferOut`] stands. The record outlives `Done` / `TimedOut` until the refer
/// subscription is terminated (a late final NOTIFY can still turn a timeout into a success),
/// but no longer blocks a new transfer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReferPhase {
    Pending,
    /// A final result was reported.
    Done,
    /// The timeout was reported as a failure.
    TimedOut,
}

// --- pure helpers -------------------------------------------------------------------------

/// Percent-encodes everything but RFC 3986 unreserved characters (so `@ ; = :` are encoded, as
/// in the RFC 3891 examples).
pub(crate) fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Reverses [`pct_encode`] (invalid escapes are kept verbatim).
pub(crate) fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A parsed `Refer-To` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReferTo {
    /// The target URI without its embedded headers.
    pub uri: String,
    /// Embedded headers, percent-decoded.
    pub headers: Vec<(String, String)>,
}

/// Parses `<sip:x@y?Replaces=...&Header=...>` (or a bare `sip:` URI).
pub(crate) fn parse_refer_to(v: &str) -> Option<ReferTo> {
    let t = v.trim();
    let inner = match (t.find('<'), t.find('>')) {
        (Some(lt), Some(gt)) if lt < gt => &t[lt + 1..gt],
        _ => t.split_whitespace().next()?,
    };
    let (base, query) = match inner.split_once('?') {
        Some((b, q)) => (b.trim(), Some(q)),
        None => (inner.trim(), None),
    };
    let lower = base.to_ascii_lowercase();
    if !(lower.starts_with("sip:") || lower.starts_with("sips:")) {
        return None;
    }
    rsipstack::sip::Uri::try_from(base).ok()?;
    let headers = query
        .map(|q| {
            q.split('&')
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.split_once('='))
                .map(|(k, v)| (pct_decode(k), pct_decode(v)))
                .collect()
        })
        .unwrap_or_default();
    Some(ReferTo {
        uri: base.to_string(),
        headers,
    })
}

/// A parsed `Replaces` header (RFC 3891).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplacesSpec {
    pub call_id: String,
    pub to_tag: String,
    pub from_tag: String,
    pub early_only: bool,
}

pub(crate) fn parse_replaces(v: &str) -> Option<ReplacesSpec> {
    let mut parts = v.split(';');
    let call_id = parts.next()?.trim().to_string();
    if call_id.is_empty() {
        return None;
    }
    let (mut to_tag, mut from_tag, mut early_only) = (None, None, false);
    for p in parts {
        let p = p.trim();
        let (k, val) = p
            .split_once('=')
            .map_or((p, ""), |(k, v)| (k.trim(), v.trim()));
        match k.to_ascii_lowercase().as_str() {
            "to-tag" => to_tag = Some(val.to_string()),
            "from-tag" => from_tag = Some(val.to_string()),
            "early-only" => early_only = true,
            _ => {}
        }
    }
    Some(ReplacesSpec {
        call_id,
        to_tag: to_tag.filter(|t| !t.is_empty())?,
        from_tag: from_tag.filter(|t| !t.is_empty())?,
        early_only,
    })
}

/// `<uri?Replaces=callid%40host%3Bto-tag%3D..%3Bfrom-tag%3D..>`.
pub(crate) fn replaces_refer_to(uri: &str, call_id: &str, to_tag: &str, from_tag: &str) -> String {
    let value = format!("{call_id};to-tag={to_tag};from-tag={from_tag}");
    let sep = if uri.contains('?') { '&' } else { '?' };
    format!("<{uri}{sep}Replaces={}>", pct_encode(&value))
}

/// First line of a `message/sipfrag` body: `SIP/2.0 180 Ringing` gives `(180, "Ringing")`.
pub(crate) fn parse_sipfrag(body: &[u8]) -> Option<(u16, String)> {
    let text = String::from_utf8_lossy(body);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut it = line.splitn(3, char::is_whitespace);
    let proto = it.next()?;
    if !proto.to_ascii_uppercase().starts_with("SIP/") {
        return None;
    }
    let code: u16 = it.next()?.trim().parse().ok()?;
    if !(100..700).contains(&code) {
        return None;
    }
    Some((code, it.next().unwrap_or("").trim().to_string()))
}

/// Embedded `Refer-To` headers that must not become INVITE headers.
fn forbidden_embedded(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "via"
            | "v"
            | "from"
            | "f"
            | "to"
            | "t"
            | "call-id"
            | "i"
            | "cseq"
            | "contact"
            | "m"
            | "max-forwards"
            | "route"
            | "record-route"
            | "content-length"
            | "l"
            | "content-type"
            | "c"
            | "refer-to"
            | "r"
            | "body"
            | ""
    )
}

fn sipfrag_end(code: u16) -> u16 {
    if (300..700).contains(&code) {
        code
    } else {
        500
    }
}

// --- manager level -----------------------------------------------------------------------------

impl CallManager {
    /// Looks up the call an incoming `Replaces` header points at (RFC 3891 §3). The error is the
    /// status to answer with and the `call.rejected` reason.
    pub(crate) fn match_replaces(
        &self,
        account: &str,
        value: &str,
    ) -> std::result::Result<Arc<Call>, (u16, &'static str)> {
        let spec = parse_replaces(value).ok_or((400, "bad_replaces"))?;
        let none = (481, "replaces_no_match");
        let call = self.find_dialog(&spec.call_id, &spec.to_tag).ok_or(none)?;
        if call.account != account {
            return Err(none);
        }
        let g = call.lock();
        if g.info.state == CallState::Ended {
            // The dialog existed but is over (RFC 3891 §3: 603 Declined).
            return Err((603, "replaces_terminated"));
        }
        let remote_tag_ok = g
            .dialog
            .as_ref()
            .is_some_and(|d| d.id().remote_tag == spec.from_tag);
        if !remote_tag_ok {
            return Err(none);
        }
        match g.info.state {
            CallState::Active | CallState::Held | CallState::RemoteHeld => {
                if spec.early_only {
                    Err((486, "replaces_early_only"))
                } else {
                    Ok(call.clone())
                }
            }
            // An early dialog may only be replaced by the UA that started it (we are its UAC).
            CallState::Early if call.direction == CallDirection::Outbound => Ok(call.clone()),
            _ => Err(none),
        }
    }
}

// --- call level --------------------------------------------------------------------------------

impl Call {
    pub(crate) fn live_cfg(&self) -> Arc<AccountConfig> {
        self.env.env().map_or_else(|| self.cfg.clone(), |e| e.cfg)
    }

    /// URI this (outbound) call was placed to: the dialog's To URI; PAI / RPID may have
    /// overwritten `remote_uri`.
    pub(super) fn dialed_uri(&self) -> String {
        let (dialog, fallback) = {
            let g = self.lock();
            (g.dialog.clone(), g.info.remote_uri.clone())
        };
        dialog
            .map(|d| d.snapshot().to.uri.to_string())
            .filter(|u| !u.is_empty())
            .unwrap_or(fallback)
    }

    pub(super) fn ensure_established(&self) -> Result<()> {
        let g = self.lock();
        if matches!(
            g.info.state,
            CallState::Active | CallState::Held | CallState::RemoteHeld
        ) {
            Ok(())
        } else {
            Err(self.state_err("call is not established"))
        }
    }

    /// Puts the call on hold before a REFER, like a phone does. Failures are only logged: the
    /// transfer goes ahead. Returns whether this put (or may still put) the call on hold, i.e.
    /// whether a failed transfer should take it off hold again; a call the user held already
    /// is not ours to resume.
    pub(super) async fn hold_for_transfer(self: &Arc<Self>) -> bool {
        let need = {
            let g = self.lock();
            matches!(g.info.state, CallState::Active | CallState::RemoteHeld) && !g.info.hold.local
        };
        if !need {
            return false;
        }
        match tokio::time::timeout(HOLD_TIMEOUT, self.clone().hold(true)).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!(call = %self.id, error = %e, "cannot hold before the transfer");
                false
            }
            Err(_) => {
                // The re-INVITE finishes in the background and may still hold the call.
                tracing::warn!(call = %self.id, "hold before the transfer timed out");
                true
            }
        }
    }

    /// `<remote AOR | Contact ?Replaces=...>` naming this call's dialog (the dialog the transfer
    /// target replaces): Call-ID, to-tag = the remote party's tag, from-tag = ours.
    pub(super) fn attended_refer_to(&self, target: AttendedReferTarget) -> Result<String> {
        let (dialog, invite) = {
            let g = self.lock();
            (g.dialog.clone(), g.invite.clone())
        };
        let dialog = dialog.ok_or_else(|| self.state_err("no dialog"))?;
        let id = dialog.id();
        if id.remote_tag.is_empty() {
            return Err(self.state_err("dialog has no remote tag yet"));
        }
        let snap = dialog.snapshot();
        let aor = match (self.direction, invite) {
            (CallDirection::Inbound, Some(inv)) => {
                util::parse_name_addr(&util::header_first(&inv.headers, "from").unwrap_or_default())
                    .1
            }
            _ => snap.to.uri.to_string(),
        };
        let uri = match target {
            AttendedReferTarget::Aor => aor,
            AttendedReferTarget::Contact => snap
                .remote_contact
                .as_ref()
                .map(|c| util::parse_name_addr(c.value()).1)
                .filter(|u| !u.is_empty())
                .unwrap_or(aor),
        };
        Ok(replaces_refer_to(
            &uri,
            &id.call_id,
            &id.remote_tag,
            &id.local_tag,
        ))
    }

    /// Sends the REFER on this call. `Ok` once it was accepted (2xx); progress and the result
    /// arrive as `TransferProgress` events. Failures answered to the REFER itself are reported
    /// as a final `TransferProgress` as well as an error.
    ///
    /// The exchange runs in its own task, so a caller that is dropped (UI timeout) can neither
    /// leave a half-registered transfer behind nor lose the timer / result handling.
    pub(super) async fn start_refer(
        self: &Arc<Self>,
        refer_to: String,
        other: Option<Arc<Call>>,
        session: String,
    ) -> Result<()> {
        let this = self.clone();
        tokio::spawn(async move { this.start_refer_inner(refer_to, other, session).await })
            .await
            .unwrap_or_else(|e| Err(SipError::Protocol(format!("transfer task failed: {e}"))))
    }

    async fn start_refer_inner(
        self: &Arc<Self>,
        refer_to: String,
        other: Option<Arc<Call>>,
        session: String,
    ) -> Result<()> {
        let dialog = match self.confirmed_dialog().await {
            Ok(d) => d,
            Err(e) => {
                self.mgr
                    .refer_failed(&session, "refer", 0, &e.to_string())
                    .await;
                return Err(e);
            }
        };
        let tid = NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed);
        {
            let mut g = self.lock();
            if g.transfer
                .as_ref()
                .is_some_and(|t| t.phase == ReferPhase::Pending)
            {
                drop(g);
                // Not retried: another REFER of this call is outstanding (no auto-resume).
                self.mgr.session_refer_failed(
                    &session,
                    "refer",
                    0,
                    "a transfer is already in progress",
                );
                return Err(self.state_err("a transfer is already in progress"));
            }
            // A settled earlier transfer only lingers for its late NOTIFYs; this one replaces it.
            if let Some(old) = g.transfer.take()
                && let Some(h) = old.timeout
            {
                h.abort();
            }
            g.transfer = Some(TransferOut {
                id: tid,
                other,
                timeout: None,
                refer_cseq: None,
                phase: ReferPhase::Pending,
                session: session.clone(),
            });
            self.sync_refer_pending(&mut g);
        }
        self.mgr.session_refer_started(&session, &self.id);
        let headers = vec![
            Header::Other("Refer-To".into(), refer_to.clone()),
            Header::Other("Referred-By".into(), format!("<{}>", self.cfg.aor())),
        ];
        tracing::info!(call = %self.id, %refer_to, "sending REFER");
        let fail = |this: &Arc<Self>, code: u16, reason: &str| {
            let mut g = this.lock();
            if g.transfer.as_ref().is_some_and(|t| t.id == tid) {
                g.transfer = None;
                this.sync_refer_pending(&mut g);
                this.emit(SipEvent::TransferProgress {
                    call_id: this.id.clone(),
                    code,
                    reason: reason.to_string(),
                    is_final: true,
                });
            }
        };
        let sid = session.clone();
        match dialog.request(Method::Refer, Some(headers), None).await {
            Ok(Some(resp)) if (200..300).contains(&resp.status_code.code()) => {
                let cseq = util::resp_cseq(&resp);
                let mut g = self.lock();
                // A NOTIFY that beat the 2xx may have settled the transfer already.
                if g.transfer
                    .as_ref()
                    .is_some_and(|t| t.id == tid && t.phase == ReferPhase::Pending)
                {
                    if let Some(t) = g.transfer.as_mut() {
                        t.refer_cseq = cseq;
                    }
                    self.arm_transfer_timer(&mut g, tid, transfer_timeout());
                    drop(g);
                    self.mgr.session_refer_accepted(&sid);
                }
                Ok(())
            }
            Ok(Some(resp)) => {
                let code = resp.status_code.code();
                let reason = resp.status_code.text().to_string();
                fail(self, code, &reason);
                if code == 481 || code == 408 {
                    self.dialog_lost(code, "REFER");
                }
                self.mgr.refer_failed(&sid, "refer", code, &reason).await;
                Err(SipError::Status { code, reason })
            }
            Ok(None) => {
                fail(self, 0, "dialog is not confirmed");
                self.mgr
                    .refer_failed(&sid, "refer", 0, "dialog is not confirmed")
                    .await;
                Err(self.state_err("REFER was not sent: dialog is not confirmed"))
            }
            Err(e) => {
                fail(self, 0, &e.to_string());
                self.request_failed(&e, "REFER");
                self.mgr
                    .refer_failed(&sid, "refer", 0, &e.to_string())
                    .await;
                Err(e.into())
            }
        }
    }

    /// Keeps `CallInfo::pending` at `Refer` exactly while a REFER of ours awaits its outcome
    /// (it takes precedence over a hold / resume marker, which never overwrites or clears it). Emits `CallUpdated` on a change.
    fn sync_refer_pending(&self, g: &mut CallInner) {
        let refer_out = g
            .transfer
            .as_ref()
            .is_some_and(|t| t.phase == ReferPhase::Pending);
        let want = match (refer_out, g.info.pending) {
            (true, p) if p != Some(PendingOp::Refer) => Some(Some(PendingOp::Refer)),
            (false, Some(PendingOp::Refer)) => Some(None),
            _ => None,
        };
        if let Some(p) = want {
            g.info.pending = p;
            if g.info.state != CallState::Ended {
                self.emit(SipEvent::CallUpdated {
                    call: g.info.clone(),
                });
            }
        }
    }

    /// (Re)starts the "no NOTIFY" timer of transfer `tid`.
    fn arm_transfer_timer(self: &Arc<Self>, g: &mut CallInner, tid: u64, wait: Duration) {
        let this = self.clone();
        let h = tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            this.transfer_timed_out(tid);
        });
        if let Some(t) = g.transfer.as_mut().filter(|t| t.id == tid) {
            if let Some(old) = t.timeout.replace(h.abort_handle()) {
                old.abort();
            }
        } else {
            h.abort();
        }
    }

    /// The timer ran out: report the failure, but keep the record for a late final NOTIFY.
    fn transfer_timed_out(self: &Arc<Self>, tid: u64) {
        let mut timed_out = None;
        let mut g = self.lock();
        if let Some(t) = g
            .transfer
            .as_mut()
            .filter(|t| t.id == tid && t.phase == ReferPhase::Pending)
        {
            t.phase = ReferPhase::TimedOut;
            t.timeout = None;
            t.other = None;
            timed_out = Some(t.session.clone());
            self.emit(SipEvent::TransferProgress {
                call_id: self.id.clone(),
                code: 408,
                reason: "no final NOTIFY for the REFER (timeout)".into(),
                is_final: true,
            });
        }
        self.sync_refer_pending(&mut g);
        drop(g);
        if let Some(sid) = timed_out {
            self.mgr.session_refer_timed_out(&sid);
        }
    }

    /// A pending transfer ends without a NOTIFY (call gone): reports a final failure.
    fn finish_transfer(self: &Arc<Self>, tid: u64, code: u16, reason: &str) {
        let mut g = self.lock();
        if let Some(t) = g.transfer.as_ref().filter(|t| t.id == tid) {
            let report = t.phase == ReferPhase::Pending;
            if let Some(h) = &t.timeout {
                h.abort();
            }
            self.mgr.session_refer_ended(&t.session);
            g.transfer = None;
            self.sync_refer_pending(&mut g);
            if report {
                self.emit(SipEvent::TransferProgress {
                    call_id: self.id.clone(),
                    code,
                    reason: reason.to_string(),
                    is_final: true,
                });
            }
        }
    }

    /// Hook of [`Call::end`]: a call that ends while its REFER is pending reports a failure
    /// shortly afterwards, unless the final NOTIFY (racing the BYE) still arrives.
    pub(crate) fn transfer_call_ended(&self) {
        let tid = self.lock().transfer.as_ref().map(|t| t.id);
        if let Some(tid) = tid
            && let Ok(this) = self.mgr.get(&self.id)
        {
            tokio::spawn(async move {
                tokio::time::sleep(ENDED_NOTIFY_GRACE).await;
                this.finish_transfer(tid, 0, "call ended before the transfer result arrived");
            });
        }
    }

    /// Whether an `Event: refer` NOTIFY can still matter (a transfer record exists).
    pub(crate) fn has_transfer(&self) -> bool {
        self.lock().transfer.is_some()
    }

    /// NOTIFY on the call's dialog. Always answered 200; `Event: refer` feeds the pending
    /// transfer.
    pub(crate) async fn on_notify(self: &Arc<Self>, req: &Request, mut r: Responder<'_>) {
        r.respond(StatusCode::OK, vec![], None).await;
        let event = util::header_first(&req.headers, "event").unwrap_or_default();
        let event = event
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if event != "refer" {
            return;
        }
        let sub_state = util::header_first(&req.headers, "subscription-state")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let terminated = sub_state.trim().starts_with("terminated");
        let expires = sub_state.split(';').skip(1).find_map(|p| {
            p.trim()
                .strip_prefix("expires=")?
                .trim()
                .parse::<u64>()
                .ok()
        });
        // `Event: refer;id=<CSeq of the REFER>`: NOTIFYs of an older REFER are not ours.
        let notify_id = util::header_first(&req.headers, "event").and_then(|e| {
            e.split(';').skip(1).find_map(|p| {
                p.trim()
                    .strip_prefix("id=")
                    .and_then(|v| v.trim().parse::<u32>().ok())
            })
        });
        let (code, reason) = parse_sipfrag(&req.body).unwrap_or_else(|| {
            (
                if terminated { 500 } else { 100 },
                "unparsable sipfrag".to_string(),
            )
        });
        let is_final = code >= 200 || terminated;
        let success = is_final && (200..300).contains(&code);
        // What to do, decided under the lock.
        let (emit, other, hangup, session, late_failure) = {
            let mut g = self.lock();
            let Some(t) = g.transfer.as_mut() else {
                tracing::debug!(call = %self.id, code, "refer NOTIFY without a pending transfer");
                return;
            };
            if let (Some(n), Some(c)) = (notify_id, t.refer_cseq)
                && n < c
            {
                tracing::debug!(call = %self.id, n, c, "refer NOTIFY of an older REFER ignored");
                return;
            }
            let tid = t.id;
            let session = t.session.clone();
            let mut emit = false;
            let mut other = None;
            let mut hangup = false;
            let mut late_failure = false;
            match t.phase {
                ReferPhase::Pending => {
                    emit = true;
                    if is_final {
                        t.phase = ReferPhase::Done;
                        if let Some(h) = t.timeout.take() {
                            h.abort();
                        }
                        other = t.other.take();
                        hangup = success;
                    }
                }
                // The timeout was reported as a failure; a final success after all still counts.
                ReferPhase::TimedOut => {
                    if success {
                        t.phase = ReferPhase::Done;
                        emit = true;
                        hangup = true;
                    } else if is_final {
                        // The session learns the real outcome (it moves UNCONFIRMED to FAILED);
                        // the 408 already reported was the final progress event.
                        t.phase = ReferPhase::Done;
                        late_failure = true;
                    }
                }
                ReferPhase::Done => {}
            }
            if terminated {
                if let Some(h) = t.timeout.take() {
                    h.abort();
                }
                g.transfer = None;
            } else if emit && !is_final {
                // Progress: wait for the next NOTIFY, as long as the notifier says (else 32 s).
                let wait = expires.map_or_else(transfer_timeout, |e| {
                    Duration::from_secs(e).clamp(Duration::from_secs(1), TRANSFER_MAX_WAIT)
                });
                self.arm_transfer_timer(&mut g, tid, wait);
            }
            if hangup {
                // The REFER succeeded: however the call ends from now on (our BYE or the
                // transferee's), it ended because it was transferred.
                g.cause_hint = Some(EndCause::Transferred);
            }
            self.sync_refer_pending(&mut g);
            (emit, other, hangup, session, late_failure)
        };
        // The session first learns of the notify, then of the outcome.
        let session_failure = (is_final && !success && (emit || late_failure)).then(|| {
            // A final failure of the REFER's own subscription.
            (code, reason.clone())
        });
        self.mgr.session_refer_notify(&session, code, &reason);
        if success && (emit || hangup) {
            self.mgr.session_refer_succeeded(&session);
        }
        if let Some((code, reason)) = session_failure {
            // The session settles now (a following hold / unhold must see it); only the
            // auto-resume waits for a re-INVITE, so it never runs in the NOTIFY handler.
            if self
                .mgr
                .session_refer_failed(&session, "notify", code, &reason)
            {
                let mgr = self.mgr.clone();
                let sid = session.clone();
                tokio::spawn(async move { mgr.auto_resume(&sid).await });
            }
        }
        if emit {
            tracing::info!(call = %self.id, code, %reason, is_final, "transfer progress");
            self.emit(SipEvent::TransferProgress {
                call_id: self.id.clone(),
                code,
                reason,
                is_final,
            });
        }
        if hangup {
            // The transferee's BYE may have beaten the final NOTIFY: the call is already over
            // and was recorded as a plain hang-up. The outcome still decides the cause.
            {
                let mut g = self.lock();
                if g.info.state == CallState::Ended
                    && let Some(end) = g.info.end.as_mut()
                    && matches!(end.cause, EndCause::Hangup | EndCause::RemoteHangup)
                {
                    end.cause = EndCause::Transferred;
                    self.emit(SipEvent::CallUpdated {
                        call: g.info.clone(),
                    });
                }
            }
            let _ = self.hangup_with(Some("transferred"));
            if let Some(o) = other {
                tokio::spawn(async move {
                    tokio::time::sleep(OTHER_CALL_GRACE).await;
                    if !o.is_ended() {
                        let _ = o.hangup_with(Some("transferred"));
                    }
                });
            }
        }
    }

    // --- inbound REFER (we are the transferee) ------------------------------------------

    pub(crate) async fn on_refer(self: &Arc<Self>, req: &Request, mut r: Responder<'_>) {
        let cfg = self.live_cfg();
        let state = self.lock().info.state;
        if !matches!(
            state,
            CallState::Active | CallState::Held | CallState::RemoteHeld
        ) {
            r.respond(StatusCode::Forbidden, vec![], None).await;
            return;
        }
        if !cfg.accept_refer {
            tracing::info!(call = %self.id, "REFER refused (accept_refer = false)");
            r.respond(StatusCode::from(603u16), vec![], None).await;
            return;
        }
        let raw = util::header_first(&req.headers, "refer-to");
        let Some((raw, parsed)) = raw.and_then(|v| parse_refer_to(&v).map(|p| (v, p))) else {
            r.respond(StatusCode::BadRequest, vec![], None).await;
            return;
        };
        let from_uri =
            util::parse_name_addr(&util::header_first(&req.headers, "from").unwrap_or_default()).1;
        let referred_by = util::header_first(&req.headers, "referred-by");
        let mut headers: Vec<(String, String)> = parsed
            .headers
            .iter()
            .filter(|(k, _)| !forbidden_embedded(k))
            .cloned()
            .collect();
        let has_replaces = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("replaces"));
        if !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("referred-by"))
        {
            headers.push((
                "Referred-By".into(),
                referred_by
                    .clone()
                    .unwrap_or_else(|| format!("<{from_uri}>")),
            ));
        }
        let mut resp_headers = vec![];
        if let Some(c) = self.contact_header() {
            resp_headers.push(c);
        }
        r.respond(StatusCode::Accepted, resp_headers, None).await;
        self.emit(SipEvent::ReferReceived {
            call_id: self.id.clone(),
            refer_to: raw,
            referred_by: referred_by.or(Some(from_uri)),
            replaces: has_replaces,
        });
        let event_id = util::req_cseq(req);
        self.spawn_run_refer(parsed.uri, headers, event_id);
    }

    /// Starts [`Call::run_refer`]. A plain function so that the `Send` proof of the spawned
    /// future (it reaches `on_refer` again through `CallManager::register`) is not part of
    /// `on_refer`'s own future type.
    fn spawn_run_refer(
        self: &Arc<Self>,
        target: String,
        headers: Vec<(String, String)>,
        id: Option<u32>,
    ) {
        let this = self.clone();
        tokio::spawn(async move { this.run_refer(target, headers, id).await });
    }

    /// Sends one `Event: refer` NOTIFY with a sipfrag body on this call's dialog.
    async fn notify_refer(&self, event_id: Option<u32>, code: u16, reason: &str, last: bool) {
        let Ok(dialog) = self.confirmed_dialog().await else {
            return;
        };
        let event = event_id.map_or_else(|| "refer".to_string(), |i| format!("refer;id={i}"));
        let state = if last {
            "terminated;reason=noresource"
        } else {
            "active;expires=60"
        };
        let headers = vec![
            Header::Event(event.into()),
            Header::SubscriptionState(state.into()),
            Header::ContentType("message/sipfrag;version=2.0".into()),
        ];
        let body = format!("SIP/2.0 {code} {reason}\r\n").into_bytes();
        match dialog
            .request(Method::Notify, Some(headers), Some(body))
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => tracing::debug!(call = %self.id, "refer NOTIFY not sent"),
            Err(e) => tracing::debug!(call = %self.id, error = %e, "refer NOTIFY failed"),
        }
    }

    /// The transferee side of a REFER: hold the original call, call the Refer-To target and
    /// report to the transferor; on success end the original call.
    async fn run_refer(
        self: Arc<Self>,
        target: String,
        headers: Vec<(String, String)>,
        event_id: Option<u32>,
    ) {
        self.notify_refer(event_id, 100, "Trying", false).await;
        let we_held = self.hold_for_transfer().await;
        let (source, sink) = {
            let g = self.lock();
            (g.source.clone(), g.sink.clone())
        };
        let opts = DialOptions {
            headers,
            source: Some(source),
            sink: Some(sink),
            ..Default::default()
        };
        let new_id = match self
            .mgr
            .dial_ext(&self.env, &target, opts, Some(self.id.clone()))
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(call = %self.id, error = %e, "cannot place the REFER call");
                let (code, reason) = match &e {
                    SipError::Status { code, reason } => (*code, reason.clone()),
                    other => (503, other.to_string()),
                };
                self.notify_refer(event_id, sipfrag_end(code), &reason, true)
                    .await;
                self.resume_after_failed_refer(we_held).await;
                return;
            }
        };
        let Ok(new) = self.mgr.get(&new_id) else {
            return;
        };
        let deadline = tokio::time::Instant::now() + REFER_CALL_TIMEOUT;
        let mut sent_180 = false;
        loop {
            let (st, end) = {
                let g = new.lock();
                (g.info.state, g.info.end.clone())
            };
            match st {
                CallState::Active | CallState::Held | CallState::RemoteHeld => {
                    self.notify_refer(event_id, 200, "OK", true).await;
                    let _ = self.hangup_with(Some("transferred"));
                    return;
                }
                CallState::Ended => {
                    let (code, reason) =
                        end.map_or((500, "call failed".into()), |e| (e.code, e.reason));
                    self.notify_refer(event_id, sipfrag_end(code), &reason, true)
                        .await;
                    self.resume_after_failed_refer(we_held).await;
                    return;
                }
                CallState::Early if !sent_180 => {
                    sent_180 = true;
                    self.notify_refer(event_id, 180, "Ringing", false).await;
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = new.hangup_now();
                self.notify_refer(event_id, 408, "Request Timeout", true)
                    .await;
                self.resume_after_failed_refer(we_held).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The REFER call failed: take the original call off hold again, if the REFER put it there.
    async fn resume_after_failed_refer(self: &Arc<Self>, we_held: bool) {
        let held = we_held && {
            let g = self.lock();
            g.info.state != CallState::Ended && g.info.hold.local
        };
        if held {
            self.mgr.clone().resume_if_free(self).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_roundtrip() {
        let s = "abc123@host.example;to-tag=x y;from-tag=ü";
        let e = pct_encode(s);
        assert!(
            !e.contains('@') && !e.contains(';') && !e.contains('='),
            "{e}"
        );
        assert_eq!(pct_decode(&e), s);
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(pct_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn replaces_refer_to_roundtrip() {
        let v = replaces_refer_to("sip:c@10.0.0.1:5070", "12345@192.168.1.2", "tt", "ff");
        assert_eq!(
            v,
            "<sip:c@10.0.0.1:5070?Replaces=12345%40192.168.1.2%3Bto-tag%3Dtt%3Bfrom-tag%3Dff>"
        );
        let p = parse_refer_to(&v).unwrap();
        assert_eq!(p.uri, "sip:c@10.0.0.1:5070");
        assert_eq!(p.headers.len(), 1);
        assert_eq!(p.headers[0].0, "Replaces");
        let r = parse_replaces(&p.headers[0].1).unwrap();
        assert_eq!(
            r,
            ReplacesSpec {
                call_id: "12345@192.168.1.2".into(),
                to_tag: "tt".into(),
                from_tag: "ff".into(),
                early_only: false
            }
        );
    }

    #[test]
    fn refer_to_forms() {
        assert_eq!(parse_refer_to("sip:a@b").unwrap().uri, "sip:a@b");
        let p =
            parse_refer_to("\"X\" <sip:a@b;transport=tcp?Subject=hi%20there&X-A=1>;x=y").unwrap();
        assert_eq!(p.uri, "sip:a@b;transport=tcp");
        assert_eq!(
            p.headers,
            vec![
                ("Subject".to_string(), "hi there".to_string()),
                ("X-A".to_string(), "1".to_string())
            ]
        );
        assert!(parse_refer_to("tel:+1555").is_none());
        assert!(parse_refer_to("").is_none());
    }

    #[test]
    fn replaces_forms() {
        let r = parse_replaces("abc;from-tag=f;to-tag=t;early-only").unwrap();
        assert!(r.early_only && r.to_tag == "t" && r.from_tag == "f");
        assert!(parse_replaces("abc;to-tag=t").is_none());
        assert!(parse_replaces(";to-tag=t;from-tag=f").is_none());
    }

    #[test]
    fn sipfrag_forms() {
        assert_eq!(
            parse_sipfrag(b"SIP/2.0 100 Trying\r\n"),
            Some((100, "Trying".into()))
        );
        assert_eq!(
            parse_sipfrag(b"SIP/2.0 486 Busy Here\r\nContact: x\r\n"),
            Some((486, "Busy Here".into()))
        );
        assert_eq!(parse_sipfrag(b"SIP/2.0 200"), Some((200, String::new())));
        assert_eq!(parse_sipfrag(b""), None);
        assert_eq!(parse_sipfrag(b"hello"), None);
        assert_eq!(parse_sipfrag(b"SIP/2.0 999 x"), None);
    }
}
