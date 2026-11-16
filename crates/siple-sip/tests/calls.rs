//! Call tests without a PBX: two peer-mode accounts in one stack (loopback, UDP / TCP / TLS)
//! plus raw-UDP probes for the cases the stack cannot produce itself (offer-less INVITE,
//! early-dialog requests).

mod common;

use common::free_tcp_port as free_port;

use std::sync::Arc;
use std::time::Duration;

use siple_config::{AccountConfig, DtmfMode, GeneralConfig, MediaConfig, SrtpPolicy, Transport};
use siple_sip::{
    AnswerOptions, CallDirection, CallInfo, CallState, DialOptions, EndedBy, NullMediaProvider,
    NullWireSink, SipEvent, SipStack, SipStackOptions,
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

struct Rig {
    stack: SipStack,
    events: UnboundedReceiver<SipEvent>,
    /// Events received while waiting for something else; matched first by later waits.
    buf: Vec<SipEvent>,
    media: Arc<NullMediaProvider>,
}

fn rig() -> Rig {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (tx, rx) = unbounded_channel();
    let stack = SipStack::new(SipStackOptions {
        user_agent_default: "Siple-test/1".into(),
        events: tx,
        wire_sink: Arc::new(NullWireSink),
    });
    let media = NullMediaProvider::new();
    stack.set_media_provider(media.clone());
    Rig {
        stack,
        events: rx,
        buf: Vec::new(),
        media,
    }
}

fn peer(id: &str, transport: Transport, listen: bool) -> AccountConfig {
    AccountConfig {
        id: id.into(),
        username: id.into(),
        domain: "peer.test".into(),
        register: false,
        transport,
        local_port: if listen && transport != Transport::Udp {
            free_port()
        } else {
            0
        },
        bind_address: "127.0.0.1".into(),
        tls_verify: false,
        sip_trace: false,
        ..Default::default()
    }
}

async fn add(r: &Rig, cfg: AccountConfig) -> String {
    let id = cfg.id.clone();
    r.stack
        .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
        .await
        .unwrap();
    r.stack
        .account_status(&id)
        .unwrap()
        .local_addr
        .unwrap_or_default()
}

fn uri_for(user: &str, addr: &str, transport: Transport) -> String {
    let t = match transport {
        Transport::Udp => "udp",
        Transport::Tcp => "tcp",
        Transport::Tls => "tls",
        Transport::Auto => unreachable!("peer accounts use a fixed transport"),
    };
    format!("sip:{user}@{addr};transport={t}")
}

async fn until(r: &mut Rig, what: &str, pred: impl Fn(&SipEvent) -> bool) -> SipEvent {
    if let Some(i) = r.buf.iter().position(&pred) {
        return r.buf.remove(i);
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = r.events.recv().await.expect("event channel closed");
            if pred(&e) {
                return e;
            }
            r.buf.push(e);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn state_of(r: &mut Rig, id: &str, st: CallState) -> CallInfo {
    let id = id.to_string();
    match until(
        r,
        &format!("{id} -> {st:?}"),
        |e| matches!(e, SipEvent::CallState { call } if call.id == id && call.state == st),
    )
    .await
    {
        SipEvent::CallState { call } => call,
        _ => unreachable!(),
    }
}

async fn incoming(r: &mut Rig, account: &str) -> CallInfo {
    let account = account.to_string();
    match until(
        r,
        "incoming call",
        |e| matches!(e, SipEvent::CallIncoming { call } if call.account == account),
    )
    .await
    {
        SipEvent::CallIncoming { call } => call,
        _ => unreachable!(),
    }
}

/// A dials B; returns (a_call, b_call) while B rings.
async fn ring(r: &mut Rig, transport: Transport) -> (String, CallInfo) {
    add(r, peer("a", transport, false)).await;
    let b = add(r, peer("b", transport, true)).await;
    let a_id = r
        .stack
        .dial("a", &uri_for("b", &b, transport), DialOptions::default())
        .await
        .unwrap();
    let b_call = incoming(r, "b").await;
    (a_id, b_call)
}

async fn full_call(transport: Transport) {
    let mut r = rig();
    let (a_id, b_call) = ring(&mut r, transport).await;
    assert_eq!(b_call.direction, CallDirection::Inbound);
    assert_eq!(b_call.state, CallState::Ringing);
    assert!(b_call.remote_uri.contains("a@peer.test"), "{b_call:?}");
    let a_early = state_of(&mut r, &a_id, CallState::Early).await;
    assert!(!a_early.early_media);

    r.stack
        .answer(&b_call.id, AnswerOptions::default())
        .await
        .unwrap();
    let a = state_of(&mut r, &a_id, CallState::Active).await;
    let b = state_of(&mut r, &b_call.id, CallState::Active).await;
    assert!(a.answered_at.is_some() && b.answered_at.is_some());

    // Both sides negotiated the same codec; media got applied and focus.
    let (an, bn) = {
        // negotiated summary may land just after Active
        tokio::time::sleep(Duration::from_millis(300)).await;
        (
            r.stack
                .call(&a_id)
                .unwrap()
                .negotiated
                .expect("a negotiated"),
            r.stack
                .call(&b_call.id)
                .unwrap()
                .negotiated
                .expect("b negotiated"),
        )
    };
    assert_eq!(an.codec, bn.codec);
    assert!(an.rfc4733 && bn.rfc4733);
    let sa = r.media.stats(&a_id).unwrap();
    let sb = r.media.stats(&b_call.id).unwrap();
    assert!(sa.negotiated.is_some() && sb.negotiated.is_some());
    assert!(sa.focus >= 1 && sb.focus >= 1);
    assert_eq!(r.stack.calls().len(), 2);

    // SIP INFO DTMF in both directions.
    r.stack
        .send_dtmf(&a_id, "5", Some(DtmfMode::Info), Some(120))
        .await
        .unwrap();
    let e = until(
        &mut r,
        "dtmf at b",
        |e| matches!(e, SipEvent::CallDtmf { call_id, .. } if *call_id == b_call.id),
    )
    .await;
    let SipEvent::CallDtmf {
        digit,
        via,
        duration_ms,
        ..
    } = e
    else {
        unreachable!()
    };
    assert_eq!(
        (digit.as_str(), via.as_str(), duration_ms),
        ("5", "info", Some(120))
    );
    r.stack
        .send_dtmf(&b_call.id, "#", Some(DtmfMode::Info), None)
        .await
        .unwrap();
    let e = until(
        &mut r,
        "dtmf at a",
        |e| matches!(e, SipEvent::CallDtmf { call_id, .. } if *call_id == a_id),
    )
    .await;
    let SipEvent::CallDtmf { digit, .. } = e else {
        unreachable!()
    };
    assert_eq!(digit, "#");
    // auto mode uses RFC 4733 through the media engine when telephone-event was negotiated.
    r.stack.send_dtmf(&a_id, "12", None, None).await.unwrap();
    assert_eq!(
        r.media.stats(&a_id).unwrap().dtmf,
        vec![("rfc4733".to_string(), "12".to_string())]
    );

    // Mute is local.
    r.stack.mute(&a_id, true).await.unwrap();
    assert!(r.media.stats(&a_id).unwrap().muted);
    assert!(r.stack.call(&a_id).unwrap().muted);

    // A hangs up.
    r.stack.hangup(&a_id).await.unwrap();
    let a = state_of(&mut r, &a_id, CallState::Ended).await;
    let b = state_of(&mut r, &b_call.id, CallState::Ended).await;
    assert_eq!(a.end.as_ref().unwrap().by, EndedBy::Local);
    assert_eq!(b.end.as_ref().unwrap().by, EndedBy::Remote);
    assert_eq!(b.end.as_ref().unwrap().code, 200);
    assert!(r.media.stats(&a_id).unwrap().closed && r.media.stats(&b_call.id).unwrap().closed);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn call_over_udp() {
    full_call(Transport::Udp).await;
}

#[tokio::test]
async fn call_over_tcp() {
    full_call(Transport::Tcp).await;
}

#[tokio::test]
async fn call_over_tls() {
    full_call(Transport::Tls).await;
}

#[tokio::test]
async fn callee_hangs_up_is_remote_for_caller() {
    let mut r = rig();
    let (a_id, b_call) = ring(&mut r, Transport::Udp).await;
    r.stack
        .answer(&b_call.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a_id, CallState::Active).await;
    r.stack.hangup(&b_call.id).await.unwrap();
    let a = state_of(&mut r, &a_id, CallState::Ended).await;
    assert_eq!(a.end.unwrap().by, EndedBy::Remote);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn reject_with_486_and_603() {
    let mut r = rig();
    let (a_id, b_call) = ring(&mut r, Transport::Udp).await;
    r.stack.reject(&b_call.id, None, None).await.unwrap();
    let a = state_of(&mut r, &a_id, CallState::Ended).await;
    let end = a.end.unwrap();
    assert_eq!((end.code, end.by), (486, EndedBy::Remote));
    let e = until(&mut r, "rejected event", |e| {
        matches!(e, SipEvent::CallRejected { .. })
    })
    .await;
    let SipEvent::CallRejected { reason, code, .. } = e else {
        unreachable!()
    };
    assert_eq!((reason.as_str(), code), ("user", 486));
    let b = r.stack.call(&b_call.id).unwrap();
    assert_eq!(b.end.unwrap().by, EndedBy::Local);

    // hangup of a ringing inbound call declines with 603.
    let b_addr = r.stack.account_status("b").unwrap().local_addr.unwrap();
    let a2 = r
        .stack
        .dial("a", &format!("sip:b@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    let b2 = incoming(&mut r, "b").await;
    r.stack.hangup(&b2.id).await.unwrap();
    let a = state_of(&mut r, &a2, CallState::Ended).await;
    assert_eq!(a.end.unwrap().code, 603);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn cancel_while_ringing_is_missed() {
    let mut r = rig();
    let (a_id, b_call) = ring(&mut r, Transport::Udp).await;
    state_of(&mut r, &a_id, CallState::Early).await;
    r.stack.hangup(&a_id).await.unwrap();
    let b = state_of(&mut r, &b_call.id, CallState::Ended).await;
    let end = b.end.unwrap();
    assert_eq!((end.code, end.by, end.missed), (487, EndedBy::Remote, true));
    let a = state_of(&mut r, &a_id, CallState::Ended).await;
    let end = a.end.unwrap();
    assert_eq!((end.code, end.by), (487, EndedBy::Local));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn dnd_rejects_without_ringing_and_override_bookkeeping() {
    let mut r = rig();
    add(&r, peer("a", Transport::Udp, false)).await;
    let mut bcfg = peer("b", Transport::Udp, false);
    bcfg.dnd_reject_code = 480;
    let b = add(&r, bcfg.clone()).await;
    assert!(!r.stack.dnd("b").unwrap());
    r.stack.set_dnd("b", true).await.unwrap();
    assert!(r.stack.dnd("b").unwrap());
    assert!(r.stack.account_status("b").unwrap().dnd);

    let a_id = r
        .stack
        .dial("a", &format!("sip:b@{b}"), DialOptions::default())
        .await
        .unwrap();
    let a = state_of(&mut r, &a_id, CallState::Ended).await;
    assert_eq!(a.end.unwrap().code, 480);
    let e = until(&mut r, "dnd rejected", |e| {
        matches!(e, SipEvent::CallRejected { .. })
    })
    .await;
    let SipEvent::CallRejected {
        reason,
        account,
        code,
        ..
    } = e
    else {
        unreachable!()
    };
    assert_eq!((reason.as_str(), account.as_str(), code), ("dnd", "b", 480));
    // Nothing rang: no CallIncoming was produced, and A never saw an early state.
    assert!(
        r.stack
            .calls()
            .iter()
            .all(|c| c.direction == CallDirection::Outbound)
    );

    // Config update that leaves `dnd` alone keeps the override; changing it clears it.
    let mut same = bcfg.clone();
    same.display_name = "B".into();
    r.stack.update_account(same).await.unwrap();
    assert!(r.stack.dnd("b").unwrap());
    let mut changed = bcfg.clone();
    changed.dnd = true; // config now says true; override (true) is dropped, effective = config
    r.stack.update_account(changed.clone()).await.unwrap();
    assert!(r.stack.dnd("b").unwrap());
    r.stack.set_dnd("b", false).await.unwrap();
    assert!(!r.stack.dnd("b").unwrap());
    changed.dnd = false;
    r.stack.update_account(changed).await.unwrap();
    assert!(!r.stack.dnd("b").unwrap());
    r.stack.shutdown().await;
}

#[tokio::test]
async fn max_calls_rejects_with_486() {
    let mut r = rig();
    add(&r, peer("a", Transport::Udp, false)).await;
    let mut bcfg = peer("b", Transport::Udp, false);
    bcfg.max_calls = 1;
    let b = add(&r, bcfg).await;
    let target = format!("sip:b@{b}");
    r.stack
        .dial("a", &target, DialOptions::default())
        .await
        .unwrap();
    incoming(&mut r, "b").await;
    let second = r
        .stack
        .dial("a", &target, DialOptions::default())
        .await
        .unwrap();
    let a = state_of(&mut r, &second, CallState::Ended).await;
    assert_eq!(a.end.unwrap().code, 486);
    let e = until(
        &mut r,
        "max_calls",
        |e| matches!(e, SipEvent::CallRejected { reason, .. } if reason == "max_calls"),
    )
    .await;
    assert!(matches!(e, SipEvent::CallRejected { code: 486, .. }));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn auto_answer_and_intercom() {
    let mut r = rig();
    add(&r, peer("a", Transport::Udp, false)).await;
    let mut bcfg = peer("b", Transport::Udp, false);
    bcfg.auto_answer = true;
    bcfg.answer_delay_ms = 100;
    let b = add(&r, bcfg).await;
    let a_id = r
        .stack
        .dial("a", &format!("sip:b@{b}"), DialOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a_id, CallState::Active).await;
    r.stack.hangup(&a_id).await.unwrap();
    state_of(&mut r, &a_id, CallState::Ended).await;
    r.stack.shutdown().await;

    // Intercom: Call-Info answer-after (only with allow_intercom).
    let mut r = rig();
    add(&r, peer("a", Transport::Udp, false)).await;
    let mut bcfg = peer("b", Transport::Udp, false);
    bcfg.allow_intercom = true;
    let b = add(&r, bcfg).await;
    let opts = DialOptions {
        headers: vec![(
            "Call-Info".into(),
            "<sip:a@peer.test>;answer-after=0".into(),
        )],
        ..Default::default()
    };
    let a_id = r
        .stack
        .dial("a", &format!("sip:b@{b}"), opts)
        .await
        .unwrap();
    state_of(&mut r, &a_id, CallState::Active).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn hold_reinvite_marks_remote_held_and_back() {
    let mut r = rig();
    let (a_id, b_call) = ring(&mut r, Transport::Udp).await;
    r.stack
        .answer(&b_call.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a_id, CallState::Active).await;
    state_of(&mut r, &b_call.id, CallState::Active).await;
    r.stack.hold(&a_id).await.unwrap();
    let b = state_of(&mut r, &b_call.id, CallState::RemoteHeld).await;
    assert!(b.hold.remote);
    assert_eq!(r.stack.call(&a_id).unwrap().state, CallState::Held);
    assert!(r.media.stats(&a_id).unwrap().held);
    r.stack.unhold(&a_id).await.unwrap();
    let b = state_of(&mut r, &b_call.id, CallState::Active).await;
    assert!(!b.hold.remote);
    assert_eq!(r.stack.call(&a_id).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn srtp_mismatch_is_488_both_ways() {
    for (a_pol, b_pol) in [
        (SrtpPolicy::Mandatory, SrtpPolicy::Off),
        (SrtpPolicy::Off, SrtpPolicy::Mandatory),
    ] {
        let mut r = rig();
        let mut acfg = peer("a", Transport::Udp, false);
        acfg.srtp = a_pol;
        add(&r, acfg).await;
        let mut bcfg = peer("b", Transport::Udp, false);
        bcfg.srtp = b_pol;
        let b = add(&r, bcfg).await;
        let a_id = r
            .stack
            .dial("a", &format!("sip:b@{b}"), DialOptions::default())
            .await
            .unwrap();
        let a = state_of(&mut r, &a_id, CallState::Ended).await;
        assert_eq!(a.end.unwrap().code, 488, "{a_pol:?} -> {b_pol:?}");
        r.stack.shutdown().await;
    }
    // Both mandatory works and reports the suite.
    let mut r = rig();
    let mut acfg = peer("a", Transport::Udp, false);
    acfg.srtp = SrtpPolicy::Mandatory;
    add(&r, acfg).await;
    let mut bcfg = peer("b", Transport::Udp, false);
    bcfg.srtp = SrtpPolicy::Mandatory;
    bcfg.auto_answer = true;
    let b = add(&r, bcfg).await;
    let a_id = r
        .stack
        .dial("a", &format!("sip:b@{b}"), DialOptions::default())
        .await
        .unwrap();
    let a = state_of(&mut r, &a_id, CallState::Active).await;
    let n = a.negotiated.expect("negotiated");
    assert_eq!(n.srtp_suite.as_deref(), Some("AES_CM_128_HMAC_SHA1_80"));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn unknown_calls_and_bad_states_are_errors() {
    let r = rig();
    assert!(r.stack.hangup("c9999").await.is_err());
    assert!(r.stack.call("c9999").is_none());
    add(&r, peer("a", Transport::Udp, false)).await;
    assert!(
        r.stack
            .dial("nope", "1", DialOptions::default())
            .await
            .is_err()
    );
    // Extension without a registrar / next hop cannot be routed in peer mode.
    assert!(
        r.stack
            .dial("a", "1234", DialOptions::default())
            .await
            .is_err()
    );
    r.stack.shutdown().await;
}

// --- raw-UDP probes -------------------------------------------------------------------------------

struct Raw {
    sock: UdpSocket,
    dest: std::net::SocketAddr,
    call_id: String,
    local: std::net::SocketAddr,
}

impl Raw {
    async fn new(dest: &str) -> Raw {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = sock.local_addr().unwrap();
        Raw {
            sock,
            dest: dest.parse().unwrap(),
            call_id: format!("raw-{}@probe", local.port()),
            local,
        }
    }

    fn sdp(&self) -> String {
        format!(
            "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=sendrecv\r\n",
            self.local.port() + 1
        )
    }

    fn msg(
        &self,
        method: &str,
        to_tag: Option<&str>,
        cseq: u32,
        extra: &str,
        body: &str,
    ) -> String {
        let to = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        let ct = if body.is_empty() {
            String::new()
        } else {
            "Content-Type: application/sdp\r\n".into()
        };
        format!(
            "{method} sip:b@{} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bK{}{cseq}\r\nMax-Forwards: 70\r\nFrom: \"Probe\" <sip:probe@probe.test>;tag=ptag\r\nTo: <sip:b@{}>{to}\r\nCall-ID: {}\r\nCSeq: {cseq} {method}\r\nContact: <sip:probe@{}>\r\n{ct}{extra}Content-Length: {}\r\n\r\n{body}",
            self.dest,
            self.local,
            method.to_lowercase(),
            self.dest,
            self.call_id,
            self.local,
            body.len()
        )
    }

    async fn send(&self, m: &str) {
        self.sock.send_to(m.as_bytes(), self.dest).await.unwrap();
    }

    /// Next datagram that is a response to a request with this CSeq line.
    async fn response(&self, cseq_line: &str) -> String {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut buf = vec![0u8; 8192];
            loop {
                let (n, _) = self.sock.recv_from(&mut buf).await.unwrap();
                let s = String::from_utf8_lossy(&buf[..n]).into_owned();
                if s.starts_with("SIP/2.0") && s.contains(cseq_line) {
                    return s;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no response for {cseq_line}"))
    }

    /// Final response (skips 1xx).
    async fn final_response(&self, cseq_line: &str) -> String {
        loop {
            let r = self.response(cseq_line).await;
            if !r.starts_with("SIP/2.0 1") {
                return r;
            }
        }
    }
}

fn to_tag(resp: &str) -> String {
    let to = resp
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("to:"))
        .unwrap();
    to.split("tag=").nth(1).unwrap().trim().to_string()
}

#[tokio::test]
async fn offerless_invite_offer_in_200_answer_in_ack() {
    let mut r = rig();
    let mut bcfg = peer("b", Transport::Udp, false);
    bcfg.auto_answer = true;
    let b = add(&r, bcfg).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", "")).await;
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    assert!(
        ok.contains("m=audio") && ok.contains("RTP/AVP"),
        "offer expected in 200: {ok}"
    );
    let tag = to_tag(&ok);
    // Answer in the ACK.
    let ack = raw.msg("ACK", Some(&tag), 1, "", &raw.sdp());
    raw.send(&ack).await;
    let b_call = incoming(&mut r, "b").await;
    // Negotiation completes when the ACK arrives.
    let ev = until(
        &mut r,
        "media",
        |e| matches!(e, SipEvent::CallMedia { call_id, .. } if *call_id == b_call.id),
    )
    .await;
    let SipEvent::CallMedia { negotiated, .. } = ev else {
        unreachable!()
    };
    assert_eq!(negotiated.codec, "pcmu");
    assert_eq!(
        negotiated.remote_rtp,
        format!("127.0.0.1:{}", raw.local.port() + 1)
    );
    assert!(r.media.stats(&b_call.id).unwrap().negotiated.is_some());
    // BYE from the probe ends the call remotely.
    raw.send(&raw.msg(
        "BYE",
        Some(&tag),
        2,
        "Reason: SIP;cause=200;text=\"done\"\r\n",
        "",
    ))
    .await;
    let bye = raw.final_response("CSeq: 2 BYE").await;
    assert!(bye.starts_with("SIP/2.0 200"), "{bye}");
    let ended = state_of(&mut r, &b_call.id, CallState::Ended).await;
    let end = ended.end.unwrap();
    assert_eq!(
        (end.by, end.code, end.reason.as_str()),
        (EndedBy::Remote, 200, "done")
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn early_dialog_requests_and_reinvite_handling() {
    let mut r = rig();
    let b = add(&r, peer("b", Transport::Udp, false)).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "Alert-Info: <http://x/y>;info=ringtone\r\nP-Asserted-Identity: \"Alice\" <sip:alice@pbx.test>\r\n", &raw.sdp())).await;
    let ring = raw.response("CSeq: 1 INVITE").await;
    let ring = if ring.starts_with("SIP/2.0 100") {
        raw.response("CSeq: 1 INVITE").await
    } else {
        ring
    };
    assert!(ring.starts_with("SIP/2.0 180"), "{ring}");
    assert!(ring.contains("Allow:"), "{ring}");
    let tag = to_tag(&ring);
    let b_call = incoming(&mut r, "b").await;
    assert_eq!(b_call.remote_uri, "sip:alice@pbx.test");
    assert_eq!(b_call.remote_display.as_deref(), Some("Alice"));
    assert!(b_call.alert_info.as_deref().unwrap().contains("ringtone"));

    // UPDATE without SDP in the early dialog: 200, identity refresh -> CallUpdated.
    raw.send(&raw.msg(
        "UPDATE",
        Some(&tag),
        2,
        "P-Asserted-Identity: \"Alice B\" <sip:alice@pbx.test>\r\n",
        "",
    ))
    .await;
    let ok = raw.final_response("CSeq: 2 UPDATE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    let ev = until(&mut r, "updated", |e| {
        matches!(e, SipEvent::CallUpdated { .. })
    })
    .await;
    let SipEvent::CallUpdated { call } = ev else {
        unreachable!()
    };
    assert_eq!(call.remote_display.as_deref(), Some("Alice B"));

    // INFO with DTMF during the early dialog.
    raw.send(&raw.msg(
        "INFO",
        Some(&tag),
        3,
        "Content-Type: application/dtmf-relay\r\n",
        "",
    ))
    .await;
    let _ = raw.final_response("CSeq: 3 INFO").await;
    let info = raw.msg("INFO", Some(&tag), 4, "", "").replace(
        "Content-Length: 0",
        "Content-Type: application/dtmf-relay\r\nContent-Length: 20",
    );
    let info = format!(
        "{}Signal=7\r\nDuration=100\r\n",
        info.trim_end_matches("\r\n\r\n").to_string() + "\r\n\r\n"
    );
    raw.send(&info).await;
    let ok = raw.final_response("CSeq: 4 INFO").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    let ev = until(&mut r, "dtmf", |e| matches!(e, SipEvent::CallDtmf { .. })).await;
    assert!(matches!(ev, SipEvent::CallDtmf { ref digit, .. } if digit == "7"));

    // Answer, ACK, then re-INVITE sendonly -> RemoteHeld, offer-less re-INVITE -> current SDP.
    r.stack
        .answer(&b_call.id, AnswerOptions::default())
        .await
        .unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    state_of(&mut r, &b_call.id, CallState::Active).await;
    let hold_sdp = raw.sdp().replace("a=sendrecv", "a=sendonly");
    raw.send(&raw.msg("INVITE", Some(&tag), 5, "", &hold_sdp))
        .await;
    let ok = raw.final_response("CSeq: 5 INVITE").await;
    assert!(
        ok.starts_with("SIP/2.0 200") && ok.contains("a=recvonly"),
        "{ok}"
    );
    raw.send(&raw.msg("ACK", Some(&tag), 5, "", "")).await;
    let held = state_of(&mut r, &b_call.id, CallState::RemoteHeld).await;
    assert!(held.hold.remote);
    raw.send(&raw.msg("INVITE", Some(&tag), 6, "", "")).await;
    let ok = raw.final_response("CSeq: 6 INVITE").await;
    assert!(
        ok.starts_with("SIP/2.0 200") && ok.contains("m=audio"),
        "{ok}"
    );
    // The answer in the ACK un-holds.
    raw.send(&raw.msg("ACK", Some(&tag), 6, "", &raw.sdp()))
        .await;
    state_of(&mut r, &b_call.id, CallState::Active).await;
    // Unacceptable re-INVITE offer -> 488, call stays up.
    let bad = raw.sdp().replace("RTP/AVP 0 101", "RTP/SAVP 0 101");
    raw.send(&raw.msg("INVITE", Some(&tag), 7, "", &bad)).await;
    let rej = raw.final_response("CSeq: 7 INVITE").await;
    assert!(rej.starts_with("SIP/2.0 488"), "{rej}");
    raw.send(&raw.msg("ACK", Some(&tag), 7, "", "")).await;
    assert_eq!(r.stack.call(&b_call.id).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn prack_disabled_require_100rel_gets_420_and_bad_sdp_400() {
    let mut r = rig();
    let mut cfg = peer("b", Transport::Udp, false);
    cfg.prack = siple_config::PrackMode::Disabled;
    let b = add(&r, cfg).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "Require: 100rel\r\n", &raw.sdp()))
        .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(
        resp.starts_with("SIP/2.0 420") && resp.contains("100rel"),
        "{resp}"
    );
    raw.send(&raw.msg("ACK", None, 1, "", "")).await;
    let raw2 = Raw::new(&b).await;
    raw2.send(&raw2.msg("INVITE", None, 1, "", "this is not sdp"))
        .await;
    let resp = raw2.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 400"), "{resp}");
    assert!(r.stack.calls().is_empty());
    // Replaces is a Phase 7 hook: refused for now, and it must not count as DND / max_calls.
    let raw3 = Raw::new(&b).await;
    raw3.send(&raw3.msg(
        "INVITE",
        None,
        1,
        "Replaces: abc;to-tag=1;from-tag=2\r\n",
        &raw3.sdp(),
    ))
    .await;
    let resp = raw3.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 481"), "{resp}");
    let _ = &mut r;
    r.stack.shutdown().await;
}
