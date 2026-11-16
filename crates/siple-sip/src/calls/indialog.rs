//! Dialog state handling and in-dialog requests for calls.

use std::sync::Arc;

use rsipstack::dialog::dialog::{DialogState, TerminatedReason, TransactionHandle};
use rsipstack::sip::prelude::*;
use rsipstack::sip::{Header, Headers, Method, Response, StatusCode};
use rsipstack::transaction::Transaction;
use siple_media::dtmf::parse_info_dtmf;

use super::util::{self, SdpBody};
use super::{
    Call, CallDirection, CallEnd, CallInner, CallManager, CallState, EndCause, EndedBy,
    cause_for_status,
};
use crate::router::{AccountCtx, default_dialog_state};
use crate::types::{ALLOW, SipEvent};

/// Where the answer to an in-dialog request goes.
pub(crate) enum Responder<'a> {
    /// Through the dialog's handle (`DialogState::Updated` / `Info` / ...).
    Handle(TransactionHandle),
    /// Straight on the server transaction (requests we handle without the dialog).
    Tx(&'a mut Transaction, Option<Header>),
}

impl Responder<'_> {
    pub(crate) async fn respond(
        &mut self,
        status: StatusCode,
        headers: Vec<Header>,
        body: Option<Vec<u8>>,
    ) {
        match self {
            Responder::Handle(h) => {
                let _ = h.respond(status, Some(headers), body).await;
            }
            Responder::Tx(tx, contact) => {
                let mut headers = headers;
                if let Some(c) = contact
                    && status.kind() == rsipstack::sip::StatusCodeKind::Successful
                    && !headers
                        .iter()
                        .any(|h| h.name().eq_ignore_ascii_case("contact"))
                {
                    headers.push(c.clone());
                }
                let _ = tx.reply_with(status, headers, body).await;
            }
        }
    }
}

impl CallManager {
    /// Dialog state change of any dialog of the account.
    pub(crate) async fn on_dialog_state(self: &Arc<Self>, ctx: &AccountCtx, state: DialogState) {
        match self.call_for_state(&state) {
            Some(call) => {
                let terminated = match &state {
                    DialogState::Terminated(id, _) => Some(id.clone()),
                    _ => None,
                };
                self.on_dialog_state_for(&call, state).await;
                if let Some(id) = terminated {
                    ctx.dialogs.remove_dialog(&id);
                }
            }
            None => {
                if let DialogState::Terminated(id, _) = &state {
                    ctx.dialogs.remove_dialog(id);
                } else if !matches!(
                    state,
                    DialogState::Calling(_) | DialogState::Trying(_) | DialogState::Early(..)
                ) && !matches!(
                    state,
                    DialogState::Confirmed(..) | DialogState::WaitAck(..)
                ) {
                    default_dialog_state(ctx, state).await;
                }
            }
        }
    }

    pub(crate) async fn on_dialog_state_for(
        self: &Arc<Self>,
        call: &Arc<Call>,
        state: DialogState,
    ) {
        match state {
            DialogState::Calling(_) | DialogState::Trying(_) | DialogState::WaitAck(..) => {}
            DialogState::Early(_, resp) => {
                if call.direction == CallDirection::Outbound {
                    call.on_early(&resp);
                }
            }
            DialogState::Confirmed(..) => call.on_confirmed(),
            DialogState::Updated(_, req, h) => {
                call.on_remote_offer(&req, Responder::Handle(h)).await;
            }
            DialogState::Info(_, req, h) => {
                call.on_info(&req, Responder::Handle(h)).await;
            }
            DialogState::Notify(_, req, h) => {
                call.on_notify(&req, Responder::Handle(h)).await;
            }
            DialogState::Message(_, req, h) => {
                let _ = h.reply(StatusCode::OK).await;
                self.emit(SipEvent::MessageReceived {
                    account: call.account.clone(),
                    from: call.snapshot().remote_uri,
                    content_type: util::header_first(&req.headers, "content-type")
                        .unwrap_or_default(),
                    body: String::from_utf8_lossy(&req.body).into_owned(),
                });
            }
            DialogState::Options(_, _, h) => {
                let _ = h
                    .respond(
                        StatusCode::OK,
                        Some(vec![Header::Allow(ALLOW.into())]),
                        None,
                    )
                    .await;
            }
            DialogState::Refer(_, req, h) => {
                call.on_refer(&req, Responder::Handle(h)).await;
            }
            DialogState::Publish(_, _, h) => {
                let _ = h.reply(StatusCode::NotImplemented).await;
            }
            DialogState::Terminated(id, reason) => {
                if let Some(rt) = call.rt() {
                    rt.dialogs.remove_dialog(&id);
                }
                call.on_terminated(&reason);
            }
        }
    }

