//! Re-INVITEs that arrive before the ACK of the initial INVITE, and in-dialog requests that
//! time out or are refused (RFC 3261 §12.2.1.2, §14.2). A raw-UDP probe plays the peer.
mod common;

use std::time::{Duration, Instant};

use common::*;
use siple_sip::{AnswerOptions, CallState, SipEvent};

/// Our 200 to the initial INVITE is answered by a re-INVITE (CSeq 2) before any ACK: the
/// re-INVITE gets its own answer at once instead of waiting in the dialog layer.
#[tokio::test]
async fn reinvite_before_ack_is_answered() {
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
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    let tag = to_tag(&ok);

    // The ACK of the initial INVITE is held back.
    let t = Instant::now();
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let ok2 = raw.final_response("CSeq: 2 INVITE").await;
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(ok2.starts_with("SIP/2.0 200"), "{ok2}");
    assert!(ok2.contains("m=audio"), "answer SDP expected: {ok2}");

    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    raw.send(&raw.msg("ACK", Some(&tag), 2, "", "")).await;
    updated(&mut r, &call.id, |i| i.hold.remote).await;
    // Both ACKs consumed: the call stays up (no ACK timeout, no BYE).
    assert!(
        raw.request("BYE", 1).await.is_none(),
        "unexpected BYE from the stack"
    );
    assert_eq!(r.stack.call(&call.id).unwrap().state, CallState::RemoteHeld);
    // The call is in a sane state: a later re-INVITE is answered, not refused with a 491.
    raw.send(&raw.msg("INVITE", Some(&tag), 3, "", &raw.sdp_dir("sendrecv", 3)))
        .await;
    let ok3 = raw.final_response("CSeq: 3 INVITE").await;
    assert!(ok3.starts_with("SIP/2.0 200"), "{ok3}");
    raw.send(&raw.msg("ACK", Some(&tag), 3, "", "")).await;
    updated(&mut r, &call.id, |i| !i.hold.remote).await;
    r.stack.shutdown().await;
}

/// Our 200 carries the offer (offer-less INVITE): the answer is in the ACK, so a re-INVITE
/// before that ACK gets a 491 with Retry-After (RFC 3261 §14.2) and the call still completes.
#[tokio::test]
async fn reinvite_before_late_offer_ack_gets_491() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", "")).await;
    let call = incoming(&mut r).await;
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.contains("m=audio"), "offer expected in 200: {ok}");
    let tag = to_tag(&ok);

    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let busy = raw.final_response("CSeq: 2 INVITE").await;
    assert!(busy.starts_with("SIP/2.0 491"), "{busy}");
    assert!(!hdr(&busy, "retry-after").is_empty(), "{busy}");
    // ACK of the 491 (same branch as the INVITE).
    raw.send(
        &raw.msg("ACK", Some(&tag), 2, "", "")
            .replace("branch=z9hG4bKack2", "branch=z9hG4bKinvite2"),
    )
    .await;

    // The answer in the ACK completes the negotiation.
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", &raw.sdp()))
        .await;
    until(
        &mut r,
        "media",
        |e| matches!(e, SipEvent::CallMedia { call_id, .. } if *call_id == call.id),
    )
    .await;
    assert_eq!(r.stack.call(&call.id).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

/// A 408 to our hold re-INVITE ends the dialog: the call ends and a best-effort BYE goes out.
#[tokio::test]
async fn reinvite_408_ends_the_call_with_a_bye() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let (raw, id, _tag) = established(&mut r, &b).await;
    let stack = r.stack.clone();
    let cid = id.clone();
    let h = tokio::spawn(async move { stack.hold(&cid).await });
    let inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    raw.reply(&inv, "408 Request Timeout", None).await;
    assert!(h.await.unwrap().is_err());
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().code, 408);
    let bye = raw.request("BYE", 5).await.expect("BYE after the 408");
    raw.ok_to(&bye).await;
    r.stack.shutdown().await;
}

/// A 481 means the peer has no such dialog: the call ends, and no BYE is sent.
#[tokio::test]
async fn reinvite_481_ends_the_call_without_a_bye() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let (raw, id, _tag) = established(&mut r, &b).await;
    let stack = r.stack.clone();
    let cid = id.clone();
    let h = tokio::spawn(async move { stack.hold(&cid).await });
    let inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    raw.reply(&inv, "481 Call/Transaction Does Not Exist", None)
        .await;
    assert!(h.await.unwrap().is_err());
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().code, 481);
    assert!(raw.request("BYE", 1).await.is_none(), "BYE after a 481");
    r.stack.shutdown().await;
}

