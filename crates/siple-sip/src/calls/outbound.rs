//! Outgoing calls.

use std::sync::{Arc, Mutex};

use chrono::Utc;
use rsipstack::dialog::dialog::{DialogState, TerminatedReason};
use rsipstack::dialog::invitation::{InviteAsyncResult, InviteOption};
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::prelude::*;
use rsipstack::sip::{Header, Response, StatusCodeKind, Uri};
use rsipstack::transport::SipAddr;
use tokio::task::JoinHandle;

use super::SrtpProbe;
use super::util::{self, SdpBody};
use super::{
    AccountCalls, Call, CallDirection, CallEnd, CallEnv, CallId, CallInfo, CallInner, CallManager,
    CallState, DialOptions, EndCause, EndedBy, HoldFlags, RtEnv, cause_for_status,
};
use crate::account::{sip_transport, uri_destination};
use crate::client::{CONNECT_TIMEOUT, build_contact};
use crate::dns;
use crate::error::{Result, SipError};
use crate::media::CallMediaRequest;
use crate::types::{ALLOW, SipEvent};
use siple_config::{AccountConfig, AudioSink, AudioSource};
use siple_media::sdp_neg::{MediaSession, SrtpPolicy as NegPolicy};

/// 3xx hops followed per call.
const MAX_REDIRECTS: u8 = 3;

/// Test hook: extra latency (ms) in front of every redirect hop, emulating a slow DNS lookup.
pub(crate) static NEXT_HOP_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Largest UDP payload (65535 - 8 UDP - 20 IP header).
const MAX_UDP_INVITE: usize = 65_507;
/// Headroom for the headers the stack adds (Via, From, To, Call-ID, CSeq, Contact, auth, ...).
const INVITE_OVERHEAD: usize = 1_500;

/// Percent-encodes the characters of a dialled string that are not legal in a URI user part.
fn escape_user(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "-_.!~*'()&=+$,;?/".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// Request-URI for a dial target: a full `sip:`/`sips:` URI as is, `user@host` gets `sip:`, and
/// digits / an extension become `sip:<target>@<domain>`.
pub(crate) fn target_uri(cfg: &AccountConfig, target: &str) -> Result<Uri> {
    let t = target.trim();
    if t.is_empty() {
        return Err(SipError::Config("empty dial target".into()));
    }
    let lower = t.to_ascii_lowercase();
    let s = if lower.starts_with("sip:") || lower.starts_with("sips:") {
        t.to_string()
    } else if t.contains('@') {
        format!("sip:{t}")
    } else {
        if cfg.domain.is_empty() {
            return Err(SipError::Config(
                "dialling an extension needs the account's domain".into(),
            ));
        }
        format!("sip:{}@{}", escape_user(t), cfg.domain)
    };
    Uri::try_from(s.as_str()).map_err(|e| SipError::Config(format!("bad dial target {t:?}: {e}")))
}

/// Memory key of a next hop: `host:port`, plus `:transport` unless it is UDP / unspecified.
fn dest_key(dest: &SipAddr) -> String {
    match dest.r#type {
        Some(t) if t != rsipstack::sip::Transport::Udp => {
            format!("{}:{}", dest.addr, t.to_string().to_ascii_lowercase())
        }
        _ => dest.addr.to_string(),
    }
}

fn host_of(hostport: &str) -> &str {
    hostport.rsplit_once(':').map_or(hostport, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') {
            h
        } else {
            hostport
        }
    })
}

impl CallManager {
    /// Next hop for a request URI: the account's outbound proxy / registrar for its own domain,
    /// otherwise the URI's host resolved with the usual SRV / A rules.
    async fn destination(&self, cfg: &AccountConfig, rt: &RtEnv, uri: &Uri) -> Result<SipAddr> {
        let (host, port, transport) = uri_destination(uri, cfg.transport);
        let registrar = cfg.effective_registrar().to_string();
        let own = host.eq_ignore_ascii_case(&cfg.domain)
            || host.eq_ignore_ascii_case(host_of(&registrar));
        if !cfg.outbound_proxy.is_empty() || (own && port.is_none()) {
            return rt.target.clone().ok_or_else(|| {
                SipError::Config("no registrar / outbound proxy to route the call through".into())
            });
        }
        let addrs = dns::resolve(self.dns.as_ref(), &host, port, transport).await?;
        let addr = addrs
            .first()
            .ok_or_else(|| SipError::Dns(format!("{host} did not resolve")))?;
        Ok(SipAddr::new(sip_transport(transport), (*addr).into()))
    }