    /// Hook run for every in-dialog request before the dialog layer sees it. Returns the
    /// transaction when the dialog layer should handle it.
    pub(crate) async fn on_in_dialog(
        self: &Arc<Self>,
        ctx: &AccountCtx,
        mut tx: Transaction,
    ) -> Option<Transaction> {
        let req = tx.original.clone();
        let call = (|| {
            let sip_call_id = req.call_id_header().ok()?.value().to_string();
            let to_tag = req
                .to_header()
                .ok()?
                .tag()
                .ok()
                .flatten()?
                .value()
                .to_string();
            self.find_dialog(&sip_call_id, &to_tag)
        })();
        // Not one of ours: leave it to the dialog layer / router.
        let Some(call) = call else { return Some(tx) };

        if req.method == Method::Bye {
            let reason = util::header_first(&req.headers, "reason").map(|r| util::parse_reason(&r));
            call.lock().bye_reason = reason;
        }
        if call.is_ended() {
            // The transferee often BYEs right before its final `refer` NOTIFY: while a REFER of
            // ours is pending that NOTIFY still carries the transfer result.
            if req.method == Method::Notify
                && call.has_transfer()
                && util::header_first(&req.headers, "event").is_some_and(|e| {
                    e.split(';')
                        .next()
                        .is_some_and(|p| p.trim().eq_ignore_ascii_case("refer"))
                })
            {
                call.on_notify(&req, Responder::Tx(&mut tx, None)).await;
                return None;
            }
            // Late retransmissions: acknowledge a BYE, refuse the rest.
            let status = if req.method == Method::Bye {
                StatusCode::OK
            } else {
                StatusCode::CallTransactionDoesNotExist
            };
            if req.method != Method::Ack {
                let _ = tx.reply(status).await;
            }
            return None;
        }

        // PRACK of a reliable 180 (RFC 3262): matched against the outstanding RSeq here.
        if req.method == Method::PRack && call.direction == CallDirection::Inbound {
            call.on_prack(&mut tx).await;
            return None;
        }

        // The dialog layer handles requests of a confirmed dialog (and the INVITE-transaction
        // traffic of a server dialog: ACK, CANCEL, retransmitted INVITE, BYE before the ACK).
        // A re-INVITE (CSeq above the initial INVITE's) that beats the ACK would be taken for
        // the initial INVITE by the dialog layer and never answered; `early_reinvite` holds
        // whether our 2xx for the initial INVITE has been sent.
        let mut early_reinvite = None;
        let pass = match ctx.dialogs.match_dialog(&tx) {
            Some(d) => {
                let (confirmed, waiting_ack) = match &d {
                    rsipstack::dialog::dialog::Dialog::Invite(i) => {
                        (i.state().is_confirmed(), i.state().waiting_ack())
                    }
                    _ => (false, false),
                };
                let server = call.direction == CallDirection::Inbound;
                if server
                    && !confirmed
                    && req.method == Method::Invite
                    && util::req_cseq(&req) > call.lock().invite.as_ref().and_then(util::req_cseq)
                {
                    early_reinvite = Some(waiting_ack);
                }
                early_reinvite.is_none()
                    && (confirmed
                        || req.method == Method::Ack
                        || (server
                            && matches!(
                                req.method,
                                Method::Invite | Method::PRack | Method::Bye | Method::Cancel
                            )))
            }
            None => false,
        };
        match early_reinvite {
            // RFC 3261 §14.2: before the final response to the initial INVITE -> 500 with a
            // random Retry-After of 0-10 s.
            Some(false) => {
                let secs = rand::random_range(0..=10u32).to_string();
                let _ = tx
                    .reply_with(
                        StatusCode::ServerInternalError,
                        vec![Header::Other("Retry-After".into(), secs)],
                        None,
                    )
                    .await;
                return None;
            }
            // A 200 to an offer-less re-INVITE carries our offer and its ACK the answer, but
            // the ACK of a re-INVITE answered here is not routed back to us until the dialog
            // is confirmed: 491 (§14.2) instead.
            Some(true) if matches!(util::sdp_body(&req.headers, &req.body), SdpBody::None) => {
                let _ = tx
                    .reply_with(
                        StatusCode::RequestPending,
                        vec![Header::Other("Retry-After".into(), "2".into())],
                        None,
                    )
                    .await;
                return None;
            }
            _ => {}
        }
        if pass {
            return Some(tx);
        }
        call.handle_direct(&mut tx).await;
        None
    }
}

