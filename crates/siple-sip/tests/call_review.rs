//! Regression tests for the call-handling review findings (ended calls pinning the transport,
//! hang-up timing, rebuilds with live calls, Reason handling, CANCEL while Calling, ACK matching,
//! dead dialogs, Contact on 2xx, requests before the ACK, OSRTP fallback, argument bounds).

mod common;

use std::time::Duration;

use common::*;
use siple_config::{DtmfMode, GeneralConfig, MediaConfig, SrtpPolicy};
use siple_sip::{AnswerOptions, CallState, DialOptions, EndedBy, SipError, SipEvent};
use tokio::net::UdpSocket;

/// 1. An ended call holds no runtime / dialog: a fixed-port account rebuilds and takes calls.
#[tokio::test]
async fn ended_call_does_not_pin_the_transport() {
    let mut r = rig();
    let mut bcfg = peer("b");
    bcfg.local_port = free_udp_port();
    let b = add(&r, bcfg.clone()).await;
    let (raw, id, tag) = established(&mut r, &b).await;
    raw.send(&raw.msg("BYE", Some(&tag), 2, "", "")).await;
    let _ = raw.final_response("CSeq: 2 BYE").await;
    state_of(&mut r, &id, CallState::Ended).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    bcfg.public_address = "127.0.0.1".into();
    r.stack.update_account(bcfg).await.unwrap();
    // The rebuilt account must be listening on the same port again.
    let mut answered = false;
    for _ in 0..15 {
        let raw2 = Raw::new(&b).await;
        raw2.send(&raw2.msg("INVITE", None, 1, "", &raw2.sdp()))
            .await;
        if raw2
            .recv_any(1)
            .await
            .is_some_and(|m| m.starts_with("SIP/2.0"))
        {
            answered = true;
            break;
        }
    }
    assert!(
        answered,
        "rebuilt account never accepted a new INVITE on its fixed port"
    );
    r.stack.shutdown().await;
}

/// 2. hangup() ends the call locally at once, even if the peer never answers the BYE; account
/// removal is bounded.
#[tokio::test]
async fn hangup_is_immediate_and_remove_account_is_bounded() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let (_raw, id, _tag) = established(&mut r, &b).await;
    let t0 = std::time::Instant::now();
    r.stack.hangup(&id).await.unwrap();
    assert!(
        // "At once", as opposed to waiting out the BYE transaction (tens of seconds).
        t0.elapsed() < Duration::from_secs(3),
        "{:?}",
        t0.elapsed()
    );
    let c = r.stack.call(&id).unwrap();
    assert_eq!(c.state, CallState::Ended);
    assert_eq!(c.end.unwrap().by, EndedBy::Local);
    assert!(r.media.stats(&id).unwrap().closed);

    // A second call, then remove the account while the peer ignores everything.
    let (_raw2, id2, _tag2) = established(&mut r, &b).await;
    let t0 = std::time::Instant::now();
    r.stack.remove_account("b").await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    assert_eq!(r.stack.call(&id2).unwrap().state, CallState::Ended);
    assert!(r.media.stats(&id2).unwrap().closed);
    r.stack.shutdown().await;
}

/// 3. Disabling an account ends its calls.
#[tokio::test]
async fn disabling_an_account_ends_its_calls() {
    let mut r = rig();
    let mut bcfg = peer("b");
    let b = add(&r, bcfg.clone()).await;
    let (_raw, id, _tag) = established(&mut r, &b).await;
    bcfg.enabled = false;
    r.stack.update_account(bcfg).await.unwrap();
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Ended);
    assert!(r.media.stats(&id).unwrap().closed);
    r.stack.shutdown().await;
}

/// 3. A forced rebuild (reconfig_force) ends the calls instead of dropping the runtime under
/// them.
#[tokio::test]
async fn forced_rebuild_ends_calls() {
    let mut r = rig();
    let general = GeneralConfig {
        reconfig_force: true,
        ..Default::default()
    };
    let mut bcfg = peer("b");
    r.stack
        .add_account(bcfg.clone(), &general, &MediaConfig::default())
        .await
        .unwrap();
    let b = r.stack.account_status("b").unwrap().local_addr.unwrap();
    let (_raw, id, _tag) = established(&mut r, &b).await;
    bcfg.public_address = "127.0.0.2".into();
    r.stack.update_account(bcfg).await.unwrap();
    let c = r.stack.call(&id).unwrap();
    assert_eq!(c.state, CallState::Ended, "{c:?}");
    assert!(r.media.stats(&id).unwrap().closed);
    r.stack.shutdown().await;
}