/// A peer that answers the re-INVITE with a 100 Trying and then goes silent (rsipstack stops
/// Timer B at the 100) cannot hold the request forever: the overall bound ends it like a
/// Timer B timeout (408 -> BYE), and a new hold is possible afterwards only on a live call.
#[tokio::test]
async fn silent_peer_after_100_trying_is_bounded() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let (raw, id, _tag) = established(&mut r, &b).await;
    let stack = r.stack.clone();
    let cid = id.clone();
    let h = tokio::spawn(async move { stack.hold(&cid).await });
    let inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    raw.reply(&inv, "100 Trying", None).await;
    // Let the 100 reach the transaction, then jump over the bound (34 s) in virtual time.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::pause();
    let res = tokio::time::timeout(Duration::from_secs(120), h)
        .await
        .expect("hold must return within the bound")
        .unwrap();
    tokio::time::resume();
    assert!(res.is_err(), "{res:?}");
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().code, 408);
    assert!(raw.request("BYE", 5).await.is_some(), "BYE after the bound");
    r.stack.shutdown().await;
}

/// An offer-less re-INVITE before the ACK gets a 491 (its ACK would not be routed back to us).
/// After the ACK an offer-less re-INVITE works: 200 with our offer, the answer in its ACK is
/// applied, and a following re-INVITE is not refused.
#[tokio::test]
async fn offerless_reinvite_before_ack_gets_491_then_works_after() {
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

    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", "")).await;
    let busy = raw.final_response("CSeq: 2 INVITE").await;
    assert!(busy.starts_with("SIP/2.0 491"), "{busy}");
    assert!(!hdr(&busy, "retry-after").is_empty(), "{busy}");
    raw.send(
        &raw.msg("ACK", Some(&tag), 2, "", "")
            .replace("branch=z9hG4bKack2", "branch=z9hG4bKinvite2"),
    )
    .await;
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;

    // Confirmed now: offer-less re-INVITE -> 200 with our offer, the answer in the ACK.
    raw.send(&raw.msg("INVITE", Some(&tag), 3, "", "")).await;
    let ok3 = raw.final_response("CSeq: 3 INVITE").await;
    assert!(ok3.starts_with("SIP/2.0 200"), "{ok3}");
    assert!(ok3.contains("m=audio"), "offer expected in 200: {ok3}");
    raw.send(&raw.msg("ACK", Some(&tag), 3, "", &raw.sdp_dir("sendonly", 3)))
        .await;
    updated(&mut r, &call.id, |i| i.hold.remote).await;
    // Not stuck waiting for an answer: the next re-INVITE is answered.
    raw.send(&raw.msg("INVITE", Some(&tag), 4, "", &raw.sdp_dir("sendrecv", 4)))
        .await;
    let ok4 = raw.final_response("CSeq: 4 INVITE").await;
    assert!(ok4.starts_with("SIP/2.0 200"), "{ok4}");
    raw.send(&raw.msg("ACK", Some(&tag), 4, "", "")).await;
    r.stack.shutdown().await;
}

/// RFC 3261 §14.2: a re-INVITE while the initial INVITE is still unanswered gets a 500 with
/// Retry-After; the pending call keeps its original offer and can be answered normally.
#[tokio::test]
async fn reinvite_while_ringing_gets_500() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let ring = loop {
        let m = raw.response("CSeq: 1 INVITE").await;
        if !m.starts_with("SIP/2.0 100") {
            break m;
        }
    };
    assert!(ring.starts_with("SIP/2.0 180"), "{ring}");
    let tag = to_tag(&ring);
    let call = incoming(&mut r).await;

    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let err = raw.final_response("CSeq: 2 INVITE").await;
    assert!(err.starts_with("SIP/2.0 500"), "{err}");
    assert!(!hdr(&err, "retry-after").is_empty(), "{err}");
    raw.send(
        &raw.msg("ACK", Some(&tag), 2, "", "")
            .replace("branch=z9hG4bKack2", "branch=z9hG4bKinvite2"),
    )
    .await;

    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    assert!(
        ok.contains("a=sendrecv") && !ok.contains("a=recvonly"),
        "answer to the original offer expected: {ok}"
    );
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    state_of(&mut r, &call.id, CallState::Active).await;
    assert!(!r.stack.call(&call.id).unwrap().hold.remote);
    r.stack.shutdown().await;
}