    /// Dials `target` from the account behind `env`.
    pub(crate) async fn dial(
        self: &Arc<Self>,
        env: &Arc<AccountCalls>,
        target: &str,
        opts: DialOptions,
    ) -> Result<CallId> {
        self.dial_ext(env, target, opts, None).await
    }

    /// [`CallManager::dial`] for a call made on behalf of a REFER: `transferred_from` is the
    /// original call.
    pub(crate) async fn dial_ext(
        self: &Arc<Self>,
        env: &Arc<AccountCalls>,
        target: &str,
        opts: DialOptions,
        transferred_from: Option<CallId>,
    ) -> Result<CallId> {
        self.dial_inner(env, target, opts, transferred_from, true)
            .await
    }

    /// [`CallManager::dial_ext`]; `auto_hold = false` skips the auto-hold of the other lines (the
    /// caller holds the line coordinator and decides what is held: consult calls).
    pub(crate) async fn dial_inner(
        self: &Arc<Self>,
        env: &Arc<AccountCalls>,
        target: &str,
        opts: DialOptions,
        transferred_from: Option<CallId>,
        auto_hold: bool,
    ) -> Result<CallId> {
        // The OSRTP-memory generation travels inside the snapshot (`cenv.srtp_gen`), read
        // atomically with the config it belongs to.
        let cenv = env
            .env()
            .ok_or_else(|| SipError::Transport("account is not running".into()))?;
        let srtp_gen = cenv.srtp_gen;
        let rt = cenv
            .rt
            .clone()
            .ok_or_else(|| SipError::Transport("account transport is not up".into()))?;
        let cfg = cenv.cfg.clone();
        let callee = target_uri(&cfg, target)?;
        let dest = if opts.via_registrar {
            rt.target.clone().ok_or_else(|| {
                SipError::Config("no registrar / outbound proxy to route the call through".into())
            })?
        } else {
            self.destination(&cfg, &rt, &callee).await?
        };
        // Effective audio endpoints first: only a device-bound call holds other lines. Calls
        // without a device (agents, automation) never take the coordinator nor hold anything.
        let source = opts
            .source
            .clone()
            .unwrap_or_else(|| cfg.audio_source.clone());
        let sink = opts.sink.clone().unwrap_or_else(|| cfg.audio_sink.clone());
        let device_bound = source == AudioSource::Device || sink == AudioSink::Device;
        // `auto_hold`: the other un-held device calls in `hold_scope` go on hold first.
        if auto_hold && cfg.auto_hold && device_bound {
            let _line = self.lines.lock().await;
            self.auto_hold_others(&env.account, None).await;
        }

        // TCP/TLS: connect first so Via/Contact carry the real local address.
        let (conn, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            rt.endpoint.transport_layer.lookup(&dest, None),
        )
        .await
        .map_err(|_| SipError::Timeout)??;
        let mut adv = conn.get_addr().clone();
        if let Some(h) = &rt.public_host {
            adv.addr.host = h.clone();
        }
        let dest_transport = dest.r#type.unwrap_or(rt.transport);
        let registered = rt
            .contact
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let contact = match registered {
            Some(c) if dest_transport == rt.transport => c,
            _ => build_contact(&cfg.username, adv.addr.clone(), dest_transport, rt.use_sips),
        };

        // Media first: its port goes into the offer.
        let id = CallManager::next_id();
        let media = self
            .provider()
            .create(CallMediaRequest {
                call_id: id.clone(),
                account_id: env.account.clone(),
                bind_ip: rt.bind_ip,
                advertise_ip: rt.advertise_ip,
                device_bound,
                source: source.clone(),
                sink: sink.clone(),
            })
            .map_err(|e| SipError::Media(e.to_string()))?;
        // A peer that already refused an OSRTP offer on this account gets plain RTP/AVP directly
        // (the refusal makes some PBXes build and tear down the callee leg: double ring).
        let optional = opts.srtp.unwrap_or(cfg.srtp) == siple_config::SrtpPolicy::Optional;
        let srtp_key = optional.then(|| dest_key(&dest));
        let plain_first = srtp_key.as_deref().is_some_and(|k| env.srtp_remembered(k));
        let srtp = if plain_first {
            Some(siple_config::SrtpPolicy::Off)
        } else {
            opts.srtp
        };
        let caps = util::local_caps(&cfg, &cenv.media, opts.codecs.as_deref(), srtp);
        let mut session = MediaSession::new(caps, rt.advertise_ip, media.local_rtp_port());
        let offer = session.create_offer();

        let mut headers = vec![
            Header::Allow(ALLOW.into()),
            Header::Supported(cfg.supported_tokens().into()),
        ];
        if cfg.session_timers {
            headers.push(Header::Other(
                "Session-Expires".into(),
                cfg.session_expires.to_string(),
            ));
            headers.push(Header::Other("Min-SE".into(), cfg.min_se.to_string()));
        }
        for (k, v) in cfg
            .extra_headers
            .iter()
            .chain(opts.headers.iter().map(|(k, v)| (k, v)))
        {
            headers.push(Header::Other(k.clone(), v.clone()));
        }
        // An INVITE that cannot fit one UDP datagram would only fail later at every (re)send and
        // sit in `dialing` until timer B: refuse it now. (Above the MTU, IP fragmentation carries
        // it; RFC 3261 §18.1.1 prefers TCP there, which a UDP-only account cannot switch to.)
        if dest_transport == rsipstack::sip::Transport::Udp {
            let est: usize = offer.len()
                + callee.to_string().len() * 2
                + headers
                    .iter()
                    .map(|h| h.to_string().len() + 2)
                    .sum::<usize>()
                + INVITE_OVERHEAD;
            if est > MAX_UDP_INVITE {
                media.close();
                return Err(SipError::Transport(format!(
                    "INVITE is about {est} bytes, too large for one UDP datagram \
                     (max {MAX_UDP_INVITE}); shorten the target / headers or use TCP"
                )));
            }
        }
        let caller = Uri::try_from(cfg.aor().as_str())
            .map_err(|e| SipError::Config(format!("bad account AOR: {e}")))?;
        let invite = InviteOption {
            caller_display_name: (!cfg.display_name.is_empty()).then(|| cfg.display_name.clone()),
            caller,
            callee: callee.clone(),
            destination: Some(dest),
            content_type: Some("application/sdp".into()),
            offer: Some(offer.into_bytes()),
            contact,
            credential: Some(env.credential()),
            headers: Some(headers),
            // `100rel` is part of our own Supported header (rsipstack PRACKs on that).
            support_prack: false,
            ..Default::default()
        };
        let (dialog, handle) = match rt
            .dialogs
            .do_invite_async(invite.clone(), rt.state_tx.clone())
        {
            Ok(x) => x,
            Err(e) => {
                media.close();
                return Err(e.into());
            }
        };
        let dlg_id = dialog.id();
        let info = CallInfo {
            id: id.clone(),
            account: env.account.clone(),
            direction: CallDirection::Outbound,
            state: CallState::Dialing,
            remote_uri: callee.to_string(),
            remote_display: None,
            local_uri: cfg.aor(),
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
            alert_info: None,
            pai: None,
            rpid: None,
            srtp_fallback: false,
            transferred_from,
            replaces: None,
            transfer: None,
            reconnecting: false,
        };
        let call = Arc::new(Call {
            id: id.clone(),
            account: env.account.clone(),
            direction: CallDirection::Outbound,
            mgr: self.clone(),
            env: env.clone(),
            cfg: cfg.clone(),
            media_cfg: cenv.media.clone(),
            ended: tokio::sync::watch::channel(false).0,
            inner: Mutex::new(CallInner {
                info,
                local_tag: dlg_id.local_tag.clone(),
                rt: Some(rt.clone()),
                local_contact: Some(invite.contact.clone()),
                dialog: Some(dialog),
                media: Some(media),
                session: Some(session),
                neg: None,
                source,
                sink,
                offer: None,
                answer: None,
                offerless: false,
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
                invite_abort: Some(handle.abort_handle()),
                answer_timer: None,
                invite: None,
                srtp: srtp.unwrap_or(cfg.srtp),
                srtp_retried: false,
                srtp_plain_first: plain_first,
                srtp_probe: SrtpProbe::None,
                srtp_gen,
                srtp_key,
                transfer: None,
                reliable: Default::default(),
                timer_plan: None,
                timer: None,
                cause_hint: None,
            }),
        });
        call.emit(SipEvent::CallState {
            call: call.snapshot(),
        });
        self.register(call.clone()).await;
        self.notify_count(env);
        let driver = call.clone();
        tokio::spawn(async move { driver.drive_outbound(handle, invite, cenv).await });
        Ok(id)
    }
}