/// 4. `Reason: Q.850;cause=16` on a BYE is not the call's SIP code.
#[tokio::test]
async fn q850_reason_on_bye_keeps_code_200() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let (raw, id, tag) = established(&mut r, &b).await;
    raw.send(&raw.msg(
        "BYE",
        Some(&tag),
        2,
        "Reason: Q.850;cause=16;text=\"NORMAL_CLEARING\"\r\n",
        "",
    ))
    .await;
    let end = state_of(&mut r, &id, CallState::Ended).await.end.unwrap();
    assert_eq!(end.code, 200);
    assert_eq!(end.q850_cause, Some(16));
    assert_eq!(end.reason, "NORMAL_CLEARING");
    assert_eq!(end.by, EndedBy::Remote);

    // A SIP cause is the code.
    let (raw, id, tag) = established(&mut r, &b).await;
    raw.send(&raw.msg(
        "BYE",
        Some(&tag),
        2,
        "Reason: SIP;cause=480;text=\"Temporarily Unavailable\"\r\n",
        "",
    ))
    .await;
    let end = state_of(&mut r, &id, CallState::Ended).await.end.unwrap();
    assert_eq!((end.code, end.q850_cause), (480, None));
    r.stack.shutdown().await;
}

/// 5. CANCEL `Reason: SIP;cause=200` is "answered elsewhere", not a missed call.
#[tokio::test]
async fn cancel_reason_completed_elsewhere_is_not_missed() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let cancel_of = |raw: &Raw, reason: &str| {
        raw.msg("CANCEL", None, 1, reason, "")
            .replace("z9hG4bKcancel1", "z9hG4bKinvite1")
    };
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    raw.send(&cancel_of(
        &raw,
        "Reason: SIP;cause=200;text=\"Call completed elsewhere\"\r\n",
    ))
    .await;
    let end = state_of(&mut r, &call.id, CallState::Ended)
        .await
        .end
        .unwrap();
    assert_eq!(end.code, 487);
    assert!(!end.missed, "{end:?}");
    assert_eq!(end.reason, "Call completed elsewhere");

    // Without a Reason it is a missed call.
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    raw.send(&cancel_of(&raw, "")).await;
    let end = state_of(&mut r, &call.id, CallState::Ended)
        .await
        .end
        .unwrap();
    assert!(end.missed && end.code == 487, "{end:?}");
    r.stack.shutdown().await;
}

fn callee_response_base(invite: &str, caddr: std::net::SocketAddr) -> String {
    format!(
        "{}\r\n{}\r\n{};tag=callee1\r\n{}\r\n{}\r\nContact: <sip:x@{caddr}>\r\n",
        hdr(invite, "via").join("\r\n"),
        hdr(invite, "from")[0],
        hdr(invite, "to")[0],
        hdr(invite, "call-id")[0],
        hdr(invite, "cseq")[0]
    )
}

async fn collect(callee: &UdpSocket, secs: u64) -> Vec<String> {
    let mut buf = vec![0u8; 8192];
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while let Ok(Ok((n, _))) = tokio::time::timeout_at(deadline, callee.recv_from(&mut buf)).await {
        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
        got.push(s.lines().next().unwrap_or("").to_string());
    }
    got
}