/// End record of a remote BYE: only a `SIP` protocol cause becomes the code (200 otherwise);
/// a Q.850 cause is kept apart.
fn bye_end(reason: Option<&util::Reason>) -> CallEnd {
    let r = reason.cloned().unwrap_or_default();
    CallEnd {
        code: r.sip_cause().unwrap_or(200),
        reason: r.text.clone().unwrap_or_else(|| "BYE".into()),
        by: EndedBy::Remote,
        missed: false,
        q850_cause: r.q850_cause(),
        // Our own BYEs say why (`hangup_with`); a peer using the same texts is believed.
        cause: match r.text.as_deref() {
            Some(t) if t.eq_ignore_ascii_case("Call transferred") => EndCause::Transferred,
            Some(t) if t.eq_ignore_ascii_case("Call replaced") => EndCause::Replaced,
            _ => EndCause::RemoteHangup,
        },
    }
}

impl Call {
    /// Our dialog Contact as a header (for 2xx sent straight on a transaction).
    pub(crate) fn contact_header(&self) -> Option<Header> {
        let c = self.lock().local_contact.clone()?;
        Some(Header::Contact(format!("<{c}>").into()))
    }

    /// Pulls the remote identity out of `headers` (PAI first, then Remote-Party-ID). Emits
    /// `CallUpdated` when it changed and `announce` is set.
    pub(crate) fn update_identity(
        &self,
        g: &mut CallInner,
        headers: &Headers,
        announce: bool,
    ) -> bool {
        let mut changed = false;
        let pai = util::header_first(headers, "p-asserted-identity");
        let rpid = util::header_first(headers, "remote-party-id");
        let pick = pai.clone().or_else(|| rpid.clone());
        if let Some(p) = &pai
            && g.info.pai.as_deref() != Some(p.as_str())
        {
            g.info.pai = Some(p.clone());
            changed = true;
        }
        if let Some(r) = &rpid
            && g.info.rpid.as_deref() != Some(r.as_str())
        {
            g.info.rpid = Some(r.clone());
            changed = true;
        }
        if let Some(v) = pick {
            let (display, uri) = util::parse_name_addr(&v);
            if !uri.is_empty()
                && (g.info.remote_uri != uri
                    || (display.is_some() && g.info.remote_display != display))
            {
                g.info.remote_uri = uri;
                if display.is_some() {
                    g.info.remote_display = display;
                }
                changed = true;
            }
        }
        if changed && announce {
            self.emit(SipEvent::CallUpdated {
                call: g.info.clone(),
            });
        }
        changed
    }

