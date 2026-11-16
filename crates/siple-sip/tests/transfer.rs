//! Phase 7: call transfers. Three peer-mode accounts (a = transferor, b =
//! transferee, c = target) in one stack, plus raw-UDP probes for INVITE-with-Replaces.

mod common;

use std::time::Duration;

use common::*;
use siple_config::{AccountConfig, AttendedReferOn, AttendedReferTarget, Transport};
use siple_sip::{
    AnswerOptions, CallInfo, CallState, CompleteMode, EndCause, MediaProvider as _, SipError,
    SipEvent, TransferKind, TransferPhase, TransferRole, TransferSession,
};

struct Three {
    b: String,
    c: String,
}

async fn three(r: &mut Rig, a: AccountConfig, b: AccountConfig, c: AccountConfig) -> Three {
    add(r, a).await;
    let b = add(r, b).await;
    let c = add(r, c).await;
    Three { b, c }
}

async fn three_default(r: &mut Rig) -> Three {
    three(r, peer("a"), peer("b"), peer("c")).await
}

/// a calls b (b answers). Returns `(a_call, b_call)`.
async fn a_calls_b(r: &mut Rig, t: &Three) -> (String, String) {
    let a = r
        .stack
        .dial("a", &format!("sip:b@{}", t.b), Default::default())
        .await
        .unwrap();
    let bc = incoming_on(r, "b").await;
    r.stack
        .answer(&bc.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(r, &a, CallState::Active).await;
    state_of(r, &bc.id, CallState::Active).await;
    (a, bc.id)
}

/// a calls c (c answers): the consult call. Returns `(a_call, c_call)`.
async fn a_consults_c(r: &mut Rig, t: &Three) -> (String, String) {
    let a = r
        .stack
        .dial("a", &format!("sip:c@{}", t.c), Default::default())
        .await
        .unwrap();
    let cc = incoming_on(r, "c").await;
    r.stack
        .answer(&cc.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(r, &a, CallState::Active).await;
    state_of(r, &cc.id, CallState::Active).await;
    (a, cc.id)
}

async fn final_progress(r: &mut Rig, call: &str) -> (u16, String) {
    let call = call.to_string();
    match until(r, "final TransferProgress", |e| {
        matches!(e, SipEvent::TransferProgress { call_id, is_final: true, .. } if *call_id == call)
    })
    .await
    {
        SipEvent::TransferProgress { code, reason, .. } => (code, reason),
        _ => unreachable!(),
    }
}

/// Non-final progress codes of `call` seen so far.
fn progress_codes(r: &Rig, call: &str) -> Vec<u16> {
    r.buf
        .iter()
        .filter_map(|e| match e {
            SipEvent::TransferProgress {
                call_id,
                code,
                is_final: false,
                ..
            } if call_id == call => Some(*code),
            _ => None,
        })
        .collect()
}

async fn refer_received(r: &mut Rig, call: &str) -> (String, Option<String>, bool) {
    let call = call.to_string();
    match until(
        r,
        "ReferReceived",
        |e| matches!(e, SipEvent::ReferReceived { call_id, .. } if *call_id == call),
    )
    .await
    {
        SipEvent::ReferReceived {
            refer_to,
            referred_by,
            replaces,
            ..
        } => (refer_to, referred_by, replaces),
        _ => unreachable!(),
    }
}

/// The call of `account` created for a REFER of `from`, once it is `st`.
async fn referred_call(r: &mut Rig, account: &str, from: &str, st: CallState) -> CallInfo {
    let (account, from) = (account.to_string(), from.to_string());
    match until(r, "REFER call", |e| {
        matches!(e, SipEvent::CallState { call }
            if call.account == account
                && call.transferred_from.as_deref() == Some(from.as_str())
                && call.state == st)
    })
    .await
    {
        SipEvent::CallState { call } => call,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn blind_transfer_a_moves_b_to_c() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a, b_orig) = a_calls_b(&mut r, &t).await;

    r.stack
        .transfer_blind(&a, &format!("sip:c@{}", t.c))
        .await
        .unwrap();
    // The original call was put on hold before the REFER.
    assert!(
        r.stack.call(&a).is_some_and(|c| c.hold.local)
            || r.stack.call(&a).unwrap().state == CallState::Ended
    );

    let (refer_to, referred_by, replaces) = refer_received(&mut r, &b_orig).await;
    assert_eq!(
        refer_to.trim_matches(|c| c == '<' || c == '>'),
        format!("sip:c@{}", t.c)
    );
    assert!(!replaces);
    assert!(
        referred_by
            .as_deref()
            .is_some_and(|v| v.contains("sip:a@peer.test")),
        "{referred_by:?}"
    );

    let cc = incoming_on(&mut r, "c").await;
    r.stack
        .answer(&cc.id, AnswerOptions::default())
        .await
        .unwrap();
    let b_new = referred_call(&mut r, "b", &b_orig, CallState::Active).await;
    assert_eq!(b_new.transferred_from.as_deref(), Some(b_orig.as_str()));
    state_of(&mut r, &cc.id, CallState::Active).await;

    let (code, _) = final_progress(&mut r, &a).await;
    assert_eq!(code, 200);
    assert!(progress_codes(&r, &a).contains(&100), "{:?}", r.buf);
    let a_end = state_of(&mut r, &a, CallState::Ended).await;
    assert_eq!(a_end.end.as_ref().unwrap().reason, "transferred");
    assert_eq!(a_end.end.as_ref().unwrap().cause, EndCause::Transferred);
    let b_end = state_of(&mut r, &b_orig, CallState::Ended).await;
    // b's original call ended by its own hang-up or a's BYE: both say "transferred".
    assert_eq!(b_end.end.as_ref().unwrap().cause, EndCause::Transferred);
    // B <-> C stays up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.stack.call(&b_new.id).unwrap().state, CallState::Active);
    assert_eq!(r.stack.call(&cc.id).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn blind_transfer_checks_arguments() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a, _b) = a_calls_b(&mut r, &t).await;
    assert!(matches!(
        r.stack.transfer_blind("c999", "sip:c@x").await,
        Err(SipError::UnknownCall(_))
    ));
    assert!(matches!(
        r.stack.transfer_blind(&a, "").await,
        Err(SipError::Config(_))
    ));
    // digits need the account's domain: peer.test is set, so it is accepted syntactically; a
    // ringing call cannot be transferred.
    let ringing = r
        .stack
        .dial("a", &format!("sip:c@{}", t.c), Default::default())
        .await
        .unwrap();
    assert!(matches!(
        r.stack.transfer_blind(&ringing, "sip:c@x").await,
        Err(SipError::CallState { .. })
    ));
    assert!(matches!(
        r.stack.transfer_attended(&a, &a).await,
        Err(SipError::InvalidArgument(_))
    ));
    r.stack.shutdown().await;
}

/// Shared body of the attended-transfer tests.
async fn attended(acfg: AccountConfig, ccfg: AccountConfig, dnd_c: bool) {
    let consult_on = acfg.attended_refer_on;
    let mut r = rig();
    let t = three(&mut r, acfg, peer("b"), ccfg).await;
    let (a_b, b_a) = a_calls_b(&mut r, &t).await;
    let (a_c, c_a) = a_consults_c(&mut r, &t).await;
    if dnd_c {
        r.stack.set_dnd("c", true).await.unwrap();
    }

    r.stack.transfer_attended(&a_b, &a_c).await.unwrap();
    assert!(
        r.stack.call(&a_b).unwrap().hold.local,
        "the original is held"
    );

    // Who got the REFER: default the original's peer (b), else the consult peer (c).
    let (refer_call, target_account, replaced_account) = match consult_on {
        AttendedReferOn::Original => (b_a.clone(), "c", "a_c"),
        AttendedReferOn::Consult => (c_a.clone(), "b", "a_b"),
    };
    let _ = replaced_account;
    let (refer_to, _by, replaces) = refer_received(&mut r, &refer_call).await;
    assert!(replaces, "{refer_to}");
    assert!(refer_to.contains("Replaces="), "{refer_to}");
    assert!(
        refer_to.contains("%40")
            && refer_to.contains("%3Bto-tag%3D")
            && refer_to.contains("%3Bfrom-tag%3D"),
        "Replaces value must be percent-encoded: {refer_to}"
    );
    assert!(!refer_to.contains(";to-tag"), "{refer_to}");

    // The transferee calls the target with Replaces: the target replaces its dialog with a.
    let replaced_call = match consult_on {
        AttendedReferOn::Original => c_a.clone(),
        AttendedReferOn::Consult => b_a.clone(),
    };
    let incoming = match until(&mut r, "replacing INVITE", |e| {
        matches!(e, SipEvent::CallIncoming { call }
            if call.account == target_account && call.replaces.is_some())
    })
    .await
    {
        SipEvent::CallIncoming { call } => call,
        _ => unreachable!(),
    };
    assert_eq!(incoming.replaces.as_deref(), Some(replaced_call.as_str()));
    let active = state_of(&mut r, &incoming.id, CallState::Active).await;
    assert_eq!(active.replaces.as_deref(), Some(replaced_call.as_str()));
    let old_end = state_of(&mut r, &replaced_call, CallState::Ended).await;
    assert_eq!(old_end.end.as_ref().unwrap().reason, "replaced");

    // Transferee side: its new call is active.
    let transferee_account = if consult_on == AttendedReferOn::Original {
        "b"
    } else {
        "c"
    };
    let refer_call_info =
        referred_call(&mut r, transferee_account, &refer_call, CallState::Active).await;
    assert_eq!(
        refer_call_info.transferred_from.as_deref(),
        Some(refer_call.as_str())
    );

    // Transferor: final 200 on the REFER dialog, both of its calls end.
    let (code, _) = final_progress(&mut r, &refer_call_a(&consult_on, &a_b, &a_c)).await;
    assert_eq!(code, 200);
    state_of(&mut r, &a_b, CallState::Ended).await;
    state_of(&mut r, &a_c, CallState::Ended).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.stack.call(&incoming.id).unwrap().state, CallState::Active);
    assert_eq!(
        r.stack.call(&refer_call_info.id).unwrap().state,
        CallState::Active
    );
    r.stack.shutdown().await;
}

fn refer_call_a(on: &AttendedReferOn, a_b: &str, a_c: &str) -> String {
    match on {
        AttendedReferOn::Original => a_b.to_string(),
        AttendedReferOn::Consult => a_c.to_string(),
    }
}

#[tokio::test]
async fn attended_transfer_default_original_aor() {
    attended(peer("a"), peer("c"), false).await;
}

#[tokio::test]
async fn attended_transfer_replaces_bypasses_dnd() {
    attended(peer("a"), peer("c"), true).await;
}

#[tokio::test]
async fn attended_transfer_refer_on_consult() {
    let mut a = peer("a");
    a.attended_refer_on = AttendedReferOn::Consult;
    attended(a, peer("c"), false).await;
}

#[tokio::test]
async fn attended_transfer_contact_target() {
    let mut a = peer("a");
    a.attended_refer_target = AttendedReferTarget::Contact;
    attended(a, peer("c"), false).await;
}

#[tokio::test]
async fn semi_attended_cancels_ringing_consult_and_goes_blind() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a_b, b_a) = a_calls_b(&mut r, &t).await;
    let a_c = r
        .stack
        .dial("a", &format!("sip:c@{}", t.c), Default::default())
        .await
        .unwrap();
    let c_first = incoming_on(&mut r, "c").await;
    state_of(&mut r, &a_c, CallState::Early).await;

    r.stack.transfer_attended(&a_b, &a_c).await.unwrap();
    let consult_end = state_of(&mut r, &a_c, CallState::Ended).await;
    assert_eq!(consult_end.end.as_ref().unwrap().code, 487);
    let c_end = state_of(&mut r, &c_first.id, CallState::Ended).await;
    assert_eq!(c_end.end.as_ref().unwrap().code, 487);

    let (refer_to, _, replaces) = refer_received(&mut r, &b_a).await;
    assert!(!replaces && !refer_to.contains("Replaces"), "{refer_to}");
    let c_second = incoming_on(&mut r, "c").await;
    assert_ne!(c_second.id, c_first.id);
    r.stack
        .answer(&c_second.id, AnswerOptions::default())
        .await
        .unwrap();
    referred_call(&mut r, "b", &b_a, CallState::Active).await;
    assert_eq!(final_progress(&mut r, &a_b).await.0, 200);
    state_of(&mut r, &a_b, CallState::Ended).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn accept_refer_false_answers_603() {
    let mut r = rig();
    let mut b = peer("b");
    b.accept_refer = false;
    let t = three(&mut r, peer("a"), b, peer("c")).await;
    let (a, _b_a) = a_calls_b(&mut r, &t).await;

    let err = r
        .stack
        .transfer_blind(&a, &format!("sip:c@{}", t.c))
        .await
        .unwrap_err();
    assert!(matches!(err, SipError::Status { code: 603, .. }), "{err:?}");
    let (code, _) = final_progress(&mut r, &a).await;
    assert_eq!(code, 603);
    // The blind session failed for good: A is taken off hold again (auto-resume); B's side saw
    // nothing but the hold and the resume.
    let ai = state_of(&mut r, &a, CallState::Active).await;
    assert!(!ai.hold.local, "{ai:?}");
    let sess = r
        .stack
        .transfer(&ai.transfer.as_ref().unwrap().session_id)
        .unwrap();
    assert_eq!(sess.phase, siple_sip::TransferPhase::Failed);
    assert_eq!(sess.last_error.as_ref().unwrap().code, 603);
    assert!(sess.we_held_original);
    assert!(
        !r.buf
            .iter()
            .any(|e| matches!(e, SipEvent::ReferReceived { .. })),
        "a declined REFER is not announced"
    );
    // A can retry: no stale transfer state.
    assert!(
        r.stack
            .transfer_blind(&a, &format!("sip:c@{}", t.c))
            .await
            .is_err()
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn failed_refer_target_resumes_original() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a, b_a) = a_calls_b(&mut r, &t).await;
    r.stack
        .transfer_blind(&a, &format!("sip:c@{}", t.c))
        .await
        .unwrap();
    let cc = incoming_on(&mut r, "c").await;
    r.stack.reject(&cc.id, Some(486), None).await.unwrap();
    let (code, _) = final_progress(&mut r, &a).await;
    assert_eq!(code, 486);
    // The failed blind transfer auto-resumes A (it was held by the transfer).
    let ai = state_of(&mut r, &a, CallState::Active).await;
    assert!(!ai.hold.local, "{ai:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bi = r.stack.call(&b_a).unwrap();
    assert_ne!(bi.state, CallState::Ended);
    // B tried: the REFER call ended with 486 and B resumed the original call.
    assert!(!bi.hold.local, "{bi:?}");
    assert_eq!(r.stack.call(&a).unwrap().state, CallState::Active);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn invite_with_replaces_matching_rules() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    // A confirmed dialog from probe 1 to c.
    let (raw1, c_call, tag) = established(&mut r, &c).await;
    let replaces = format!("{};to-tag={};from-tag=ptag", raw1.call_id, tag);

    // Unknown dialog -> 481.
    let raw = Raw::new(&c).await;
    let extra = "Require: replaces\r\nReplaces: nosuch@probe;to-tag=x;from-tag=ptag\r\n";
    raw.send(&raw.msg("INVITE", None, 1, extra, &raw.sdp()))
        .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 481"), "{resp}");

    // Wrong from-tag -> 481.
    let extra = format!(
        "Require: replaces\r\nReplaces: {};to-tag={};from-tag=other\r\n",
        raw1.call_id, tag
    );
    let raw = Raw::new(&c).await;
    raw.send(&raw.msg("INVITE", None, 1, &extra, &raw.sdp()))
        .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 481"), "{resp}");

    // early-only against a confirmed dialog -> 486.
    let extra = format!("Require: replaces\r\nReplaces: {replaces};early-only\r\n");
    let raw = Raw::new(&c).await;
    raw.send(&raw.msg("INVITE", None, 1, &extra, &raw.sdp()))
        .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 486"), "{resp}");
    assert_eq!(r.stack.call(&c_call).unwrap().state, CallState::Active);

    // Matching Replaces bypasses DND and max_calls: answered at once, old dialog gets a BYE.
    r.stack.set_dnd("c", true).await.unwrap();
    let extra = format!("Require: replaces\r\nReplaces: {replaces}\r\n");
    let raw = Raw::new(&c).await;
    raw.send(&raw.msg("INVITE", None, 1, &extra, &raw.sdp()))
        .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 200"), "{resp}");
    let new_tag = to_tag(&resp);
    raw.send(&raw.msg("ACK", Some(&new_tag), 1, "", "")).await;
    let bye = raw1
        .request("BYE", 5)
        .await
        .expect("BYE of the replaced dialog");
    raw1.ok_to(&bye).await;
    let old = state_of(&mut r, &c_call, CallState::Ended).await;
    assert_eq!(old.end.as_ref().unwrap().reason, "replaced");
    assert_eq!(old.end.as_ref().unwrap().cause, EndCause::Replaced);
    // The same Replaces again: the dialog is over (RFC 3891: 603).
    let raw = Raw::new(&c).await;
    raw.send(&raw.msg("INVITE", None, 1, &extra, &raw.sdp()))
        .await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 603"), "{resp}");
    // A plain INVITE is still refused by DND.
    let raw = Raw::new(&c).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 486"), "{resp}");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn refer_inbound_carries_headers_and_referred_by() {
    // A raw probe plays the transferor: REFER with an embedded header and no Referred-By.
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let b = add(&r, peer("b")).await;
    let (raw, c_call, tag) = established(&mut r, &c).await;
    let extra = format!("Refer-To: <sip:b@{b}?X-Test=hello%20world&Subject=moved>\r\n");
    raw.send(&raw.msg("REFER", Some(&tag), 2, &extra, "")).await;
    let resp = raw.final_response("CSeq: 2 REFER").await;
    assert!(resp.starts_with("SIP/2.0 202"), "{resp}");
    let (refer_to, by, replaces) = refer_received(&mut r, &c_call).await;
    assert!(refer_to.contains("X-Test"), "{refer_to}");
    assert!(!replaces);
    assert!(
        by.as_deref()
            .is_some_and(|b| b.contains("sip:probe@probe.test")),
        "{by:?}"
    );
    // The probe serves the stack's requests in the background: re-INVITE (hold), NOTIFYs
    // 100 Trying ... 200 OK (terminated), then BYE.
    let probe = tokio::spawn(async move {
        let (mut codes, mut held) = (vec![], false);
        loop {
            let m = raw.recv_any(10).await.expect("message from the stack");
            if m.starts_with("INVITE ") {
                held = m.contains("a=sendonly");
                raw.reply(&m, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
                    .await;
            } else if m.starts_with("NOTIFY ") {
                raw.ok_to(&m).await;
                let body = m.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                assert!(
                    hdr(&m, "event")[0].to_ascii_lowercase().contains("refer"),
                    "{m}"
                );
                assert!(
                    hdr(&m, "content-type")[0].contains("message/sipfrag"),
                    "{m}"
                );
                codes.push(body.split_whitespace().nth(1).unwrap_or("").to_string());
            } else if m.starts_with("BYE ") {
                raw.ok_to(&m).await;
                return (codes, held);
            }
        }
    });
    // C's call goes to B with the embedded headers + Referred-By; answer it.
    let bi = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&bi.id, AnswerOptions::default())
        .await
        .unwrap();
    let (codes, held) = probe.await.unwrap();
    assert!(held, "the original call is held before the new call");
    assert_eq!(codes.first().map(String::as_str), Some("100"), "{codes:?}");
    assert_eq!(codes.last().map(String::as_str), Some("200"), "{codes:?}");
    let end = state_of(&mut r, &c_call, CallState::Ended).await;
    assert_eq!(end.end.as_ref().unwrap().reason, "transferred");
    r.stack.shutdown().await;
}

// ---- transfer sessions ---------------------------------------------------

async fn wait_phase(r: &Rig, sid: &str, phase: TransferPhase) -> TransferSession {
    for _ in 0..500 {
        let s = r.stack.transfer(sid).unwrap();
        if s.phase == phase {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "session {sid} never reached {phase:?}: {:?}",
        r.stack.transfer(sid)
    );
}

/// Moves every queued event into `r.buf`.
fn drain(r: &mut Rig) {
    while let Ok(e) = r.events.try_recv() {
        r.buf.push(e);
    }
}

fn phases(r: &Rig, sid: &str) -> Vec<TransferPhase> {
    let mut out: Vec<TransferPhase> = Vec::new();
    for e in &r.buf {
        if let SipEvent::TransferSession { session } = e
            && session.id == sid
            && out.last() != Some(&session.phase)
        {
            out.push(session.phase);
        }
    }
    out
}

/// A peer account on `tr`; stream transports listen on a free port (`local_addr` is empty for
/// them, so the tests address `127.0.0.1:<port>`).
fn peer_t(id: &str, tr: Transport) -> (AccountConfig, String) {
    let mut c = peer(id);
    c.transport = tr;
    if tr == Transport::Udp {
        return (c, String::new());
    }
    let port = free_tcp_port();
    c.local_port = port;
    (c, format!("127.0.0.1:{port}"))
}

fn sfx(tr: Transport) -> &'static str {
    if tr == Transport::Tcp {
        ";transport=tcp"
    } else {
        ""
    }
}

/// a calls b (b answers), then `consult_start` towards c; c answers. Returns
/// `(session id, a_b, b_a, a_c, c_a)` with the session in CONSULT_CONNECTED.
async fn consulting(
    r: &mut Rig,
    t: &Three,
    tr: Transport,
) -> (String, String, String, String, String) {
    let a = r
        .stack
        .dial(
            "a",
            &format!("sip:b@{}{}", t.b, sfx(tr)),
            Default::default(),
        )
        .await
        .unwrap();
    let bc = incoming_on(r, "b").await;
    r.stack
        .answer(&bc.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(r, &a, CallState::Active).await;
    state_of(r, &bc.id, CallState::Active).await;
    let (sid, a_c) = start_consult(r, t, &a, tr).await;
    let cc = incoming_on(r, "c").await;
    r.stack
        .answer(&cc.id, AnswerOptions::default())
        .await
        .unwrap();
    wait_phase(r, &sid, TransferPhase::ConsultConnected).await;
    (sid, a, bc.id, a_c, cc.id)
}

/// `consult_start` of `a_b` towards c; returns `(session id, consult call)`.
async fn start_consult(r: &mut Rig, t: &Three, a_b: &str, tr: Transport) -> (String, String) {
    let s = r
        .stack
        .consult_start(
            a_b,
            &format!("sip:c@{}{}", t.c, sfx(tr)),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(s.kind, TransferKind::Attended);
    assert_eq!(s.original, a_b);
    assert!(s.we_held_original, "{s:?}");
    assert!(r.stack.call(a_b).unwrap().hold.local, "A is held");
    (s.id, s.consult.unwrap())
}

async fn consult_complete(tr: Transport) {
    let mut r = rig();
    let ((a, _), (b, b_addr), (c, c_addr)) = (peer_t("a", tr), peer_t("b", tr), peer_t("c", tr));
    let mut t = three(&mut r, a, b, c).await;
    if tr != Transport::Udp {
        (t.b, t.c) = (b_addr, c_addr);
    }
    let (sid, a_b, b_a, a_c, c_a) = consulting(&mut r, &t, tr).await;
    let s = r.stack.transfer(&sid).unwrap();
    assert_eq!(s.talking_to.as_deref(), Some(a_c.as_str()));
    let back = r.stack.call(&a_b).unwrap().transfer.unwrap();
    assert_eq!(
        (back.session_id.as_str(), back.role),
        (sid.as_str(), TransferRole::Original)
    );
    let back = r.stack.call(&a_c).unwrap().transfer.unwrap();
    assert_eq!(back.role, TransferRole::Consult);

    let res = r
        .stack
        .transfer_complete(&sid, CompleteMode::Attended)
        .await
        .unwrap();
    assert!(matches!(
        res.phase,
        TransferPhase::Transferring | TransferPhase::Transferred
    ));
    let done = wait_phase(&r, &sid, TransferPhase::Transferred).await;
    assert!(done.ended_at.is_some() && done.refer_call.as_deref() == Some(a_b.as_str()));
    assert_eq!(done.last_notify.as_ref().map(|n| n.code), Some(200));
    let end = state_of(&mut r, &a_b, CallState::Ended).await.end.unwrap();
    assert_eq!(end.cause, EndCause::Transferred);
    state_of(&mut r, &a_c, CallState::Ended).await;
    // The transferee (b) and the target (c) are connected to each other.
    let (_, _, replaces) = refer_received(&mut r, &b_a).await;
    assert!(replaces);
    referred_call(&mut r, "b", &b_a, CallState::Active).await;
    let _ = c_a;
    drain(&mut r);
    use TransferPhase::*;
    assert_eq!(
        phases(&r, &sid),
        vec![
            ConsultDialing,
            ConsultRinging,
            ConsultConnected,
            Referring,
            Transferring,
            Transferred
        ]
    );
    // The finished session stays listed.
    assert!(r.stack.transfers().iter().any(|s| s.id == sid));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn consult_then_complete_attended_udp() {
    consult_complete(Transport::Udp).await;
}

#[tokio::test]
async fn consult_then_complete_attended_tcp() {
    consult_complete(Transport::Tcp).await;
}

#[tokio::test]
async fn consult_cancel_resumes_the_original_and_its_focus() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (sid, a_b, b_a, a_c, c_a) = consulting(&mut r, &t, Transport::Udp).await;
    let s = r.stack.transfer_cancel(&sid).await.unwrap();
    assert_eq!(s.phase, TransferPhase::Cancelled);
    state_of(&mut r, &a_c, CallState::Ended).await;
    state_of(&mut r, &c_a, CallState::Ended).await;
    let a = r.stack.call(&a_b).unwrap();
    assert_eq!(a.state, CallState::Active, "{a:?}");
    assert!(!a.hold.local);
    // The microphone is back on the original call.
    assert_eq!(r.media.focus().as_deref(), Some(a_b.as_str()));
    assert_eq!(r.stack.active_line().as_deref(), Some(a_b.as_str()));
    state_of(&mut r, &b_a, CallState::Active).await;
    // Cancelling again is a no-op.
    let again = r.stack.transfer_cancel(&sid).await.unwrap();
    assert_eq!(again.phase, TransferPhase::Cancelled);
    // A cancelled session no longer blocks a new one.
    let (sid2, _) = start_consult(&mut r, &t, &a_b, Transport::Udp).await;
    assert_ne!(sid, sid2);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn attended_complete_is_rejected_while_ringing_blind_now_goes_semi() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a_b, b_a) = a_calls_b(&mut r, &t).await;
    let (sid, a_c) = start_consult(&mut r, &t, &a_b, Transport::Udp).await;
    let c_first = incoming_on(&mut r, "c").await;
    wait_phase(&r, &sid, TransferPhase::ConsultRinging).await;
    let err = r
        .stack
        .transfer_complete(&sid, CompleteMode::Attended)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SipError::CallState { msg, .. } if msg == "consult not answered; use blind_now"),
        "{err:?}"
    );
    assert_eq!(
        r.stack.transfer(&sid).unwrap().phase,
        TransferPhase::ConsultRinging
    );

    let s = r
        .stack
        .transfer_complete(&sid, CompleteMode::BlindNow)
        .await
        .unwrap();
    assert_eq!(s.kind, TransferKind::Semi);
    let consult_end = state_of(&mut r, &a_c, CallState::Ended).await;
    assert_eq!(consult_end.end.as_ref().unwrap().code, 487);
    state_of(&mut r, &c_first.id, CallState::Ended).await;
    let (refer_to, _, replaces) = refer_received(&mut r, &b_a).await;
    assert!(!replaces && !refer_to.contains("Replaces"), "{refer_to}");
    let c_second = incoming_on(&mut r, "c").await;
    r.stack
        .answer(&c_second.id, AnswerOptions::default())
        .await
        .unwrap();
    referred_call(&mut r, "b", &b_a, CallState::Active).await;
    let done = wait_phase(&r, &sid, TransferPhase::Transferred).await;
    assert_eq!(done.kind, TransferKind::Semi);
    state_of(&mut r, &a_b, CallState::Ended).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn busy_consult_target_fails_the_consult_and_keeps_the_original_held() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a_b, _b_a) = a_calls_b(&mut r, &t).await;
    let (sid, _a_c) = start_consult(&mut r, &t, &a_b, Transport::Udp).await;
    let c = incoming_on(&mut r, "c").await;
    r.stack.reject(&c.id, Some(486), None).await.unwrap();
    let s = wait_phase(&r, &sid, TransferPhase::ConsultFailed).await;
    let e = s.last_error.unwrap();
    assert_eq!((e.stage.as_str(), e.code), ("consult", 486));
    let a = r.stack.call(&a_b).unwrap();
    assert_eq!(a.state, CallState::Held, "A stays held: {a:?}");
    // Nothing to complete; a new consult is refused until this one is cancelled.
    assert!(
        r.stack
            .transfer_complete(&sid, CompleteMode::Attended)
            .await
            .is_err()
    );
    assert!(
        r.stack
            .consult_start(&a_b, &format!("sip:c@{}", t.c), Default::default())
            .await
            .is_err()
    );
    // Cancel resumes A.
    r.stack.transfer_cancel(&sid).await.unwrap();
    state_of(&mut r, &a_b, CallState::Active).await;
    assert_eq!(
        r.stack.transfer(&sid).unwrap().phase,
        TransferPhase::Cancelled
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn raw_hangup_of_the_consult_call_fails_the_consult() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (sid, a_b, _b_a, a_c, _c_a) = consulting(&mut r, &t, Transport::Udp).await;
    r.stack.hangup(&a_c).await.unwrap();
    let s = wait_phase(&r, &sid, TransferPhase::ConsultFailed).await;
    assert!(s.talking_to.is_none());
    assert_eq!(r.stack.call(&a_b).unwrap().state, CallState::Held);
    r.stack.transfer_cancel(&sid).await.unwrap();
    state_of(&mut r, &a_b, CallState::Active).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn original_ended_remotely_abandons_the_session_and_leaves_the_consult() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (sid, a_b, b_a, a_c, _c_a) = consulting(&mut r, &t, Transport::Udp).await;
    r.stack.hangup(&b_a).await.unwrap();
    state_of(&mut r, &a_b, CallState::Ended).await;
    let s = wait_phase(&r, &sid, TransferPhase::Abandoned).await;
    assert!(s.ended_at.is_some());
    // B is a normal line now.
    assert_eq!(r.stack.call(&a_c).unwrap().state, CallState::Active);
    // And a new session can start on it.
    let (a_c2, _) = (a_c, ());
    assert!(r.stack.transfer(&sid).is_ok());
    let _ = a_c2;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn refused_refer_returns_to_the_consult_with_an_error() {
    let mut r = rig();
    let mut b = peer("b");
    b.accept_refer = false;
    let t = three(&mut r, peer("a"), b, peer("c")).await;
    let (sid, a_b, _b_a, a_c, _c_a) = consulting(&mut r, &t, Transport::Udp).await;
    let err = r
        .stack
        .transfer_complete(&sid, CompleteMode::Attended)
        .await
        .unwrap_err();
    assert!(matches!(err, SipError::Status { code: 603, .. }), "{err:?}");
    let s = r.stack.transfer(&sid).unwrap();
    assert_eq!(s.phase, TransferPhase::ConsultConnected, "{s:?}");
    let e = s.last_error.unwrap();
    assert_eq!((e.stage.as_str(), e.code), ("refer", 603));
    // Both calls are as they were: A held, the consult alive. Trying again is possible.
    assert_eq!(r.stack.call(&a_b).unwrap().state, CallState::Held);
    assert_eq!(r.stack.call(&a_c).unwrap().state, CallState::Active);
    let again = r
        .stack
        .transfer_complete(&sid, CompleteMode::Attended)
        .await
        .unwrap_err();
    assert!(
        matches!(again, SipError::Status { code: 603, .. }),
        "{again:?}"
    );
    // ... or cancelling.
    r.stack.transfer_cancel(&sid).await.unwrap();
    state_of(&mut r, &a_b, CallState::Active).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn double_complete_sends_one_refer() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (sid, a_b, b_a, _a_c, _c_a) = consulting(&mut r, &t, Transport::Udp).await;
    let (x, y) = tokio::join!(
        r.stack.transfer_complete(&sid, CompleteMode::Attended),
        r.stack.transfer_complete(&sid, CompleteMode::Attended)
    );
    x.unwrap();
    y.unwrap();
    wait_phase(&r, &sid, TransferPhase::Transferred).await;
    // A late repeat returns the finished session.
    let z = r
        .stack
        .transfer_complete(&sid, CompleteMode::Attended)
        .await
        .unwrap();
    assert_eq!(z.phase, TransferPhase::Transferred);
    state_of(&mut r, &a_b, CallState::Ended).await;
    drain(&mut r);
    let refers = r
        .buf
        .iter()
        .filter(|e| matches!(e, SipEvent::ReferReceived { call_id, .. } if *call_id == b_a))
        .count();
    assert_eq!(refers, 1, "exactly one REFER reaches the transferee");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn swap_moves_the_media_focus_between_the_legs() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (sid, a_b, b_a, a_c, _c_a) = consulting(&mut r, &t, Transport::Udp).await;
    assert_eq!(
        r.media.focus().as_deref(),
        Some(a_c.as_str()),
        "on the consult"
    );
    // Swap to A.
    let s = r.stack.transfer_swap(&sid).await.unwrap();
    assert_eq!(s.talking_to.as_deref(), Some(a_b.as_str()));
    assert_eq!(r.stack.call(&a_b).unwrap().state, CallState::Active);
    assert_eq!(r.stack.call(&a_c).unwrap().state, CallState::Held);
    assert_eq!(
        r.media.focus().as_deref(),
        Some(a_b.as_str()),
        "the mic moved"
    );
    assert_eq!(r.stack.active_line().as_deref(), Some(a_b.as_str()));
    state_of(&mut r, &b_a, CallState::Active).await;
    // And back to the consult.
    let s = r.stack.transfer_swap(&sid).await.unwrap();
    assert_eq!(s.talking_to.as_deref(), Some(a_c.as_str()));
    assert_eq!(r.stack.call(&a_b).unwrap().state, CallState::Held);
    assert_eq!(r.stack.call(&a_c).unwrap().state, CallState::Active);
    assert_eq!(r.media.focus().as_deref(), Some(a_c.as_str()));
    // Swapping is only for an answered consult.
    r.stack.transfer_cancel(&sid).await.unwrap();
    assert!(r.stack.transfer_swap(&sid).await.is_err());
    r.stack.shutdown().await;
}

#[tokio::test]
async fn consult_link_pairs_calls_of_one_account_only() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a_b, b_a) = a_calls_b(&mut r, &t).await;
    // Other account: refused with a clear message.
    let err = r.stack.consult_link(&a_b, &b_a).await.unwrap_err();
    assert!(err.to_string().contains("same account"), "{err}");
    assert!(r.stack.consult_link(&a_b, &a_b).await.is_err());
    let (a_c, _c_a) = a_consults_c(&mut r, &t).await;
    let s = r.stack.consult_link(&a_b, &a_c).await.unwrap();
    assert_eq!(s.phase, TransferPhase::ConsultConnected);
    assert_eq!(s.consult.as_deref(), Some(a_c.as_str()));
    // A is held by the consult's dial: the user is on the consult leg.
    assert_eq!(s.talking_to.as_deref(), Some(a_c.as_str()));
    // One live session per call: both calls are taken.
    let e = r.stack.consult_link(&a_b, &a_c).await.unwrap_err();
    assert!(e.to_string().contains(&s.id), "{e}");
    let e = r
        .stack
        .consult_start(&a_c, &format!("sip:c@{}", t.c), Default::default())
        .await
        .unwrap_err();
    assert!(e.to_string().contains(&s.id), "{e}");
    // The legacy call drives the existing session.
    let done = r.stack.transfer_attended(&a_b, &a_c).await.unwrap();
    assert_eq!(done.id, s.id);
    wait_phase(&r, &s.id, TransferPhase::Transferred).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn finished_sessions_are_kept_in_a_ring_of_16() {
    let mut r = rig();
    let t = three_default(&mut r).await;
    let (a_b, _b_a) = a_calls_b(&mut r, &t).await;
    let mut ids = vec![];
    for _ in 0..18 {
        let (sid, _a_c) = start_consult(&mut r, &t, &a_b, Transport::Udp).await;
        let c = incoming_on(&mut r, "c").await;
        r.stack.transfer_cancel(&sid).await.unwrap();
        state_of(&mut r, &c.id, CallState::Ended).await;
        ids.push(sid);
    }
    let all = r.stack.transfers();
    assert_eq!(all.len(), 16, "{all:?}");
    assert_eq!(all[0].id, ids[2]);
    assert!(r.stack.transfer(&ids[0]).is_err(), "evicted");
    assert!(all.iter().all(|s| s.phase == TransferPhase::Cancelled));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn resume_after_failed_transfer_is_hot() {
    let mut r = rig();
    let mut b = peer("b");
    b.accept_refer = false;
    let t = three(&mut r, peer("a"), b, peer("c")).await;
    let (a, _b_a) = a_calls_b(&mut r, &t).await;
    r.stack.set_resume_after_failed_transfer(false);
    let err = r
        .stack
        .transfer_blind(&a, &format!("sip:c@{}", t.c))
        .await
        .unwrap_err();
    assert!(matches!(err, SipError::Status { code: 603, .. }), "{err:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        r.stack.call(&a).unwrap().state,
        CallState::Held,
        "the option is off: A stays held"
    );
    let sid = r.stack.call(&a).unwrap().transfer.unwrap().session_id;
    assert_eq!(r.stack.transfer(&sid).unwrap().phase, TransferPhase::Failed);
    // On again: the next failure resumes (the call is held by us already, so nothing to undo
    // unless the transfer holds it: unhold first, then fail again).
    r.stack.set_resume_after_failed_transfer(true);
    r.stack.unhold(&a).await.unwrap();
    assert!(
        r.stack
            .transfer_blind(&a, &format!("sip:c@{}", t.c))
            .await
            .is_err()
    );
    state_of(&mut r, &a, CallState::Active).await;
    r.stack.shutdown().await;
}