/// 6. Hanging up before any provisional response: the call ends locally at once, a late 180
/// still gets a CANCEL.
#[tokio::test]
async fn hangup_while_calling_cancels_after_late_provisional() {
    let r = rig();
    add(&r, peer("a")).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let (n, from) = callee.recv_from(&mut buf).await.unwrap();
    let invite = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(invite.starts_with("INVITE"));
    let t0 = std::time::Instant::now();
    r.stack.hangup(&id).await.unwrap();
    assert!(t0.elapsed() < Duration::from_millis(500));
    let c = r.stack.call(&id).unwrap();
    assert_eq!(c.state, CallState::Ended);
    assert_eq!(c.end.unwrap().code, 487);

    let base = callee_response_base(&invite, caddr);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let ringing = format!("SIP/2.0 180 Ringing\r\n{base}Content-Length: 0\r\n\r\n");
    callee.send_to(ringing.as_bytes(), from).await.unwrap();
    let got = collect(&callee, 2).await;
    assert!(
        got.iter().any(|l| l.starts_with("CANCEL")),
        "no CANCEL after the late 180: {got:?}"
    );
    r.stack.shutdown().await;
}

/// 6. ... and a late 200 gets ACK + BYE.
#[tokio::test]
async fn hangup_while_calling_acks_and_byes_late_200() {
    let r = rig();
    add(&r, peer("a")).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let (n, from) = callee.recv_from(&mut buf).await.unwrap();
    let invite = String::from_utf8_lossy(&buf[..n]).into_owned();
    r.stack.hangup(&id).await.unwrap();
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Ended);

    let base = callee_response_base(&invite, caddr);
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
    let ok = format!(
        "SIP/2.0 200 OK\r\n{base}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    callee.send_to(ok.as_bytes(), from).await.unwrap();
    let got = collect(&callee, 2).await;
    assert!(got.iter().any(|l| l.starts_with("ACK")), "no ACK: {got:?}");
    assert!(got.iter().any(|l| l.starts_with("BYE")), "no BYE: {got:?}");
    r.stack.shutdown().await;
}

/// 7. A stale ACK (an older one, re-notified by an INFO) is not taken for the answer to an
/// offer-less re-INVITE.
#[tokio::test]
async fn info_between_offerless_reinvite_and_its_ack_does_not_kill_the_call() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let (raw, id, tag) = established(&mut r, &b).await;
    raw.send(&raw.msg("INVITE", Some(&tag), 5, "", "")).await;
    let ok = raw.final_response("CSeq: 5 INVITE").await;
    assert!(
        ok.starts_with("SIP/2.0 200") && ok.contains("m=audio"),
        "{ok}"
    );
    let body = "Signal=1\r\nDuration=100\r\n";
    let info = raw
        .msg(
            "INFO",
            Some(&tag),
            6,
            "Content-Type: application/dtmf-relay\r\n",
            "",
        )
        .replace(
            "Content-Length: 0\r\n\r\n",
            &format!("Content-Length: {}\r\n\r\n{body}", body.len()),
        );
    raw.send(&info).await;
    let ok_info = raw.final_response("CSeq: 6 INFO").await;
    assert!(ok_info.starts_with("SIP/2.0 200"), "{ok_info}");
    assert!(raw.request("BYE", 1).await.is_none(), "the stack hung up");
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    // The real ACK with the answer still completes the exchange.
    raw.send(&raw.msg("ACK", Some(&tag), 5, "", &raw.sdp_dir("sendrecv", 7)))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

/// 8. 481 to our in-dialog INFO ends the call.
#[tokio::test]
async fn info_answered_481_ends_the_call() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let (raw, id, _tag) = established(&mut r, &b).await;
    let stack = r.stack.clone();
    let id2 = id.clone();
    let h =
        tokio::spawn(async move { stack.send_dtmf(&id2, "5", Some(DtmfMode::Info), None).await });
    let info = raw.request("INFO", 5).await.expect("INFO");
    raw.reply(&info, "481 Call/Transaction Does Not Exist", None)
        .await;
    let res = h.await.unwrap();
    assert!(res.is_err());
    let c = state_of(&mut r, &id, CallState::Ended).await;
    let end = c.end.unwrap();
    assert_eq!((end.code, end.by), (481, EndedBy::Remote));
    r.stack.shutdown().await;
}

/// 9. A 2xx to an early-dialog UPDATE (sent straight on the transaction) carries a Contact.
#[tokio::test]
async fn early_update_200_has_contact() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let mut ring = raw.response("CSeq: 1 INVITE").await;
    if ring.starts_with("SIP/2.0 100") {
        ring = raw.response("CSeq: 1 INVITE").await;
    }
    let tag = to_tag(&ring);
    let _ = incoming(&mut r).await;
    raw.send(&raw.msg("UPDATE", Some(&tag), 2, "", &raw.sdp()))
        .await;
    let ok = raw.final_response("CSeq: 2 UPDATE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    assert!(!hdr(&ok, "contact").is_empty(), "no Contact in {ok}");
    r.stack.shutdown().await;
}

