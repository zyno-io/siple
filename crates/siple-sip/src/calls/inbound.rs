//! Incoming INVITEs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use rsipstack::sip::{Header, StatusCode};
use rsipstack::transaction::Transaction;
use siple_config::{AudioSink, AudioSource};
use siple_media::sdp_neg::MediaSession;

use super::util::{self, SdpBody};
use super::{
    AccountCalls, AnswerOptions, Call, CallDirection, CallInfo, CallInner, CallManager, CallState,
    EndCause, HoldFlags,
};
use super::{reliable, timer};
use crate::error::{Result, SipError};
use crate::media::CallMediaRequest;
use crate::router::AccountCtx;
use crate::types::{ALLOW, SipEvent};

/// `Require` option tags the account can honor: `replaces` always, `100rel` unless
/// `prack = disabled`, `timer` with `session_timers`.
fn honoured_require(cfg: &siple_config::AccountConfig) -> Vec<&'static str> {
    let mut v = vec!["replaces"];
    if cfg.prack != siple_config::PrackMode::Disabled {
        v.push("100rel");
    }
    if cfg.session_timers {
        v.push("timer");
    }
    v
}

impl CallManager {
    /// Answers an INVITE without creating a call and reports it as rejected.
    async fn refuse(
        &self,
        env: &AccountCalls,
        tx: &mut Transaction,
        from: &str,
        code: u16,
        reason: &str,
        headers: Vec<Header>,
    ) {
        tracing::info!(account = %env.account, code, reason, %from, "rejecting incoming INVITE");
        let _ = tx.reply_with(StatusCode::from(code), headers, None).await;
        self.emit(SipEvent::CallRejected {
            account: env.account.clone(),
            from: from.to_string(),
            code,
            reason: reason.to_string(),
            call_id: None,
        });
    }