impl Call {
    /// Waits for the INVITE outcome, following redirects, and finishes the call.
    async fn drive_outbound(
        self: Arc<Self>,
        handle: JoinHandle<InviteAsyncResult>,
        invite: InviteOption,
        cenv: CallEnv,
    ) {
        self.drive_outbound_inner(handle, invite, cenv).await;
        // The INVITE is over: nothing is left for a background hang-up task to wait for.
        self.lock().late_dialog = None;
    }

    async fn drive_outbound_inner(
        self: &Arc<Self>,
        mut handle: JoinHandle<InviteAsyncResult>,
        mut invite: InviteOption,
        cenv: CallEnv,
    ) {
        let mut hops = 0u8;
        let mut timer_retries = 0u8;
        loop {
            let result = handle.await;
            let (dlg_id, resp) = match result {
                Ok(Ok((id, Some(resp)))) => (id, resp),
                Ok(Ok((_, None))) => {
                    self.fail_dial(0, "no response to INVITE", EndedBy::Local);
                    return;
                }
                Ok(Err(e)) => {
                    let local = self.lock().local_hangup;
                    if local {
                        self.local_end(487, "Request Terminated", EndCause::Cancelled);
                    } else if let Some(msg) = crate::authz::refusal(&e) {
                        // The challenge came from a host that is not the account's provider.
                        let code = match &e {
                            rsipstack::Error::DialogError(_, _, c) => c.code(),
                            _ => 401,
                        };
                        self.fail_dial(code, &msg, EndedBy::Local);
                    } else {
                        self.fail_dial(0, &e.to_string(), EndedBy::Local);
                    }
                    return;
                }
                Err(_) => {
                    // Aborted by hangup before any response.
                    self.local_end(487, "Request Terminated", EndCause::Cancelled);
                    return;
                }
            };
            let status = resp.status_code.clone();
            match status.kind() {
                StatusCodeKind::Successful => {
                    self.on_answered(&resp).await;
                    return;
                }
                StatusCodeKind::Redirection
                    if self.cfg.follow_redirects
                        && hops < MAX_REDIRECTS
                        && !self.lock().local_hangup =>
                {
                    match self.redirect_target(&resp) {
                        Some(uri) => {
                            tracing::info!(call = %self.id, code = status.code(), target = %uri, "following redirect");
                            hops += 1;
                            match self.next_hop(&mut invite, uri, &cenv).await {
                                Ok(h) => {
                                    handle = h;
                                    continue;
                                }
                                Err(e) => {
                                    self.fail_dial(0, &e.to_string(), EndedBy::Local);
                                    return;
                                }
                            }
                        }
                        None => {
                            self.finish_failed(&resp, &dlg_id.call_id);
                            return;
                        }
                    }
                }
                _ if status.code() == 422 && self.cfg.session_timers && timer_retries < 2 => {
                    // RFC 4028 §7.3: retry with Session-Expires / Min-SE of at least the
                    // peer's Min-SE.
                    timer_retries += 1;
                    match util::parse_min_se(&resp.headers) {
                        Some(min_se) if min_se > 0 => {
                            match self.timer_retry(&mut invite, min_se).await {
                                Ok(Some(h)) => {
                                    handle = h;
                                    continue;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::warn!(call = %self.id, error = %e, "422 retry failed");
                                }
                            }
                        }
                        _ => tracing::warn!(call = %self.id, "422 without a usable Min-SE"),
                    }
                    self.finish_failed(&resp, &dlg_id.call_id);
                    return;
                }
                _ if matches!(status.code(), 488 | 415 | 606) => {
                    match self.srtp_fallback(&mut invite, &cenv).await {
                        Ok(Some(h)) => {
                            handle = h;
                            continue;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(call = %self.id, error = %e, "plain-RTP retry failed");
                        }
                    }
                    self.finish_failed(&resp, &dlg_id.call_id);
                    return;
                }
                _ => {
                    self.finish_failed(&resp, &dlg_id.call_id);
                    return;
                }
            }
        }
    }