/// 10. send_dtmf (INFO) / hold before the ACK wait, then fail with a clear error (not Timeout).
#[tokio::test]
async fn requests_before_the_ack_return_invalid_state() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let (s1, s2) = (r.stack.clone(), r.stack.clone());
    let (i1, i2) = (call.id.clone(), call.id.clone());
    let (d, h) = tokio::join!(
        async move { s1.send_dtmf(&i1, "5", Some(DtmfMode::Info), None).await },
        async move { s2.hold(&i2).await },
    );
    for res in [d, h] {
        assert!(matches!(res, Err(SipError::CallState { .. })), "{res:?}");
    }
    r.stack.shutdown().await;
}

/// 10. ... but when the ACK arrives within the wait, the request goes out.
#[tokio::test]
async fn hold_right_after_answer_waits_for_the_ack() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    let tag = to_tag(&ok);
    let stack = r.stack.clone();
    let id = call.id.clone();
    let h = tokio::spawn(async move { stack.hold(&id).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    let req = raw
        .request("INVITE", 5)
        .await
        .expect("re-INVITE after the ACK");
    raw.reply(&req, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    h.await.unwrap().unwrap();
    assert_eq!(r.stack.call(&call.id).unwrap().state, CallState::Held);
    r.stack.shutdown().await;
}

/// 12. answer() after a CANCEL fails and never reports Active.
#[tokio::test]
async fn answer_after_cancel_is_invalid_state_without_active() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    let cancel = raw
        .msg("CANCEL", None, 1, "", "")
        .replace("z9hG4bKcancel1", "z9hG4bKinvite1");
    raw.send(&cancel).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let res = r.stack.answer(&call.id, AnswerOptions::default()).await;
    assert!(matches!(res, Err(SipError::CallState { .. })), "{res:?}");
    state_of(&mut r, &call.id, CallState::Ended).await;
    let active = r.buf.iter().any(|e| {
        matches!(e, SipEvent::CallState { call: c } if c.id == call.id && c.state == CallState::Active)
    });
    assert!(!active, "Active was reported for a cancelled call");
    r.stack.shutdown().await;
}

/// 13. `srtp = optional` refused with 488: one retry with a plain RTP/AVP offer.
#[tokio::test]
async fn srtp_optional_488_retries_once_with_plain_rtp() {
    let mut r = rig();
    let mut acfg = peer("a");
    acfg.srtp = SrtpPolicy::Optional;
    add(&r, acfg).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let (n, from) = callee.recv_from(&mut buf).await.unwrap();
    let invite1 = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(
        invite1.contains("a=crypto"),
        "the first offer must carry crypto"
    );
    let base = callee_response_base(&invite1, caddr);
    let resp = format!("SIP/2.0 488 Not Acceptable Here\r\n{base}Content-Length: 0\r\n\r\n");
    callee.send_to(resp.as_bytes(), from).await.unwrap();

    // The retry: a new INVITE (new Call-ID) without crypto.
    let invite2 = loop {
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), callee.recv_from(&mut buf))
            .await
            .expect("no retry INVITE")
            .unwrap();
        let m = String::from_utf8_lossy(&buf[..n]).into_owned();
        if m.starts_with("INVITE") {
            break m;
        }
    };
    assert!(!invite2.contains("a=crypto"), "{invite2}");
    assert!(invite2.contains("RTP/AVP"));
    assert_ne!(hdr(&invite1, "call-id"), hdr(&invite2, "call-id"));
    let base = callee_response_base(&invite2, caddr);
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
    let ok = format!(
        "SIP/2.0 200 OK\r\n{base}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    callee.send_to(ok.as_bytes(), from).await.unwrap();
    let c = state_of(&mut r, &id, CallState::Active).await;
    assert!(c.srtp_fallback);
    assert!(c.negotiated.unwrap().srtp_suite.is_none());
    r.stack.shutdown().await;
}