    /// A new out-of-dialog INVITE.
    pub(crate) async fn on_invite(self: &Arc<Self>, ctx: AccountCtx, mut tx: Transaction) {
        let env = ctx.calls.clone();
        let req = tx.original.clone();
        let from_raw = util::header_first(&req.headers, "from").unwrap_or_default();
        let (from_display, from_uri) = util::parse_name_addr(&from_raw);

        let Some(cenv) = env.env() else {
            let _ = tx.reply(StatusCode::TemporarilyUnavailable).await;
            return;
        };
        let (cfg, rt) = match (&cenv.rt, cenv.cfg.clone()) {
            (Some(rt), cfg) => (cfg, rt.clone()),
            _ => {
                let _ = tx.reply(StatusCode::TemporarilyUnavailable).await;
                return;
            }
        };

        // A non-TLS account whose peer talks TLS in its Contact / Record-Route will reach it
        // over TLS (in-dialog requests, BYE): give it a client config now, best effort.
        if rt.transport != rsipstack::sip::Transport::Tls {
            let wants_tls = ["contact", "record-route"]
                .iter()
                .flat_map(|h| util::header_values(&req.headers, h))
                .any(|v| {
                    let v = v.to_ascii_lowercase();
                    v.contains("sips:") || v.contains(";transport=tls")
                });
            if wants_tls && let Err(e) = rt.tls.ensure(&rt.endpoint.transport_layer) {
                tracing::warn!(account = %env.account, error = %e, "client TLS config for a TLS peer");
            }
        }

        // Unsupported extensions.
        let honoured = honoured_require(&cfg);
        let unsupported: Vec<String> = util::tokens(&req.headers, "require")
            .into_iter()
            .filter(|t| !honoured.contains(&t.as_str()))
            .collect();
        if !unsupported.is_empty() {
            let h = vec![Header::Other("Unsupported".into(), unsupported.join(", "))];
            self.refuse(&env, &mut tx, &from_uri, 420, "unsupported_extension", h)
                .await;
            return;
        }

        // Session timer (RFC 4028): an interval below our Min-SE gets 422.
        let timer_plan = match timer::plan_inbound(&cfg, &req.headers) {
            Ok(p) => p,
            Err(min_se) => {
                let h = vec![Header::Other("Min-SE".into(), min_se.to_string())];
                self.refuse(
                    &env,
                    &mut tx,
                    &from_uri,
                    422,
                    "session_interval_too_small",
                    h,
                )
                .await;
                return;
            }
        };

        // INVITE with Replaces (attended transfer target): DND and max_calls must NOT apply to
        // it, so it is matched before them (RFC 3891).
        let replace = match util::header_first(&req.headers, "replaces") {
            Some(v) => match self.match_replaces(&env.account, &v) {
                Ok(c) => Some(c),
                Err((code, reason)) => {
                    self.refuse(&env, &mut tx, &from_uri, code, reason, vec![])
                        .await;
                    return;
                }
            },
            None => None,
        };

        if replace.is_none() && env.dnd.load(std::sync::atomic::Ordering::Relaxed) {
            let code = if (400..=699).contains(&cfg.dnd_reject_code) {
                cfg.dnd_reject_code
            } else {
                486
            };
            self.refuse(&env, &mut tx, &from_uri, code, "dnd", vec![])
                .await;
            return;
        }
        if replace.is_none() && self.live_count(&env.account) >= cfg.max_calls as usize {
            self.refuse(&env, &mut tx, &from_uri, 486, "max_calls", vec![])
                .await;
            return;
        }

        // SDP offer (validated before ringing so a bad offer never rings).
        let sdp = util::sdp_body(&req.headers, &req.body);
        match &sdp {
            SdpBody::WrongType => {
                self.refuse(
                    &env,
                    &mut tx,
                    &from_uri,
                    415,
                    "unsupported_media_type",
                    vec![],
                )
                .await;
                return;
            }
            SdpBody::Malformed(e) => {
                tracing::debug!(error = %e, "malformed SDP offer");
                self.refuse(&env, &mut tx, &from_uri, 400, "bad_sdp", vec![])
                    .await;
                return;
            }
            _ => {}
        }

        let id = CallManager::next_id();
        // A replacing call keeps the audio binding of the call it replaces.
        let (source, sink) = match &replace {
            Some(old) => {
                let g = old.lock();
                (g.source.clone(), g.sink.clone())
            }
            None => (cfg.audio_source.clone(), cfg.audio_sink.clone()),
        };
        let media = match self.provider().create(CallMediaRequest {
            call_id: id.clone(),
            account_id: env.account.clone(),
            bind_ip: rt.bind_ip,
            advertise_ip: rt.advertise_ip,
            device_bound: source == siple_config::AudioSource::Device
                || sink == siple_config::AudioSink::Device,
            source: source.clone(),
            sink: sink.clone(),
        }) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "cannot create call media");
                self.refuse(&env, &mut tx, &from_uri, 500, "media_error", vec![])
                    .await;
                return;
            }
        };
        let caps = util::local_caps(&cfg, &cenv.media, None, None);
        let mut session = MediaSession::new(caps, rt.advertise_ip, media.local_rtp_port());
        let (offer, answer, offerless) = match sdp {
            SdpBody::Sdp(_, sd) => match session.apply_offer(&sd) {
                Ok((answer_sdp, neg)) => (Some(*sd), Some((answer_sdp, neg)), false),
                Err(e) => {
                    media.close();
                    let code = e.status_code();
                    tracing::info!(error = %e, "incoming offer not acceptable");
                    self.refuse(
                        &env,
                        &mut tx,
                        &from_uri,
                        code,
                        if code == 488 {
                            "not_acceptable"
                        } else {
                            "bad_sdp"
                        },
                        vec![Header::Other(
                            "Warning".into(),
                            format!("399 siple \"{e}\""),
                        )],
                    )
                    .await;
                    return;
                }
            },
            _ => (None, None, true),
        };

        // Dialog (generates our To tag) and call record.
        let dialog = match rt.dialogs.get_or_create_server_invite(
            &tx,
            rt.state_tx.clone(),
            Some(env.credential()),
            ctx.contact(),
        ) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "cannot create server dialog");
                media.close();
                let _ = tx.reply(StatusCode::ServerInternalError).await;
                return;
            }
        };
        let to_raw = util::header_first(&req.headers, "to").unwrap_or_default();
        let (_, local_uri) = util::parse_name_addr(&to_raw);
        let (remote_display, remote_uri) = (from_display, from_uri.clone());
        let dlg_id = dialog.id();
        let info = CallInfo {
            id: id.clone(),
            account: env.account.clone(),
            direction: CallDirection::Inbound,
            state: CallState::Ringing,
            remote_uri,
            remote_display,
            local_uri,
            sip_call_id: dlg_id.call_id.clone(),
            created_at: Utc::now(),
            answered_at: None,
            ended_at: None,
            end: None,
            hold: HoldFlags::default(),
            hold_changed_at: None,
            pending: None,
            muted: false,
            negotiated: None,
            early_media: false,
            alert_info: util::header_first(&req.headers, "alert-info"),
            pai: None,
            rpid: None,
            srtp_fallback: false,
            transferred_from: None,
            replaces: replace.as_ref().map(|c| c.id.clone()),
            transfer: None,
            reconnecting: false,
        };
        let call = Arc::new(Call {
            id: id.clone(),
            account: env.account.clone(),
            direction: CallDirection::Inbound,
            mgr: self.clone(),
            env: env.clone(),
            cfg: cfg.clone(),
            media_cfg: cenv.media.clone(),
            ended: tokio::sync::watch::channel(false).0,
            inner: Mutex::new(CallInner {
                info,
                local_tag: dlg_id.local_tag.clone(),
                rt: Some(rt.clone()),
                local_contact: ctx.contact(),
                dialog: Some(dialog.clone()),
                media: Some(media),
                session: Some(session),
                neg: None,
                source,
                sink,
                offer,
                answer,
                offerless,
                awaiting_ack_sdp: false,
                awaiting_ack_cseq: None,
                early_sdp: None,
                local_hangup: false,
                late_dialog: None,
                reinvite_out: false,
                move_gen: 0,
                move_pending: false,
                move_seq: 0,
                fallback_contact: false,
                pending_negotiation_error: None,
                bye_reason: None,
                invite_abort: None,
                answer_timer: None,
                invite: Some(req.clone()),
                srtp: cfg.srtp,
                srtp_retried: false,
                srtp_plain_first: false,
                srtp_probe: Default::default(),
                srtp_key: None,
                srtp_gen: 0,
                transfer: None,
                reliable: Default::default(),
                timer_plan,
                timer: None,
                cause_hint: None,
            }),
        });
        {
            let mut g = call.lock();
            call.update_identity(&mut g, &req.headers, false);
        }
        self.register(call.clone()).await;

        // The dialog sends 100 Trying and drives the INVITE transaction (ACK, CANCEL).
        let mut d = dialog.clone();
        let account = env.account.clone();
        tokio::spawn(async move {
            if let Err(e) = d.handle(&mut tx).await {
                tracing::debug!(%account, error = %e, "INVITE handler ended with error");
            }
        });
        if replace.is_none() {
            if reliable::wants_reliable(cfg.prack, &req.headers) {
                call.send_reliable_ringing(&dialog);
            } else {
                let alerting = vec![
                    Header::Allow(ALLOW.into()),
                    Header::Supported(cfg.supported_tokens().into()),
                ];
                if let Err(e) = dialog.ringing(Some(alerting), None) {
                    tracing::warn!(call = %id, error = %e, "cannot send 180");
                }
            }
        }
        self.emit(SipEvent::CallIncoming {
            call: call.snapshot(),
        });
        self.notify_count(&env);

        // Replaces: answer at once (no ringing), then end the replaced call.
        if let Some(old) = replace {
            match call.answer(AnswerOptions::default()) {
                Ok(()) => {
                    tracing::info!(call = %id, replaced = %old.id, "INVITE with Replaces answered");
                    let _ = old.hangup_with(Some("replaced"));
                }
                Err(e) => {
                    tracing::warn!(call = %id, error = %e, "cannot answer the replacing INVITE");
                    let _ = call.reject(Some(500), None, false);
                }
            }
            return;
        }

        // auto_answer / intercom.
        let delay = if cfg.auto_answer {
            Some(Duration::from_millis(u64::from(cfg.answer_delay_ms)))
        } else if cfg.allow_intercom {
            util::intercom_delay(&req.headers)
        } else {
            None
        };
        if let Some(delay) = delay {
            let c = call.clone();
            let task = tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Err(e) = c.mgr.answer(&c.id, AnswerOptions::default()).await {
                    tracing::debug!(call = %c.id, error = %e, "auto-answer skipped");
                }
            });
            call.lock().answer_timer = Some(task.abort_handle());
        }
    }
}

