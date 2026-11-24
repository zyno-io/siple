//! Multi-line engine: `hold_scope`, auto-hold exemptions, `activate`, answer modes and end
//! causes. Peer-mode accounts in one stack plus raw-UDP probes.

mod common;

use std::time::Duration;

use common::*;
use siple_config::{AudioSink, AudioSource, HoldScope};
use siple_sip::{CallState, DialOptions, EndCause, SipEvent};
use tokio::net::UdpSocket;

/// A raw probe calls account `name` (added with `cfg`); returns the probe and the active call.
async fn raw_call(r: &mut Rig, cfg: siple_config::AccountConfig) -> (Raw, String, String) {
    let addr = add(r, cfg).await;
    let (raw, id, tag) = established(r, &addr).await;
    (raw, id, tag)
}

/// Answers the next re-INVITE of the stack as a hold (`recvonly`).
async fn accept_hold(raw: &Raw) {
    let req = raw.request("INVITE", 8).await.expect("hold re-INVITE");
    raw.reply(&req, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
}

async fn no_reinvite(raw: &Raw) {
    assert!(
        raw.request("INVITE", 1).await.is_none(),
        "the call must not be put on hold"
    );
}

/// Account `a` dials account `c` (a self-call of the process); returns `(a call, c address)`.
async fn dial_c(r: &Rig, c_addr: &str) -> siple_sip::CallId {
    r.stack
        .dial("a", &format!("sip:c@{c_addr}"), DialOptions::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn hold_scope_all_holds_other_accounts_calls() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let (raw, x1, _) = raw_call(&mut r, peer("x")).await;
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    let stack = r.stack.clone();
    let dial = tokio::spawn(async move {
        stack
            .dial("a", &format!("sip:c@{c_addr}"), DialOptions::default())
            .await
    });
    accept_hold(&raw).await;
    dial.await.unwrap().unwrap();
    let x = r.stack.call(&x1).unwrap();
    assert_eq!(x.state, CallState::Held, "another account's call is held");
    assert!(x.hold.local);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn hold_scope_account_leaves_other_accounts_alone() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::Account);
    let (raw, x1, _) = raw_call(&mut r, peer("x")).await;
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    dial_c(&r, &c_addr).await;
    no_reinvite(&raw).await;
    assert_eq!(r.stack.call(&x1).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn hold_scope_is_hot() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::Account);
    let (raw, x1, _) = raw_call(&mut r, peer("x")).await;
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    dial_c(&r, &c_addr).await;
    no_reinvite(&raw).await;
    r.stack.set_hold_scope(HoldScope::All);
    let stack = r.stack.clone();
    let dial = tokio::spawn(async move {
        stack
            .dial("a", &format!("sip:c@{c_addr}"), DialOptions::default())
            .await
    });
    accept_hold(&raw).await;
    dial.await.unwrap().unwrap();
    assert_eq!(r.stack.call(&x1).unwrap().state, CallState::Held);
    r.stack.shutdown().await;
}

/// Two calls of this process with the same SIP Call-ID are the two legs of a self-call: with
/// `hold_scope = "all"` only the dialer's leg is held, the other leg goes `RemoteHeld` by itself
/// and is never held locally.
#[tokio::test]
async fn self_call_other_leg_is_exempt() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let (a1, b1) = active_pair(&mut r).await;
    assert_eq!(
        r.stack.call(&a1).unwrap().sip_call_id,
        r.stack.call(&b1).unwrap().sip_call_id
    );
    let b_addr = r.stack.account_status("b").unwrap().local_addr.unwrap();
    let a2 = r
        .stack
        .dial("a", &format!("sip:b@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    assert_eq!(r.stack.call(&a1).unwrap().state, CallState::Held);
    let b1_info = state_of(&mut r, &b1, CallState::RemoteHeld).await;
    assert!(!b1_info.hold.local, "the other leg is not held locally");
    // b answers the second call: b1 (RemoteHeld, peer leg already held) stays untouched.
    let b2 = incoming_on(&mut r, "b").await;
    r.stack.answer(&b2.id, Default::default()).await.unwrap();
    state_of(&mut r, &a2, CallState::Active).await;
    let b1_info = r.stack.call(&b1).unwrap();
    assert_eq!(b1_info.state, CallState::RemoteHeld);
    assert!(!b1_info.hold.local);
    r.stack.shutdown().await;
}

/// Calls that use no audio device (agent / automation) are never auto-held.
#[tokio::test]
async fn non_device_calls_are_never_auto_held() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let mut x = peer("x");
    x.audio_source = AudioSource::Silence;
    x.audio_sink = AudioSink::Null;
    let (raw, x1, _) = raw_call(&mut r, x).await;
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    dial_c(&r, &c_addr).await;
    no_reinvite(&raw).await;
    assert_eq!(r.stack.call(&x1).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

/// A call the remote has put on hold is auto-holdable too (otherwise a remote resume would
/// leave two live lines).
#[tokio::test]
async fn remote_held_call_is_auto_held() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let (raw, x1, tag) = raw_call(&mut r, peer("x")).await;
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let ok = raw.final_response("CSeq: 2 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    raw.send(&raw.msg("ACK", Some(&tag), 2, "", "")).await;
    state_of(&mut r, &x1, CallState::RemoteHeld).await;
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    let stack = r.stack.clone();
    let dial = tokio::spawn(async move {
        stack
            .dial("a", &format!("sip:c@{c_addr}"), DialOptions::default())
            .await
    });
    accept_hold(&raw).await;
    dial.await.unwrap().unwrap();
    let x = r.stack.call(&x1).unwrap();
    assert!(x.hold.local && x.hold.remote, "{x:?}");
    assert_eq!(x.state, CallState::Held);
    r.stack.shutdown().await;
}

/// `auto_hold = false` exempts an account's calls from being held, whatever the scope.
#[tokio::test]
async fn per_account_auto_hold_false_exempts_its_calls() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let mut x = peer("x");
    x.auto_hold = false;
    let (raw, x1, _) = raw_call(&mut r, x).await;
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    dial_c(&r, &c_addr).await;
    no_reinvite(&raw).await;
    assert_eq!(r.stack.call(&x1).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

/// ... and stops it from triggering a hold of other accounts' calls.
#[tokio::test]
async fn per_account_auto_hold_false_does_not_trigger_holds() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let (raw, y1, _) = raw_call(&mut r, peer("y")).await;
    let mut x = peer("x");
    x.auto_hold = false;
    add(&r, x).await;
    let z_addr = add(&r, peer("z")).await;
    r.stack
        .dial("x", &format!("sip:z@{z_addr}"), DialOptions::default())
        .await
        .unwrap();
    no_reinvite(&raw).await;
    assert_eq!(r.stack.call(&y1).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

// ---- end causes ------------------------------------------------------------------------

async fn cause_of(r: &mut Rig, id: &str) -> EndCause {
    state_of(r, id, CallState::Ended)
        .await
        .end
        .expect("end record")
        .cause
}

#[tokio::test]
async fn end_cause_hangup_and_remote_hangup() {
    let mut r = rig();
    let (a, b) = active_pair(&mut r).await;
    r.stack.hangup(&a).await.unwrap();
    assert_eq!(cause_of(&mut r, &a).await, EndCause::Hangup);
    assert_eq!(cause_of(&mut r, &b).await, EndCause::RemoteHangup);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn end_cause_missed_and_cancelled_elsewhere() {
    let mut r = rig();
    let b = add(&r, peer("b")).await;
    let cancel_of = |raw: &Raw, reason: &str| {
        raw.msg("CANCEL", None, 1, reason, "")
            .replace("z9hG4bKcancel1", "z9hG4bKinvite1")
    };
    // Abandoned while ringing: missed.
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    raw.send(&cancel_of(&raw, "")).await;
    let end = state_of(&mut r, &call.id, CallState::Ended)
        .await
        .end
        .unwrap();
    assert!(end.missed);
    assert_eq!(end.cause, EndCause::Missed);
    // Answered elsewhere: canceled, not missed.
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
    assert!(!end.missed);
    assert_eq!(end.cause, EndCause::Cancelled);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn end_cause_cancelled_by_us() {
    let mut r = rig();
    add(&r, peer("a")).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    r.stack.hangup(&id).await.unwrap();
    assert_eq!(cause_of(&mut r, &id).await, EndCause::Cancelled);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn end_cause_rejected_by_remote_and_by_us() {
    let mut r = rig();
    add(&r, peer("a")).await;
    // The remote answers our INVITE with 486.
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (invite, from) = recv_request(&callee, "INVITE").await;
    let resp = format!(
        "SIP/2.0 486 Busy Here\r\n{}\r\n{}\r\n{};tag=callee1\r\n{}\r\n{}\r\nContent-Length: 0\r\n\r\n",
        hdr(&invite, "via").join("\r\n"),
        hdr(&invite, "from")[0],
        hdr(&invite, "to")[0],
        hdr(&invite, "call-id")[0],
        hdr(&invite, "cseq")[0]
    );
    callee.send_to(resp.as_bytes(), from).await.unwrap();
    let end = state_of(&mut r, &id, CallState::Ended).await.end.unwrap();
    assert_eq!((end.code, end.cause), (486, EndCause::Rejected));

    // We decline an incoming call.
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    r.stack.reject(&call.id, Some(603), None).await.unwrap();
    assert_eq!(cause_of(&mut r, &call.id).await, EndCause::Rejected);
    r.stack.shutdown().await;
}

fn refer_notify(raw: &Raw, tag: &str, cseq: u32, event: &str, state: &str, frag: &str) -> String {
    raw.msg(
        "NOTIFY",
        Some(tag),
        cseq,
        &format!(
            "Event: {event}\r\nSubscription-State: {state}\r\nContent-Type: message/sipfrag\r\n"
        ),
        frag,
    )
    .replace("Content-Type: application/sdp\r\n", "")
}

/// Starts a blind transfer on an established inbound call and plays the transferee up to the
/// accepted REFER; returns the CSeq of the REFER.
async fn start_transfer(raw: &Raw, r: &Rig, call: &str) -> u32 {
    let stack = r.stack.clone();
    let id = call.to_string();
    let t = tokio::spawn(async move { stack.transfer_blind(&id, "sip:x@127.0.0.1:9").await });
    let refer = loop {
        let m = raw.recv_any(10).await.expect("message");
        if m.starts_with("INVITE ") {
            raw.reply(&m, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
                .await;
        } else if m.starts_with("REFER ") {
            break m;
        }
    };
    raw.reply(&refer, "202 Accepted", None).await;
    t.await.unwrap().unwrap();
    hdr(&refer, "cseq")[0]
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

/// The final 2xx NOTIFY decides the cause when our own BYE wins ...
#[tokio::test]
async fn end_cause_transferred_when_notify_comes_first() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, call, tag) = established(&mut r, &c).await;
    let cseq = start_transfer(&raw, &r, &call).await;
    raw.send(&refer_notify(
        &raw,
        &tag,
        10,
        &format!("refer;id={cseq}"),
        "terminated;reason=noresource",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    assert_eq!(cause_of(&mut r, &call).await, EndCause::Transferred);
    r.stack.shutdown().await;
}

/// ... and when the transferee's BYE gets in first: the call is recorded as ended by the remote
/// and the late final NOTIFY upgrades the cause to `transferred`.
#[tokio::test]
async fn end_cause_transferred_when_transferee_byes_first() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, call, tag) = established(&mut r, &c).await;
    let cseq = start_transfer(&raw, &r, &call).await;
    raw.send(&raw.msg("BYE", Some(&tag), 20, "", "")).await;
    let end = state_of(&mut r, &call, CallState::Ended).await.end.unwrap();
    assert_eq!(end.by, siple_sip::EndedBy::Remote);
    assert_eq!(end.cause, EndCause::RemoteHangup);
    raw.send(&refer_notify(
        &raw,
        &tag,
        21,
        &format!("refer;id={cseq}"),
        "terminated;reason=noresource",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    let up = until(&mut r, "cause upgrade", |e| {
        matches!(e, SipEvent::CallUpdated { call: c }
            if c.id == call && c.end.as_ref().is_some_and(|e| e.cause == EndCause::Transferred))
    })
    .await;
    let _ = up;
    assert_eq!(
        r.stack.call(&call).unwrap().end.unwrap().cause,
        EndCause::Transferred
    );
    // The session follows the NOTIFY, not the BYE order.
    let sid = r.stack.call(&call).unwrap().transfer.unwrap().session_id;
    assert_eq!(
        r.stack.transfer(&sid).unwrap().phase,
        siple_sip::TransferPhase::Transferred
    );
    r.stack.shutdown().await;
}

/// `pending` is `refer` while a REFER of ours awaits its outcome and clears on a failure.
#[tokio::test]
async fn pending_refer_while_transfer_runs() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, call, tag) = established(&mut r, &c).await;
    assert_eq!(r.stack.call(&call).unwrap().pending, None);
    let cseq = start_transfer(&raw, &r, &call).await;
    assert_eq!(
        r.stack.call(&call).unwrap().pending,
        Some(siple_sip::PendingOp::Refer)
    );
    raw.send(&refer_notify(
        &raw,
        &tag,
        10,
        &format!("refer;id={cseq}"),
        "terminated;reason=noresource",
        "SIP/2.0 503 Service Unavailable\r\n",
    ))
    .await;
    let up = updated(&mut r, &call, |c| c.pending.is_none()).await;
    assert_ne!(up.state, CallState::Ended);
    r.stack.shutdown().await;
}

/// `hold_changed_at` follows the hold flags of either side.
#[tokio::test]
async fn hold_changed_at_tracks_hold_flags() {
    let mut r = rig();
    let (a, b) = active_pair(&mut r).await;
    assert!(r.stack.call(&a).unwrap().hold_changed_at.is_none());
    r.stack.hold(&a).await.unwrap();
    let held_at = r.stack.call(&a).unwrap().hold_changed_at.expect("set");
    let b_info = updated(&mut r, &b, |c| c.hold.remote).await;
    assert!(b_info.hold_changed_at.is_some());
    tokio::time::sleep(Duration::from_millis(20)).await;
    r.stack.unhold(&a).await.unwrap();
    let resumed_at = r.stack.call(&a).unwrap().hold_changed_at.unwrap();
    assert!(resumed_at > held_at);
    r.stack.shutdown().await;
}

// ---- activate -------------------------------------------------------------------------

/// Two calls of account `a` to account `c`: returns `(a1, a2)`, `a2` is the live one (a1 was
/// auto-held by the second dial). Both far legs are answered.
async fn two_lines(r: &mut Rig) -> (String, String) {
    add(r, peer("a")).await;
    let c_addr = add(r, peer("c")).await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let a = r
            .stack
            .dial("a", &format!("sip:c@{c_addr}"), DialOptions::default())
            .await
            .unwrap();
        let c = incoming_on(r, "c").await;
        r.stack.answer(&c.id, Default::default()).await.unwrap();
        state_of(r, &a, CallState::Active).await;
        ids.push(a);
    }
    (ids.remove(0), ids.remove(0))
}

fn state(r: &Rig, id: &str) -> CallState {
    r.stack.call(id).unwrap().state
}

#[tokio::test]
async fn activate_swaps_lines_and_is_idempotent() {
    let mut r = rig();
    let (a1, a2) = two_lines(&mut r).await;
    assert_eq!(state(&r, &a1), CallState::Held, "the second dial held a1");
    assert_eq!(r.stack.active_line().as_deref(), Some(a2.as_str()));

    r.stack.activate(&a1).await.unwrap();
    assert_eq!(state(&r, &a1), CallState::Active);
    assert_eq!(state(&r, &a2), CallState::Held);
    assert_eq!(r.stack.active_line().as_deref(), Some(a1.as_str()));
    // The coordinator marked both calls while it worked.
    updated(&mut r, &a1, |c| {
        c.pending == Some(siple_sip::PendingOp::Activate)
    })
    .await;
    updated(&mut r, &a2, |c| {
        c.pending == Some(siple_sip::PendingOp::Hold)
    })
    .await;
    assert!(r.stack.call(&a1).unwrap().pending.is_none());
    assert!(r.stack.call(&a2).unwrap().pending.is_none());

    // Idempotent: nothing is sent, the focus is not touched again.
    let focus_before = r.media.stats(&a1).unwrap().focus;
    let held_at = r.stack.call(&a2).unwrap().hold_changed_at;
    r.stack.activate(&a1).await.unwrap();
    assert_eq!(r.media.stats(&a1).unwrap().focus, focus_before);
    assert_eq!(r.stack.call(&a2).unwrap().hold_changed_at, held_at);

    // And back.
    r.stack.activate(&a2).await.unwrap();
    assert_eq!(state(&r, &a2), CallState::Active);
    assert_eq!(state(&r, &a1), CallState::Held);
    assert_eq!(r.stack.active_line().as_deref(), Some(a2.as_str()));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn activate_rejects_calls_that_are_not_lines() {
    let mut r = rig();
    assert!(matches!(
        r.stack.activate("c999").await,
        Err(siple_sip::SipError::UnknownCall(_))
    ));
    // A ringing call is not a line.
    let b = add(&r, peer("b")).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(&mut r).await;
    assert!(r.stack.activate(&call.id).await.is_err());
    r.stack.hangup(&call.id).await.ok();
    // A call without an audio device is not a line either.
    let mut x = peer("x");
    x.audio_source = AudioSource::Silence;
    x.audio_sink = AudioSink::Null;
    let (_raw, x1, _) = raw_call(&mut r, x).await;
    assert!(r.stack.activate(&x1).await.is_err());
    assert_eq!(r.stack.active_line(), None);
    r.stack.shutdown().await;
}

/// `activate` holds other accounts' calls (hold_scope all) before it resumes the target.
#[tokio::test]
async fn activate_holds_other_accounts_then_resumes() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    let a1 = dial_c(&r, &c_addr).await;
    let c1 = incoming_on(&mut r, "c").await;
    r.stack.answer(&c1.id, Default::default()).await.unwrap();
    state_of(&mut r, &a1, CallState::Active).await;
    // A raw call on account x: x's answer holds a1 (all accounts).
    let x_addr = add(&r, peer("x")).await;
    let stack = r.stack.clone();
    let (raw, x1) = {
        let raw = Raw::new(&x_addr).await;
        raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
        let call = incoming_on(&mut r, "x").await;
        let id = call.id.clone();
        let answer = tokio::spawn(async move { stack.answer(&id, Default::default()).await });
        let ok = raw.final_response("CSeq: 1 INVITE").await;
        let tag = to_tag(&ok);
        raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
        answer.await.unwrap().unwrap();
        (raw, call.id)
    };
    assert_eq!(state(&r, &a1), CallState::Held);
    assert_eq!(state(&r, &x1), CallState::Active);

    let stack = r.stack.clone();
    let id = a1.clone();
    let act = tokio::spawn(async move { stack.activate(&id).await });
    accept_hold(&raw).await;
    act.await.unwrap().unwrap();
    assert_eq!(state(&r, &x1), CallState::Held, "x's call was held");
    assert_eq!(state(&r, &a1), CallState::Active);
    assert_eq!(r.stack.active_line().as_deref(), Some(a1.as_str()));
    r.stack.shutdown().await;
}

/// A failed hold aborts the activation: the target stays held (nothing is resumed) and the
/// error is the hold's.
#[tokio::test]
async fn activate_aborts_when_a_hold_fails() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    // x's call is a raw probe that will refuse the hold re-INVITE; x opts out of auto-hold so
    // setting the scene does not touch it.
    let mut x = peer("x");
    x.auto_hold = false;
    let (raw, x1, _) = raw_call(&mut r, x).await;
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    let a1 = dial_c(&r, &c_addr).await;
    let c1 = incoming_on(&mut r, "c").await;
    r.stack.answer(&c1.id, Default::default()).await.unwrap();
    state_of(&mut r, &a1, CallState::Active).await;
    r.stack.hold(&a1).await.unwrap();
    let focus_before = r.media.stats(&a1).unwrap().focus;

    let stack = r.stack.clone();
    let id = a1.clone();
    let act = tokio::spawn(async move { stack.activate(&id).await });
    let req = raw.request("INVITE", 8).await.expect("hold re-INVITE");
    raw.reply(&req, "488 Not Acceptable Here", None).await;
    let err = act.await.unwrap().unwrap_err();
    assert!(
        matches!(err, siple_sip::SipError::Status { code: 488, .. }),
        "{err:?}"
    );
    assert_eq!(
        state(&r, &a1),
        CallState::Held,
        "the target was not resumed"
    );
    assert!(r.media.stats(&a1).unwrap().held);
    assert_eq!(r.media.stats(&a1).unwrap().focus, focus_before);
    assert_eq!(state(&r, &x1), CallState::Active);
    assert!(r.stack.call(&a1).unwrap().pending.is_none());
    r.stack.shutdown().await;
}

/// Concurrent activations are serialized: no re-INVITE overlap, exactly one live line at the end.
#[tokio::test]
async fn concurrent_activations_never_leave_two_live_lines() {
    let mut r = rig();
    let (a1, a2) = two_lines(&mut r).await;
    let mut tasks = Vec::new();
    for i in 0..6 {
        let stack = r.stack.clone();
        let id = if i % 2 == 0 { a1.clone() } else { a2.clone() };
        tasks.push(tokio::spawn(async move { stack.activate(&id).await }));
    }
    for t in tasks {
        t.await.unwrap().expect("activation");
    }
    let live: Vec<_> = [&a1, &a2]
        .into_iter()
        .filter(|id| !r.stack.call(id).unwrap().hold.local)
        .collect();
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(r.stack.active_line().as_deref(), Some(live[0].as_str()));
    r.stack.shutdown().await;
}

// ---- answer modes ---------------------------------------------------------------------

/// A raw probe rings account `x` (a second line); returns the probe and the ringing call.
async fn ring(r: &mut Rig, x_addr: &str) -> (Raw, String) {
    let raw = Raw::new(x_addr).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming_on(r, "x").await;
    (raw, call.id)
}

async fn finish_answer(raw: &Raw) {
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    raw.send(&raw.msg("ACK", Some(&to_tag(&ok)), 1, "", ""))
        .await;
}

#[tokio::test]
async fn answer_mode_hold_others_ignores_auto_hold_false() {
    let mut r = rig();
    let mut x = peer("x");
    x.auto_hold = false;
    let x_addr = add(&r, x).await;
    let (raw1, x1, _) = {
        let raw = Raw::new(&x_addr).await;
        raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
        let call = incoming_on(&mut r, "x").await;
        r.stack.answer(&call.id, Default::default()).await.unwrap();
        finish_answer(&raw).await;
        (raw, call.id, ())
    };
    // Default mode with auto_hold = false: the first call is left alone.
    let (raw2, x2) = ring(&mut r, &x_addr).await;
    r.stack.answer(&x2, Default::default()).await.unwrap();
    finish_answer(&raw2).await;
    no_reinvite(&raw1).await;
    assert_eq!(state(&r, &x1), CallState::Active);
    r.stack.hangup(&x2).await.unwrap();

    // HoldOthers holds it anyway.
    let (raw3, x3) = ring(&mut r, &x_addr).await;
    let stack = r.stack.clone();
    let id = x3.clone();
    let ans = tokio::spawn(async move {
        stack
            .answer(
                &id,
                siple_sip::AnswerOptions {
                    mode: siple_sip::AnswerMode::HoldOthers,
                    ..Default::default()
                },
            )
            .await
    });
    accept_hold(&raw1).await;
    ans.await.unwrap().unwrap();
    finish_answer(&raw3).await;
    assert_eq!(state(&r, &x1), CallState::Held);
    assert_eq!(state(&r, &x3), CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn answer_mode_end_others_answers_first_then_hangs_up() {
    let mut r = rig();
    let x_addr = add(&r, peer("x")).await;
    let (raw1, x1, _) = established(&mut r, &x_addr).await;
    let (raw2, x2) = ring(&mut r, &x_addr).await;
    let outcome = r
        .stack
        .answer(
            &x2,
            siple_sip::AnswerOptions {
                mode: siple_sip::AnswerMode::EndOthers(x1.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Success as soon as the 200 is out; the other call waits for the ACK.
    assert_eq!(outcome, siple_sip::AnswerOutcome::EndOthersPendingAck);
    assert_eq!(state(&r, &x2), CallState::Active);
    assert_eq!(state(&r, &x1), CallState::Active);
    assert!(
        raw1.request("BYE", 1).await.is_none(),
        "no BYE before the ACK"
    );
    finish_answer(&raw2).await;
    // The first call got a BYE (and no hold re-INVITE before it).
    let bye = raw1.request("BYE", 5).await.expect("BYE of the ended line");
    raw1.ok_to(&bye).await;
    for _ in 0..100 {
        if state(&r, &x1) == CallState::Ended {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let end = r.stack.call(&x1).unwrap().end.expect("ended");
    assert_eq!(end.cause, EndCause::Hangup);
    assert_eq!(end.by, siple_sip::EndedBy::Local);
    assert_eq!(state(&r, &x2), CallState::Active);
    while let Ok(e) = r.events.try_recv() {
        r.buf.push(e);
    }

    // Ordering: the answered call became Active before the other one ended.
    let order: Vec<(String, CallState)> = r
        .buf
        .iter()
        .filter_map(|e| match e {
            SipEvent::CallState { call } if call.id == x1 || call.id == x2 => {
                Some((call.id.clone(), call.state))
            }
            _ => None,
        })
        .collect();
    let pos = |id: &str, st: CallState| order.iter().position(|(i, s)| i == id && *s == st);
    assert!(
        pos(&x2, CallState::Active).unwrap() < pos(&x1, CallState::Ended).unwrap(),
        "{order:?}"
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn answer_mode_end_others_failed_answer_leaves_the_other_call() {
    let mut r = rig();
    let x_addr = add(&r, peer("x")).await;
    let (raw1, x1, _) = established(&mut r, &x_addr).await;
    let (raw2, x2) = ring(&mut r, &x_addr).await;
    // The caller gives up before we answer: the answer fails.
    raw2.send(
        &raw2
            .msg("CANCEL", None, 1, "", "")
            .replace("z9hG4bKcancel1", "z9hG4bKinvite1"),
    )
    .await;
    state_of(&mut r, &x2, CallState::Ended).await;
    let res = r
        .stack
        .answer(
            &x2,
            siple_sip::AnswerOptions {
                mode: siple_sip::AnswerMode::EndOthers(x1.clone()),
                ..Default::default()
            },
        )
        .await;
    assert!(res.is_err());
    assert!(raw1.request("BYE", 1).await.is_none(), "no BYE");
    assert_eq!(state(&r, &x1), CallState::Active);

    // An unknown line to end is refused before anything is answered.
    let (_raw3, x3) = ring(&mut r, &x_addr).await;
    let res = r
        .stack
        .answer(
            &x3,
            siple_sip::AnswerOptions {
                mode: siple_sip::AnswerMode::EndOthers("c404".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(res, Err(siple_sip::SipError::UnknownCall(_))));
    assert_eq!(state(&r, &x3), CallState::Ringing);
    r.stack.shutdown().await;
}

// ---- review fixes: non-device calls, coordinator scope, ACK, markers ----------------------

fn bot(id: &str) -> siple_config::AccountConfig {
    let mut c = peer(id);
    c.audio_source = AudioSource::Silence;
    c.audio_sink = AudioSink::Null;
    c
}

async fn ring_on(r: &mut Rig, account: &str, addr: &str) -> (Raw, String) {
    let raw = Raw::new(addr).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming_on(r, account).await;
    (raw, call.id)
}

/// A device call on account x that must stay un-held in the tests below.
async fn user_call(r: &mut Rig) -> (Raw, String) {
    r.stack.set_hold_scope(HoldScope::All);
    let (raw, x1, _) = raw_call(r, peer("x")).await;
    (raw, x1)
}

#[tokio::test]
async fn non_device_dial_never_holds_device_calls() {
    let mut r = rig();
    let (raw, x1) = user_call(&mut r).await;
    add(&r, bot("bot")).await;
    add(&r, peer("a")).await;
    let z_addr = add(&r, peer("z")).await;
    let to_z = format!("sip:z@{z_addr}");
    // Account default is not a device.
    r.stack
        .dial("bot", &to_z, DialOptions::default())
        .await
        .unwrap();
    no_reinvite(&raw).await;
    // A device account dialing with a non-device override.
    r.stack
        .dial(
            "a",
            &to_z,
            DialOptions {
                source: Some(AudioSource::Silence),
                sink: Some(AudioSink::Null),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    no_reinvite(&raw).await;
    assert_eq!(state(&r, &x1), CallState::Active);
    // The opposite override: a non-device account dialing with the device does hold.
    let stack = r.stack.clone();
    let dial = tokio::spawn(async move {
        stack
            .dial(
                "bot",
                &to_z,
                DialOptions {
                    source: Some(AudioSource::Device),
                    sink: Some(AudioSink::Device),
                    ..Default::default()
                },
            )
            .await
    });
    accept_hold(&raw).await;
    dial.await.unwrap().unwrap();
    assert_eq!(state(&r, &x1), CallState::Held);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn non_device_answer_never_holds_device_calls() {
    let mut r = rig();
    let (raw, x1) = user_call(&mut r).await;
    let b_addr = add(&r, bot("bot")).await;
    let y_addr = add(&r, peer("y")).await;
    // Default answer on a non-device account.
    let (_p1, c1) = ring_on(&mut r, "bot", &b_addr).await;
    r.stack.answer(&c1, Default::default()).await.unwrap();
    // HoldOthers on a non-device call just answers.
    let (_p2, c2) = ring_on(&mut r, "bot", &b_addr).await;
    r.stack
        .answer(
            &c2,
            siple_sip::AnswerOptions {
                mode: siple_sip::AnswerMode::HoldOthers,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // A device account answering with a non-device override.
    let (_p3, c3) = ring_on(&mut r, "y", &y_addr).await;
    r.stack
        .answer(
            &c3,
            siple_sip::AnswerOptions {
                source: Some(AudioSource::Silence),
                sink: Some(AudioSink::Null),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for c in [&c1, &c2, &c3] {
        assert_eq!(state(&r, c), CallState::Active);
    }
    no_reinvite(&raw).await;
    assert_eq!(state(&r, &x1), CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn non_device_auto_answer_never_holds_device_calls() {
    let mut r = rig();
    let (raw, x1) = user_call(&mut r).await;
    let mut b = bot("bot");
    b.auto_answer = true;
    let b_addr = add(&r, b).await;
    let (_p, c) = ring_on(&mut r, "bot", &b_addr).await;
    state_of(&mut r, &c, CallState::Active).await;
    no_reinvite(&raw).await;
    assert_eq!(state(&r, &x1), CallState::Active);
    r.stack.shutdown().await;
}

/// The call placed on behalf of a REFER inherits the original call's non-device audio: it holds
/// nothing either.
#[tokio::test]
async fn non_device_refer_dial_never_holds_device_calls() {
    let mut r = rig();
    let (raw, x1) = user_call(&mut r).await;
    let b_addr = add(&r, bot("bot")).await;
    let z_addr = add(&r, bot("z")).await;
    let (probe, bot_call, tag) = established(&mut r, &b_addr).await;
    let extra = format!("Refer-To: <sip:z@{z_addr}>\r\n");
    probe
        .send(&probe.msg("REFER", Some(&tag), 2, &extra, ""))
        .await;
    let probe_task = tokio::spawn(async move {
        loop {
            let Some(m) = probe.recv_any(10).await else {
                return;
            };
            if m.starts_with("INVITE ") {
                probe
                    .reply(&m, "200 OK", Some(&probe.sdp_dir("recvonly", 2)))
                    .await;
            } else if m.starts_with("NOTIFY ") || m.starts_with("BYE ") {
                probe.ok_to(&m).await;
            }
        }
    });
    let zc = incoming_on(&mut r, "z").await;
    r.stack.answer(&zc.id, Default::default()).await.unwrap();
    state_of(&mut r, &bot_call, CallState::Ended).await;
    no_reinvite(&raw).await;
    assert_eq!(state(&r, &x1), CallState::Active);
    probe_task.abort();
    r.stack.shutdown().await;
}

/// `auto_hold` is read from the live account config, not the one the call was created with.
#[tokio::test]
async fn auto_hold_flag_is_read_live() {
    let mut r = rig();
    let (raw, x1) = user_call(&mut r).await;
    let mut x = peer("x");
    x.auto_hold = false;
    r.stack.update_account(x).await.unwrap();
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    dial_c(&r, &c_addr).await;
    no_reinvite(&raw).await;
    assert_eq!(state(&r, &x1), CallState::Active);
    r.stack.shutdown().await;
}

// ---- coordinator scope ------------------------------------------------------------------

/// A hold that the peer never answers does not block anything else: an unrelated answer is
/// quick, and a waiting `activate` is released the moment the stuck call hangs up.
#[tokio::test]
async fn stuck_hold_does_not_block_other_lines() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    let a1 = dial_c(&r, &c_addr).await;
    let c1 = incoming_on(&mut r, "c").await;
    r.stack.answer(&c1.id, Default::default()).await.unwrap();
    state_of(&mut r, &a1, CallState::Active).await;
    // The raw call on x is answered last: it holds a1 (all accounts).
    let x_addr = add(&r, peer("x")).await;
    let w_addr = add(&r, peer("w")).await;
    let stack = r.stack.clone();
    let (raw, x1) = {
        let raw = Raw::new(&x_addr).await;
        raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
        let call = incoming_on(&mut r, "x").await;
        let id = call.id.clone();
        let answer = tokio::spawn(async move { stack.answer(&id, Default::default()).await });
        let ok = raw.final_response("CSeq: 1 INVITE").await;
        raw.send(&raw.msg("ACK", Some(&to_tag(&ok)), 1, "", ""))
            .await;
        answer.await.unwrap().unwrap();
        (raw, call.id)
    };
    assert_eq!(state(&r, &a1), CallState::Held);

    // x1's hold re-INVITE is never answered.
    let stack = r.stack.clone();
    let id = x1.clone();
    let stuck = tokio::spawn(async move { stack.hold(&id).await });
    raw.request("INVITE", 5).await.expect("hold re-INVITE");

    // An unrelated answer is not delayed (the stuck line fails fast, it is not waited for).
    let (_p, w1) = ring_on(&mut r, "w", &w_addr).await;
    let t0 = std::time::Instant::now();
    r.stack.answer(&w1, Default::default()).await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert_eq!(state(&r, &w1), CallState::Active);
    r.stack.hangup(&w1).await.unwrap();

    // An explicit activate has to hold x1 and waits for its exchange ...
    let stack = r.stack.clone();
    let id = a1.clone();
    let act = tokio::spawn(async move { stack.activate(&id).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!act.is_finished());
    // ... until the stuck call hangs up: released promptly.
    r.stack.hangup(&x1).await.unwrap();
    let t0 = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(2), act)
        .await
        .expect("activate released by the hang-up")
        .unwrap()
        .unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1));
    assert_eq!(state(&r, &a1), CallState::Active);
    assert!(stuck.await.unwrap().is_err(), "the stuck hold gave up");
    r.stack.shutdown().await;
}

/// EndOthers succeeds once the 200 is out; the other call is hung up only when the ACK comes,
/// however late it is.
#[tokio::test]
async fn end_others_late_ack_still_ends_the_other_call() {
    siple_sip::set_end_others_ack_wait_ms(3000);
    let mut r = rig();
    let x_addr = add(&r, peer("x")).await;
    let (raw1, x1, _) = established(&mut r, &x_addr).await;
    let (raw2, x2) = ring(&mut r, &x_addr).await;
    let outcome = r
        .stack
        .answer(
            &x2,
            siple_sip::AnswerOptions {
                mode: siple_sip::AnswerMode::EndOthers(x1.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome, siple_sip::AnswerOutcome::EndOthersPendingAck);
    assert_eq!(state(&r, &x2), CallState::Active);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(state(&r, &x1), CallState::Active, "no ACK yet");
    assert!(raw1.request("BYE", 1).await.is_none());
    // The late ACK ends the other call.
    finish_answer(&raw2).await;
    let bye = raw1
        .request("BYE", 5)
        .await
        .expect("BYE after the late ACK");
    raw1.ok_to(&bye).await;
    state_of(&mut r, &x1, CallState::Ended).await;
    assert_eq!(state(&r, &x2), CallState::Active);
    r.stack.shutdown().await;
}

/// ... and without any ACK the other call is left alone and is the active line again.
#[tokio::test]
async fn end_others_without_ack_leaves_the_other_call() {
    siple_sip::set_end_others_ack_wait_ms(3000);
    let mut r = rig();
    let x_addr = add(&r, peer("x")).await;
    let (raw1, x1, _) = established(&mut r, &x_addr).await;
    let (_raw2, x2) = ring(&mut r, &x_addr).await;
    r.stack
        .answer(
            &x2,
            siple_sip::AnswerOptions {
                mode: siple_sip::AnswerMode::EndOthers(x1.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // The new call is Active and owns the line while its 200 awaits the ACK.
    assert_eq!(state(&r, &x2), CallState::Active);
    assert_eq!(r.stack.active_line().as_deref(), Some(x2.as_str()));
    tokio::time::sleep(Duration::from_millis(4000)).await;
    assert!(raw1.request("BYE", 1).await.is_none(), "no BYE");
    assert_eq!(state(&r, &x1), CallState::Active);
    assert_eq!(
        r.stack.active_line().as_deref(),
        Some(x1.as_str()),
        "the other call is the active line again"
    );
    r.stack.shutdown().await;
}

/// An outbound device call that becomes active holds the lines that were switched on while it
/// rang (the user dialed this party).
#[tokio::test]
async fn outbound_answer_holds_lines_activated_while_ringing() {
    let mut r = rig();
    let (a1, _a2_unused) = {
        add(&r, peer("a")).await;
        let c_addr = add(&r, peer("c")).await;
        let a = dial_c(&r, &c_addr).await;
        let c = incoming_on(&mut r, "c").await;
        r.stack.answer(&c.id, Default::default()).await.unwrap();
        state_of(&mut r, &a, CallState::Active).await;
        (a, ())
    };
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    // The second call rings; the first one is held by the dial.
    let a2 = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    assert_eq!(state(&r, &a1), CallState::Held);
    let (invite, from) = recv_request(&callee, "INVITE").await;
    // The user goes back to the first call while the second still rings.
    r.stack.activate(&a1).await.unwrap();
    assert_eq!(state(&r, &a1), CallState::Active);
    // The callee answers: the first line goes on hold again, the new one is the active line.
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
    let ok = format!(
        "SIP/2.0 200 OK\r\n{}\r\n{}\r\n{};tag=callee1\r\n{}\r\n{}\r\nContact: <sip:x@{caddr}>\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        hdr(&invite, "via").join("\r\n"),
        hdr(&invite, "from")[0],
        hdr(&invite, "to")[0],
        hdr(&invite, "call-id")[0],
        hdr(&invite, "cseq")[0],
        sdp.len()
    );
    callee.send_to(ok.as_bytes(), from).await.unwrap();
    state_of(&mut r, &a2, CallState::Active).await;
    for _ in 0..100 {
        if state(&r, &a1) == CallState::Held {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state(&r, &a1), CallState::Held, "two live lines");
    assert_eq!(r.stack.active_line().as_deref(), Some(a2.as_str()));
    r.stack.shutdown().await;
}

/// `active_line` is tracked synchronously: right after the operation, never stale.
#[tokio::test]
async fn active_line_is_synchronous() {
    let mut r = rig();
    let x_addr = add(&r, peer("x")).await;
    assert_eq!(r.stack.active_line(), None);
    let (raw1, x1, _) = established(&mut r, &x_addr).await;
    assert_eq!(r.stack.active_line().as_deref(), Some(x1.as_str()));
    // Holding the active line clears it ...
    let stack = r.stack.clone();
    let id = x1.clone();
    let hold = tokio::spawn(async move { stack.hold(&id).await });
    accept_hold(&raw1).await;
    hold.await.unwrap().unwrap();
    assert_eq!(r.stack.active_line(), None);
    // ... resuming sets it again.
    let stack = r.stack.clone();
    let id = x1.clone();
    let resume = tokio::spawn(async move { stack.unhold(&id).await });
    let req = raw1.request("INVITE", 5).await.unwrap();
    raw1.reply(&req, "200 OK", Some(&raw1.sdp_dir("sendrecv", 3)))
        .await;
    resume.await.unwrap().unwrap();
    assert_eq!(r.stack.active_line().as_deref(), Some(x1.as_str()));
    // Ending it clears it.
    r.stack.hangup(&x1).await.unwrap();
    assert_eq!(r.stack.active_line(), None);
    r.stack.shutdown().await;
}

/// While a REFER of ours is outstanding the session refuses hold / resume of the call, so no
/// re-INVITE can race the REFER and the `refer` marker stays.
#[tokio::test]
async fn hold_is_refused_while_the_refer_marker_is_set() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, call, _tag) = established(&mut r, &c).await;
    let _cseq = start_transfer(&raw, &r, &call).await;
    assert_eq!(
        r.stack.call(&call).unwrap().pending,
        Some(siple_sip::PendingOp::Refer)
    );
    let err = r.stack.unhold(&call).await.unwrap_err();
    assert!(
        matches!(err, siple_sip::SipError::CallState { .. }),
        "{err:?}"
    );
    assert!(raw.request("INVITE", 1).await.is_none(), "no re-INVITE");
    assert_eq!(
        r.stack.call(&call).unwrap().pending,
        Some(siple_sip::PendingOp::Refer),
        "the marker survives the refused resume"
    );
    r.stack.shutdown().await;
}

// ---- failed REFER: resume only when no other line is live ----------------------------

/// A raw transferee call on account x is asked (REFER) to call z, which refuses (DND).
async fn failed_refer(other_line: bool) -> (Rig, String, tokio::task::JoinHandle<()>) {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let mut x = peer("x");
    x.auto_hold = false;
    let x_addr = add(&r, x).await;
    let z_addr = add(&r, peer("z")).await;
    r.stack.set_dnd("z", true).await.unwrap();
    let (probe, x1, tag) = established(&mut r, &x_addr).await;
    if other_line {
        let mut y = peer("y");
        y.auto_hold = false;
        let y_addr = add(&r, y).await;
        let (_p, y1) = ring_on(&mut r, "y", &y_addr).await;
        r.stack.answer(&y1, Default::default()).await.unwrap();
    }
    let extra = format!("Refer-To: <sip:z@{z_addr}>\r\n");
    probe
        .send(&probe.msg("REFER", Some(&tag), 2, &extra, ""))
        .await;
    let task = tokio::spawn(async move {
        loop {
            let Some(m) = probe.recv_any(10).await else {
                return;
            };
            if m.starts_with("INVITE ") {
                let dir = if m.contains("a=sendonly") || m.contains("a=inactive") {
                    "recvonly"
                } else {
                    "sendrecv"
                };
                probe
                    .reply(&m, "200 OK", Some(&probe.sdp_dir(dir, 3)))
                    .await;
            } else if m.starts_with("NOTIFY ") {
                probe.ok_to(&m).await;
            }
        }
    });
    (r, x1, task)
}

#[tokio::test]
async fn failed_refer_resumes_when_no_other_line_is_live() {
    let (mut r, x1, task) = failed_refer(false).await;
    for _ in 0..100 {
        let i = r.stack.call(&x1).unwrap();
        if i.state == CallState::Active && i.hold_changed_at.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state(&r, &x1), CallState::Active);
    assert_eq!(r.stack.active_line().as_deref(), Some(x1.as_str()));
    task.abort();
    r.stack.shutdown().await;
    let _ = &mut r;
}

#[tokio::test]
async fn failed_refer_leaves_the_call_held_when_another_line_is_live() {
    let (r, x1, task) = failed_refer(true).await;
    for _ in 0..100 {
        if state(&r, &x1) == CallState::Held {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Let the failed REFER run its course (DND answers at once).
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        state(&r, &x1),
        CallState::Held,
        "not resumed over another line"
    );
    assert_ne!(r.stack.active_line().as_deref(), Some(x1.as_str()));
    task.abort();
    r.stack.shutdown().await;
}

/// An outbound call that becomes active while the coordinator is busy does not override what
/// the user did meanwhile: here the user held it, and the line they resumed stays live.
#[tokio::test]
async fn outbound_answered_rechecks_after_waiting_for_the_coordinator() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    add(&r, peer("a")).await;
    let c_addr = add(&r, peer("c")).await;
    let d_addr = add(&r, bot("d")).await;
    let a1 = dial_c(&r, &c_addr).await;
    let c1 = incoming_on(&mut r, "c").await;
    r.stack.answer(&c1.id, Default::default()).await.unwrap();
    state_of(&mut r, &a1, CallState::Active).await;
    // A second outbound call rings at d (a non-device peer answers it later); a1 is held by
    // the dial.
    let a2 = r
        .stack
        .dial("a", &format!("sip:d@{d_addr}"), DialOptions::default())
        .await
        .unwrap();
    let d2 = incoming_on(&mut r, "d").await;
    // A raw call on x whose hold never completes keeps the coordinator busy (activate waits).
    let x_addr = add(&r, peer("x")).await;
    let stack = r.stack.clone();
    let (raw, x1) = {
        let raw = Raw::new(&x_addr).await;
        raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
        let call = incoming_on(&mut r, "x").await;
        let id = call.id.clone();
        let answer = tokio::spawn(async move { stack.answer(&id, Default::default()).await });
        let ok = raw.final_response("CSeq: 1 INVITE").await;
        raw.send(&raw.msg("ACK", Some(&to_tag(&ok)), 1, "", ""))
            .await;
        answer.await.unwrap().unwrap();
        (raw, call.id)
    };
    let stack = r.stack.clone();
    let id = x1.clone();
    let stuck = tokio::spawn(async move { stack.hold(&id).await });
    raw.request("INVITE", 5).await.expect("hold re-INVITE");
    let stack = r.stack.clone();
    let id = a1.clone();
    let act = tokio::spawn(async move { stack.activate(&id).await });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // d answers a2 while the coordinator is busy; the user holds a2 straight away.
    r.stack.answer(&d2.id, Default::default()).await.unwrap();
    state_of(&mut r, &a2, CallState::Active).await;
    r.stack.hold(&a2).await.unwrap();
    assert_eq!(state(&r, &a2), CallState::Held);

    // Release the coordinator: activate completes and resumes a1; the queued outbound-answer
    // rule must not hold it again for the (now held) a2.
    r.stack.hangup(&x1).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), act)
        .await
        .expect("activate released")
        .unwrap()
        .unwrap();
    let _ = stuck.await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        state(&r, &a1),
        CallState::Active,
        "the resumed line stays live"
    );
    assert_eq!(r.stack.active_line().as_deref(), Some(a1.as_str()));
    r.stack.shutdown().await;
}

// ---- transfer sessions and non-device calls ----------------

/// `consult_start` on a call that uses no audio device (agent): the original is held, as a
/// phone does for a transfer, the consult call inherits the agent's audio binding, and the
/// user's un-held device call is never touched. Canceling resumes the agent call.
#[tokio::test]
async fn non_device_consult_never_holds_the_users_device_call() {
    let mut r = rig();
    let (user_raw, x1) = user_call(&mut r).await;
    let (bot_raw, bot_call, _tag) = raw_call(&mut r, bot("bot")).await;
    add(&r, peer("z")).await;
    let z_addr = r.stack.account_status("z").unwrap().local_addr.unwrap();
    let stack = r.stack.clone();
    let id = bot_call.clone();
    let consult = tokio::spawn(async move {
        stack
            .consult_start(&id, &format!("sip:z@{z_addr}"), DialOptions::default())
            .await
    });
    accept_hold(&bot_raw).await;
    let s = consult.await.unwrap().unwrap();
    assert!(s.we_held_original);
    assert_eq!(
        state(&r, &bot_call),
        CallState::Held,
        "the agent's call is held"
    );
    no_reinvite(&user_raw).await;
    assert_eq!(
        state(&r, &x1),
        CallState::Active,
        "the user's call is untouched"
    );
    let b = s.consult.clone().unwrap();
    let st = r.media.stats(&b).unwrap();
    assert_eq!(
        (st.source, st.sink),
        (Some(AudioSource::Silence), Some(AudioSink::Null)),
        "the consult inherits the original's audio"
    );
    // Cancel: the consult ends, the agent call is resumed; still nothing for the user's call.
    let stack = r.stack.clone();
    let sid = s.id.clone();
    let cancel = tokio::spawn(async move { stack.transfer_cancel(&sid).await });
    let req = bot_raw
        .request("INVITE", 8)
        .await
        .expect("resume re-INVITE");
    bot_raw
        .reply(&req, "200 OK", Some(&bot_raw.sdp_dir("sendrecv", 3)))
        .await;
    cancel.await.unwrap().unwrap();
    assert_eq!(state(&r, &bot_call), CallState::Active);
    no_reinvite(&user_raw).await;
    assert_eq!(state(&r, &x1), CallState::Active);
    r.stack.shutdown().await;
}

/// The consult's source / sink can be overridden.
#[tokio::test]
async fn consult_source_and_sink_can_be_overridden() {
    let mut r = rig();
    let (bot_raw, bot_call, _tag) = raw_call(&mut r, bot("bot")).await;
    add(&r, peer("z")).await;
    let z_addr = r.stack.account_status("z").unwrap().local_addr.unwrap();
    let stack = r.stack.clone();
    let id = bot_call.clone();
    let consult = tokio::spawn(async move {
        stack
            .consult_start(
                &id,
                &format!("sip:z@{z_addr}"),
                DialOptions {
                    source: Some(AudioSource::Tone(800.0)),
                    sink: Some(AudioSink::Null),
                    ..Default::default()
                },
            )
            .await
    });
    accept_hold(&bot_raw).await;
    let s = consult.await.unwrap().unwrap();
    let st = r.media.stats(&s.consult.unwrap()).unwrap();
    assert_eq!(st.source, Some(AudioSource::Tone(800.0)));
    r.stack.shutdown().await;
}

// ---- review fixes: detached session operations, unhold races, resume exclusivity ----------

/// Answers every re-INVITE (the first: `recvonly` after `hold_delay`, later ones `sendrecv`) and
/// REFER (`refer_status`) the probe receives for `secs`; resolves to the REFER count.
fn serve(
    raw: std::sync::Arc<Raw>,
    secs: u64,
    hold_delay: Duration,
    refer_status: &'static str,
) -> tokio::task::JoinHandle<usize> {
    tokio::spawn(async move {
        let end = tokio::time::Instant::now() + Duration::from_secs(secs);
        let (mut refers, mut invites) = (0usize, 0u32);
        while tokio::time::Instant::now() < end {
            let Some(m) = raw.recv_any(1).await else {
                continue;
            };
            if m.starts_with("INVITE ") {
                invites += 1;
                if invites == 1 {
                    tokio::time::sleep(hold_delay).await;
                }
                let dir = if invites == 1 { "recvonly" } else { "sendrecv" };
                raw.reply(&m, "200 OK", Some(&raw.sdp_dir(dir, 2 + invites)))
                    .await;
            } else if m.starts_with("REFER ") {
                refers += 1;
                raw.reply(&m, refer_status, None).await;
            }
        }
        refers
    })
}

/// A caller that gives up while the transfer still holds the call (control connection closed)
/// must not strand the session in REFERRING: the operation runs on, the REFER fails here (403)
/// and the call is resumed.
#[tokio::test]
async fn dropped_transfer_caller_does_not_strand_the_session() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, call, _tag) = established(&mut r, &c).await;
    let server = serve(
        std::sync::Arc::new(raw),
        10,
        Duration::from_millis(1500),
        "403 Forbidden",
    );
    let res = tokio::time::timeout(
        Duration::from_millis(300),
        r.stack.transfer_blind(&call, "sip:x@127.0.0.1:9"),
    )
    .await;
    assert!(res.is_err(), "the caller was dropped mid-hold");
    let sid = r.stack.call(&call).unwrap().transfer.unwrap().session_id;
    let mut phase = r.stack.transfer(&sid).unwrap().phase;
    for _ in 0..200 {
        phase = r.stack.transfer(&sid).unwrap().phase;
        if phase.is_terminal() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(phase, siple_sip::TransferPhase::Failed);
    // Not locked out: unhold is a plain success (the automatic resume may still be running).
    r.stack.unhold(&call).await.unwrap();
    assert_eq!(state(&r, &call), CallState::Active);
    r.stack.shutdown().await;
    assert_eq!(server.await.unwrap(), 1);
}

/// An agent's `unhold` right after the NOTIFY-reported failure races the automatic resume: the
/// session is settled before the final progress event, and the unhold waits for the resume's
/// re-INVITE instead of failing. While the REFER is outstanding `activate` is refused.
#[tokio::test]
async fn unhold_right_after_a_failed_transfer_is_ok() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, call, tag) = established(&mut r, &c).await;
    let cseq = start_transfer(&raw, &r, &call).await;
    let err = r.stack.activate(&call).await.unwrap_err();
    assert!(
        matches!(err, siple_sip::SipError::CallState { .. }),
        "{err:?}"
    );
    let raw = std::sync::Arc::new(raw);
    let server = serve(raw.clone(), 6, Duration::ZERO, "202 Accepted");
    raw.send(&refer_notify(
        &raw,
        &tag,
        10,
        &format!("refer;id={cseq}"),
        "terminated;reason=noresource",
        "SIP/2.0 503 Service Unavailable\r\n",
    ))
    .await;
    until(&mut r, "final progress", |e| {
        matches!(e, SipEvent::TransferProgress { call_id, is_final: true, .. } if *call_id == call)
    })
    .await;
    r.stack.unhold(&call).await.unwrap();
    assert_eq!(state(&r, &call), CallState::Active);
    r.stack.shutdown().await;
    let _ = server.await;
}

/// Canceling a consult resumes the original only after the other un-held device lines went on
/// hold: never two live lines.
#[tokio::test]
async fn cancel_holds_other_lines_before_resuming_the_original() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let (raw_a, a_call, _) = raw_call(&mut r, peer("p")).await;
    add(&r, peer("z")).await;
    let z_addr = r.stack.account_status("z").unwrap().local_addr.unwrap();
    let stack = r.stack.clone();
    let id = a_call.clone();
    let start = tokio::spawn(async move {
        stack
            .consult_start(&id, &format!("sip:z@{z_addr}"), DialOptions::default())
            .await
    });
    accept_hold(&raw_a).await;
    let s = start.await.unwrap().unwrap();
    // The user answers another call meanwhile.
    let q_addr = add(&r, peer("q")).await;
    let (raw_x, x_call) = ring_on(&mut r, "q", &q_addr).await;
    r.stack.answer(&x_call, Default::default()).await.unwrap();
    let ok = raw_x.final_response("CSeq: 1 INVITE").await;
    raw_x
        .send(&raw_x.msg("ACK", Some(&to_tag(&ok)), 1, "", ""))
        .await;
    assert_eq!(state(&r, &x_call), CallState::Active);
    let stack = r.stack.clone();
    let sid = s.id.clone();
    let cancel = tokio::spawn(async move { stack.transfer_cancel(&sid).await });
    accept_hold(&raw_x).await; // X is held first ...
    let req = raw_a.request("INVITE", 8).await.expect("resume re-INVITE");
    raw_a
        .reply(&req, "200 OK", Some(&raw_a.sdp_dir("sendrecv", 4)))
        .await;
    cancel.await.unwrap().unwrap();
    assert_eq!(state(&r, &a_call), CallState::Active);
    assert_eq!(state(&r, &x_call), CallState::Held, "only A is un-held");
    r.stack.shutdown().await;
}

/// The original ends while `consult_start` holds and dials: no session for a dead call, the
/// consult is hung up.
#[tokio::test]
async fn consult_start_refuses_an_original_that_ended_meanwhile() {
    let mut r = rig();
    let (raw, a_call, tag) = raw_call(&mut r, bot("bot")).await;
    add(&r, peer("z")).await;
    let z_addr = r.stack.account_status("z").unwrap().local_addr.unwrap();
    let stack = r.stack.clone();
    let id = a_call.clone();
    let start = tokio::spawn(async move {
        stack
            .consult_start(&id, &format!("sip:z@{z_addr}"), DialOptions::default())
            .await
    });
    let _hold = raw.request("INVITE", 8).await.expect("hold re-INVITE");
    raw.send(&raw.msg("BYE", Some(&tag), 20, "", "")).await;
    let res = start.await.unwrap();
    assert!(res.is_err(), "{res:?}");
    assert!(r.stack.transfers().is_empty(), "{:?}", r.stack.transfers());
    r.stack.shutdown().await;
}

/// While a REFER is outstanding only the call carrying it is left alone: answering another call
/// still puts the consult leg on hold (never two un-held lines).
#[tokio::test]
async fn answering_during_transferring_holds_the_consult_leg() {
    let mut r = rig();
    r.stack.set_hold_scope(HoldScope::All);
    let (raw_a, a_call, _) = raw_call(&mut r, peer("p")).await;
    add(&r, peer("z")).await;
    let z_addr = r.stack.account_status("z").unwrap().local_addr.unwrap();
    let raw_a = std::sync::Arc::new(raw_a);
    let server = serve(raw_a.clone(), 8, Duration::ZERO, "202 Accepted");
    let s = r
        .stack
        .consult_start(&a_call, &format!("sip:z@{z_addr}"), DialOptions::default())
        .await
        .unwrap();
    let b_call = s.consult.clone().unwrap();
    let zc = incoming_on(&mut r, "z").await;
    r.stack.answer(&zc.id, Default::default()).await.unwrap();
    state_of(&mut r, &b_call, CallState::Active).await;
    // The REFER (on A's dialog) is accepted and never finishes.
    let done = r
        .stack
        .transfer_complete(&s.id, siple_sip::CompleteMode::Attended)
        .await
        .unwrap();
    assert_eq!(done.phase, siple_sip::TransferPhase::Transferring);
    // The user answers another call: the consult leg B goes on hold.
    let q_addr = add(&r, peer("q")).await;
    let (raw_x, x_call) = ring_on(&mut r, "q", &q_addr).await;
    r.stack.answer(&x_call, Default::default()).await.unwrap();
    let ok = raw_x.final_response("CSeq: 1 INVITE").await;
    raw_x
        .send(&raw_x.msg("ACK", Some(&to_tag(&ok)), 1, "", ""))
        .await;
    assert_eq!(
        state(&r, &b_call),
        CallState::Held,
        "B is held, A untouched"
    );
    r.stack.shutdown().await;
    let _ = server.await;
}