async fn next_invite(callee: &UdpSocket, needle: &str) -> (String, std::net::SocketAddr) {
    let mut buf = vec![0u8; 8192];
    loop {
        let (n, from) = tokio::time::timeout(Duration::from_secs(5), callee.recv_from(&mut buf))
            .await
            .expect("no INVITE")
            .unwrap();
        let m = String::from_utf8_lossy(&buf[..n]).into_owned();
        if m.starts_with("INVITE") && m.contains(needle) {
            return (m, from);
        }
    }
}

async fn reply_488(callee: &UdpSocket, from: std::net::SocketAddr, invite: &str) {
    let caddr = callee.local_addr().unwrap();
    let base = callee_response_base(invite, caddr);
    let resp = format!("SIP/2.0 488 Not Acceptable Here\r\n{base}Content-Length: 0\r\n\r\n");
    callee.send_to(resp.as_bytes(), from).await.unwrap();
}

async fn reply_200(callee: &UdpSocket, from: std::net::SocketAddr, invite: &str) {
    let caddr = callee.local_addr().unwrap();
    let base = callee_response_base(invite, caddr);
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
    let ok = format!(
        "SIP/2.0 200 OK\r\n{base}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    callee.send_to(ok.as_bytes(), from).await.unwrap();
}

async fn downgraded(r: &Rig, want: &[String]) -> Vec<String> {
    let mut cur = Vec::new();
    for _ in 0..60 {
        cur = r
            .stack
            .account_status("a")
            .unwrap()
            .srtp_optional_downgraded;
        if cur == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cur
}

/// A 488 that is not about crypto (codec mismatch): the plain retry is refused too, so nothing
/// is remembered.
#[tokio::test]
async fn srtp_optional_488_then_plain_488_remembers_nothing() {
    let mut r = rig();
    let mut acfg = peer("a");
    acfg.srtp = SrtpPolicy::Optional;
    add(&r, acfg).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee, "sip:x@").await;
    assert!(i1.contains("a=crypto"));
    reply_488(&callee, from, &i1).await;
    let (i2, from) = next_invite(&callee, "sip:x@").await;
    assert!(!i2.contains("a=crypto"));
    reply_488(&callee, from, &i2).await;
    state_of(&mut r, &id, CallState::Ended).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        r.stack
            .account_status("a")
            .unwrap()
            .srtp_optional_downgraded
            .is_empty()
    );
    r.stack.shutdown().await;
}