    /// `srtp = optional` offers (RTP/AVP + `a=crypto`, OSRTP) are refused by some PBXes with
    /// 488 / 415 / 606. Desk phones retry with a plain offer; so do we, once: a new INVITE (new
    /// SIP Call-ID, same call id) without crypto.
    async fn srtp_fallback(
        self: &Arc<Self>,
        invite: &mut InviteOption,
        cenv: &CallEnv,
    ) -> Result<Option<JoinHandle<InviteAsyncResult>>> {
        // The hang-up check and the swap to the new dialog happen under one lock, so a hang-up
        // either comes first (nothing is sent) or finds the new dialog to CANCEL.
        let mut g = self.lock();
        if !(g.srtp == siple_config::SrtpPolicy::Optional || g.srtp_plain_first)
            || g.srtp_retried
            || g.local_hangup
            || g.info.state == CallState::Ended
        {
            return Ok(None);
        }
        let (Some(s), Some(m), Some(rt)) = (g.session.as_ref(), g.media.as_ref(), g.rt.clone())
        else {
            return Ok(None);
        };
        let (mut caps, port) = (s.caps().clone(), m.local_rtp_port());
        let plain_first = g.srtp_plain_first;
        if plain_first {
            tracing::warn!(call = %self.id, "plain RTP/AVP offer refused by a remembered destination; retrying once with the OSRTP offer");
            caps.srtp = NegPolicy::Optional;
        } else {
            tracing::warn!(call = %self.id, "SRTP (optional) offer refused; retrying once with a plain RTP/AVP offer");
            caps.srtp = NegPolicy::Off;
        }
        if let Some(d) = invite.destination.as_ref() {
            g.srtp_key = Some(dest_key(d));
        }
        let mut session = MediaSession::new(caps, rt.advertise_ip, port);
        invite.offer = Some(session.create_offer().into_bytes());
        invite.call_id = None;
        let (dialog, handle) = rt
            .dialogs
            .do_invite_async(invite.clone(), rt.state_tx.clone())?;
        let id = dialog.id();
        let _ = cenv;
        g.dialog = Some(dialog);
        g.local_tag = id.local_tag;
        g.info.sip_call_id = id.call_id;
        g.invite_abort = Some(handle.abort_handle());
        g.session = Some(session);
        g.early_sdp = None;
        g.srtp_retried = true;
        if plain_first {
            g.srtp = siple_config::SrtpPolicy::Optional;
            g.srtp_probe = SrtpProbe::Forget;
        } else {
            g.srtp = siple_config::SrtpPolicy::Off;
            g.srtp_probe = SrtpProbe::Remember;
            g.info.srtp_fallback = true;
        }
        self.emit(SipEvent::CallUpdated {
            call: g.info.clone(),
        });
        Ok(Some(handle))
    }