impl Call {
    /// Answers a ringing inbound call: 200 with the answer (or our offer for an offer-less
    /// INVITE).
    pub(crate) fn answer(self: &Arc<Self>, opts: AnswerOptions) -> Result<()> {
        if self.direction != CallDirection::Inbound {
            return Err(self.state_err("only inbound calls can be answered"));
        }
        let mut g = self.lock();
        if g.info.state != CallState::Ringing {
            return Err(self.state_err("call is not ringing"));
        }
        let dialog = g
            .dialog
            .clone()
            .ok_or_else(|| self.state_err("no dialog"))?;
        // A CANCEL that is already handled terminated the dialog: do not answer a dead call
        // (and never report Active before the Ended 487).
        if matches!(
            dialog.state(),
            rsipstack::dialog::dialog::DialogState::Terminated(..)
        ) {
            return Err(self.state_err("call was canceled"));
        }
        let rt = g.rt.clone().ok_or_else(|| self.state_err("call is over"))?;
        // RFC 3262 §3: no 2xx while a reliable provisional response is unacknowledged; the
        // PRACK releases it.
        if g.reliable.outstanding.is_some() {
            tracing::debug!(call = %self.id, "answer deferred until the PRACK arrives");
            g.reliable.deferred = Some(opts);
            return Ok(());
        }

        // Different audio source / sink than the media was created with: re-create it (new
        // port) and redo the negotiation so the SDP carries the new port.
        let source = opts.source.unwrap_or_else(|| g.source.clone());
        let sink = opts.sink.unwrap_or_else(|| g.sink.clone());
        if source != g.source || sink != g.sink {
            let new = self
                .mgr
                .provider()
                .create(self.make_media_request(&rt, &source, &sink))
                .map_err(|e| SipError::Media(e.to_string()))?;
            if let Some(old) = g.media.replace(new.clone()) {
                old.close();
            }
            g.source = source;
            g.sink = sink;
            let port = new.local_rtp_port();
            let offer = g.offer.clone();
            let ip = rt.advertise_ip;
            let session = g
                .session
                .as_mut()
                .ok_or_else(|| self.state_err("no media session"))?;
            session.set_local_address(ip, port);
            if let Some(offer) = offer {
                let (sdp, neg) = session
                    .apply_offer(&offer)
                    .map_err(|e| SipError::Protocol(e.to_string()))?;
                g.answer = Some((sdp, neg));
            }
        }

        let (body, neg) = if g.offerless {
            let session = g
                .session
                .as_mut()
                .ok_or_else(|| self.state_err("no media session"))?;
            (session.create_offer(), None)
        } else {
            let (sdp, neg) = g
                .answer
                .clone()
                .ok_or_else(|| self.state_err("no SDP answer prepared"))?;
            (sdp, Some(neg))
        };
        let mut headers = util::sdp_headers(&self.cfg);
        let plan = g.timer_plan.take();
        if let Some((interval, refresher)) = plan {
            headers.extend(timer::answer_headers(interval, refresher));
        }
        dialog.accept(Some(headers), Some(body.into_bytes()))?;
        if let Some((interval, refresher)) = plan {
            let peer_update = g
                .invite
                .as_ref()
                .is_some_and(|i| util::allows(&i.headers, "UPDATE"));
            self.install_timer(&mut g, interval, refresher, peer_update);
        }

        if let Some(t) = g.answer_timer.take() {
            t.abort();
        }
        g.awaiting_ack_sdp = g.offerless;
        g.awaiting_ack_cseq = g.invite.as_ref().and_then(util::req_cseq);
        g.info.answered_at = Some(Utc::now());
        // Negotiation first so the `Active` snapshot already carries the media summary.
        if let Some(neg) = neg {
            self.apply_negotiated(&mut g, neg);
        }
        self.set_state(&mut g, CallState::Active);
        self.sync_hold_state(&mut g);
        if let Some(m) = &g.media
            && (g.source == AudioSource::Device || g.sink == AudioSink::Device)
        {
            m.set_focus();
            self.mgr.set_active(&self.id);
        }
        Ok(())
    }