    /// A provisional response (18x) of an outbound INVITE.
    pub(crate) fn on_early(&self, resp: &Response) {
        let body = util::sdp_body(&resp.headers, &resp.body);
        let mut g = self.lock();
        if !matches!(g.info.state, CallState::Dialing | CallState::Early) {
            return;
        }
        let mut changed = self.update_identity(&mut g, &resp.headers, false);
        if let SdpBody::Sdp(text, sd) = body
            && g.early_sdp.as_deref() != Some(text.as_str())
            && let Some(session) = g.session.as_mut()
        {
            match session.apply_answer(&sd) {
                Ok(neg) => {
                    g.early_sdp = Some(text);
                    if !g.info.early_media {
                        g.info.early_media = true;
                        changed = true;
                    }
                    self.apply_negotiated(&mut g, neg);
                }
                Err(e) => tracing::debug!(call = %self.id, error = %e, "early-media SDP ignored"),
            }
        }
        if g.info.state == CallState::Dialing {
            self.set_state(&mut g, CallState::Early);
        } else if changed {
            self.emit(SipEvent::CallUpdated {
                call: g.info.clone(),
            });
        }
    }

    /// The dialog became confirmed (inbound: ACK received).
    pub(crate) fn on_confirmed(self: &Arc<Self>) {
        let (dialog, awaiting, want_cseq) = {
            let g = self.lock();
            (g.dialog.clone(), g.awaiting_ack_sdp, g.awaiting_ack_cseq)
        };
        if !awaiting {
            return;
        }
        let Some(dialog) = dialog else { return };
        let Some(ack) = dialog.last_remote_ack() else {
            return;
        };
        // `Confirmed` is also notified after other mid-dialog requests, and the stored ACK may
        // be an older one: only the ACK of the INVITE we wait for counts.
        if want_cseq.is_some() && util::req_cseq(&ack) != want_cseq {
            return;
        }
        let body = util::sdp_body(&ack.headers, &ack.body);
        let mut g = self.lock();
        g.awaiting_ack_sdp = false;
        g.awaiting_ack_cseq = None;
        let remote_holds = g.info.hold.remote;
        let result = match (body, g.session.as_mut()) {
            (SdpBody::Sdp(_, sd), Some(s)) => {
                super::answer_sdp(s, remote_holds, &sd).map_err(|e| e.to_string())
            }
            (_, Some(_)) => Err("ACK without the SDP answer".to_string()),
            _ => Err("no media session".to_string()),
        };
        match result {
            Ok(neg) => self.apply_negotiated(&mut g, neg),
            Err(e) => {
                tracing::warn!(call = %self.id, error = %e, "answer in ACK unusable; hanging up");
                g.local_hangup = true;
                drop(g);
                let _ = self.hangup_now();
            }
        }
    }

    /// The dialog layer says the dialog is over.
    pub(crate) fn on_terminated(&self, reason: &TerminatedReason) {
        if self.direction == CallDirection::Outbound && self.outbound_terminated(reason) {
            return;
        }
        let (local, bye, state) = {
            let g = self.lock();
            (g.local_hangup, g.bye_reason.clone(), g.info.state)
        };
        match reason {
            TerminatedReason::UacBye | TerminatedReason::UasBye => {
                if local {
                    self.local_end(200, "BYE", EndCause::Hangup);
                } else {
                    self.end(bye_end(bye.as_ref()));
                }
            }
            TerminatedReason::UacCancel => {
                if self.direction == CallDirection::Inbound {
                    let reason = self
                        .lock()
                        .dialog
                        .as_ref()
                        .and_then(|d| d.last_remote_cancel())
                        .and_then(|r| util::header_first(&r.headers, "reason"))
                        .map(|v| util::parse_reason(&v));
                    let elsewhere = reason.as_ref().and_then(util::Reason::sip_cause) == Some(200);
                    let text = reason
                        .as_ref()
                        .and_then(|r| r.text.clone())
                        .unwrap_or_else(|| {
                            if elsewhere {
                                "Call completed elsewhere".into()
                            } else {
                                "Request Terminated".into()
                            }
                        });
                    self.end(CallEnd {
                        code: 487,
                        reason: text,
                        by: EndedBy::Remote,
                        missed: state == CallState::Ringing && !elsewhere,
                        q850_cause: reason.as_ref().and_then(util::Reason::q850_cause),
                        cause: if state == CallState::Ringing && !elsewhere {
                            EndCause::Missed
                        } else {
                            EndCause::Cancelled
                        },
                    });
                } else {
                    self.local_end(487, "Request Terminated", EndCause::Cancelled);
                }
            }
            TerminatedReason::UasDecline => {
                self.local_end(603, "Decline", EndCause::Rejected);
            }
            TerminatedReason::Timeout => {
                self.remote_end(408, "Request Timeout", EndCause::Failed);
            }
            TerminatedReason::UasOther(c)
            | TerminatedReason::UacOther(c)
            | TerminatedReason::ProxyError(c) => {
                self.remote_end(c.code(), c.text().to_string(), cause_for_status(c.code()));
            }
            TerminatedReason::UacBusy | TerminatedReason::UasBusy => {
                self.remote_end(486, "Busy Here", EndCause::Rejected);
            }
            TerminatedReason::ProxyAuthRequired => {
                self.remote_end(407, "Proxy Authentication Required", EndCause::Failed);
            }
        }
    }