/// The refusal is remembered per destination once the plain retry is accepted; other
/// destinations still get OSRTP; a remembered destination that later refuses plain gets one
/// OSRTP retry and its entry is dropped on success.
#[tokio::test]
async fn srtp_optional_refusal_is_remembered_per_destination() {
    let mut r = rig();
    let mut acfg = peer("a");
    acfg.srtp = SrtpPolicy::Optional;
    add(&r, acfg).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let oaddr = other.local_addr().unwrap();
    assert!(
        r.stack
            .account_status("a")
            .unwrap()
            .srtp_optional_downgraded
            .is_empty()
    );

    // Refusal alone is not enough: nothing is remembered before the retry is accepted.
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee, "sip:x@").await;
    assert!(i1.contains("a=crypto"));
    reply_488(&callee, from, &i1).await;
    let (i2, from) = next_invite(&callee, "sip:x@").await;
    assert!(!i2.contains("a=crypto"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        r.stack
            .account_status("a")
            .unwrap()
            .srtp_optional_downgraded
            .is_empty()
    );
    reply_200(&callee, from, &i2).await;
    state_of(&mut r, &id, CallState::Active).await;
    assert_eq!(
        downgraded(&r, &[caddr.to_string()]).await,
        [caddr.to_string()]
    );

    // Same destination: plain straight away.
    r.stack
        .dial("a", &format!("sip:y@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (iy, fromy) = next_invite(&callee, "sip:y@").await;
    assert!(!iy.contains("a=crypto"), "{iy}");
    assert!(iy.contains("RTP/AVP"));

    // Another destination still gets the OSRTP offer first.
    r.stack
        .dial("a", &format!("sip:z@{oaddr}"), DialOptions::default())
        .await
        .unwrap();
    let (iz, _) = next_invite(&other, "sip:z@").await;
    assert!(iz.contains("a=crypto"), "{iz}");

    // The remembered destination refuses plain: one OSRTP retry; success drops the entry.
    reply_488(&callee, fromy, &iy).await;
    let (iy2, fromy2) = next_invite(&callee, "sip:y@").await;
    assert!(iy2.contains("a=crypto"), "{iy2}");
    reply_200(&callee, fromy2, &iy2).await;
    assert!(downgraded(&r, &[]).await.is_empty());
    r.stack.shutdown().await;
}

/// A change of the transport (like srtp / registrar / codecs ...) clears the memory.
#[tokio::test]
async fn srtp_optional_memory_cleared_by_config_change() {
    let mut r = rig();
    let mut acfg = peer("a");
    acfg.srtp = SrtpPolicy::Optional;
    add(&r, acfg.clone()).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee, "sip:x@").await;
    reply_488(&callee, from, &i1).await;
    let (i2, from) = next_invite(&callee, "sip:x@").await;
    reply_200(&callee, from, &i2).await;
    state_of(&mut r, &id, CallState::Active).await;
    assert_eq!(downgraded(&r, &[caddr.to_string()]).await.len(), 1);
    let mut changed = acfg;
    changed.transport = siple_config::Transport::Tcp;
    r.stack.update_account(changed).await.unwrap();
    assert!(
        r.stack
            .account_status("a")
            .unwrap()
            .srtp_optional_downgraded
            .is_empty()
    );
    r.stack.shutdown().await;
}

/// 13. No retry unless the policy is `optional`.
#[tokio::test]
async fn srtp_off_488_is_final() {
    let mut r = rig();
    let mut acfg = peer("a");
    acfg.srtp = SrtpPolicy::Off;
    add(&r, acfg).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let (n, from) = callee.recv_from(&mut buf).await.unwrap();
    let invite = String::from_utf8_lossy(&buf[..n]).into_owned();
    let base = callee_response_base(&invite, caddr);
    let resp = format!("SIP/2.0 488 Not Acceptable Here\r\n{base}Content-Length: 0\r\n\r\n");
    callee.send_to(resp.as_bytes(), from).await.unwrap();
    let end = state_of(&mut r, &id, CallState::Ended).await.end.unwrap();
    assert_eq!(end.code, 488);
    assert!(!r.stack.call(&id).unwrap().srtp_fallback);
    r.stack.shutdown().await;
}

/// B. send_dtmf bounds (no overflow panic).
#[tokio::test]
async fn send_dtmf_arguments_are_bounded() {
    let mut r = rig();
    let (a, _b) = active_pair(&mut r).await;
    for d in [0, 9, 10_001, u32::MAX] {
        let res = r
            .stack
            .send_dtmf(&a, "1", Some(DtmfMode::Info), Some(d))
            .await;
        assert!(
            matches!(res, Err(SipError::InvalidArgument(_))),
            "{d}: {res:?}"
        );
    }
    let long = "1".repeat(33);
    for digits in ["", "x", "12e", long.as_str()] {
        let res = r.stack.send_dtmf(&a, digits, None, None).await;
        assert!(
            matches!(res, Err(SipError::InvalidArgument(_))),
            "{digits:?}: {res:?}"
        );
    }
    r.stack
        .send_dtmf(&a, "1", Some(DtmfMode::Rfc4733), Some(10_000))
        .await
        .unwrap();
    r.stack.shutdown().await;
}

/// A. `set_media` reaches existing accounts: the next call offers the new codec list.
#[tokio::test]
async fn set_media_updates_codecs_for_new_calls() {
    let r = rig();
    add(&r, peer("a")).await;
    r.stack.set_media(&MediaConfig {
        codecs: vec![siple_config::Codec::Pcma],
        ..Default::default()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let _ = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let (n, _) = callee.recv_from(&mut buf).await.unwrap();
    let invite = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(invite.contains("PCMA/8000"), "{invite}");
    assert!(!invite.contains("opus"), "{invite}");
    r.stack.shutdown().await;
}