    /// Rejects a ringing inbound call.
    pub(crate) fn reject(
        self: &Arc<Self>,
        code: Option<u16>,
        reason: Option<String>,
        user: bool,
    ) -> Result<()> {
        let code = code.unwrap_or(486);
        if !(400..=699).contains(&code) {
            return Err(SipError::Config(format!(
                "reject code {code} is not a 4xx-6xx status"
            )));
        }
        let (dialog, from) = {
            let g = self.lock();
            if self.direction != CallDirection::Inbound || g.info.state != CallState::Ringing {
                return Err(self.state_err("call is not ringing"));
            }
            (
                g.dialog
                    .clone()
                    .ok_or_else(|| self.state_err("no dialog"))?,
                g.info.remote_uri.clone(),
            )
        };
        let status = StatusCode::from(code);
        let text = reason.clone().unwrap_or_else(|| status.text().to_string());
        let reason_hdr = reason.map(|r| format!("SIP;cause={code};text=\"{r}\""));
        dialog.reject(Some(status), reason_hdr)?;
        self.local_end(code, text.clone(), EndCause::Rejected);
        if user {
            self.emit(SipEvent::CallRejected {
                account: self.account.clone(),
                from,
                code,
                reason: "user".into(),
                call_id: Some(self.id.clone()),
            });
        }
        Ok(())
    }
}