    /// The retry INVITE got its final 2xx: update the OSRTP refusal memory of its destination
    /// (ignored when the memory was cleared since the call started).
    pub(crate) fn confirm_srtp_probe(&self, g: &mut CallInner) {
        let probe = std::mem::take(&mut g.srtp_probe);
        let Some(key) = g.srtp_key.clone() else {
            return;
        };
        let changed = match probe {
            SrtpProbe::None => false,
            SrtpProbe::Remember => {
                let new = self.env.remember_srtp_dest(&key, g.srtp_gen);
                if new {
                    tracing::info!(account = %self.account, dest = %key, "peer refused OSRTP but accepted plain RTP; offering plain RTP first to it");
                }
                new
            }
            SrtpProbe::Forget => {
                let was = self.env.forget_srtp_dest(&key, g.srtp_gen);
                if was {
                    tracing::info!(account = %self.account, dest = %key, "peer accepts OSRTP again; offering it first");
                }
                was
            }
        };
        if changed {
            let _ = self.env.cmd.send(crate::account::Cmd::Refresh);
        }
    }

    /// 422 Session Interval Too Small: a new INVITE (new Call-ID) whose `Session-Expires` and
    /// `Min-SE` are at least the peer's `Min-SE`.
    async fn timer_retry(
        self: &Arc<Self>,
        invite: &mut InviteOption,
        min_se: u32,
    ) -> Result<Option<JoinHandle<InviteAsyncResult>>> {
        let mut g = self.lock();
        if g.local_hangup || g.info.state == CallState::Ended {
            return Ok(None);
        }
        let Some(rt) = g.rt.clone() else {
            return Ok(None);
        };
        let interval = self.cfg.session_expires.max(min_se);
        tracing::info!(call = %self.id, min_se, interval, "422: retrying with a longer session interval");
        let hs = invite.headers.get_or_insert_with(Vec::new);
        hs.retain(|h| {
            !h.name().eq_ignore_ascii_case("session-expires")
                && !h.name().eq_ignore_ascii_case("min-se")
        });
        hs.push(Header::Other(
            "Session-Expires".into(),
            interval.to_string(),
        ));
        hs.push(Header::Other("Min-SE".into(), min_se.to_string()));
        invite.call_id = None;
        let (dialog, handle) = rt
            .dialogs
            .do_invite_async(invite.clone(), rt.state_tx.clone())?;
        let id = dialog.id();
        g.dialog = Some(dialog);
        g.local_tag = id.local_tag;
        g.info.sip_call_id = id.call_id;
        g.invite_abort = Some(handle.abort_handle());
        g.early_sdp = None;
        Ok(Some(handle))
    }

