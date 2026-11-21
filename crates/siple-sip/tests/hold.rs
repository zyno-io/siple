//! Hold / unhold. Peer-mode accounts in one stack plus raw-UDP probes.

mod common;

use std::time::Duration;

use common::*;
use siple_config::{HoldScope, HoldStyle};
use siple_sip::{AnswerOptions, CallState, DialOptions, SipError};

fn neg(r: &Rig, id: &str) -> siple_media::sdp_neg::Negotiated {
    r.media
        .stats(id)
        .unwrap()
        .negotiated
        .expect("media negotiated")
}

#[tokio::test]
async fn hold_matrix_sendonly_and_back() {
    let mut r = rig();
    let (a, b) = active_pair(&mut r).await;

    r.stack.hold(&a).await.unwrap();
    let a_info = r.stack.call(&a).unwrap();
    assert_eq!(a_info.state, CallState::Held);
    assert!(a_info.hold.local && !a_info.hold.remote);
    let b_info = state_of(&mut r, &b, CallState::RemoteHeld).await;
    assert!(b_info.hold.remote && !b_info.hold.local);
    // A stopped sending; B answered recvonly, so B must not send either.
    assert!(r.media.stats(&a).unwrap().held);
    let bn = neg(&r, &b);
    assert!(
        bn.remote_hold && !bn.send_enabled && bn.recv_enabled,
        "{bn:?}"
    );
    let an = neg(&r, &a);
    assert!(!an.remote_hold, "the remote only answered recvonly: {an:?}");

    // Idempotent.
    r.stack.hold(&a).await.unwrap();

    r.stack.unhold(&a).await.unwrap();
    assert_eq!(r.stack.call(&a).unwrap().state, CallState::Active);
    let b_info = state_of(&mut r, &b, CallState::Active).await;
    assert!(!b_info.hold.remote);
    assert!(!r.media.stats(&a).unwrap().held);
    let bn = neg(&r, &b);
    assert!(
        bn.send_enabled && bn.recv_enabled && !bn.remote_hold,
        "{bn:?}"
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn hold_emits_call_updated_for_flag_changes() {
    let mut r = rig();
    let (a, b) = active_pair(&mut r).await;
    r.stack.hold(&a).await.unwrap();
    let up = updated(&mut r, &a, |c| c.hold.local).await;
    assert_eq!(up.state, CallState::Held);
    let up = updated(&mut r, &b, |c| c.hold.remote).await;
    assert_eq!(up.state, CallState::RemoteHeld);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn hold_style_inactive() {
    let mut r = rig();
    let mut acfg = peer("a");
    acfg.hold_style = HoldStyle::Inactive;
    add(&r, acfg).await;
    let b_addr = add(&r, peer("b")).await;
    let a = r
        .stack
        .dial("a", &format!("sip:b@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    let bc = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&bc.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a, CallState::Active).await;
    state_of(&mut r, &bc.id, CallState::Active).await;

    r.stack.hold(&a).await.unwrap();
    state_of(&mut r, &bc.id, CallState::RemoteHeld).await;
    let bn = neg(&r, &bc.id);
    assert!(
        bn.remote_hold && !bn.send_enabled && !bn.recv_enabled,
        "{bn:?}"
    );
    r.stack.unhold(&a).await.unwrap();
    state_of(&mut r, &bc.id, CallState::Active).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn both_sides_hold_gives_inactive_then_unhold_in_either_order() {
    let mut r = rig();
    let (a, b) = active_pair(&mut r).await;
    r.stack.hold(&a).await.unwrap();
    state_of(&mut r, &b, CallState::RemoteHeld).await;
    // B holds while A holds it: the offer is inactive.
    r.stack.hold(&b).await.unwrap();
    let bi = r.stack.call(&b).unwrap();
    assert_eq!(bi.state, CallState::Held);
    assert!(bi.hold.local && bi.hold.remote, "{bi:?}");
    let bn = neg(&r, &b);
    assert!(!bn.send_enabled && !bn.recv_enabled, "{bn:?}");
    let ai = r.stack.call(&a).unwrap();
    assert_eq!(ai.state, CallState::Held);
    assert!(ai.hold.local && ai.hold.remote, "{ai:?}");
    let an = neg(&r, &a);
    assert!(!an.send_enabled && !an.recv_enabled, "{an:?}");

    // B lets go first: A still holds, so B is RemoteHeld.
    r.stack.unhold(&b).await.unwrap();
    assert_eq!(r.stack.call(&b).unwrap().state, CallState::RemoteHeld);
    assert_eq!(r.stack.call(&a).unwrap().state, CallState::Held);
    r.stack.unhold(&a).await.unwrap();
    assert_eq!(r.stack.call(&a).unwrap().state, CallState::Active);
    assert_eq!(r.stack.call(&b).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn simultaneous_hold_both_end_up_held() {
    let mut r = rig();
    let (a, b) = active_pair(&mut r).await;
    let (sa, sb) = (r.stack.clone(), r.stack.clone());
    let (a2, b2) = (a.clone(), b.clone());
    let t0 = std::time::Instant::now();
    let (ra, rb) = tokio::join!(async move { sa.hold(&a2).await }, async move {
        sb.hold(&b2).await
    });
    ra.unwrap();
    rb.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(12), "{:?}", t0.elapsed());
    for id in [&a, &b] {
        let i = r.stack.call(id).unwrap();
        assert_eq!(i.state, CallState::Held, "{i:?}");
        assert!(i.hold.local && i.hold.remote, "{i:?}");
    }
    r.stack.shutdown().await;
}

#[tokio::test]
async fn failed_hold_keeps_state_and_media() {
    let mut r = rig();
    add(&r, peer("b")).await;
    let b_addr = r.stack.account_status("b").unwrap().local_addr.unwrap();
    let (raw, id, _tag) = established(&mut r, &b_addr).await;
    let stack = r.stack.clone();
    let id2 = id.clone();
    let h = tokio::spawn(async move { stack.hold(&id2).await });
    let req = raw.request("INVITE", 5).await.expect("re-INVITE");
    assert!(req.contains("a=sendonly"), "{req}");
    raw.reply(&req, "488 Not Acceptable Here", None).await;
    let err = h.await.unwrap().unwrap_err();
    assert!(matches!(err, SipError::Status { code: 488, .. }), "{err:?}");
    let c = r.stack.call(&id).unwrap();
    assert_eq!(c.state, CallState::Active);
    assert!(!c.hold.local);
    assert!(!r.media.stats(&id).unwrap().held);

    // The next attempt works (and starts from the previous SDP, not the failed one).
    let stack = r.stack.clone();
    let id2 = id.clone();
    let h = tokio::spawn(async move { stack.hold(&id2).await });
    let req = raw.request("INVITE", 5).await.expect("second re-INVITE");
    raw.reply(&req, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    h.await.unwrap().unwrap();
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Held);
    assert!(r.media.stats(&id).unwrap().held);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn glare_491_is_retried() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, id, _tag) = established(&mut r, &b_addr).await;
    let stack = r.stack.clone();
    let id2 = id.clone();
    let h = tokio::spawn(async move { stack.hold(&id2).await });
    let req = raw.request("INVITE", 5).await.expect("re-INVITE");
    raw.reply(&req, "491 Request Pending", None).await;
    // Inbound call: we are not the Call-ID owner, so the retry comes within 0-2 s.
    let t0 = std::time::Instant::now();
    let req = raw.request("INVITE", 5).await.expect("retried re-INVITE");
    assert!(
        t0.elapsed() < Duration::from_millis(2600),
        "{:?}",
        t0.elapsed()
    );
    raw.reply(&req, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    h.await.unwrap().unwrap();
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Held);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn glare_gives_up_after_two_retries() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, id, _tag) = established(&mut r, &b_addr).await;
    let stack = r.stack.clone();
    let id2 = id.clone();
    let h = tokio::spawn(async move { stack.hold(&id2).await });
    for _ in 0..3 {
        let req = raw.request("INVITE", 6).await.expect("re-INVITE");
        raw.reply(&req, "491 Request Pending", None).await;
    }
    let err = h.await.unwrap().unwrap_err();
    assert!(matches!(err, SipError::Status { code: 491, .. }), "{err:?}");
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn remote_hold_via_reinvite_and_direction_gating() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, id, tag) = established(&mut r, &b_addr).await;

    // sendonly: we answer recvonly and must not send.
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let ok = raw.final_response("CSeq: 2 INVITE").await;
    assert!(ok.contains("a=recvonly"), "{ok}");
    raw.send(&raw.msg("ACK", Some(&tag), 2, "", "")).await;
    let c = state_of(&mut r, &id, CallState::RemoteHeld).await;
    assert!(c.hold.remote);
    let n = neg(&r, &id);
    assert!(!n.send_enabled && n.recv_enabled && n.remote_hold, "{n:?}");

    // inactive.
    raw.send(&raw.msg("INVITE", Some(&tag), 3, "", &raw.sdp_dir("inactive", 3)))
        .await;
    let ok = raw.final_response("CSeq: 3 INVITE").await;
    assert!(ok.contains("a=inactive"), "{ok}");
    raw.send(&raw.msg("ACK", Some(&tag), 3, "", "")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let n = neg(&r, &id);
    assert!(!n.send_enabled && !n.recv_enabled && n.remote_hold, "{n:?}");

    // back to sendrecv.
    raw.send(&raw.msg("INVITE", Some(&tag), 4, "", &raw.sdp_dir("sendrecv", 4)))
        .await;
    let ok = raw.final_response("CSeq: 4 INVITE").await;
    assert!(ok.contains("a=sendrecv"), "{ok}");
    raw.send(&raw.msg("ACK", Some(&tag), 4, "", "")).await;
    let c = state_of(&mut r, &id, CallState::Active).await;
    assert!(!c.hold.remote);
    let n = neg(&r, &id);
    assert!(n.send_enabled && n.recv_enabled, "{n:?}");

    // c=0.0.0.0 hold (RFC 2543 style).
    let zero = raw
        .sdp_dir("sendrecv", 5)
        .replace("c=IN IP4 127.0.0.1", "c=IN IP4 0.0.0.0");
    raw.send(&raw.msg("INVITE", Some(&tag), 5, "", &zero)).await;
    let _ = raw.final_response("CSeq: 5 INVITE").await;
    raw.send(&raw.msg("ACK", Some(&tag), 5, "", "")).await;
    state_of(&mut r, &id, CallState::RemoteHeld).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn local_hold_while_remote_holds_us_offers_inactive() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, id, tag) = established(&mut r, &b_addr).await;
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let _ = raw.final_response("CSeq: 2 INVITE").await;
    raw.send(&raw.msg("ACK", Some(&tag), 2, "", "")).await;
    state_of(&mut r, &id, CallState::RemoteHeld).await;

    let stack = r.stack.clone();
    let id2 = id.clone();
    let h = tokio::spawn(async move { stack.hold(&id2).await });
    let req = raw.request("INVITE", 5).await.expect("re-INVITE");
    assert!(req.contains("a=inactive"), "{req}");
    raw.reply(&req, "200 OK", Some(&raw.sdp_dir("inactive", 6)))
        .await;
    h.await.unwrap().unwrap();
    let c = r.stack.call(&id).unwrap();
    assert!(c.hold.local && c.hold.remote);
    assert_eq!(c.state, CallState::Held);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn session_refresh_gets_identical_answer() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, _id, tag) = established(&mut r, &b_addr).await;
    let body = |ok: &str| ok.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    let mut answers = Vec::new();
    for cseq in 2..=4u32 {
        // Identical SDP (same o= version) each time: a session refresh.
        raw.send(&raw.msg("INVITE", Some(&tag), cseq, "", &raw.sdp()))
            .await;
        let ok = raw.final_response(&format!("CSeq: {cseq} INVITE")).await;
        assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
        raw.send(&raw.msg("ACK", Some(&tag), cseq, "", "")).await;
        answers.push(body(&ok));
    }
    assert!(answers[0].contains("m=audio"));
    assert_eq!(
        answers[0], answers[1],
        "refresh must get the identical answer"
    );
    assert_eq!(answers[1], answers[2]);
    // UPDATE refresh too.
    raw.send(&raw.msg("UPDATE", Some(&tag), 5, "", &raw.sdp()))
        .await;
    let ok = raw.final_response("CSeq: 5 UPDATE").await;
    assert_eq!(body(&ok), answers[0]);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn auto_hold_second_call_on_same_account() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    add(&r, peer("a")).await;
    // First call: a -> b.
    let a1 = r
        .stack
        .dial("a", &format!("sip:b@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    let b1 = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&b1.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a1, CallState::Active).await;
    state_of(&mut r, &b1.id, CallState::Active).await;

    // Second call on account a: the first one goes on hold before the INVITE.
    let a2 = r
        .stack
        .dial("a", &format!("sip:b@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    assert_eq!(r.stack.call(&a1).unwrap().state, CallState::Held);
    state_of(&mut r, &b1.id, CallState::RemoteHeld).await;
    let b2 = incoming_on(&mut r, "b").await;

    // b answers the second call. Its first call is already held by a (RemoteHeld, not Active),
    // so there is nothing to auto-hold and it must not be touched.
    r.stack
        .answer(&b2.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a2, CallState::Active).await;
    assert_eq!(r.stack.call(&b1.id).unwrap().state, CallState::RemoteHeld);
    assert!(!r.stack.call(&b1.id).unwrap().hold.local);
    assert_eq!(r.stack.call(&b2.id).unwrap().state, CallState::Active);
    assert_eq!(r.stack.call(&a2).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn auto_hold_disabled_and_other_accounts_untouched() {
    let mut r = rig();
    // Relies on the same-account scope: with the default `hold_scope = "all"` b's answer would
    // also hold the x <-> y call and a's calls.
    r.stack.set_hold_scope(HoldScope::Account);
    let mut acfg = peer("a");
    acfg.auto_hold = false;
    add(&r, acfg).await;
    let b_addr = add(&r, peer("b")).await;
    add(&r, peer("x")).await;
    let y_addr = add(&r, peer("y")).await;

    // x <-> y call stays untouched by everything on a / b.
    let x1 = r
        .stack
        .dial("x", &format!("sip:y@{y_addr}"), DialOptions::default())
        .await
        .unwrap();
    let y1 = incoming_on(&mut r, "y").await;
    r.stack
        .answer(&y1.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &x1, CallState::Active).await;

    // a has auto_hold = false: two calls, both stay Active.
    let a1 = r
        .stack
        .dial("a", &format!("sip:b@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    let b1 = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&b1.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a1, CallState::Active).await;
    let a2 = r
        .stack
        .dial("a", &format!("sip:b@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    let b2 = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&b2.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a2, CallState::Active).await;
    // a's own calls were not held by a (auto_hold = false), but b has auto_hold: its first call
    // went on hold when it answered the second one (a's side sees RemoteHeld). The x <-> y call
    // of other accounts is unaffected.
    assert!(!r.stack.call(&a1).unwrap().hold.local);
    assert_eq!(r.stack.call(&a1).unwrap().state, CallState::RemoteHeld);
    assert_eq!(r.stack.call(&b1.id).unwrap().state, CallState::Held);
    assert_eq!(r.stack.call(&a2).unwrap().state, CallState::Active);
    assert_eq!(r.stack.call(&x1).unwrap().state, CallState::Active);
    assert_eq!(r.stack.call(&y1.id).unwrap().state, CallState::Active);
    assert!(!r.media.stats(&x1).unwrap().held);
    r.stack.shutdown().await;
}

/// While our own re-INVITE is outstanding, a re-INVITE from the remote gets 491 (RFC 3261
/// §14.2). A dropped hold() future does not cancel the re-INVITE: it still completes in the
/// background (the flag clears when it does) and the hold takes effect.
#[tokio::test]
async fn incoming_reinvite_during_own_gets_491_and_dropped_hold_still_completes() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, id, tag) = established(&mut r, &b_addr).await;
    let stack = r.stack.clone();
    let id2 = id.clone();
    let h = tokio::spawn(async move { stack.hold(&id2).await });
    let ours = raw.request("INVITE", 5).await.expect("our re-INVITE");
    // Do not answer it yet; send our own re-INVITE.
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let resp = raw.final_response("CSeq: 2 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 491"), "{resp}");
    // Drop the pending hold(): the re-INVITE still runs to completion.
    h.abort();
    let _ = h.await;
    raw.reply(&ours, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    let held = updated(&mut r, &id, |i| i.hold.local).await;
    assert_eq!(held.state, CallState::Held);
    // The flag is clear again: the remote's re-INVITE is answered, and with the direction that
    // matches the hold (sendonly offer from the remote while we hold -> inactive).
    raw.send(&raw.msg("INVITE", Some(&tag), 3, "", &raw.sdp_dir("sendonly", 3)))
        .await;
    let resp = raw.final_response("CSeq: 3 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 200"), "{resp}");
    assert!(resp.contains("a=inactive"), "{resp}");
    r.stack.shutdown().await;
}

/// A successful local resume of a device-bound call gives it the media focus again; a hold does
/// not touch the focus (the media engine moves it off a held call by itself).
#[tokio::test]
async fn resume_of_device_call_sets_media_focus() {
    let mut r = rig();
    let (a, _b) = active_pair(&mut r).await;
    let answered = r.media.stats(&a).unwrap().focus;
    assert_eq!(answered, 1, "answer set the focus once");
    r.stack.hold(&a).await.unwrap();
    assert_eq!(r.media.stats(&a).unwrap().focus, answered);
    r.stack.unhold(&a).await.unwrap();
    assert_eq!(r.media.stats(&a).unwrap().focus, answered + 1);
    // Resuming a call that is not held changes nothing.
    r.stack.unhold(&a).await.unwrap();
    assert_eq!(r.media.stats(&a).unwrap().focus, answered + 1);
    r.stack.shutdown().await;
}