    /// Requests of a dialog the dialog layer cannot (yet) handle: early dialogs (both roles) and
    /// the window before a confirmed client dialog is registered.
    pub(crate) async fn handle_direct(self: &Arc<Self>, tx: &mut Transaction) {
        let req = tx.original.clone();
        match req.method {
            Method::Bye => {
                let _ = tx.reply(StatusCode::OK).await;
                let (state, dialog, bye) = {
                    let g = self.lock();
                    (g.info.state, g.dialog.clone(), g.bye_reason.clone())
                };
                let ended = self.end(bye_end(bye.as_ref()));
                if ended
                    && matches!(state, CallState::Dialing | CallState::Early)
                    && let Some(d) = dialog
                {
                    // Our INVITE is still pending: stop it.
                    tokio::spawn(async move {
                        let _ = d.cancel().await;
                    });
                }
            }
            Method::Update | Method::Invite => {
                let c = self.contact_header();
                self.on_remote_offer(&req, Responder::Tx(tx, c)).await;
            }
            Method::Info => {
                let c = self.contact_header();
                self.on_info(&req, Responder::Tx(tx, c)).await;
            }
            Method::Options => {
                let _ = tx
                    .reply_with(StatusCode::OK, vec![Header::Allow(ALLOW.into())], None)
                    .await;
            }
            Method::Notify => {
                let c = self.contact_header();
                self.on_notify(&req, Responder::Tx(tx, c)).await;
            }
            Method::Message => {
                let _ = tx.reply(StatusCode::OK).await;
                self.emit(SipEvent::MessageReceived {
                    account: self.account.clone(),
                    from: self.snapshot().remote_uri,
                    content_type: util::header_first(&req.headers, "content-type")
                        .unwrap_or_default(),
                    body: String::from_utf8_lossy(&req.body).into_owned(),
                });
            }
            Method::Refer => {
                let c = self.contact_header();
                self.on_refer(&req, Responder::Tx(tx, c)).await;
            }
            Method::Ack | Method::Cancel => {}
            _ => {
                let _ = tx
                    .reply_with(
                        StatusCode::MethodNotAllowed,
                        vec![Header::Allow(ALLOW.into())],
                        None,
                    )
                    .await;
            }
        }
    }

    /// SIP INFO: DTMF becomes a `CallDtmf` event; everything else is acknowledged.
    pub(crate) async fn on_info(&self, req: &rsipstack::sip::Request, mut r: Responder<'_>) {
        let ct = util::header_first(&req.headers, "content-type").unwrap_or_default();
        let body = String::from_utf8_lossy(&req.body).into_owned();
        if let Some(d) = parse_info_dtmf(&ct, &body) {
            self.emit(SipEvent::CallDtmf {
                call_id: self.id.clone(),
                digit: d.digit.to_string(),
                duration_ms: d.duration_ms,
                via: "info".into(),
            });
        }
        r.respond(StatusCode::OK, vec![], None).await;
    }