    fn redirect_target(&self, resp: &Response) -> Option<Uri> {
        let c = resp.typed_contact_headers().ok()?;
        let mut uri = c.first()?.uri.clone();
        uri.headers.clear();
        Some(uri)
    }

    /// Sends the INVITE again to a redirect target (fresh Call-ID, same offer).
    async fn next_hop(
        self: &Arc<Self>,
        invite: &mut InviteOption,
        uri: Uri,
        cenv: &CallEnv,
    ) -> Result<JoinHandle<InviteAsyncResult>> {
        let rt = self.rt().ok_or_else(|| self.state_err("call is over"))?;
        let delay = NEXT_HOP_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let dest = self.mgr.destination(&self.cfg, &rt, &uri).await?;
        tokio::time::timeout(
            CONNECT_TIMEOUT,
            rt.endpoint.transport_layer.lookup(&dest, None),
        )
        .await
        .map_err(|_| SipError::Timeout)??;
        let key = dest_key(&dest);
        invite.callee = uri.clone();
        invite.destination = Some(dest);
        invite.call_id = None;
        // Re-check the hang-up under the lock that also swaps the dialogs (the lookups above
        // took time): a hang-up in between must not leave a ghost call behind.
        let mut g = self.lock();
        if g.local_hangup || g.info.state == CallState::Ended {
            return Err(SipError::CallState {
                call: self.id.clone(),
                msg: "call is over".into(),
            });
        }
        // A redirect starts over for the new destination: any pending probe belongs to the old
        // one and is dropped (never re-keyed), and the first offer is re-evaluated against the
        // new destination's own memory entry.
        g.srtp_probe = SrtpProbe::None;
        let mut offer_changed = false;
        if g.srtp_key.is_some()
            && let (Some(s), Some(m)) = (g.session.as_ref(), g.media.as_ref())
        {
            let plain_first = self.env.srtp_remembered(&key);
            let (mut caps, port) = (s.caps().clone(), m.local_rtp_port());
            caps.srtp = if plain_first {
                NegPolicy::Off
            } else {
                NegPolicy::Optional
            };
            let mut session = MediaSession::new(caps, rt.advertise_ip, port);
            invite.offer = Some(session.create_offer().into_bytes());
            g.session = Some(session);
            g.early_sdp = None;
            g.srtp_retried = false;
            g.srtp_plain_first = plain_first;
            g.srtp = if plain_first {
                siple_config::SrtpPolicy::Off
            } else {
                siple_config::SrtpPolicy::Optional
            };
            offer_changed = g.info.srtp_fallback;
            g.info.srtp_fallback = false;
        }
        let (dialog, handle) = rt
            .dialogs
            .do_invite_async(invite.clone(), rt.state_tx.clone())?;
        let id = dialog.id();
        let _ = cenv;
        g.dialog = Some(dialog);
        g.local_tag = id.local_tag;
        g.info.sip_call_id = id.call_id;
        g.info.remote_uri = uri.to_string();
        if g.srtp_key.is_some() {
            g.srtp_key = Some(key);
        }
        if offer_changed {
            self.emit(SipEvent::CallUpdated {
                call: g.info.clone(),
            });
        }
        g.invite_abort = Some(handle.abort_handle());
        Ok(handle)
    }

