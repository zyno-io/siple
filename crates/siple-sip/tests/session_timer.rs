//! Session timers (RFC 4028) in both roles: 422 handling, refresher selection, the
//! refresh we send (UPDATE / re-INVITE with an unchanged `o=`), peer refreshes, and the BYE when
//! the session lapses. Peer-mode stacks and a raw-UDP probe; intervals of a few seconds.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use siple_config::AccountConfig;
use siple_sip::{
    AnswerOptions, CallState, NullMediaProvider, SipStack, SipStackOptions, WireEvent, WireSink,
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::unbounded_channel;

fn timed(id: &str, expires: u32, min_se: u32) -> AccountConfig {
    let mut c = peer(id);
    c.session_expires = expires;
    c.min_se = min_se;
    c
}

fn header_value(m: &str, name: &str) -> String {
    hdr(m, name)
        .first()
        .map(|l| l.split_once(':').unwrap().1.trim().to_string())
        .unwrap_or_default()
}

fn o_line(sdp: &str) -> String {
    sdp.lines()
        .find(|l| l.starts_with("o="))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn body_of(m: &str) -> &str {
    m.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

/// Inbound call from the probe with the given extra INVITE headers; answered and ACKed.
async fn established_with(r: &mut Rig, raw: &Raw, extra: &str) -> (String, String, String) {
    raw.send(&raw.msg("INVITE", None, 1, extra, &raw.sdp()))
        .await;
    let call = incoming(r).await;
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    let tag = to_tag(&ok);
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    state_of(r, &call.id, CallState::Active).await;
    (call.id, tag, ok)
}

#[tokio::test]
async fn uas_uac_refresher_peer_lapses_we_bye_with_408() {
    let mut r = rig();
    let b = add(&r, timed("b", 1800, 1)).await;
    let raw = Raw::new(&b).await;
    let (id, _tag, ok) = established_with(
        &mut r,
        &raw,
        "Supported: timer\r\nSession-Expires: 3\r\nAllow: INVITE, ACK, BYE, UPDATE\r\n",
    )
    .await;
    assert_eq!(
        header_value(&ok, "session-expires"),
        "3;refresher=uac",
        "{ok}"
    );
    assert!(header_value(&ok, "require").contains("timer"), "{ok}");
    assert!(header_value(&ok, "supported").contains("timer"), "{ok}");
    // Nobody refreshes: BYE at interval - min(32, interval/3) = 2 s after the 200.
    let bye = raw
        .request("BYE", 6)
        .await
        .expect("BYE when the session lapses");
    assert!(header_value(&bye, "reason").contains("cause=408"), "{bye}");
    raw.ok_to(&bye).await;
    let info = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(info.end.unwrap().code, 408);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn uas_uac_refresher_peer_refreshes_with_update_and_reinvite() {
    let mut r = rig();
    let b = add(&r, timed("b", 1800, 1)).await;
    let raw = Raw::new(&b).await;
    let (id, tag, _ok) = established_with(
        &mut r,
        &raw,
        "Supported: timer\r\nSession-Expires: 3\r\nAllow: INVITE, ACK, BYE, UPDATE\r\n",
    )
    .await;
    // Alternate UPDATE / re-INVITE every ~1 s for 5 s (well past the 3 s interval).
    let mut cseq = 2;
    for i in 0..5 {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if i % 2 == 0 {
            raw.send(&raw.msg(
                "UPDATE",
                Some(&tag),
                cseq,
                "Session-Expires: 3;refresher=uac\r\nSupported: timer\r\n",
                "",
            ))
            .await;
            let resp = raw.final_response(&format!("CSeq: {cseq} UPDATE")).await;
            assert!(resp.starts_with("SIP/2.0 200"), "{resp}");
            assert_eq!(header_value(&resp, "session-expires"), "3;refresher=uac");
        } else {
            raw.send(&raw.msg(
                "INVITE",
                Some(&tag),
                cseq,
                "Session-Expires: 3;refresher=uac\r\nSupported: timer\r\n",
                &raw.sdp(),
            ))
            .await;
            let resp = raw.final_response(&format!("CSeq: {cseq} INVITE")).await;
            assert!(resp.starts_with("SIP/2.0 200"), "{resp}");
            assert_eq!(header_value(&resp, "session-expires"), "3;refresher=uac");
            raw.send(&raw.msg("ACK", Some(&tag), cseq, "", "")).await;
        }
        cseq += 1;
        assert_eq!(
            r.stack.call(&id).unwrap().state,
            CallState::Active,
            "alive after refresh {i}"
        );
    }
    // Stop refreshing: it lapses.
    let bye = raw
        .request("BYE", 6)
        .await
        .expect("BYE after the refreshes stop");
    raw.ok_to(&bye).await;
    state_of(&mut r, &id, CallState::Ended).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn interval_below_min_se_gets_422_in_invite_and_refresh() {
    let mut r = rig();
    let b = add(&r, timed("b", 1800, 5)).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg(
        "INVITE",
        None,
        1,
        "Supported: timer\r\nSession-Expires: 2\r\n",
        &raw.sdp(),
    ))
    .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 422"), "{resp}");
    assert_eq!(header_value(&resp, "min-se"), "5", "{resp}");
    assert!(r.stack.calls().is_empty());

    // In an established dialog a refresh below Min-SE is refused too.
    let raw2 = Raw::new(&b).await;
    let (id, tag, _) =
        established_with(&mut r, &raw2, "Supported: timer\r\nSession-Expires: 60\r\n").await;
    raw2.send(&raw2.msg(
        "UPDATE",
        Some(&tag),
        2,
        "Session-Expires: 2;refresher=uac\r\n",
        "",
    ))
    .await;
    let resp = raw2.final_response("CSeq: 2 UPDATE").await;
    assert!(resp.starts_with("SIP/2.0 422"), "{resp}");
    assert_eq!(header_value(&resp, "min-se"), "5");
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn we_are_refresher_uas_update_every_half_interval() {
    let mut r = rig();
    let b = add(&r, timed("b", 1800, 1)).await;
    let raw = Raw::new(&b).await;
    let (id, tag, ok) = established_with(
        &mut r,
        &raw,
        "Supported: timer\r\nSession-Expires: 4;refresher=uas\r\nAllow: INVITE, ACK, BYE, UPDATE\r\n",
    )
    .await;
    assert_eq!(
        header_value(&ok, "session-expires"),
        "4;refresher=uas",
        "{ok}"
    );
    assert!(
        !header_value(&ok, "require").contains("timer"),
        "no Require: timer when the UAS refreshes: {ok}"
    );
    // Two refreshes, ~2 s apart (half of 4 s); each must carry the interval and no body.
    for n in 1..=2 {
        let upd = raw
            .request("UPDATE", 4)
            .await
            .unwrap_or_else(|| panic!("refresh {n} (UPDATE)"));
        assert_eq!(
            header_value(&upd, "session-expires"),
            "4;refresher=uas",
            "{upd}"
        );
        assert!(body_of(&upd).is_empty(), "UPDATE refresh carries no SDP");
        raw.reply(&upd, "200 OK", None).await;
    }
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    // Silence from the probe's side now: nothing else to check but that BYE works.
    let _ = tag;
    r.stack.hangup(&id).await.unwrap();
    r.stack.shutdown().await;
}

#[tokio::test]
async fn we_are_refresher_reinvite_keeps_the_sdp_version() {
    let mut r = rig();
    let b = add(&r, timed("b", 1800, 1)).await;
    let raw = Raw::new(&b).await;
    // No UPDATE in Allow: refresh with a re-INVITE.
    let (id, tag, ok) = established_with(
        &mut r,
        &raw,
        "Supported: timer\r\nSession-Expires: 4;refresher=uas\r\nAllow: INVITE, ACK, BYE\r\n",
    )
    .await;
    let first_o = o_line(body_of(&ok));
    assert!(first_o.starts_with("o="), "{ok}");
    for n in 1..=2 {
        let inv = raw
            .request("INVITE", 5)
            .await
            .unwrap_or_else(|| panic!("refresh {n} (re-INVITE)"));
        assert_eq!(
            header_value(&inv, "session-expires"),
            "4;refresher=uas",
            "{inv}"
        );
        assert_eq!(
            o_line(body_of(&inv)),
            first_o,
            "o= must not change on refresh"
        );
        raw.reply(&inv, "200 OK", Some(&raw.sdp())).await;
        let ack = raw.request("ACK", 3).await;
        assert!(ack.is_some(), "ACK for the refresh 200");
    }
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    let _ = tag;
    r.stack.hangup(&id).await.unwrap();
    r.stack.shutdown().await;
}

#[tokio::test]
async fn timers_disabled_ignore_session_expires_and_refuse_require_timer() {
    let mut r = rig();
    let mut cfg = peer("b");
    cfg.session_timers = false;
    let b = add(&r, cfg).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg(
        "INVITE",
        None,
        1,
        "Require: timer\r\nSession-Expires: 600\r\n",
        &raw.sdp(),
    ))
    .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(
        resp.starts_with("SIP/2.0 420") && header_value(&resp, "unsupported").contains("timer"),
        "{resp}"
    );
    let raw2 = Raw::new(&b).await;
    let (_, _, ok) = established_with(
        &mut r,
        &raw2,
        "Supported: timer\r\nSession-Expires: 600\r\n",
    )
    .await;
    assert!(hdr(&ok, "session-expires").is_empty(), "{ok}");
    assert!(!header_value(&ok, "supported").contains("timer"), "{ok}");
    r.stack.shutdown().await;
}

// --- UAC role -------------------------------------------------------------------------------

/// A response from the probe acting as UAS of `req` (adds a To tag).
async fn uas_reply(
    sock: &UdpSocket,
    to: std::net::SocketAddr,
    req: &str,
    status: &str,
    extra: &str,
    body: &str,
) {
    let ct = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/sdp\r\n"
    };
    let to_hdr = hdr(req, "to")[0];
    let to_hdr = if to_hdr.contains("tag=") {
        to_hdr.to_string()
    } else {
        format!("{to_hdr};tag=utag")
    };
    let resp = format!(
        "SIP/2.0 {status}\r\n{}\r\n{}\r\n{to_hdr}\r\n{}\r\n{}\r\nContact: <sip:probe@{}>\r\n{ct}{extra}Content-Length: {}\r\n\r\n{body}",
        hdr(req, "via").join("\r\n"),
        hdr(req, "from")[0],
        hdr(req, "call-id")[0],
        hdr(req, "cseq")[0],
        sock.local_addr().unwrap(),
        body.len()
    );
    sock.send_to(resp.as_bytes(), to).await.unwrap();
}

async fn recv_req(
    sock: &UdpSocket,
    method: &str,
    secs: u64,
) -> Option<(String, std::net::SocketAddr)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut buf = vec![0u8; 8192];
        match tokio::time::timeout(left, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                let s = String::from_utf8_lossy(&buf[..n]).into_owned();
                if s.starts_with(&format!("{method} ")) {
                    return Some((s, from));
                }
            }
            _ => return None,
        }
    }
}