    /// Re-INVITE or UPDATE from the remote party (also in early dialogs): renegotiate through
    /// `apply_offer`, apply to the media, answer. Offer-less requests are answered with the
    /// current SDP (INVITE) or plainly (UPDATE).
    pub(crate) async fn on_remote_offer(
        self: &Arc<Self>,
        req: &rsipstack::sip::Request,
        mut r: Responder<'_>,
    ) {
        let is_invite = req.method == Method::Invite;
        let (status, mut headers, body, established) = {
            let mut g = self.lock();
            let established = matches!(
                g.info.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            );
            let (st, h, b) = if g.info.state == CallState::Ended {
                (StatusCode::CallTransactionDoesNotExist, vec![], None)
            } else if g.reinvite_out || (is_invite && g.awaiting_ack_sdp) {
                // Glare: we have a request of our own outstanding (RFC 3261 §14.1), or our
                // 2xx offer is still waiting for its answer in the ACK (§14.2).
                (
                    StatusCode::RequestPending,
                    vec![Header::Other("Retry-After".into(), "2".into())],
                    None,
                )
            } else if let Some(min_se) = established.then(|| self.refresh_too_small(req)).flatten()
            {
                // RFC 4028 §6: a Session-Expires below our Min-SE.
                (
                    StatusCode::from(422),
                    vec![Header::Other("Min-SE".into(), min_se.to_string())],
                    None,
                )
            } else {
                self.update_identity(&mut g, &req.headers, true);
                self.negotiate_remote(&mut g, req, is_invite)
            };
            (st, h, b, established)
        };
        // A 2xx to a refresh restates the interval and restarts the session timer.
        if status == StatusCode::OK && established {
            headers.extend(self.timer_on_remote_refresh(req));
        }
        r.respond(status, headers, body.map(String::into_bytes))
            .await;
    }

    fn negotiate_remote(
        &self,
        g: &mut CallInner,
        req: &rsipstack::sip::Request,
        is_invite: bool,
    ) -> (StatusCode, Vec<Header>, Option<String>) {
        let established = matches!(
            g.info.state,
            CallState::Active | CallState::Held | CallState::RemoteHeld
        );
        match util::sdp_body(&req.headers, &req.body) {
            SdpBody::None => {
                if is_invite {
                    let remote_holds = g.info.hold.remote;
                    let Some(session) = g.session.as_mut() else {
                        return (StatusCode::ServerInternalError, vec![], None);
                    };
                    let sdp = super::offer_sdp(session, remote_holds);
                    g.awaiting_ack_sdp = true;
                    g.awaiting_ack_cseq = util::req_cseq(req);
                    (StatusCode::OK, util::sdp_headers(&self.cfg), Some(sdp))
                } else {
                    (StatusCode::OK, vec![Header::Allow(ALLOW.into())], None)
                }
            }
            SdpBody::WrongType => (StatusCode::UnsupportedMediaType, vec![], None),
            SdpBody::Malformed(_) => (StatusCode::BadRequest, vec![], None),
            SdpBody::Sdp(_, sd) => {
                let ringing_in = g.info.state == CallState::Ringing;
                let early_out = matches!(g.info.state, CallState::Dialing | CallState::Early);
                if early_out && g.neg.is_none() {
                    // Our own offer is still unanswered: cannot take another one.
                    return (StatusCode::RequestPending, vec![], None);
                }
                let Some(session) = g.session.as_mut() else {
                    return (StatusCode::ServerInternalError, vec![], None);
                };
                match session.apply_offer(&sd) {
                    Ok((answer, neg)) => {
                        if ringing_in {
                            // Not answered yet: the new answer goes into the 200.
                            g.offer = Some(*sd);
                            g.answer = Some((answer.clone(), neg));
                        } else if established || early_out {
                            self.apply_negotiated(g, neg);
                        }
                        (StatusCode::OK, util::sdp_headers(&self.cfg), Some(answer))
                    }
                    Err(e) => {
                        tracing::info!(call = %self.id, error = %e, "re-negotiation refused");
                        (StatusCode::from(e.status_code()), vec![], None)
                    }
                }
            }
        }
    }
}