    fn fail_dial(&self, code: u16, reason: &str, by: EndedBy) {
        self.end(CallEnd {
            code,
            reason: reason.to_string(),
            by,
            missed: false,
            q850_cause: None,
            cause: EndCause::Failed,
        });
    }

    /// A final non-2xx response ended the INVITE.
    fn finish_failed(&self, resp: &Response, _sip_call_id: &str) {
        let code = resp.status_code.code();
        let local = self.lock().local_hangup;
        let reason = util::header_first(&resp.headers, "reason")
            .map(|r| util::parse_reason(&r).text.unwrap_or(r))
            .unwrap_or_else(|| resp.status_code.text().to_string());
        let by = if local && code == 487 {
            EndedBy::Local
        } else {
            EndedBy::Remote
        };
        self.end(CallEnd {
            code,
            reason: if code == 487 && local {
                "Request Terminated".into()
            } else if reason.is_empty() || reason == "Other" {
                resp.status_code.text().to_string()
            } else {
                reason
            },
            by,
            missed: false,
            q850_cause: None,
            cause: if local && code == 487 {
                EndCause::Cancelled
            } else {
                cause_for_status(code)
            },
        });
    }

    /// 2xx received: final answer, media, Active.
    async fn on_answered(self: &Arc<Self>, resp: &Response) {
        let body = util::sdp_body(&resp.headers, &resp.body);
        let bye_dialog = {
            let mut g = self.lock();
            if g.local_hangup {
                // Hang-up raced the answer (the call may already be Ended locally): the call
                // exists now, so end it with a BYE.
                g.late_dialog.clone().or_else(|| g.dialog.clone())
            } else if g.info.state == CallState::Ended {
                // The remote BYE won the race; nothing left to set up.
                None
            } else {
                self.confirm_srtp_probe(&mut g);
                let outcome = match body {
                    SdpBody::Sdp(text, sd) => {
                        if g.early_sdp.as_deref() == Some(text.as_str()) && g.neg.is_some() {
                            Ok(None)
                        } else {
                            match g.session.as_mut() {
                                Some(s) => s.apply_answer(&sd).map(Some),
                                None => Err(siple_media::sdp_neg::NegError::Malformed(
                                    "no media session".into(),
                                )),
                            }
                        }
                    }
                    _ if g.neg.is_some() => Ok(None),
                    _ => Err(siple_media::sdp_neg::NegError::NotAcceptable(
                        "2xx without a usable SDP answer".into(),
                    )),
                };
                match outcome {
                    Ok(neg) => {
                        g.info.answered_at = Some(Utc::now());
                        self.update_identity(&mut g, &resp.headers, false);
                        // Negotiation first so the `Active` snapshot carries the summary.
                        if let Some(neg) = neg.or_else(|| g.neg.clone()) {
                            self.apply_negotiated(&mut g, neg);
                        }
                        self.set_state(&mut g, CallState::Active);
                        self.sync_hold_state(&mut g);
                        if let Some(m) = &g.media
                            && (g.source == AudioSource::Device || g.sink == AudioSink::Device)
                        {
                            m.set_focus();
                            self.mgr.set_active(&self.id);
                            // The user dialled this party: other un-held device lines go on
                            // hold (through the coordinator, off this task's lock).
                            let call = self.clone();
                            tokio::spawn(async move {
                                call.mgr.clone().outbound_answered(&call).await;
                            });
                        }
                        // RFC 4028: the 2xx names interval and refresher (`uac` = us; a
                        // missing refresher is taken as us, which is the safe side).
                        if self.cfg.session_timers
                            && let Some((delta, r)) = util::parse_session_expires(&resp.headers)
                        {
                            self.install_timer(
                                &mut g,
                                delta,
                                r.unwrap_or(util::Refresher::Uac),
                                util::allows(&resp.headers, "UPDATE"),
                            );
                        }
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(call = %self.id, error = %e, "answer SDP rejected; hanging up");
                        g.local_hangup = true;
                        g.pending_negotiation_error = Some(e.status_code());
                        g.dialog.clone()
                    }
                }
            }
        };
        let Some(dialog) = bye_dialog else {
            let (d, rt) = {
                let g = self.lock();
                (g.dialog.clone(), g.rt.clone())
            };
            if let (Some(d), Some(rt)) = (d, rt) {
                rt.dialogs.remove_dialog(&d.id());
            }
            return;
        };
        self.bye_after_failed_answer(dialog).await;
    }