#[tokio::test]
async fn uac_422_retry_then_we_refresh_as_uac() {
    let mut r = rig();
    let a = add(&r, timed("a", 4, 1)).await;
    let _ = a;
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let paddr = sock.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:probe@{paddr}"), Default::default())
        .await
        .unwrap();

    let (inv1, from) = recv_req(&sock, "INVITE", 5).await.expect("INVITE");
    assert!(header_value(&inv1, "supported").contains("timer"), "{inv1}");
    assert_eq!(header_value(&inv1, "session-expires"), "4", "{inv1}");
    assert_eq!(header_value(&inv1, "min-se"), "1", "{inv1}");
    uas_reply(
        &sock,
        from,
        &inv1,
        "422 Session Interval Too Small",
        "Min-SE: 6\r\n",
        "",
    )
    .await;

    let (inv2, from) = recv_req(&sock, "INVITE", 5).await.expect("retried INVITE");
    assert_eq!(header_value(&inv2, "session-expires"), "6", "{inv2}");
    assert_eq!(header_value(&inv2, "min-se"), "6", "{inv2}");
    let sdp = format!(
        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n",
        paddr.port() + 1
    );
    uas_reply(
        &sock,
        from,
        &inv2,
        "200 OK",
        "Session-Expires: 6;refresher=uac\r\nRequire: timer\r\nAllow: INVITE, ACK, BYE, UPDATE\r\n",
        &sdp,
    )
    .await;
    state_of(&mut r, &id, CallState::Active).await;
    let (ack, _) = recv_req(&sock, "ACK", 3).await.expect("ACK");
    assert!(ack.contains("ACK "), "{ack}");

    // We are the refresher: UPDATE after half of 6 s.
    let (upd, from) = recv_req(&sock, "UPDATE", 6).await.expect("session refresh");
    assert_eq!(
        header_value(&upd, "session-expires"),
        "6;refresher=uac",
        "{upd}"
    );
    uas_reply(
        &sock,
        from,
        &upd,
        "200 OK",
        "Session-Expires: 6;refresher=uac\r\n",
        "",
    )
    .await;
    let (_upd2, _) = recv_req(&sock, "UPDATE", 6).await.expect("second refresh");
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    r.stack.hangup(&id).await.unwrap();
    r.stack.shutdown().await;
}