    async fn bye_after_failed_answer(self: &Arc<Self>, dialog: InviteDialog) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(6), dialog.bye()).await;
        let code = self.lock().pending_negotiation_error.take();
        match code {
            Some(c) => {
                self.local_end(c, "answer SDP not acceptable", EndCause::Failed);
            }
            None => {
                self.local_end(487, "Request Terminated", EndCause::Cancelled);
            }
        }
    }

    /// Background part of hanging up an outbound call that has no final response yet (the
    /// call is already Ended locally). CANCEL may only follow a provisional response (RFC 3261
    /// §9.1), so wait for one (at most 64*T1); a 2xx that crosses gets ACK + BYE from the
    /// driver ([`Call::on_answered`]).
    pub(crate) async fn finish_cancel(self: Arc<Self>, d: InviteDialog, rt: Option<Arc<RtEnv>>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(32);
        loop {
            match d.state() {
                DialogState::Trying(_) | DialogState::Early(..) => {
                    if let Err(e) = d.cancel().await {
                        tracing::debug!(call = %self.id, error = %e, "CANCEL failed");
                    }
                    break;
                }
                DialogState::Calling(_) => {
                    if self.lock().late_dialog.is_none() || tokio::time::Instant::now() >= deadline
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                _ => break,
            }
        }
        // Let the driver see the final response (487, or a 2xx it has to BYE).
        while self.lock().late_dialog.is_some() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        {
            let mut g = self.lock();
            if g.late_dialog.take().is_some()
                && let Some(a) = g.invite_abort.take()
            {
                a.abort();
            }
        }
        if let Some(rt) = rt {
            rt.dialogs.remove_dialog(&d.id());
        }
    }

    /// Terminal state of an outbound INVITE reported by the dialog layer.
    pub(crate) fn outbound_terminated(&self, reason: &TerminatedReason) -> bool {
        // Failures of the INVITE itself are finished by the driver (it has the response).
        let st = self.lock().info.state;
        matches!(st, CallState::Dialing | CallState::Early)
            && matches!(
                reason,
                TerminatedReason::UasOther(_)
                    | TerminatedReason::ProxyAuthRequired
                    | TerminatedReason::ProxyError(_)
                    | TerminatedReason::UacOther(_)
                    | TerminatedReason::UasBusy
                    | TerminatedReason::UacBusy
                    | TerminatedReason::UasDecline
                    | TerminatedReason::Timeout
                    | TerminatedReason::UacCancel
            )
    }
}