#[derive(Default)]
struct Capture(Mutex<Vec<(String, String)>>);

impl WireSink for Capture {
    fn on_wire(&self, account: &str, ev: &WireEvent<'_>) {
        self.0.lock().unwrap().push((
            account.to_string(),
            String::from_utf8_lossy(ev.bytes).into_owned(),
        ));
    }
}

#[tokio::test]
async fn two_stacks_negotiate_and_keep_refreshing() {
    let wire = Arc::new(Capture::default());
    let (tx, rx) = unbounded_channel();
    let stack = SipStack::new(SipStackOptions {
        user_agent_default: "Siple-test/1".into(),
        events: tx,
        wire_sink: wire.clone(),
    });
    stack.set_media_provider(NullMediaProvider::new());
    let mut r = Rig {
        stack,
        events: rx,
        buf: Vec::new(),
        media: NullMediaProvider::new(),
    };
    let mut a = timed("a", 4, 1);
    a.sip_trace = true;
    add(&r, a).await;
    let b = add(&r, timed("b", 4, 1)).await;
    let a_id = r
        .stack
        .dial("a", &format!("sip:b@{b}"), Default::default())
        .await
        .unwrap();
    let b_call = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&b_call.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a_id, CallState::Active).await;
    state_of(&mut r, &b_call.id, CallState::Active).await;
    // 4 s interval, a refreshes at 2, 4, 6 s: still up after 7.5 s, nobody sent a BYE.
    tokio::time::sleep(Duration::from_millis(7500)).await;
    assert_eq!(r.stack.call(&a_id).unwrap().state, CallState::Active);
    assert_eq!(r.stack.call(&b_call.id).unwrap().state, CallState::Active);
    let sent: Vec<String> = wire
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(acct, _)| acct == "a")
        .map(|(_, t)| t.clone())
        .collect();
    let updates = sent.iter().filter(|m| m.starts_with("UPDATE ")).count();
    assert!(updates >= 3, "UPDATE refreshes sent by a: {updates}");
    // Also a hold/unhold re-INVITE counts as a refresh and stays consistent.
    r.stack.hold(&a_id).await.unwrap();
    r.stack.unhold(&a_id).await.unwrap();
    r.stack.hangup(&a_id).await.unwrap();
    r.stack.shutdown().await;
}
